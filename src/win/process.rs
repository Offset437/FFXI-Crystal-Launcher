//! Process helpers: the process list, image paths, creation times and elevation.
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
pub(crate) use windows_sys::Win32::System::Threading::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};

/// One process in a snapshot.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) pid: u32,
    pub(crate) parent: u32,
    /// The image's file name, as Windows lists it.
    pub(crate) name: String,
}

/// Every running process. `None` when the list could not be read completely.
pub(crate) fn processes() -> Option<Vec<Entry>> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot handle owned below, and an entry sized as the API requires.
    unsafe {
        let raw = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if raw == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = OwnedHandle::from_raw_handle(raw as _);
        let raw = snapshot.as_raw_handle() as _;
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(raw, &mut entry) == 0 {
            return None;
        }
        let mut list = Vec::new();
        loop {
            let end = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
            list.push(Entry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                name: String::from_utf16_lossy(&entry.szExeFile[..end]),
            });
            if Process32NextW(raw, &mut entry) == 0 {
                return (GetLastError() == ERROR_NO_MORE_FILES).then_some(list);
            }
        }
    }
}

/// `pid` opened with `access`; the error is Windows' (ERROR_INVALID_PARAMETER: no such process).
pub(crate) fn open(pid: u32, access: u32) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::Threading::OpenProcess;
    // SAFETY: OpenProcess returns an owned handle or null.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(raw as _) })
    }
}

/// When `process` started (FILETIME ticks): with its PID, a process's identity.
pub(crate) fn created(process: &OwnedHandle) -> Option<u64> {
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
    let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut exited, mut kernel, mut user) = (created, created, created);
    // SAFETY: a live handle and four FILETIME out values.
    if unsafe { GetProcessTimes(process.as_raw_handle() as _, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// `pid` opened with `access` (which must allow a limited query) and its creation time.
pub(crate) fn open_with_created(pid: u32, access: u32) -> Option<(OwnedHandle, u64)> {
    let process = open(pid, access).ok()?;
    let created = created(&process)?;
    Some((process, created))
}

/// The full Win32 path of `process`'s image.
pub(crate) fn image_path_of(process: &OwnedHandle) -> io::Result<PathBuf> {
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;
    let mut buffer = vec![0u16; 32 * 1024];
    let mut size = buffer.len() as u32;
    // SAFETY: a live handle and a buffer of `size` units.
    if unsafe { QueryFullProcessImageNameW(process.as_raw_handle() as _, 0, buffer.as_mut_ptr(), &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(super::from_wide(&buffer[..size as usize])))
}

/// The full Win32 path of `pid`'s image.
pub(crate) fn image_path(pid: u32) -> io::Result<PathBuf> {
    image_path_of(&open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?)
}

/// Whether this process runs with an elevated token. Read once: it cannot change while
/// the process runs.
pub(crate) fn is_elevated() -> bool {
    static ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ELEVATED.get_or_init(|| {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: the process pseudo handle, a token closed below, and a correctly sized struct.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
            let mut returned = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_listed_with_its_image_and_identity() {
        let me = std::process::id();
        let list = processes().expect("the process list");
        let entry = list.iter().find(|e| e.pid == me).expect("this process is listed");
        let image = image_path(me).unwrap();
        assert!(image.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case(&entry.name));
        assert_eq!(image, std::env::current_exe().unwrap());
        let (_, first) = open_with_created(me, PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        assert_eq!(open_with_created(me, PROCESS_QUERY_LIMITED_INFORMATION).unwrap().1, first);
        assert!(open(0xFFFF_FFF0, PROCESS_QUERY_LIMITED_INFORMATION).is_err());
    }
}
