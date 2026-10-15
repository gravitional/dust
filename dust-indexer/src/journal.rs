use crate::{
    Result, scanner,
    store::{Cursor, Store},
};
use std::{
    collections::{HashMap, HashSet},
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    Storage::FileSystem::*,
    System::{IO::DeviceIoControl, Ioctl::*},
};

pub struct Journal {
    handle: scanner::Handle,
    serial: u64,
}
#[derive(Debug)]
struct Change {
    file: u64,
    parent: u64,
}
impl Journal {
    pub fn open(scope: &Path) -> Result<Self> {
        let (_, serial) = scanner::identity(scope)?;
        let s = crate::path_key(scope)?;
        if s.as_bytes().get(1) != Some(&b':') {
            return Err("journal requires a drive-letter NTFS volume".into());
        }
        let volume = format!(r"\\.\{}", &s[..2]);
        let wide: Vec<u16> = std::ffi::OsStr::new(&volume)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x80000000,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null_mut(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let journal = Self {
            handle: scanner::Handle(h),
            serial,
        };
        journal.state()?;
        Ok(journal)
    }
    pub fn state(&self) -> Result<Cursor> {
        let state = self.raw_state()?;
        Ok(Cursor {
            volume_serial: self.serial,
            journal_id: state.UsnJournalID,
            next_usn: state.NextUsn,
        })
    }
    fn raw_state(&self) -> Result<USN_JOURNAL_DATA_V0> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut returned = 0;
        if unsafe {
            DeviceIoControl(
                self.handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                null_mut(),
                0,
                (&mut data as *mut USN_JOURNAL_DATA_V0).cast(),
                std::mem::size_of_val(&data) as u32,
                &mut returned,
                null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if returned < std::mem::size_of_val(&data) as u32 {
            return Err("short journal query".into());
        }
        Ok(data)
    }
    pub fn synchronize(&self, store: &mut Store) -> Result<()> {
        let saved = store
            .cursor()?
            .ok_or("index is a snapshot without USN cursor; rebuild elevated")?;
        let raw = self.raw_state()?;
        if saved.volume_serial != self.serial
            || saved.journal_id != raw.UsnJournalID
            || saved.next_usn < raw.FirstUsn
            || saved.next_usn < raw.LowestValidUsn
            || saved.next_usn > raw.NextUsn
        {
            return Err("journal reset/wrapped or volume replaced; rebuild this scope".into());
        }
        if saved.next_usn == raw.NextUsn {
            return Ok(());
        }
        let target = raw.NextUsn;
        let mut start = saved.next_usn;
        let mut changes = Vec::new();
        let mut buf = vec![0u8; 1024 * 1024];
        while start < target {
            let input = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: start,
                ReasonMask: 0xffffffff,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: saved.journal_id,
            };
            let mut returned = 0;
            if unsafe {
                DeviceIoControl(
                    self.handle.0,
                    FSCTL_READ_USN_JOURNAL,
                    (&input as *const READ_USN_JOURNAL_DATA_V0).cast(),
                    std::mem::size_of_val(&input) as u32,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let data = &buf[..returned as usize];
            if data.len() < 8 {
                return Err("short USN reply".into());
            }
            let next = i64::from_le_bytes(data[..8].try_into()?);
            if next <= start {
                return Err("USN cursor did not advance".into());
            }
            changes.extend(parse_changes(&data[8..], target)?);
            start = next;
        }
        let cursor = Cursor {
            next_usn: target,
            ..saved
        };
        let entries = store.entries()?;
        let mut by_file = HashMap::<u64, Vec<usize>>::new();
        let mut by_id = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_file.entry(e.file_id).or_default().push(i);
            by_id.insert(e.id, i);
        }
        let mut dirty = HashSet::new();
        for c in changes {
            if let Some(indices) = by_file.get(&c.parent) {
                for i in indices {
                    if entries[*i].is_dir {
                        dirty.insert(entries[*i].id);
                    }
                }
            }
            if let Some(indices) = by_file.get(&c.file) {
                for i in indices {
                    let e = &entries[*i];
                    if let Some(p) = e.parent {
                        dirty.insert(p);
                    } else {
                        dirty.insert(e.id);
                    }
                }
            }
        }
        // Coalesce overlapping trees. Only touched trees are re-enumerated;
        // old and new rename parents, deletions and all hard-link aliases qualify.
        let roots: Vec<_> = dirty
            .iter()
            .copied()
            .filter(|id| {
                let mut p = entries[by_id[id]].parent;
                while let Some(parent) = p {
                    if dirty.contains(&parent) {
                        return false;
                    }
                    p = entries[by_id[&parent]].parent;
                }
                true
            })
            .collect();
        if roots.is_empty() {
            store.advance(&cursor)?;
            return Ok(());
        }
        let mut replacements = Vec::new();
        let mut next = store.next_id()?;
        for id in &roots {
            let old = &entries[by_id[id]];
            if !Path::new(&old.path).exists() {
                return Err("changed directory no longer exists; rebuild scope".into());
            }
            let mut scanned = scanner::scan(Path::new(&old.path), None, next)?;
            let scanned_root = scanned[0].id;
            for entry in &mut scanned {
                if entry.parent == Some(scanned_root) {
                    entry.parent = Some(old.id);
                }
            }
            scanned[0].id = old.id;
            scanned[0].parent = old.parent;
            scanned[0].parent_file_id = old.parent_file_id;
            next += scanned.len() as i64;
            replacements.extend(scanned);
        }
        // Check journal continuity once more before committing the cursor.
        let after = self.raw_state()?;
        if after.UsnJournalID != cursor.journal_id || after.FirstUsn > cursor.next_usn {
            return Err("journal changed during refresh".into());
        }
        store.apply(&roots, &replacements, &cursor)
    }
}

fn parse_changes(mut data: &[u8], target: i64) -> Result<Vec<Change>> {
    let mut result = Vec::new();
    while !data.is_empty() {
        if data.len() < 8 {
            return Err("truncated USN header".into());
        }
        let len = u32::from_le_bytes(data[..4].try_into()?) as usize;
        let version = u16::from_le_bytes(data[4..6].try_into()?);
        if len < 60 || len > data.len() || len % 8 != 0 {
            return Err("invalid USN record length".into());
        }
        if version != 2 {
            return Err(
                format!("unsupported USN record version {version}; rebuild required").into(),
            );
        }
        let usn = i64::from_le_bytes(data[24..32].try_into()?);
        if usn < target {
            result.push(Change {
                file: u64::from_le_bytes(data[8..16].try_into()?),
                parent: u64::from_le_bytes(data[16..24].try_into()?),
            });
        }
        data = &data[len..];
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_usn_records() {
        assert!(parse_changes(&[0; 16], 99).is_err());
        let mut b = vec![0; 64];
        b[..4].copy_from_slice(&64u32.to_le_bytes());
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        b[8..16].copy_from_slice(&9u64.to_le_bytes());
        b[16..24].copy_from_slice(&8u64.to_le_bytes());
        b[24..32].copy_from_slice(&10i64.to_le_bytes());
        let c = parse_changes(&b, 11).unwrap();
        assert_eq!(c[0].file, 9);
        assert_eq!(c[0].parent, 8);
        assert!(parse_changes(&b, 10).unwrap().is_empty());
    }
}
