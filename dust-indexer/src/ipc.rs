use crate::{Result, Tree};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{LocalFree, *},
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, RevertToSelf,
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::*,
    System::Pipes::*,
};
pub const PIPE_NAME: &str = r"\\.\pipe\dust-indexer-v2";
const MAX_REQUEST: usize = 64 * 1024;
const MAX_REPLY: usize = 64 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Request {
    path: std::path::PathBuf,
    apparent: bool,
    depth: usize,
}
#[derive(Serialize, Deserialize)]
enum Response {
    Ok(Tree),
    Error(String),
}
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
fn retryable(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(232 | 231 | 536))
        || matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        )
}
fn transfer(pipe: &mut File, buf: &mut [u8], write: bool, deadline: Instant) -> Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        if Instant::now() > deadline {
            return Err("named pipe timed out".into());
        }
        let result = if write {
            pipe.write(&buf[pos..])
        } else {
            pipe.read(&mut buf[pos..])
        };
        match result {
            Ok(0) => return Err("named pipe disconnected".into()),
            Ok(n) => pos += n,
            Err(e) if retryable(&e) => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn send<T: Serialize>(pipe: &mut File, value: &T, deadline: Instant) -> Result<()> {
    let mut data = serde_json::to_vec(value)?;
    if data.len() > MAX_REPLY {
        return Err("pipe response exceeds size limit".into());
    }
    transfer(pipe, &mut (data.len() as u32).to_le_bytes(), true, deadline)?;
    transfer(pipe, &mut data, true, deadline)
}
fn receive<T: serde::de::DeserializeOwned>(
    pipe: &mut File,
    max: usize,
    deadline: Instant,
) -> Result<T> {
    let mut len = [0; 4];
    transfer(pipe, &mut len, false, deadline)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err("pipe message exceeds size limit".into());
    }
    let mut data = vec![0; len];
    transfer(pipe, &mut data, false, deadline)?;
    Ok(serde_json::from_slice(&data)?)
}

pub fn query_service(path: &Path, apparent: bool, depth: usize) -> Result<Tree> {
    // Identification-level credentials prohibit a fake local server from using
    // the client's token to access other resources.
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)?;
    let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
    if unsafe {
        SetNamedPipeHandleState(
            pipe.as_raw_handle(),
            &mode,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    send(
        &mut pipe,
        &Request {
            path: std::fs::canonicalize(path)?,
            apparent,
            depth: depth.min(256),
        },
        deadline,
    )?;
    match receive(&mut pipe, MAX_REPLY, deadline)? {
        Response::Ok(tree) => Ok(tree),
        Response::Error(e) => Err(e.into()),
    }
}

pub fn serve(
    database: &Path,
    stop: Arc<AtomicBool>,
    health: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let name = wide(PIPE_NAME);
    // Authenticated local clients may query; network clients are rejected. The
    // service database remains readable only by SYSTEM and administrators.
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)");
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &security,
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut pipe = unsafe { File::from_raw_handle(handle) };
    while !stop.load(Ordering::Relaxed) {
        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if connected == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                if retryable(&e) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                return Err(e.into());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let response = match receive::<Request>(&mut pipe, MAX_REQUEST, deadline) {
            Ok(r) => match answer(database, &r, handle, &health) {
                Ok(t) => Response::Ok(t),
                Err(e) => Response::Error(e.to_string()),
            },
            Err(e) => Response::Error(e.to_string()),
        };
        let _ = send(&mut pipe, &response, deadline);
        // Keep the pipe open until the client has consumed the reply or the
        // deadline expires; DisconnectNamedPipe discards unread data.
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            let mut byte = [0];
            match pipe.read(&mut byte) {
                Err(e) if retryable(&e) => thread::sleep(Duration::from_millis(5)),
                _ => break,
            }
        }
        unsafe {
            DisconnectNamedPipe(handle);
        }
    }
    Ok(())
}
fn answer(
    database: &Path,
    r: &Request,
    handle: HANDLE,
    health: &Mutex<Option<String>>,
) -> Result<Tree> {
    if let Some(e) = health
        .lock()
        .map_err(|_| "index health lock poisoned")?
        .as_ref()
    {
        return Err(e.clone().into());
    }
    let store = crate::store::Store::open_existing(database)?;
    let tree = store.query(&r.path, r.apparent, r.depth.min(256))?;
    // Capture an identification token without allowing the server to use it
    // for arbitrary filesystem operations. AccessCheck evaluates the ACLs.
    if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut token = std::ptr::null_mut();
    let opened = unsafe {
        windows_sys::Win32::System::Threading::OpenThreadToken(
            windows_sys::Win32::System::Threading::GetCurrentThread(),
            windows_sys::Win32::Security::TOKEN_QUERY,
            1,
            &mut token,
        )
    };
    let open_error = std::io::Error::last_os_error();
    if unsafe { RevertToSelf() } == 0 {
        std::process::abort();
    }
    if opened == 0 {
        return Err(open_error.into());
    }
    let token = crate::scanner::Handle(token);
    // Check every directory covered by an aggregate, including descendants
    // omitted by --depth. Never disclose totals for unreadable subtrees.
    let result = store.check_access(&r.path, |path, is_dir| check_access(path, is_dir, token.0));

    result?;
    Ok(tree)
}
fn check_access(path: &Path, is_dir: bool, token: HANDLE) -> Result<()> {
    use windows_sys::Win32::Security::{Authorization::*, *};
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    let error = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32).into());
    }
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    let mut privileges = vec![0u64; 512];
    let mut length = (privileges.len() * 8) as u32;
    let mut granted = 0;
    let mut allowed = 0;
    let ok = unsafe {
        AccessCheck(
            descriptor,
            token,
            if is_dir {
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES
            } else {
                FILE_READ_ATTRIBUTES
            },
            &mapping,
            privileges.as_mut_ptr().cast(),
            &mut length,
            &mut granted,
            &mut allowed,
        )
    };
    let error = std::io::Error::last_os_error();
    unsafe {
        LocalFree(descriptor);
    }
    if ok == 0 {
        return Err(error.into());
    }
    if allowed == 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
    }
    Ok(())
}
