use std::io;
#[cfg(not(windows))]
pub fn run_elevated() -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "elevation requires Windows",
    ))
}
#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
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

        eprintln!("Requesting administrator privileges through the Windows UAC prompt...");
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
        unsafe { winapi::um::handleapi::CloseHandle(info.hProcess) };
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
}
#[cfg(windows)]
pub use windows::run_elevated;
