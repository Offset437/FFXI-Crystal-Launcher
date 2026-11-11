//! One Crystal at a time: the instance mutex, and handing a second start over to the first.

use crate::i18n::t;
use crate::{startup_task, tray};
use std::time::{Duration, Instant};

pub(crate) struct InstanceGuard(windows_sys::Win32::Foundation::HANDLE);
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
pub(crate) fn claim_instance() -> Result<Option<InstanceGuard>, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
    };
    let name: Vec<u16> = "Local\\XI-Iced-Launcher-Test-1"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let failed = || t("Windows could not start the launcher instance. Close any other copy and try again.").to_owned();
    let background = std::env::args().any(|a| a == startup_task::BACKGROUND_ARG);
    // A closing Crystal holds the mutex while it signs idle sessions out (close_idle_sessions,
    // up to 30 s, plus the park close) but has already destroyed its tray window: wait for it.
    let deadline = Instant::now() + INSTANCE_CLOSE_WAIT;
    loop {
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() { return Err(failed()); }
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS { return Ok(Some(InstanceGuard(handle))); }
        unsafe { CloseHandle(handle) };
        // A background start (Start with Windows) finding Crystal already running changes nothing.
        if background { return Ok(None); }
        // The running Crystal (its hidden window exists from its start) opens or brings forward
        // its own window, whether or not it has one.
        match tray::ask_running_instance_to_open() {
            Some(tray::OpenAnswer::Opened) => return Ok(None),
            // Found but not reachable (a running Crystal at a higher integrity level refuses the
            // message): bring its window forward directly, if it has one.
            Some(tray::OpenAnswer::Unreachable) => {
                use windows_sys::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_RESTORE};
                if let Some(window) = tray::running_instance_window() {
                    unsafe {
                        ShowWindow(window as _, SW_RESTORE);
                        SetForegroundWindow(window as _);
                    }
                }
                return Ok(None);
            }
            // Closing (or its hidden window is already gone): wait for it to end.
            Some(tray::OpenAnswer::Closing) | None => {}
        }
        if Instant::now() >= deadline { return Err(failed()); }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The longest a new start waits for a closing Crystal to finish its cleanup (idle sign-outs
/// up to 30 s, then the park close).
const INSTANCE_CLOSE_WAIT: Duration = Duration::from_secs(45);
