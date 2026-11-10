//! Wait for this launcher instance, replace its package, then restart it.
//! Game processes and session workers are never enumerated or stopped.
use super::*;
use std::{process::Command, time::Instant};

pub(super) fn start(stage: &Stage) -> Result<(), String> {
    verify(&stage.release.envelope, &trusted_key()?)?;
    verify_archive(&stage.directory.join("release.zip"), &stage.release)?;
    let target = std::env::current_exe().map_err(|e| e.to_string())?;
    validate_target(&target)?;
    let helper = stage.directory.join("apply-helper.exe");
    fs::copy(&target, &helper).map_err(|e| e.to_string())?;
    let mut command = Command::new(helper);
    command
        .arg("--apply-launcher-update")
        .arg(std::process::id().to_string())
        .arg(&target)
        .arg(&stage.directory);
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let ready = stage.directory.join("helper-ready");
    let started = Instant::now();
    loop {
        if fs::read_to_string(&ready).ok().as_deref() == Some(&child.id().to_string()) {
            return Ok(());
        }
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Err(fs::read_to_string(stage.directory.join("last-error.txt"))
                .unwrap_or_else(|_| t("The launcher update helper could not start.").into()));
        }
        if started.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(t("The launcher update helper did not become ready.").into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn ordinary_file(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    use std::os::windows::fs::MetadataExt;
    if meta.file_attributes() & 0x400 != 0 {
        return Err(t("Launcher update paths cannot be links.").into());
    }
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(t("Launcher update paths must be ordinary files.").into());
    }
    Ok(())
}

fn validate_target(target: &Path) -> Result<(), String> {
    if !target.is_absolute()
        || target
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
    {
        return Err(t("Invalid launcher update destination.").into());
    }
    ordinary_file(target)?;
    let parent = target
        .parent()
        .ok_or(t("Invalid launcher update destination."))?;
    if parent.join("Cargo.toml").is_file() {
        return Err(t("Update a packaged launcher copy outside the source folder.").into());
    }
    Ok(())
}

fn replace(source: &Path, destination: &Path) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    if !destination.exists() {
        return fs::rename(source, destination).map_err(|e| e.to_string());
    }
    ordinary_file(destination)?;
    let source = crate::win::wide_path(source);
    let destination = crate::win::wide_path(destination);
    let ok = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// The executable keeps its (possibly renamed) path; the notices sit beside it.
fn destination(target: &Path, parent: &Path, index: usize, name: &str) -> PathBuf {
    if index == 0 {
        target.to_owned()
    } else {
        parent.join(name)
    }
}

/// Copy everything first. Any replacement failure restores all affected files.
fn install_package(extracted: &Path, target: &Path) -> Result<PathBuf, String> {
    validate_target(target)?;
    let parent = target.parent().unwrap();
    for (index, name) in FILES.iter().enumerate() {
        ordinary_file(&extracted.join(name))?;
        let destination = destination(target, parent, index, name);
        if destination.try_exists().map_err(|e| e.to_string())? {
            ordinary_file(&destination)?;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = parent.join(format!(".crystal-previous-{}-{stamp}", std::process::id()));
    fs::create_dir(&backup).map_err(|e| e.to_string())?;
    let mut changes = Vec::new();
    for (index, name) in FILES.iter().enumerate() {
        let source = extracted.join(name);
        ordinary_file(&source)?;
        let destination = destination(target, parent, index, name);
        let existed = destination.try_exists().map_err(|e| e.to_string())?;
        if existed {
            ordinary_file(&destination)?;
            fs::copy(&destination, backup.join(name)).map_err(|e| e.to_string())?;
        }
        let temporary = parent.join(format!(
            ".crystal-next-{}-{stamp}-{index}",
            std::process::id()
        ));
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        std::io::copy(
            &mut fs::File::open(&source).map_err(|e| e.to_string())?,
            &mut out,
        )
        .and_then(|_| out.sync_all())
        .map_err(|e| e.to_string())?;
        changes.push((destination, temporary, existed, *name));
    }
    for (committed, (destination, temporary, _, _)) in changes.iter().enumerate() {
        if let Err(error) = replace(temporary, destination) {
            let mut restore_errors = Vec::new();
            for (destination, temporary, existed, name) in changes[..committed].iter().rev() {
                if *existed {
                    let restored = fs::copy(backup.join(name), temporary)
                        .map_err(|e| e.to_string())
                        .and_then(|_| replace(temporary, destination));
                    if let Err(e) = restored {
                        restore_errors.push(e);
                    }
                } else if let Err(e) = fs::remove_file(destination) {
                    restore_errors.push(e.to_string());
                }
            }
            for (_, temporary, _, _) in &changes {
                let _ = fs::remove_file(temporary);
            }
            if !restore_errors.is_empty() {
                keep_backup(&backup);
            }
            return Err(format!(
                "{error}. Previous package: {}. {}",
                backup.display(),
                restore_errors.join("; ")
            ));
        }
    }
    Ok(backup)
}

/// Marks a backup that holds the only copy of the previous package (its restore failed), so
/// later starts never delete it.
pub(super) const RESTORE_FAILED: &str = "restore-failed.txt";

fn keep_backup(backup: &Path) {
    let _ = fs::write(backup.join(RESTORE_FAILED), "");
}

fn restore_package(backup: &Path, target: &Path) -> Result<(), String> {
    let parent = target.parent().ok_or("Invalid backup destination.")?;
    for (index, name) in FILES.iter().enumerate() {
        let old = backup.join(name);
        let destination = destination(target, parent, index, name);
        if old.try_exists().map_err(|e| e.to_string())? {
            ordinary_file(&old)?;
            let temporary = backup.join(format!("restore-{index}"));
            fs::copy(&old, &temporary).map_err(|e| e.to_string())?;
            replace(&temporary, &destination)?;
        } else if destination.try_exists().map_err(|e| e.to_string())? {
            ordinary_file(&destination)?;
            fs::remove_file(destination).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

struct Parent(std::os::windows::io::OwnedHandle);
impl Parent {
    fn open(pid: u32, target: &Path) -> Result<Self, String> {
        use crate::win::process::{self, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
        let parent = process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)
            .map(Self)
            .map_err(|_| t("Could not verify the launcher instance being updated.").to_owned())?;
        let actual = process::image_path_of(&parent.0).map_err(|e| e.to_string())?;
        if fs::canonicalize(actual).map_err(|e| e.to_string())?
            != fs::canonicalize(target).map_err(|e| e.to_string())?
        {
            return Err(
                t("Launcher update destination does not match the running launcher.").into(),
            );
        }
        Ok(parent)
    }
    fn wait(&self) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        let status = unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(self.0.as_raw_handle() as _, 30_000)
        };
        if status == 0 {
            Ok(())
        } else {
            Err(t("The launcher did not close; its files were left unchanged.").into())
        }
    }
}

fn refresh_installed_version(target: &Path, version: &str, key_path: &str) -> Result<bool, String> {
    use crate::win::reg::{Key, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, RRF_RT_REG_SZ};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND},
        Globalization::{CompareStringOrdinal, CSTR_EQUAL},
    };
    let win_error = |status| std::io::Error::from_raw_os_error(status as i32).to_string();
    let key = match Key::open(HKEY_CURRENT_USER, key_path, KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY) {
        Ok(key) => key,
        // Portable copies have no installer registration.
        Err(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => return Ok(false),
        Err(status) => {
            return Err(format!("Could not open Installed Apps registration: {}", win_error(status)))
        }
    };
    let mut location = vec![0u16; 32768];
    let bytes = match key.get_into("InstallLocation", RRF_RT_REG_SZ, &mut location) {
        Ok(bytes) => bytes,
        Err(ERROR_FILE_NOT_FOUND) => return Ok(false),
        Err(status) => {
            return Err(format!("Could not read Installed Apps InstallLocation: {}", win_error(status)))
        }
    };
    if bytes < 2 || bytes % 2 != 0 {
        return Err("Invalid Installed Apps InstallLocation.".into());
    }
    let location = &location[..bytes / 2 - 1];
    if location.contains(&0) {
        return Err("Invalid Installed Apps InstallLocation.".into());
    }
    let location = PathBuf::from(crate::win::from_wide(location));
    if !location.is_absolute() {
        return Ok(false);
    }
    let registered = match fs::canonicalize(location) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not resolve Installed Apps InstallLocation: {error}"
            ))
        }
    };
    let parent = fs::canonicalize(
        target
            .parent()
            .ok_or("Invalid launcher update destination.")?,
    )
    .map_err(|error| error.to_string())?;
    let registered: Vec<u16> = registered.as_os_str().encode_wide().collect();
    let parent: Vec<u16> = parent.as_os_str().encode_wide().collect();
    if unsafe {
        CompareStringOrdinal(
            registered.as_ptr(),
            registered.len() as i32,
            parent.as_ptr(),
            parent.len() as i32,
            1,
        )
    } != CSTR_EQUAL
    {
        return Ok(false); // A different installed copy owns this registration.
    }
    key.set_string("DisplayVersion", version)
        .map_err(|status| format!("Could not refresh Installed Apps DisplayVersion: {}", win_error(status)))?;
    Ok(true)
}

