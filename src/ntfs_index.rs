//! Optional Windows persistent index.
//!
//! The index is intentionally separate from the normal walker.  It stores a
//! compact path/size snapshot and records the NTFS USN journal cursor used to
//! decide when the snapshot needs rebuilding.  A missing or unavailable USN
//! journal never changes the default walker behavior.

use crate::node::Node;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
pub fn build_index(_path: &Path) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "NTFS indexing is only available on Windows",
    ))
}

#[cfg(not(windows))]
pub fn load_index(_paths: &[PathBuf], _apparent_size: bool) -> io::Result<Vec<Node>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "NTFS indexing is only available on Windows",
    ))
}

#[cfg(not(windows))]
pub fn run_elevated() -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "--elevate is only available on Windows",
    ))
}

#[cfg(windows)]
mod windows_index {
    use super::*;
    use crate::platform::get_metadata;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use winapi::shared::minwindef::{DWORD, FALSE};
    use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::ioapiset::DeviceIoControl;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
    use winapi::um::winioctl::FSCTL_QUERY_USN_JOURNAL;
    use winapi::um::winnt::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, HANDLE,
    };

    const INDEX_VERSION: u32 = 1;
    // Writing the index itself creates a few USN records on the same volume.
    // Ignore this small self-generated tail; larger changes trigger a refresh.
    const SELF_USN_SLACK: i64 = 8;

    #[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
    #[repr(C)]
    struct UsnJournalData {
        journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
        min_major_version: u16,
        max_major_version: u16,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct IndexEntry {
        relative_path: String,
        is_directory: bool,
        allocated_size: u64,
        apparent_size: u64,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct DiskIndex {
        version: u32,
        volume_root: String,
        scope_root: String,
        journal: Option<UsnJournalData>,
        entries: Vec<IndexEntry>,
    }

    pub fn build_index(path: &Path) -> io::Result<PathBuf> {
        let scope_root = fs::canonicalize(path)?;
        let volume_root = volume_root(&scope_root)?;
        let mut entries = vec![IndexEntry {
            relative_path: String::new(),
            is_directory: true,
            allocated_size: 0,
            apparent_size: 0,
        }];
        scan_directory(&scope_root, &scope_root, Path::new(""), &mut entries)?;
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

        let index_path = index_path(&volume_root)?;
        if let Some(parent) = index_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let disk_index = DiskIndex {
            version: INDEX_VERSION,
            volume_root: volume_root.to_string_lossy().into_owned(),
            scope_root: normalize_path_text(&scope_root),
            journal: query_journal(&volume_root).ok().flatten(),
            entries,
        };
        if disk_index.journal.is_none() {
            eprintln!(
                "warning: NTFS USN Journal is unavailable; indexed data requires manual refresh"
            );
        }
        let temporary_path = index_path.with_extension("json.tmp");
        fs::write(
            &temporary_path,
            serde_json::to_vec(&disk_index).map_err(json_error)?,
        )?;
        if index_path.exists() {
            fs::remove_file(&index_path)?;
        }
        fs::rename(&temporary_path, &index_path)?;
        Ok(index_path)
    }

    pub fn load_index(paths: &[PathBuf], apparent_size: bool) -> io::Result<Vec<Node>> {
        let Some(first_path) = paths.first() else {
            return Ok(Vec::new());
        };
        let volume_root = volume_root(first_path)?;
        let index_path = index_path(&volume_root)?;
        let mut disk_index = match read_index(&index_path) {
            Ok(index) => index,
            Err(_) => {
                build_index(&volume_root)?;
                read_index(&index_path)?
            }
        };

        if disk_index.version != INDEX_VERSION
            || !same_volume(&disk_index.volume_root, &volume_root)
            || journal_requires_refresh(&volume_root, disk_index.journal.as_ref())
        {
            build_index(&volume_root)?;
            disk_index = read_index(&index_path)?;
        }
        if disk_index.journal.is_none() {
            eprintln!("warning: NTFS USN Journal is unavailable; using the last indexed snapshot");
        }

        let mut result = Vec::with_capacity(paths.len());
        for path in paths {
            let query = fs::canonicalize(path)?;
            if !same_volume_path(&query, &volume_root) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "indexed paths must be on one NTFS volume",
                ));
            }
            if !normalize_path_text(&query).starts_with(&disk_index.scope_root.to_ascii_lowercase())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "indexed path is outside the index scope; rebuild with --build-index",
                ));
            }
            if let Some(node) = build_query_node(&disk_index, &query, apparent_size) {
                result.push(node);
            }
        }
        Ok(result)
    }

    pub fn run_elevated() -> io::Result<i32> {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null;
        use winapi::shared::minwindef::{DWORD, FALSE};
        use winapi::shared::winerror::ERROR_CANCELLED;
        use winapi::um::errhandlingapi::GetLastError;
        use winapi::um::processthreadsapi::GetExitCodeProcess;
        use winapi::um::shellapi::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
        use winapi::um::synchapi::WaitForSingleObject;
        use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};
        use winapi::um::winuser::SW_SHOWNORMAL;

        let executable = std::env::current_exe()?;
        let arguments: Vec<_> = std::env::args_os()
            .skip(1)
            .filter(|arg| arg != "--elevate")
            .collect();
        let parameters = join_quoted_arguments(&arguments);
        let parameters: Vec<u16> = parameters.encode_wide().chain(Some(0)).collect();
        let executable: Vec<u16> = executable
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let verb: Vec<u16> = OsStr::new("runas").encode_wide().chain(Some(0)).collect();
        let directory: Vec<u16> = std::env::current_dir()?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as DWORD;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = executable.as_ptr();
        info.lpParameters = if arguments.is_empty() {
            null()
        } else {
            parameters.as_ptr()
        };
        info.lpDirectory = directory.as_ptr();
        info.nShow = SW_SHOWNORMAL;

        if unsafe { ShellExecuteExW(&mut info) } == FALSE {
            let error = unsafe { GetLastError() };
            if error == ERROR_CANCELLED {
                return Ok(1223);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }

        if info.hProcess.is_null() {
            return Ok(0);
        }
        let wait_result = unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
        let mut exit_code: DWORD = 1;
        let got_exit_code = unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) };
        unsafe { CloseHandle(info.hProcess) };
        if wait_result != WAIT_OBJECT_0 || got_exit_code == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(exit_code as i32)
    }

    fn join_quoted_arguments(arguments: &[std::ffi::OsString]) -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        let mut joined = Vec::new();
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                joined.push(b' ' as u16);
            }
            joined.extend(quote_windows_argument(
                &argument.encode_wide().collect::<Vec<_>>(),
            ));
        }
        std::ffi::OsString::from_wide(&joined)
    }

    fn quote_windows_argument(argument: &[u16]) -> Vec<u16> {
        let mut quoted = Vec::with_capacity(argument.len() + 2);
        quoted.push(b'"' as u16);
        let mut backslashes = 0;
        for unit in argument {
            if *unit == b'\\' as u16 {
                backslashes += 1;
            } else if *unit == b'"' as u16 {
                quoted.extend(std::iter::repeat(b'\\' as u16).take(backslashes * 2 + 1));
                quoted.push(*unit);
                backslashes = 0;
            } else {
                quoted.extend(std::iter::repeat(b'\\' as u16).take(backslashes));
                quoted.push(*unit);
                backslashes = 0;
            }
        }
        quoted.extend(std::iter::repeat(b'\\' as u16).take(backslashes * 2));
        quoted.push(b'"' as u16);
        quoted
    }

    fn read_index(path: &Path) -> io::Result<DiskIndex> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(json_error)
    }

    fn json_error(error: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
    }

    fn volume_root(path: &Path) -> io::Result<PathBuf> {
        let absolute = fs::canonicalize(path)?;
        let text = absolute.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        if text.len() >= 2 && text.as_bytes()[1] == b':' {
            return Ok(PathBuf::from(format!("{}\\", &text[..2])));
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NTFS indexing currently supports drive-letter volumes only",
        ))
    }

    fn same_volume(left: &str, right: &Path) -> bool {
        left.eq_ignore_ascii_case(&right.to_string_lossy())
    }

    fn same_volume_path(path: &Path, volume_root: &Path) -> bool {
        let path_text = path.to_string_lossy();
        let path_text = path_text.strip_prefix(r"\\?\").unwrap_or(&path_text);
        let volume_text = volume_root.to_string_lossy();
        path_text
            .get(..2)
            .zip(volume_text.get(..2))
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    fn index_path(volume_root: &Path) -> io::Result<PathBuf> {
        let drive = volume_root
            .to_string_lossy()
            .chars()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing drive letter"))?;
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Ok(base.join("dust").join(format!("ntfs-index-{drive}.json")))
    }

    fn scan_directory(
        volume_root: &Path,
        directory: &Path,
        relative_directory: &Path,
        entries: &mut Vec<IndexEntry>,
    ) -> io::Result<()> {
        let read_dir = match fs::read_dir(directory) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
            Err(error) => return Err(error),
        };
        for result in read_dir {
            let entry = match result {
                Ok(value) => value,
                Err(_) => continue,
            };
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            let relative_path = relative_directory.join(entry.file_name());
            let is_directory = metadata.is_dir();
            let apparent_size = if is_directory { 0 } else { metadata.len() };
            let allocated_size = if is_directory {
                0
            } else {
                get_metadata(&entry.path(), false, false)
                    .map(|data| data.0)
                    .unwrap_or(apparent_size)
            };
            entries.push(IndexEntry {
                relative_path: relative_path.to_string_lossy().into_owned(),
                is_directory,
                allocated_size,
                apparent_size,
            });
            if is_directory {
                scan_directory(volume_root, &entry.path(), &relative_path, entries)?;
            }
        }
        let _ = volume_root;
        Ok(())
    }

    fn build_query_node(index: &DiskIndex, query: &Path, apparent_size: bool) -> Option<Node> {
        let scope_root = PathBuf::from(&index.scope_root);
        let query_text = normalize_path_text(query);
        let mut records = HashMap::<PathBuf, &IndexEntry>::new();
        for entry in &index.entries {
            let absolute = scope_root.join(&entry.relative_path);
            let absolute_text = normalize_path_text(&absolute);
            if absolute_text == query_text
                || absolute_text.starts_with(&(query_text.clone() + "\\"))
            {
                records.insert(absolute, entry);
            }
        }
        let actual_query = records
            .keys()
            .find(|path| normalize_path_text(path).eq_ignore_ascii_case(&query_text))?
            .clone();
        let mut children: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for path in records.keys() {
            if path != &actual_query {
                if let Some(parent) = path.parent() {
                    children
                        .entry(parent.to_path_buf())
                        .or_default()
                        .push(path.clone());
                }
            }
        }
        Some(build_node_recursive(
            &actual_query,
            &records,
            &children,
            apparent_size,
            0,
        ))
    }

    fn normalize_path_text(path: &Path) -> String {
        let text = path.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        text.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }

    fn build_node_recursive(
        path: &Path,
        records: &HashMap<PathBuf, &IndexEntry>,
        children: &HashMap<PathBuf, Vec<PathBuf>>,
        apparent_size: bool,
        depth: usize,
    ) -> Node {
        let entry = records[path];
        let mut child_nodes: Vec<Node> = children
            .get(path)
            .into_iter()
            .flatten()
            .map(|child| build_node_recursive(child, records, children, apparent_size, depth + 1))
            .collect();
        child_nodes.sort_by(|a, b| a.name.cmp(&b.name));
        let own_size = if apparent_size {
            entry.apparent_size
        } else {
            entry.allocated_size
        };
        let size = own_size + child_nodes.iter().map(|child| child.size).sum::<u64>();
        Node {
            name: path.to_path_buf(),
            size,
            children: child_nodes,
            inode_device: None,
            depth,
        }
    }

    fn query_journal(root: &Path) -> io::Result<Option<UsnJournalData>> {
        let drive = root.to_string_lossy();
        let volume = format!(r"\\.\{}", &drive[..2]);
        let wide: Vec<u16> = OsStr::new(&volume).encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Ok(None);
        }
        let result = unsafe { query_journal_handle(handle) };
        unsafe { CloseHandle(handle) };
        result
    }

    unsafe fn query_journal_handle(handle: HANDLE) -> io::Result<Option<UsnJournalData>> {
        let mut data = UsnJournalData::default();
        let mut returned: DWORD = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                null_mut(),
                0,
                (&mut data as *mut UsnJournalData).cast(),
                std::mem::size_of::<UsnJournalData>() as DWORD,
                &mut returned,
                null_mut(),
            )
        };
        if ok == FALSE {
            return Ok(None);
        }
        Ok(Some(data))
    }

    fn journal_requires_refresh(root: &Path, saved: Option<&UsnJournalData>) -> bool {
        let Some(saved) = saved else { return false };
        let Ok(Some(current)) = query_journal(root) else {
            return false;
        };
        current.journal_id != saved.journal_id
            || current.first_usn > saved.next_usn
            || current.next_usn > saved.next_usn + SELF_USN_SLACK
    }
}

#[cfg(windows)]
pub use windows_index::{build_index, load_index, run_elevated};
