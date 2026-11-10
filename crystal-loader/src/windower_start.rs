//! Windower saves settings.xml when its child first has a main window or exits.
//! Coordinate before game component creation and before a windowless host exits.
//! Give the verified parent a copy of the turn: its save outlives a failed host.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Mutex},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, FILETIME, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
            QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_DUP_HANDLE, PROCESS_SYNCHRONIZE,
        },
    },
};

static COORDINATING: Mutex<()> = Mutex::new(());
static RETAINED: AtomicBool = AtomicBool::new(false);

/// Armed only after hello: before that, the worker owns the injection turn and
/// stops Windower before releasing it. Later, sign-in failure and normal sign-out
/// also make Windower save, even when GameStart was never reached.
pub(crate) struct ExitGuard;

impl Drop for ExitGuard {
    fn drop(&mut self) {
        if let Err(error) = coordinate(&mut || {
            crate::native_auth::pump_idle_messages();
            true
        }) {
            log!("error: Windower profile save could not be protected at host exit: {error}");
        }
    }
}

fn created(process: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    let mut start = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = start;
    let mut kernel = start;
    let mut user = start;
    let ok = unsafe { GetProcessTimes(process, &mut start, &mut exit, &mut kernel, &mut user) };
    (ok != 0).then(|| (u64::from(start.dwHighDateTime) << 32) | u64::from(start.dwLowDateTime))
}

