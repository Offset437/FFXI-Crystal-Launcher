//! Coordinate Windower's own settings.xml saves across Crystal launches and its
//! profile editor. Games never hold this guard: their launcher process does.

use crate::i18n::t;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
    thread::JoinHandle,
    time::{Duration, Instant},
};

const WAIT_LIMIT: Duration = Duration::from_secs(30);

// A detached observer thread cannot outlive process::exit. The headless session
// worker joins these after reporting its outcome, so Windower keeps permission
// to finish its final settings.xml save even when authentication or play fails.
static LAUNCH_OBSERVERS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

pub(super) fn retain_observer(observer: JoinHandle<()>) {
    LAUNCH_OBSERVERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(observer);
}

/// Guards kept for the rest of the worker's session: a Windower.exe that relaunched
/// itself and could not be verified may still save settings.xml at any time.
static SESSION_GUARDS: Mutex<Vec<Guard>> = Mutex::new(Vec::new());

pub(super) fn hold_for_session(guard: Guard) {
    SESSION_GUARDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(guard);
}

/// Call after the worker's last session ends and before process::exit. Never
/// terminate a launcher during this wait: it may be writing settings.xml.
pub(crate) fn finish_pending_launchers() {
    let observers = std::mem::take(&mut *LAUNCH_OBSERVERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    for observer in observers {
        let _ = observer.join();
    }
    SESSION_GUARDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
}

pub(crate) struct Guard {
    _file: File,
}

#[derive(Clone, Copy)]
enum Access {
    Launch,
    Edit,
}

/// Hold from before reading the profile until Windower.exe has exited and saved.
pub(super) fn launching(folder: &Path, progress: &mut dyn FnMut(&str)) -> Result<Guard, String> {
    acquire(&super::tools::embedded::data_dir(), folder, Access::Launch, WAIT_LIMIT, progress)
}

/// Hold through the editor child's exit. An already-running game is independent
/// of this guard; a launcher still preparing a game may have a pending XML save.
pub(crate) fn editing(folder: &Path, progress: &mut dyn FnMut(&str)) -> Result<Guard, String> {
    acquire(&super::tools::embedded::data_dir(), folder, Access::Edit, WAIT_LIMIT, progress)
}

fn lock_path(root: &Path, folder: &Path) -> Result<PathBuf, String> {
    let folder = fs::canonicalize(folder)
        .map(|path| crate::backend::plain_windows_path(&path))
        .map_err(|_| t("Could not resolve the Windower profile folder."))?;
    let identity = folder.to_string_lossy().replace('/', "\\").to_lowercase();
    let hash = super::tools::embedded::sha256_hex(identity.as_bytes());
    Ok(root.join("locks").join(format!("windower-profile-{hash}.lock")))
}

fn open(path: &Path, access: Access) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    // Every launch may read/write the same non-truncated lock file. The editor
    // denies all sharing, so no launch can be carrying an older XML snapshot.
    OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .share_mode(match access { Access::Launch => FILE_SHARE_READ | FILE_SHARE_WRITE, Access::Edit => 0 })
        .open(path)
}

