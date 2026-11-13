//! A notification only: command files still authenticate the exact worker and request.
//! Missing notifications retain the supervisor's existing timeout fallback.

use super::Identity;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, Weak};
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{
    FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
};
use windows_sys::Win32::System::Threading::WaitForMultipleObjects;

#[derive(Clone)]
pub(crate) struct CommandWake {
    handle: Arc<OwnedHandle>,
}

fn name(worker: Identity) -> Vec<u16> {
    crate::win::wide(&format!("Local\\Crystal-command-{}-{:016x}", worker.pid, worker.created))
}

impl CommandWake {
    #[cfg(test)]
    pub(crate) fn test_listener() -> (Self, impl Fn() + Send + 'static) {
        let worker = Identity { pid: std::process::id(), created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 };
        (Self::listen(worker).unwrap(), move || signal(worker))
    }

    pub(super) fn listen(worker: Identity) -> io::Result<Self> {
        Self::create(Some(&name(worker)))
    }

    pub(super) fn anonymous() -> io::Result<Self> {
        Self::create(None)
    }

    fn create(name: Option<&[u16]>) -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::Threading::CreateEventW;
        // Auto-reset notifications coalesce; the durable files are read on every wake.
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.map_or(std::ptr::null(), |name| name.as_ptr())) };
        if handle.is_null() { return Err(io::Error::last_os_error()); }
        Ok(Self { handle: Arc::new(unsafe { OwnedHandle::from_raw_handle(handle as _) }) })
    }

    pub(super) fn wake(&self) {
        unsafe { windows_sys::Win32::System::Threading::SetEvent(self.raw()); }
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as _
    }
}

pub(super) fn signal(worker: Identity) {
    signal_named(&name(worker));
}

pub(super) struct StatusMode {
    enabled: AtomicBool,
    listeners: Mutex<Vec<Weak<CommandWake>>>,
}

impl StatusMode {
    pub(super) const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            listeners: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(super) fn set(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::AcqRel) == enabled { return; }
        let mut listeners = self.listeners.lock().unwrap_or_else(|error| error.into_inner());
        listeners.retain(|listener| {
            let Some(listener) = listener.upgrade() else { return false };
            listener.wake();
            true
        });
    }

    pub(super) fn listen(&self) -> io::Result<Arc<CommandWake>> {
        let wake = Arc::new(CommandWake::anonymous()?);
        let mut listeners = self.listeners.lock().unwrap_or_else(|error| error.into_inner());
        listeners.retain(|listener| listener.strong_count() != 0);
        listeners.push(Arc::downgrade(&wake));
        Ok(wake)
    }
}

/// The session worker atomically replaces its status files. Watch the directory
/// for renames as well as writes, and rearm before reading the new state.
pub(super) struct StatusWatch(HANDLE);

impl StatusWatch {
    pub(super) fn open(directory: &Path) -> Option<Self> {
        let path = crate::win::wide_path(directory);
        let handle = unsafe {
            FindFirstChangeNotificationW(path.as_ptr(), 0,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE)
        };
        (!handle.is_null() && handle as isize != -1).then_some(Self(handle))
    }

    /// Returns false if the directory watch stopped working and polling should take over.
    pub(super) fn wait(&self, process: &OwnedHandle, control: &CommandWake, timeout_ms: u32) -> bool {
        let handles = [process.as_raw_handle() as HANDLE, control.raw(), self.0];
        let result = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout_ms) };
        match result {
            WAIT_TIMEOUT | WAIT_OBJECT_0 => true,
            result if result == WAIT_OBJECT_0 + 1 => true,
            result if result == WAIT_OBJECT_0 + 2 => unsafe { FindNextChangeNotification(self.0) != 0 },
            _ => false,
        }
    }
}

impl Drop for StatusWatch {
    fn drop(&mut self) {
        unsafe { FindCloseChangeNotification(self.0); }
    }
}

fn signal_named(name: &[u16]) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if !handle.is_null() {
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        unsafe { SetEvent(handle.as_raw_handle() as _); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::WaitForSingleObject};

    #[test]
    fn notifications_coalesce_and_cannot_wake_another_worker_lifetime() {
        let worker = Identity { pid: std::process::id(), created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 };
        let listener = CommandWake::listen(worker).unwrap();
        signal(Identity { created: worker.created + 1, ..worker });
        assert_eq!(unsafe { WaitForSingleObject(listener.raw(), 0) }, WAIT_TIMEOUT);
        signal(worker);
        signal(worker);
        assert_eq!(unsafe { WaitForSingleObject(listener.raw(), 0) }, WAIT_OBJECT_0);
        assert_eq!(unsafe { WaitForSingleObject(listener.raw(), 0) }, WAIT_TIMEOUT);
        drop(listener);
        signal(worker); // Old builds and ended workers are harmless; the command file remains.
    }

}