fn same_folder(first: &Path, second: &Path) -> bool {
    match (fs::canonicalize(first), fs::canonicalize(second)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

fn windower_parent() -> Result<Option<OwnedHandle>, String> {
    let Some(hook) = crate::hook_layout::hook_base() else { return Ok(None) };
    let hook = hook as windows_sys::Win32::Foundation::HMODULE;
    let Some(hook_path) = crate::win::module_path(hook) else {
        return Err("Could not locate Windower before saving profiles.".into());
    };
    let Some(folder) = hook_path.parent() else {
        return Err("Could not locate Windower's folder.".into());
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("Could not identify Windower's launcher process.".into());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut next = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
    let own = unsafe { GetCurrentProcessId() };
    let mut parent = None;
    while next != 0 {
        if entry.th32ProcessID == own {
            parent = Some(entry.th32ParentProcessID);
            break;
        }
        next = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    let Some(pid) = parent else {
        return Err("Could not identify the game host's parent process.".into());
    };
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_DUP_HANDLE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(None)
        } else {
            Err(format!(
                "Could not monitor Windower's profile save: {error}"
            ))
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut image = vec![0u16; 32768];
    let mut size = image.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(handle.as_raw_handle(), 0, image.as_mut_ptr(), &mut size)
    } == 0
    {
        if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
            return Ok(None);
        }
        return Err("Could not verify Windower's launcher process.".into());
    }
    let image = PathBuf::from(String::from_utf16_lossy(&image[..size as usize]));
    let named_windower = image
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Windower.exe"));
    let folder_matches = image.parent().is_some_and(|path| same_folder(path, folder));
    if !named_windower || !folder_matches {
        log!("note: Windower profile save coordination off: the game host's parent is not Windower.exe beside Hook.dll (named_windower={named_windower} folder_matches={folder_matches})");
        return Ok(None);
    }
    let (Some(parent_created), Some(host_created)) = (
        created(handle.as_raw_handle()),
        created(unsafe { GetCurrentProcess() }),
    ) else {
        return Err("Could not verify Windower's process identity.".into());
    };
    if parent_created > host_created {
        return Ok(None);
    } // reused parent PID
    Ok(Some(handle))
}

fn turn_path() -> Result<PathBuf, String> {
    // Same path as session_worker::windower_lock_path, including isolated test roots.
    let root = std::env::var_os("CRYSTAL_DATA_ROOT")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .ok_or("Could not locate the Windower launch lock directory.")?;
    Ok(PathBuf::from(root).join("Crystal Launcher/locks/windower.lock"))
}

fn open_turn(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

fn acquire(
    path: &Path,
    parent: &OwnedHandle,
    pump: &mut dyn FnMut() -> bool,
) -> Result<Option<File>, String> {
    fs::create_dir_all(
        path.parent()
            .ok_or("Could not locate the Windower launch lock directory.")?,
    )
    .map_err(|e| format!("Could not protect Windower's profile save: {e}"))?;
    let until = Instant::now() + Duration::from_secs(90);
    let mut announced = false;
    loop {
        match unsafe { WaitForSingleObject(parent.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => return Ok(None),
            WAIT_TIMEOUT => {}
            _ => return Err("Could not monitor Windower's profile save.".into()),
        }
        match open_turn(path) {
            Ok(file) => return Ok(Some(file)),
            Err(e) if matches!(e.raw_os_error(), Some(32) | Some(33)) => {}
            Err(e) => return Err(format!("Could not protect Windower's profile save: {e}")),
        }
        if !announced {
            log!("stage: waiting for the previous Windower profile save");
            announced = true;
        }
        if Instant::now() >= until {
            return Err("Windower did not finish its previous profile save.".into());
        }
        if !pump() {
            return Err("The session ended while waiting for Windower's profile save.".into());
        }
        // File share locks have no waitable handle. Wait on the parent between
        // attempts so its exit wakes us and failure cleanup cannot busy-spin.
        unsafe { WaitForSingleObject(parent.as_raw_handle(), 20) };
    }
}

pub(crate) fn coordinate(pump: &mut dyn FnMut() -> bool) -> Result<(), String> {
    let _coordination = COORDINATING.lock().unwrap_or_else(|e| e.into_inner());
    if RETAINED.load(Ordering::Acquire) {
        return Ok(());
    }
    let Some(parent) = windower_parent()? else {
        return Ok(());
    };
    if !coordinate_parent(&turn_path()?, &parent, pump)? {
        return Ok(());
    }
    RETAINED.store(true, Ordering::Release);
    log!("stage: Windower profile-save turn retained by its launcher until exit");
    Ok(())
}

fn coordinate_parent(path: &Path, parent: &OwnedHandle, pump: &mut dyn FnMut() -> bool) -> Result<bool, String> {
    let Some(turn) = acquire(path, parent, pump)? else { return Ok(false); };
    if let Err(error) = retain_turn(parent, turn) {
        if unsafe { WaitForSingleObject(parent.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
            return Ok(false);
        }
        return Err(format!("Could not retain Windower's final profile-save turn: {error}"));
    }
    Ok(true)
}

fn retain_turn(parent: &OwnedHandle, turn: File) -> io::Result<()> {
    let mut remote = std::ptr::null_mut();
    // The target is our verified Windower parent, not a process found by name.
    // This duplicates only a non-inheritable file handle, never code or memory.
    // Its handle table owns the duplicate until Windower exits after Profile.Save;
    // a host thread cannot retain the turn after this host's own process exit.
    if unsafe {
        DuplicateHandle(GetCurrentProcess(), turn.as_raw_handle(), parent.as_raw_handle(),
            &mut remote, 0, 0, DUPLICATE_SAME_ACCESS)
    } == 0 {
        return Err(io::Error::last_os_error());
    }
    // `remote` belongs to the parent's handle table. Closing it here would close
    // an unrelated handle in this process. Parent process exit closes it exactly once.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::windows::{io::IntoRawHandle, process::CommandExt},
        process::{Child, Command, Stdio},
    };

    #[test]
    fn turn_excludes_a_second_launch_until_the_tool_process_exits() {
        let root = std::env::temp_dir().join(format!(
            "crystal-windower-save-{}-{}",
            std::process::id(),
            unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("windower.lock");
        let first = open_turn(&path).unwrap();
        assert_eq!(open_turn(&path).unwrap_err().raw_os_error(), Some(32));
        let mut child = Command::new("sort.exe")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let child = unsafe { OwnedHandle::from_raw_handle(child.into_raw_handle()) };
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        retain_turn(&child, first).unwrap();
        assert_eq!(open_turn(&path).unwrap_err().raw_os_error(), Some(32));
        drop(input); // Windower stand-in finishes its final save and exits.
        assert_eq!(unsafe { WaitForSingleObject(child.as_raw_handle(), 10_000) }, WAIT_OBJECT_0);
        assert!(open_turn(&path).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn blocked_start_keeps_pumping_and_honors_cancellation() {
        let root =
            std::env::temp_dir().join(format!("crystal-windower-cancel-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("windower.lock");
        let _first = open_turn(&path).unwrap();
        let live = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, GetCurrentProcessId()) };
        assert!(!live.is_null());
        let live = unsafe { OwnedHandle::from_raw_handle(live) };
        let mut calls = 0;
        let error = acquire(&path, &live, &mut || {
            calls += 1;
            false
        })
        .unwrap_err();
        assert!(error.contains("session ended"));
        assert_eq!(calls, 1);
    }

    struct FixtureProcess(Child);
    impl Drop for FixtureProcess {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn fixture_command(test: &str, root: &Path, role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--ignored", "--nocapture"])
            .env("CRYSTAL_SAVE_FIXTURE", root).env("CRYSTAL_SAVE_ROLE", role)
            .stdin(Stdio::null()).stdout(Stdio::null())
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready() {
            assert!(Instant::now() < deadline, "profile-save process fixture timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "executed only by the profile-save process fixture"]
    fn exiting_host_fixture() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_SAVE_FIXTURE").unwrap());
        let role = std::env::var("CRYSTAL_SAVE_ROLE").unwrap();
        let pid = std::env::var("CRYSTAL_SAVE_PARENT").unwrap().parse::<u32>().unwrap();
        let parent = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_DUP_HANDLE, 0, pid) };
        assert!(!parent.is_null());
        let parent = unsafe { OwnedHandle::from_raw_handle(parent) };
        fs::write(root.join(&role).join("host-ready"), []).unwrap();
        until(|| root.join("allow-host-exits").exists());
        // The same acquire/retain operation used before a windowless host exits.
        // No thread in this process will remain after this fixture returns.
        assert!(coordinate_parent(&root.join("windower.lock"), &parent, &mut || true).unwrap());
    }

    #[test]
    #[ignore = "executed only by the profile-save process fixture"]
    fn saving_parent_fixture() {
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let root = PathBuf::from(std::env::var_os("CRYSTAL_SAVE_FIXTURE").unwrap());
        let role = std::env::var("CRYSTAL_SAVE_ROLE").unwrap();
        let settings = root.join("settings.xml");
        let mut snapshot = Vec::new();
        OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&settings).unwrap()
            .read_to_end(&mut snapshot).unwrap();
        let mut host = FixtureProcess(fixture_command(
            "windower_start::tests::exiting_host_fixture", &root, &role)
            .env("CRYSTAL_SAVE_PARENT", std::process::id().to_string()).spawn().unwrap());
        assert!(host.0.wait().unwrap().success());
        fs::write(root.join(&role).join("host-exited"), []).unwrap();
        // .NET Framework's XDocument.Save opens Create/Write/FileShare.Read.
        // Hold the truncating write open after host exit to expose its lifetime.
        let mut save = OpenOptions::new().write(true).create(true).truncate(true)
            .share_mode(FILE_SHARE_READ).open(&settings).unwrap();
        let half = snapshot.len() / 2;
        save.write_all(&snapshot[..half]).unwrap();
        fs::write(root.join(&role).join("writer-open"), []).unwrap();
        until(|| root.join(&role).join("finish-save").exists());
        save.write_all(&snapshot[half..]).unwrap();
        save.flush().unwrap();
    }

    #[test]
    fn six_host_exits_keep_parent_xml_saves_exclusive_after_parallel_readiness() {
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let root = std::env::temp_dir().join(format!("crystal-parent-save-{}-{}", std::process::id(),
            unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }));
        fs::create_dir_all(&root).unwrap();
        let xml = b"<settings><profile name=\"\"><width>1920</width><height>1080</height><uiscale>1.5</uiscale></profile><profile name=\"Second\"><width>1280</width></profile></settings>";
        let settings = root.join("settings.xml");
        fs::write(&settings, xml).unwrap();
        let roles = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
        let mut parents: Vec<_> = roles.iter().map(|role| {
            fs::create_dir(root.join(role)).unwrap();
            FixtureProcess(fixture_command("windower_start::tests::saving_parent_fixture", &root, role)
                .spawn().unwrap())
        }).collect();
        // All six independent hosts finish their simulated sign-in before any
        // takes the save turn: this guard never serializes account authentication.
        until(|| roles.iter().all(|role| root.join(role).join("host-ready").exists()));
        fs::write(root.join("allow-host-exits"), []).unwrap();
        let mut finished = std::collections::BTreeSet::new();
        for _ in roles {
            let mut saving = None;
            until(|| {
                saving = roles.iter().copied().find(|role| !finished.contains(role)
                    && root.join(role).join("writer-open").exists());
                saving.is_some()
            });
            let role = saving.unwrap();
            assert!(root.join(role).join("host-exited").exists());
            assert_eq!(open_turn(&root.join("windower.lock")).unwrap_err().raw_os_error(), Some(32),
                "the live parent must retain its exited host's turn");
            assert_eq!(roles.iter().filter(|role| !finished.contains(*role)
                && root.join(role).join("writer-open").exists()).count(), 1);
            // A matching second .NET writer really would fail here; this is not
            // merely a mutex test. Rust's share-write reader is allowed, however.
            assert_eq!(OpenOptions::new().write(true).share_mode(FILE_SHARE_READ)
                .open(&settings).unwrap_err().raw_os_error(), Some(32));
            assert!(fs::read(&settings).is_ok());
            fs::write(root.join(role).join("finish-save"), []).unwrap();
            finished.insert(role);
        }
        for parent in &mut parents { assert!(parent.0.wait().unwrap().success()); }
        assert_eq!(fs::read(&settings).unwrap(), xml);
        drop(open_turn(&root.join("windower.lock")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