fn acquire(root: &Path, folder: &Path, access: Access, timeout: Duration, progress: &mut dyn FnMut(&str)) -> Result<Guard, String> {
    let path = lock_path(root, folder)?;
    let failed = || t("Could not protect Windower's profile settings. Check access to Crystal's data folder.").to_owned();
    fs::create_dir_all(path.parent().ok_or_else(failed)?).map_err(|_| failed())?;
    let deadline = Instant::now() + timeout;
    let mut announced = false;
    loop {
        match open(&path, access) {
            Ok(file) => return Ok(Guard { _file: file }),
            Err(error) if matches!(error.raw_os_error(), Some(32) | Some(33)) => {
                if !announced {
                    progress(match access {
                        Access::Launch => t("Waiting for Windower's profile editor to close..."),
                        Access::Edit => t("Waiting for Windower to finish starting and saving profiles..."),
                    });
                    announced = true;
                }
                if Instant::now() >= deadline {
                    return Err(match access {
                        Access::Launch => t("Windower's profile editor is still open. Close it and try launching again."),
                        Access::Edit => t("Windower is still starting or saving profiles. Wait for that start to finish, then edit profiles."),
                    }.into());
                }
                std::thread::sleep(Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(_) => return Err(failed()),
        }
    }
}

/// A launch guard under a test's own data root.
#[cfg(test)]
pub(super) fn launching_at(root: &Path, folder: &Path) -> Result<Guard, String> {
    acquire(root, folder, Access::Launch, Duration::ZERO, &mut |_| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("crystal-profile-access-{}", crate::backend::unique_id().unwrap()));
        let folder = root.join("Windower");
        fs::create_dir_all(&folder).unwrap();
        (root, folder)
    }

    fn take(root: &Path, folder: &Path, access: Access) -> Result<Guard, String> {
        acquire(root, folder, access, Duration::ZERO, &mut |_| {})
    }

    #[test]
    fn concurrent_launches_exclude_editor_until_the_last_launch_exits() {
        let (root, folder) = fixture();
        let first = take(&root, &folder, Access::Launch).unwrap();
        let second = take(&root, &folder.join("."), Access::Launch).unwrap();
        assert!(take(&root, &folder, Access::Edit).is_err());
        drop(first);
        assert!(take(&root, &folder, Access::Edit).is_err());
        drop(second);
        let editor = take(&root, &folder, Access::Edit).unwrap();
        assert!(take(&root, &folder, Access::Launch).is_err());
        assert!(take(&root, &folder, Access::Edit).is_err());
        drop(editor);
        drop(take(&root, &folder, Access::Launch).unwrap());
        assert!(lock_path(&root, &folder).unwrap().is_file(), "releasing access never removes the lock file");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_aliases_share_a_lock_and_other_installations_remain_independent() {
        let (root, folder) = fixture();
        assert_eq!(lock_path(&root, &folder).unwrap(), lock_path(&root, &PathBuf::from(folder.to_string_lossy().to_uppercase())).unwrap());
        let second = root.join("Other Windower");
        fs::create_dir_all(&second).unwrap();
        let editor = take(&root, &folder, Access::Edit).unwrap();
        drop(take(&root, &second, Access::Launch).unwrap());
        drop(editor);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timeout_is_bounded_and_failed_preparation_releases_access() {
        let (root, folder) = fixture();
        let editor = take(&root, &folder, Access::Edit).unwrap();
        let mut messages = Vec::new();
        let before = Instant::now();
        let result = acquire(&root, &folder, Access::Launch, Duration::from_millis(30), &mut |line| messages.push(line.to_owned()));
        assert!(result.err().unwrap().contains("profile editor is still open"));
        assert_eq!(messages.len(), 1);
        assert!(before.elapsed() < Duration::from_secs(2));
        drop(editor);
        let failed: Result<(), String> = (|| {
            let _guard = take(&root, &folder, Access::Launch)?;
            Err("Fixture profile read failed".into())
        })();
        assert!(failed.is_err());
        drop(take(&root, &folder, Access::Edit).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn late_injector_exit_keeps_profile_access_after_the_log_timeout() {
        use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
        use crate::pol_less::{watch_windower_exit, Injector, LaunchedInjector};
        let (root, folder) = fixture();
        let guard = take(&root, &folder, Access::Launch).unwrap();
        let exited = Arc::new(AtomicBool::new(false));
        let child = LaunchedInjector { injector: Injector::Fake(exited.clone()), profile_access: Some(guard), stop_on_drop: true };
        let log = root.join("late.log");
        let now = Instant::now();
        let watcher = std::thread::spawn(watch_windower_exit(child, now, now, log.clone()));
        let until = Instant::now() + Duration::from_secs(2);
        while fs::read_to_string(&log).is_err() && Instant::now() < until { std::thread::sleep(Duration::from_millis(5)); }
        assert!(fs::read_to_string(&log).unwrap().contains("was still running"));
        assert!(take(&root, &folder, Access::Edit).is_err());
        exited.store(true, Ordering::SeqCst);
        watcher.join().unwrap();
        drop(take(&root, &folder, Access::Edit).unwrap());
        // If creating the observer thread fails, dropping its closure must also
        // release access after stopping the launcher it owned.
        let child = LaunchedInjector { injector: Injector::Fake(Arc::new(AtomicBool::new(false))),
            profile_access: Some(take(&root, &folder, Access::Launch).unwrap()), stop_on_drop: true };
        drop(watch_windower_exit(child, now, now, log));
        drop(take(&root, &folder, Access::Edit).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture_process(test: &str, root: &Path) -> std::process::Command {
        use std::{os::windows::process::CommandExt, process::{Command, Stdio}};
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--ignored", "--nocapture"])
            .env("CRYSTAL_PROFILE_SAVE_FIXTURE", root)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "profile save fixture timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "executed only in a fixture subprocess"]
    fn late_save_child_fixture() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_PROFILE_SAVE_FIXTURE").unwrap());
        let settings = root.join("Windower/settings.xml");
        let snapshot = fs::read(&settings).unwrap();
        fs::write(root.join("child-ready"), []).unwrap();
        until(|| root.join("allow-save").exists());
        // Stand in for Windower's delayed save of the original launch snapshot.
        fs::write(settings, snapshot).unwrap();
        fs::write(root.join("child-saved"), []).unwrap();
    }

    #[test]
    #[ignore = "executed only in a fixture subprocess"]
    fn exiting_worker_fixture() {
        use crate::pol_less::{observe_windower_exit, Injector, LaunchedInjector};
        let root = PathBuf::from(std::env::var_os("CRYSTAL_PROFILE_SAVE_FIXTURE").unwrap());
        let guard = take(&root, &root.join("Windower"), Access::Launch).unwrap();
        let child = fixture_process("pol_less::profile_access::tests::late_save_child_fixture", &root).spawn().unwrap();
        let child = LaunchedInjector { injector: Injector::Child(child), profile_access: Some(guard), stop_on_drop: true };
        let now = Instant::now();
        observe_windower_exit(child, now, now, root.join("launch.log")).unwrap();
        let code = if root.join("failed-session").exists() { 1 } else { 0 };
        // The UI must receive Failed/Finished before worker shutdown waits.
        fs::write(root.join("session-outcome"), code.to_string()).unwrap();
        finish_pending_launchers();
        std::process::exit(code);
    }

    #[test]
    fn worker_exit_keeps_access_until_the_childs_late_save_after_failure_or_session_end() {
        for failed in [true, false] {
            let (root, folder) = fixture();
            let settings = folder.join("settings.xml");
            fs::write(&settings, b"<settings><profile name=\"\" /></settings>").unwrap();
            let plugin = folder.join("plugins/settings/FFXIDB.xml");
            fs::create_dir_all(plugin.parent().unwrap()).unwrap();
            let plugin_settings = b"<settings><position><x>123</x><y>456</y></position></settings>";
            fs::write(&plugin, plugin_settings).unwrap();
            if failed { fs::write(root.join("failed-session"), []).unwrap(); }
            let mut owner = fixture_process("pol_less::profile_access::tests::exiting_worker_fixture", &root).spawn().unwrap();
            until(|| root.join("session-outcome").exists() && root.join("child-ready").exists());
            assert_eq!(fs::read_to_string(root.join("session-outcome")).unwrap(), if failed { "1" } else { "0" });
            // process::exit would release the lock and kill the observer thread
            // here unless the headless worker retained and joined that observer.
            let pending_until = Instant::now() + Duration::from_millis(100);
            while Instant::now() < pending_until {
                assert!(owner.try_wait().unwrap().is_none());
                assert!(take(&root, &folder, Access::Edit).is_err());
                std::thread::sleep(Duration::from_millis(10));
            }
            fs::write(root.join("allow-save"), []).unwrap();
            until(|| owner.try_wait().unwrap().is_some());
            assert_eq!(owner.wait().unwrap().code(), Some(if failed { 1 } else { 0 }));
            assert!(root.join("child-saved").exists());
            let editor = take(&root, &folder, Access::Edit).unwrap();
            fs::write(&settings, b"<settings>next-launch edit</settings>").unwrap();
            assert_eq!(fs::read(&settings).unwrap(), b"<settings>next-launch edit</settings>");
            assert_eq!(fs::read(plugin).unwrap(), plugin_settings);
            drop(editor);
            fs::remove_dir_all(root).unwrap();
        }
    }
}
