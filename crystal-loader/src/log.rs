//! `crystal-loader.log` next to the executable: stages and errors only. Never
//! pass session values, pipe payloads or anything derived from them to `log!`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static SINK: Mutex<Option<File>> = Mutex::new(None);
static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The log file's path once [`init`] opened it; its folder also holds the
/// per-build feature check.
pub fn path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

const MAX_LOG_BYTES: u64 = 1 << 20;
pub const LOG_NAME: &str = "crystal-loader.log";

/// Opens the log next to the executable (falls back to %TEMP% if that folder is
/// read-only). Returns the path used.
pub fn init() -> Option<PathBuf> {
    let beside_exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(LOG_NAME)));
    let candidates = beside_exe.into_iter().chain(std::iter::once(std::env::temp_dir().join(LOG_NAME)));
    for path in candidates {
        if let Some(f) = open(&path) {
            *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
            let _ = PATH.set(path.clone());
            return Some(path);
        }
    }
    None
}

fn open(path: &Path) -> Option<File> {
    open_rotating(path, MAX_LOG_BYTES)
}

/// Numbered logs a host may open when every earlier one is full and still held
/// (`crystal-loader.log`, `crystal-loader.2.log`, ...).
const LOG_GENERATIONS: u32 = 8;

/// Hosts append to a shared log. Rotate only under the machine-wide turn and
/// only a log no running host holds open: a renamed log keeps its writers, and
/// a later rotation would delete the file they still write to. A full log that
/// another host still holds sends this host to the next numbered log, so the
/// total stays bounded by `LOG_GENERATIONS` logs plus one previous generation
/// each.
fn open_rotating(path: &Path, max_bytes: u64) -> Option<File> {
    let _turn = RotationTurn::take();
    let last = generation(path, LOG_GENERATIONS);
    for n in 1..=LOG_GENERATIONS {
        let candidate = generation(path, n);
        let full = std::fs::metadata(&candidate).is_ok_and(|m| m.len() > max_bytes);
        if full && !held(&candidate) {
            // A previous generation that can't be replaced (a reader holds it):
            // start this log over rather than append past the cap.
            if std::fs::rename(&candidate, candidate.with_extension("log.old")).is_err() {
                let _ = OpenOptions::new().write(true).truncate(true).open(&candidate);
            }
        } else if full && candidate != last {
            continue;
        }
        return OpenOptions::new().create(true).append(true).open(&candidate).ok();
    }
    None
}

/// The `n`th log: the base name for 1, `name.n.log` after that.
fn generation(path: &Path, n: u32) -> PathBuf {
    if n == 1 { return path.to_path_buf(); }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("crystal-loader");
    path.with_file_name(format!("{stem}.{n}.log"))
}

/// Open for writing somewhere: opening it without sharing write access fails
/// while another host has it open.
fn held(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path).is_err()
}

/// Serialises the size check, rename and open across hosts starting together.
struct RotationTurn(windows_sys::Win32::Foundation::HANDLE, bool);
impl RotationTurn {
    fn take() -> Self {
        use windows_sys::Win32::{Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0}, System::Threading::{CreateMutexW, WaitForSingleObject}};
        let name: Vec<u16> = "Local\\crystal-loader-log\0".encode_utf16().collect();
        // SAFETY: a NUL-terminated name and default security.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() { return Self(handle, false); }
        // SAFETY: a live mutex handle owned by this value.
        let owned = matches!(unsafe { WaitForSingleObject(handle, 2000) }, WAIT_OBJECT_0 | WAIT_ABANDONED);
        Self(handle, owned)
    }
}
impl Drop for RotationTurn {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::ReleaseMutex};
        if self.0.is_null() { return; }
        // SAFETY: the handle is owned here; release only an acquired mutex.
        unsafe {
            if self.1 { ReleaseMutex(self.0); }
            CloseHandle(self.0);
        }
    }
}

// Tests read this thread's lines (no file is opened under test).
#[cfg(test)]
thread_local! { static CAPTURE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) fn capture(work: impl FnOnce()) -> Vec<String> {
    CAPTURE.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
    work();
    CAPTURE.with(|lines| lines.borrow_mut().take().unwrap())
}

pub fn write(msg: std::fmt::Arguments<'_>) {
    #[cfg(test)]
    CAPTURE.with(|lines| { if let Some(lines) = lines.borrow_mut().as_mut() { lines.push(msg.to_string()); } });
    let mut guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = guard.as_mut() else { return };
    // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    // Format first so one line uses one append write. Formatting directly into
    // File performs many writes, interleaving lines from simultaneous clients.
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{}] {}\n",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        t.wMilliseconds,
        std::process::id(),
        msg
    );
    let _ = f.write_all(line.as_bytes());
    let _ = f.flush();
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(format_args!($($t)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_another_host_still_writes_is_never_rotated() {
        let dir = std::env::temp_dir().join(format!("crystal-loader-log-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_NAME);
        let old = path.with_extension("log.old");
        std::fs::write(&path, b"running host\n").unwrap();
        let running = OpenOptions::new().append(true).open(&path).unwrap();
        // Over the limit, but a running host holds it: no rename under it, and
        // this host writes the next numbered log instead.
        let second = generation(&path, 2);
        let mut next = open_rotating(&path, 4).unwrap();
        next.write_all(b"second host\n").unwrap();
        drop(next);
        assert!(!old.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"running host\n");
        assert_eq!(std::fs::read(&second).unwrap(), b"second host\n");
        // That one is full too but nobody holds it: it rotates in place.
        drop(open_rotating(&path, 4).unwrap());
        assert_eq!(std::fs::read(second.with_extension("log.old")).unwrap(), b"second host\n");
        assert_eq!(std::fs::metadata(&second).unwrap().len(), 0);
        drop(running);
        // Nobody writes it any more: one previous generation is kept.
        drop(open_rotating(&path, 4).unwrap());
        assert_eq!(std::fs::read(&old).unwrap(), b"running host\n");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // Under the limit: unchanged.
        drop(open_rotating(&path, 4).unwrap());
        assert_eq!(std::fs::read(&old).unwrap(), b"running host\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_full_log_whose_previous_generation_is_held_starts_over() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("crystal-loader-held-old-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_NAME);
        let old = path.with_extension("log.old");
        std::fs::write(&old, b"previous\n").unwrap();
        std::fs::write(&path, b"full log\n").unwrap();
        // A reader without delete sharing: the rename cannot replace it.
        let reader = OpenOptions::new().read(true).share_mode(1).open(&old).unwrap();
        drop(open_rotating(&path, 4).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(std::fs::read(&old).unwrap(), b"previous\n");
        drop(reader);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn every_log_full_and_held_appends_to_the_last_generation() {
        let dir = std::env::temp_dir().join(format!("crystal-loader-gen-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_NAME);
        let held: Vec<File> = (1..=LOG_GENERATIONS).map(|n| {
            let p = generation(&path, n);
            std::fs::write(&p, b"full\n").unwrap();
            OpenOptions::new().append(true).open(&p).unwrap()
        }).collect();
        let mut f = open_rotating(&path, 4).unwrap();
        f.write_all(b"x\n").unwrap();
        drop(f);
        assert_eq!(std::fs::read(generation(&path, LOG_GENERATIONS)).unwrap(), b"full\nx\n");
        assert_eq!(generation(&path, 2).file_name().unwrap(), "crystal-loader.2.log");
        drop(held);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