fn refresh_registration_after_restart(
    target: &Path,
    version: &str,
    directory: &Path,
    key_path: &str,
) {
    // Metadata is best effort after restart. Never roll back a running new launcher.
    let warning = refresh_installed_version(target, version, key_path).err().map(|error| {
        format!("Launcher update to {version} succeeded, but Installed Apps metadata was not refreshed: {error}")
    });
    for directory in std::iter::once(directory).chain(directory.parent()) {
        let path = directory.join("last-registration-warning.txt");
        if let Some(warning) = &warning {
            let _ = fs::write(path, warning);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

pub(super) fn run(args: &[String]) -> Result<(), String> {
    if args.len() != 5 {
        return Err("Invalid update helper arguments.".into());
    }
    let pid = args[2].parse::<u32>().map_err(|e| e.to_string())?;
    let target = PathBuf::from(&args[3]);
    let directory = PathBuf::from(&args[4]);
    if !directory.is_absolute() {
        return Err("An absolute staging directory is required.".into());
    }
    let mut parent_closed = false;
    let result = (|| {
        validate_target(&target)?;
        let parent = Parent::open(pid, &target)?;
        ordinary_file(&directory.join("launcher-update.json"))?;
        let metadata = read_metadata(&directory.join("launcher-update.json"))?;
        let release = verify(&metadata, &trusted_key()?)?;
        if release.version <= current_version() {
            return Err(
                t("Launcher updates cannot install an older or identical version.").into(),
            );
        }
        let archive = directory.join("release.zip");
        ordinary_file(&archive)?;
        verify_archive(&archive, &release)?;
        let extracted = directory.join("apply-verified");
        fs::create_dir(&extracted).map_err(|e| e.to_string())?;
        unpack(&archive, &extracted)?;
        fs::write(
            directory.join("helper-ready"),
            std::process::id().to_string(),
        )
        .map_err(|e| e.to_string())?;
        parent.wait()?;
        parent_closed = true;
        let backup = install_package(&extracted, &target)?;
        match Command::new(&target).spawn() {
            Ok(_) => {
                // No rollback is possible past this point; the backup is only clutter.
                let _ = fs::remove_dir_all(&backup);
                refresh_registration_after_restart(
                    &target,
                    &release.payload.version,
                    &directory,
                    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\CrystalLauncher",
                );
                Ok(())
            }
            Err(error) => {
                // The new executable could not start: restore the complete old package.
                restore_package(&backup, &target).inspect_err(|_| keep_backup(&backup))?;
                Err(format!("Could not restart the new launcher: {error}"))
            }
        }
    })();
    if let Err(error) = &result {
        let _ = fs::write(directory.join("last-error.txt"), error);
        if let Some(root) = directory.parent() {
            let _ = fs::write(root.join("last-error.txt"), error);
        }
        if parent_closed {
            let _ = Command::new(&target).spawn();
        }
    } else if let Some(root) = directory.parent() {
        let _ = fs::remove_file(root.join("last-error.txt"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "crystal-self-update-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }
    #[test]
    fn package_replacement_preserves_the_old_files_and_renamed_executable() {
        let root = fixture();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let target = root.join("My Crystal.exe");
        fs::write(&target, b"old executable").unwrap();
        for name in FILES {
            fs::write(package.join(name), format!("new {name}")).unwrap();
        }
        fs::write(root.join("README.md"), b"old readme").unwrap();
        // Even legacy portable data beside the executable is outside the package.
        let data = root.join("iced-launcher");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("accounts.dpapi"), b"saved encrypted accounts").unwrap();
        fs::write(data.join("settings.json"), b"saved settings").unwrap();
        let backup = install_package(&package, &target).unwrap();
        assert_eq!(fs::read(data.join("accounts.dpapi")).unwrap(), b"saved encrypted accounts");
        assert_eq!(fs::read(data.join("settings.json")).unwrap(), b"saved settings");
        assert_eq!(fs::read(&target).unwrap(), b"new Crystal-Launcher.exe");
        assert_eq!(
            fs::read(backup.join("Crystal-Launcher.exe")).unwrap(),
            b"old executable"
        );
        assert_eq!(fs::read(backup.join("README.md")).unwrap(), b"old readme");
        assert!(!root.join("Crystal-Launcher.exe").exists());
        assert_eq!(fs::read(root.join("LICENSE")).unwrap(), b"new LICENSE");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn missing_package_files_and_source_folders_leave_the_launcher_unchanged() {
        let root = fixture();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let target = root.join("Crystal.exe");
        fs::write(&target, b"old").unwrap();
        fs::write(package.join(FILES[0]), b"new").unwrap();
        assert!(install_package(&package, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        fs::write(root.join("Cargo.toml"), b"[package]").unwrap();
        assert!(validate_target(&target).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn restoring_an_executable_only_package_removes_new_notices() {
        let root = fixture();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let target = root.join("Crystal.exe");
        fs::write(&target, b"old").unwrap();
        for name in FILES {
            fs::write(package.join(name), b"new").unwrap();
        }
        let backup = install_package(&package, &target).unwrap();
        restore_package(&backup, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!root.join("LICENSE").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_locked_notice_rolls_back_an_already_replaced_executable() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let target = root.join("Crystal.exe");
        fs::write(&target, b"old executable").unwrap();
        fs::write(root.join("README.md"), b"old readme").unwrap();
        for name in FILES {
            fs::write(package.join(name), b"new").unwrap();
        }
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(root.join("README.md"))
            .unwrap();
        assert!(install_package(&package, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old executable");
        assert_eq!(fs::read(root.join("README.md")).unwrap(), b"old readme");
        assert!(!root.join("LICENSE").exists());
        drop(locked);
        fs::remove_dir_all(root).unwrap();
    }

    mod registration {
        use super::*;
        use crate::win::reg::{Key, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, RRF_RT_REG_SZ};

        struct Registration {
            path: String,
            key: Option<Key>,
        }
        impl Registration {
            fn create(root: &Path) -> Self {
                // A unique leaf outside the real uninstall tree; never touch the real identity.
                let path = format!(
                    "Software\\CrystalLauncherUpdateTest-{}",
                    root.file_name().unwrap().to_string_lossy()
                );
                let (key, created) =
                    Key::create(HKEY_CURRENT_USER, &path, KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY).unwrap();
                assert!(created);
                Self { path, key: Some(key) }
            }
            fn key(&self) -> &Key {
                self.key.as_ref().unwrap()
            }
            fn set(&self, name: &str, value: &str) {
                self.key().set_string(name, value).unwrap();
            }
            fn version(&self) -> String {
                String::from_utf16(&self.key().string("DisplayVersion", RRF_RT_REG_SZ, 256).unwrap()).unwrap()
            }
        }
        impl Drop for Registration {
            fn drop(&mut self) {
                drop(self.key.take());
                let path = crate::win::wide(&self.path);
                unsafe {
                    windows_sys::Win32::System::Registry::RegDeleteKeyExW(HKEY_CURRENT_USER, path.as_ptr(), KEY_WOW64_64KEY, 0);
                }
            }
        }

        #[test]
        fn updates_only_the_matching_installation_and_keeps_the_signed_version() {
            let root = fixture();
            let installed = root.join("Installed Crystal");
            let portable = root.join("Portable Crystal");
            fs::create_dir(&installed).unwrap();
            fs::create_dir(&portable).unwrap();
            let registration = Registration::create(&root);
            registration.set("InstallLocation", &installed.to_string_lossy());
            registration.set("DisplayVersion", "0.9.0");
            let version = "2.3.4-beta.7+signed";
            assert!(!refresh_installed_version(
                &portable.join("Crystal.exe"),
                version,
                &registration.path
            )
            .unwrap());
            assert_eq!(registration.version(), "0.9.0");
            // Windows path spelling/case and a trailing separator do not change ownership.
            registration.set(
                "InstallLocation",
                &format!("{}\\", installed.to_string_lossy().to_uppercase()),
            );
            assert!(refresh_installed_version(
                &installed.join("Renamed Crystal.exe"),
                version,
                &registration.path
            )
            .unwrap());
            assert_eq!(registration.version(), version);
            let key_path = registration.path.clone();
            drop(registration);
            assert!(
                !refresh_installed_version(&portable.join("Crystal.exe"), version, &key_path)
                    .unwrap()
            );
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn registration_errors_leave_a_success_warning_without_changing_the_version() {
            let root = fixture();
            let stage = root.join("stage");
            fs::create_dir(&stage).unwrap();
            let registration = Registration::create(&root);
            registration.set("DisplayVersion", "0.9.0");
            assert!(!refresh_installed_version(
                &root.join("Crystal.exe"),
                "1.2.3",
                &registration.path
            )
            .unwrap());
            registration.set("InstallLocation", "invalid\0location");
            refresh_registration_after_restart(
                &root.join("Crystal.exe"),
                "1.2.3",
                &stage,
                &registration.path,
            );
            assert_eq!(registration.version(), "0.9.0");
            for directory in [&root, &stage] {
                let warning =
                    fs::read_to_string(directory.join("last-registration-warning.txt")).unwrap();
                assert!(warning.contains("update to 1.2.3 succeeded"));
                assert!(warning.contains("Invalid Installed Apps InstallLocation"));
                assert!(!directory.join("last-error.txt").exists());
            }
            registration.set("InstallLocation", &root.to_string_lossy());
            refresh_registration_after_restart(
                &root.join("Crystal.exe"),
                "1.2.3",
                &stage,
                &registration.path,
            );
            assert_eq!(registration.version(), "1.2.3");
            assert!(!root.join("last-registration-warning.txt").exists());
            assert!(!stage.join("last-registration-warning.txt").exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn parent_identity_is_checked_and_the_handle_waits_for_that_process() {
        use std::os::windows::process::CommandExt;
        let shell = PathBuf::from(std::env::var_os("WINDIR").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut child = Command::new(&shell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let parent = Parent::open(child.id(), &shell);
        let mismatch = Parent::open(child.id(), &std::env::current_exe().unwrap());
        // Stop only the process this test created, even if an assertion fails.
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(mismatch.is_err());
        parent.unwrap().wait().unwrap();
    }

    #[test]
    #[ignore = "only executed as the update image fixture subprocess"]
    fn running_image_fixture() {
        let Some(ready) = std::env::var_os("CRYSTAL_UPDATE_IMAGE_READY") else {
            return;
        };
        fs::write(ready, b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn a_surviving_session_process_does_not_prevent_the_update() {
        use std::os::windows::process::CommandExt;
        let root = fixture();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        let target = root.join("Crystal.exe");
        let original = std::env::current_exe().unwrap();
        fs::copy(&original, &target).unwrap();
        for name in FILES {
            fs::write(package.join(name), b"updated package").unwrap();
        }
        let ready = root.join("fixture-ready");
        let mut child = Command::new(&target)
            .args([
                "--exact",
                "self_update::apply::tests::running_image_fixture",
                "--ignored",
            ])
            .env("CRYSTAL_UPDATE_IMAGE_READY", &ready)
            .creation_flags(0x0800_0000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let was_ready = ready.exists();
        let result = install_package(&package, &target);
        let still_running = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(was_ready);
        let backup = result.unwrap();
        assert!(still_running);
        assert_eq!(fs::read(&target).unwrap(), b"updated package");
        assert_eq!(
            fs::metadata(backup.join(FILES[0])).unwrap().len(),
            fs::metadata(original).unwrap().len()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
