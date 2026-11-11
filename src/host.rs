//! A session's host (loader) process, told apart from a later process that reuses its PID.

use crate::pol_less;
use std::path::{Path, PathBuf};

/// A session's host (loader) process: its PID and, when it could be read while the host ran,
/// its creation time, so a PID Windows has since reused is never taken for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostId {
    pub(crate) pid: u32,
    pub(crate) created: Option<u64>,
}

/// `pid`'s creation time, read while it is known to run (the session's start).
#[cfg(not(test))]
pub(crate) fn process_created(pid: u32) -> Option<u64> {
    crate::win::process::open_with_created(pid, crate::win::process::PROCESS_QUERY_LIMITED_INFORMATION).map(|(_, created)| created)
}
#[cfg(test)]
pub(crate) fn process_created(_pid: u32) -> Option<u64> { None }

/// The loader a signed-in session runs: its host process's own file (a ReShade session runs a
/// copy in its own host folder, where the game reads dgVoodoo's files), else the configured one.
pub(crate) fn session_loader(host: Option<HostId>, loader_setting: &str) -> Option<PathBuf> {
    let configured = pol_less::tools::loader_path(loader_setting);
    let running = host.and_then(process_image).filter(|image| {
        let name = |path: &Path| path.file_name().map(|name| name.to_ascii_lowercase());
        configured.as_deref().is_some_and(|configured| name(image) == name(configured))
    });
    running.or(configured)
}

/// The host's image file, unless its PID now names a later process (creation time differs).
#[cfg(not(test))]
pub(crate) fn process_image(host: HostId) -> Option<PathBuf> {
    let process = crate::win::process::open(host.pid, crate::win::process::PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let same = host.created.is_none_or(|created| crate::win::process::created(&process) == Some(created));
    if !same { return None; }
    crate::win::process::image_path_of(&process).ok()
}
#[cfg(test)]
pub(crate) fn process_image(_host: HostId) -> Option<PathBuf> { None }

/// Whether the ended session's host process still runs. One this process may not open (an
/// elevated host) counts as running, unless it was opened before (then the PID is another
/// process's now): a sign-in after the game then waits for its limit.
#[cfg(not(test))]
pub(crate) fn host_running(host: HostId) -> bool {
    use std::os::windows::io::AsRawHandle;
    use crate::win::process::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    use windows_sys::Win32::{Foundation::{ERROR_ACCESS_DENIED, WAIT_TIMEOUT}, System::Threading::WaitForSingleObject};
    let process = match crate::win::process::open(host.pid, PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(process) => process,
        Err(error) => return host.created.is_none() && error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32),
    };
    let waiting = unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) };
    waiting == WAIT_TIMEOUT && host.created.is_none_or(|created| crate::win::process::created(&process) == Some(created))
}
#[cfg(test)]
thread_local! { pub(crate) static RUNNING_HOSTS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) }; }
#[cfg(test)]
pub(crate) fn host_running(host: HostId) -> bool { RUNNING_HOSTS.with(|hosts| hosts.borrow().contains(&host.pid)) }
