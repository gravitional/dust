use crate::{Entry, Result};
use std::{
    collections::VecDeque,
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
};

pub(crate) struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(crate) fn open_directory(path: &Path) -> Result<Handle> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Handle(h))
}

pub fn identity(path: &Path) -> Result<(u64, u64)> {
    let h = open_directory(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(h.0, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        info.dwVolumeSerialNumber as u64,
    ))
}

#[derive(Debug)]
pub struct Record {
    pub file_id: u64,
    pub eof: u64,
    pub allocation: u64,
    pub attributes: u32,
    pub name: OsString,
}

fn should_descend(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_DIRECTORY != 0 && attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

pub fn enumerate(path: &Path) -> Result<Vec<Record>> {
    let h = open_directory(path)?;
    // u64 storage guarantees alignment for the native variable-length records.
    let mut buffer = vec![0u64; 64 * 1024 / 8];
    let mut result = Vec::new();
    loop {
        buffer.fill(0);
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h.0,
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error.into());
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8) };
        result.extend(parse_records(bytes)?);
    }
    Ok(result)
}

fn parse_records(buffer: &[u8]) -> Result<Vec<Record>> {
    const HEADER: usize = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let mut offset = 0;
    let mut result = Vec::new();
    loop {
        if offset + HEADER > buffer.len() {
            return Err("truncated directory record".into());
        }
        let u32_at = |i| u32::from_le_bytes(buffer[offset + i..offset + i + 4].try_into().unwrap());
        let i64_at = |i| i64::from_le_bytes(buffer[offset + i..offset + i + 8].try_into().unwrap());
        let next = u32_at(0) as usize;
        let len = u32_at(60) as usize;
        let record_end = offset
            .checked_add(HEADER)
            .and_then(|value| value.checked_add(len))
            .ok_or("record length overflow")?;
        let end = if next == 0 {
            record_end
        } else {
            offset.checked_add(next).ok_or("offset overflow")?
        };
        if len % 2 != 0 || end > buffer.len() || record_end > end || (next != 0 && next < HEADER) {
            return Err("invalid directory record length".into());
        }
        let name: Vec<u16> = buffer[offset + HEADER..offset + HEADER + len]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let name = OsString::from_wide(&name);
        if name != "." && name != ".." {
            result.push(Record {
                file_id: i64_at(96) as u64,
                eof: i64_at(40).max(0) as u64,
                allocation: i64_at(48).max(0) as u64,
                attributes: u32_at(56),
                name,
            });
        }
        if next == 0 {
            break;
        }
        offset = end;
    }
    Ok(result)
}

pub fn scan(root: &Path, _ignored: Option<&[PathBuf]>, first_id: i64) -> Result<Vec<Entry>> {
    let root = std::fs::canonicalize(root)?;
    if !std::fs::symlink_metadata(&root)?.is_dir() {
        return Err("index scope must be a directory".into());
    }
    let (root_id, _) = identity(&root)?;
    let mut entries = vec![Entry {
        id: first_id,
        parent: None,
        file_id: root_id,
        parent_file_id: 0,
        path: crate::path_key(&root)?,
        is_dir: true,
        apparent: 0,
        allocated: 0,
        attributes: 0,
    }];
    let mut next_id = first_id + 1;
    let mut queue = VecDeque::from([(root, root_id, first_id)]);
    while let Some((path, fid, id)) = queue.pop_front() {
        for r in enumerate(&path)? {
            let child = path.join(&r.name);
            let is_dir = r.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let apparent = if is_dir { 0 } else { r.eof };
            let allocated = if is_dir { 0 } else { r.allocation };
            entries.push(Entry {
                id: next_id,
                parent: Some(id),
                file_id: r.file_id,
                parent_file_id: fid,
                path: crate::path_key(&child)?,
                is_dir,
                apparent,
                allocated,
                attributes: r.attributes,
            });
            // Reparse points may cross volumes or form cycles. Keep the link
            // itself in the index, but never follow its target during a scan.
            if should_descend(r.attributes) {
                queue.push_back((child, r.file_id, next_id));
            }
            next_id += 1;
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batched_enumeration_does_not_truncate_and_preserves_ids() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..1000 {
            std::fs::write(tmp.path().join(format!("file_{i:04}.txt")), b"abc").unwrap();
        }
        let records = enumerate(tmp.path()).unwrap();
        assert_eq!(records.len(), 1000);
        let r = &records[0];
        let (id, _) = identity(&tmp.path().join(&r.name)).unwrap();
        assert_eq!(r.file_id, id);
        assert_eq!(r.eof, 3);
        let entries = scan(tmp.path(), None, 1).unwrap();
        assert_eq!(entries.len(), 1001);
        assert_eq!(entries[1].parent_file_id, entries[0].file_id);
    }
    #[test]
    fn rejects_malformed_record() {
        assert!(parse_records(&[0; 32]).is_err());
    }

    #[test]
    fn records_reparse_directories_without_following_them() {
        assert!(should_descend(FILE_ATTRIBUTE_DIRECTORY));
        assert!(!should_descend(
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT
        ));
        assert!(!should_descend(FILE_ATTRIBUTE_REPARSE_POINT));
    }
}
