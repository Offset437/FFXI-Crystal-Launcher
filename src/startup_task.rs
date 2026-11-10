//! Start with Windows: a Task Scheduler logon task that starts Crystal in the background
//! (`--background`) for this Windows user.
//!
//! Why a scheduled task and not `HKCU\…\Run`: Crystal's manifest requires administrator
//! rights. Windows does not elevate Run-key programs at sign-in (it either blocks them or asks
//! through UAC every time), so a Run entry would never start Crystal quietly. A task created by
//! the elevated Crystal with "Run with highest privileges" starts it elevated at logon without
//! a prompt. The task runs at normal priority (Task Scheduler's default, 7, is below normal,
//! which the game hosts Crystal starts would inherit) and with no time limit.
use std::path::Path;

/// Starts straight into background mode: no window, notification-area icon only.
pub(crate) const BACKGROUND_ARG: &str = "--background";

/// The task's name, per Windows user (each signs in to their own Crystal).
pub(crate) fn task_name(user: &str) -> String {
    let user: String = user.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    format!("Crystal Launcher background start ({user})")
}

/// This Windows user as Task Scheduler names them (`DOMAIN\user`).
pub(crate) fn current_user() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
    Some(match std::env::var("USERDOMAIN").ok().filter(|d| !d.is_empty()) {
        Some(domain) => format!("{domain}\\{user}"),
        None => user,
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The task definition: at this user's logon, run `exe --background` elevated, at normal
/// priority, with no run-time limit, on battery too, never a second copy.
pub(crate) fn task_xml(exe: &Path, user: &str) -> String {
    let exe = escape(&exe.display().to_string());
    let user = escape(user);
    format!(r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Crystal Launcher in the background when you sign in to Windows, so your accounts are signed in to PlayOnline before you press Play. Turn it off in Crystal: Settings, System.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT10S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
      <Arguments>{BACKGROUND_ARG}</Arguments>
    </Exec>
  </Actions>
</Task>
"#)
}

/// One schtasks.exe run: its exit success and standard output.
pub(crate) type Run<'a> = dyn FnMut(&[String]) -> Result<(bool, String), String> + 'a;

/// Register (or replace) the task from `xml_path`, which holds `task_xml` (UTF-16).
pub(crate) fn create_args(name: &str, xml_path: &Path) -> Vec<String> {
    vec!["/Create".into(), "/TN".into(), name.into(), "/XML".into(), xml_path.display().to_string(), "/F".into()]
}

pub(crate) fn delete_args(name: &str) -> Vec<String> {
    vec!["/Delete".into(), "/TN".into(), name.into(), "/F".into()]
}

pub(crate) fn query_args(name: &str) -> Vec<String> {
    vec!["/Query".into(), "/TN".into(), name.into(), "/XML".into()]
}

/// Whether the registered task (its `/Query /XML` output) starts this exe in the background.
/// Task Scheduler re-serialises the definition (an apostrophe comes back literal), so both
/// sides are compared unescaped. schtasks writes code-page text, so a path with characters
/// outside ASCII may not match here; `ensure` then relies on the path it saved itself.
pub(crate) fn matches(query_xml: &str, exe: &Path) -> bool {
    let unescaped = query_xml.replace("&apos;", "'").replace("&quot;", "\"").replace("&lt;", "<")
        .replace("&gt;", ">").replace("&amp;", "&").to_lowercase();
    let command = format!("<command>\"{}\"</command>", exe.display()).to_lowercase();
    unescaped.contains(&command) && unescaped.contains(&format!("<arguments>{BACKGROUND_ARG}</arguments>"))
}

/// The exe path Crystal last registered, kept beside the definition file's folder.
fn saved_path(scratch: &Path) -> std::path::PathBuf {
    scratch.join("startup-task-exe.txt")
}

fn same_exe(saved: &str, exe: &Path) -> bool {
    saved.trim().to_lowercase() == exe.display().to_string().to_lowercase()
}

/// Turn Start with Windows on (register, replacing a task that points elsewhere) or off.
/// `scratch`: a folder for the definition file schtasks reads.
pub(crate) fn set(on: bool, exe: &Path, user: &str, scratch: &Path, run: &mut Run<'_>) -> Result<(), String> {
    use crate::i18n::t;
    let name = task_name(user);
    if !on {
        let _ = std::fs::remove_file(saved_path(scratch));
        // Already absent is fine: the switch only promises there is none now.
        let (_, _) = run(&delete_args(&name))?;
        return match run(&query_args(&name)) {
            Ok((true, _)) => Err(t("Windows did not remove Crystal's startup task.").into()),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(scratch).map_err(|_| t("Could not prepare Crystal's startup task."))?;
    let path = scratch.join("startup-task.xml");
    let mut bytes = vec![0xff, 0xfe];
    for unit in task_xml(exe, user).encode_utf16() { bytes.extend_from_slice(&unit.to_le_bytes()); }
    std::fs::write(&path, bytes).map_err(|_| t("Could not prepare Crystal's startup task."))?;
    let created = run(&create_args(&name, &path));
    let _ = std::fs::remove_file(&path);
    match created? {
        (true, _) => {
            let _ = std::fs::write(saved_path(scratch), exe.display().to_string());
            Ok(())
        }
        (false, _) => Err(t("Windows did not accept Crystal's startup task. Crystal must run as administrator to create it.").into()),
    }
}

/// At Crystal's start with the switch on: the task is there and starts this exe (the exe may
/// have moved); re-registered when not.
pub(crate) fn ensure(exe: &Path, user: &str, scratch: &Path, run: &mut Run<'_>) -> Result<bool, String> {
    if let Ok((true, xml)) = run(&query_args(&task_name(user))) {
        // The registered task decides. Only a path schtasks can't print faithfully (outside
        // ASCII) falls back to the path Crystal saved, and the task must still start in the
        // background.
        let saved = std::fs::read_to_string(saved_path(scratch)).ok();
        let current = if matches(&xml, exe) {
            if !saved.as_deref().is_some_and(|s| same_exe(s, exe)) {
                let _ = std::fs::write(saved_path(scratch), exe.display().to_string());
            }
            true
        } else {
            !exe.display().to_string().is_ascii()
                && saved.as_deref().is_some_and(|s| same_exe(s, exe))
                && xml.to_lowercase().contains(&format!("<arguments>{BACKGROUND_ARG}</arguments>"))
        };
        if current { return Ok(false); }
    }
    set(true, exe, user, scratch, run).map(|()| true)
}

/// schtasks.exe itself, without a console window.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn schtasks(args: &[String]) -> Result<(bool, String), String> {
    let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    let mut command = std::process::Command::new(system.join("System32").join("schtasks.exe"));
    command.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let output = command.output().map_err(|_| crate::i18n::t("Could not run Windows Task Scheduler.").to_owned())?;
    Ok((output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_task_starts_this_exe_in_the_background_elevated_at_this_users_logon_at_normal_priority() {
        let xml = task_xml(Path::new(r"C:\Games\Crystal & Co\Crystal-Launcher.exe"), r"PC\Player");
        assert!(xml.contains(r#"<Command>"C:\Games\Crystal &amp; Co\Crystal-Launcher.exe"</Command>"#), "{xml}");
        assert!(xml.contains("<Arguments>--background</Arguments>"));
        assert!(xml.contains("<LogonTrigger>") && xml.contains(r"<UserId>PC\Player</UserId>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"), "no UAC prompt at logon");
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"), "no stored password");
        assert!(xml.contains("<Priority>4</Priority>"), "not Task Scheduler's below-normal default");
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"), "never stopped after 72 h");
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }

    #[test]
    fn task_names_are_per_user_and_safe() {
        assert_eq!(task_name(r"PC\Player"), "Crystal Launcher background start (PC_Player)");
        assert_eq!(task_name("a/b:c"), "Crystal Launcher background start (a_b_c)");
    }

    struct Fake { calls: Vec<Vec<String>>, registered: Option<String>, accept: bool, lossy: bool }
    impl Fake {
        fn new(accept: bool) -> Self { Self { calls: Vec::new(), registered: None, accept, lossy: false } }
        /// What schtasks really prints: re-serialised (apostrophes literal), code-page text
        /// decoded as UTF-8 (anything outside ASCII lost).
        fn printed(&self, xml: &str) -> String {
            if !self.lossy { return xml.to_owned(); }
            xml.replace("&apos;", "'").chars().map(|c| if c.is_ascii() { c } else { '\u{FFFD}' }).collect()
        }
        fn run(&mut self, args: &[String]) -> Result<(bool, String), String> {
            self.calls.push(args.to_vec());
            match args[0].as_str() {
                "/Create" => {
                    let bytes = std::fs::read(&args[4]).unwrap();
                    assert_eq!(&bytes[..2], &[0xff, 0xfe], "UTF-16 with its byte order mark");
                    let units: Vec<u16> = bytes[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    if self.accept { self.registered = Some(String::from_utf16(&units).unwrap()); }
                    Ok((self.accept, String::new()))
                }
                "/Delete" => Ok((self.registered.take().is_some(), String::new())),
                "/Query" => Ok(match &self.registered { Some(xml) => (true, self.printed(xml)), None => (false, String::new()) }),
                other => panic!("unexpected {other}"),
            }
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-startup-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn turning_it_on_registers_and_off_removes_through_schtasks_never_the_run_key() {
        let exe = Path::new(r"C:\Crystal\Crystal-Launcher.exe");
        let dir = scratch("toggle");
        let mut fake = Fake::new(true);
        set(true, exe, r"PC\Player", &dir, &mut |args| fake.run(args)).unwrap();
        assert!(matches(fake.registered.as_deref().unwrap(), exe));
        assert_eq!(fake.calls[0][..3], ["/Create", "/TN", "Crystal Launcher background start (PC_Player)"]);
        assert_eq!(fake.calls[0][5], "/F");
        assert!(!dir.join("startup-task.xml").exists(), "the definition file is removed");
        // Already registered for this exe: nothing to do at start.
        assert!(!ensure(exe, r"PC\Player", &dir, &mut |args| fake.run(args)).unwrap());
        // The exe moved: re-registered for the new path.
        let moved = Path::new(r"D:\Crystal\Crystal-Launcher.exe");
        assert!(ensure(moved, r"PC\Player", &dir, &mut |args| fake.run(args)).unwrap());
        assert!(matches(fake.registered.as_deref().unwrap(), moved) && !matches(fake.registered.as_deref().unwrap(), exe));
        set(false, moved, r"PC\Player", &dir, &mut |args| fake.run(args)).unwrap();
        assert!(fake.registered.is_none());
        // Off with nothing registered is fine too.
        set(false, moved, r"PC\Player", &dir, &mut |args| fake.run(args)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_registration_is_an_error() {
        let dir = scratch("refused");
        let mut fake = Fake::new(false);
        assert!(set(true, Path::new(r"C:\Crystal\c.exe"), "u", &dir, &mut |args| fake.run(args)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Apostrophes and non-ASCII profile names (C:\Users\Jürgen, 山田) used to re-register the
    /// task on every start.
    #[test]
    fn a_registered_task_is_recognised_whatever_schtasks_prints() {
        let dir = scratch("lossy");
        let exe = Path::new(r"C:\Users\O'Neil Jürgen 山田\Crystal-Launcher.exe");
        let mut fake = Fake::new(true);
        fake.lossy = true;
        set(true, exe, "u", &dir, &mut |args| fake.run(args)).unwrap();
        fake.calls.clear();
        assert!(!ensure(exe, "u", &dir, &mut |args| fake.run(args)).unwrap());
        assert_eq!(fake.calls.len(), 1, "only the query: {:?}", fake.calls);
        // A task registered before the path was saved: an ASCII path is read back once.
        let ascii = Path::new(r"C:\Users\O'Neil\Crystal-Launcher.exe");
        set(true, ascii, "u", &dir, &mut |args| fake.run(args)).unwrap();
        std::fs::remove_file(saved_path(&dir)).unwrap();
        fake.calls.clear();
        assert!(!ensure(ascii, "u", &dir, &mut |args| fake.run(args)).unwrap());
        assert_eq!(fake.calls.len(), 1);
        assert!(same_exe(&std::fs::read_to_string(saved_path(&dir)).unwrap(), ascii));
        // Turned off: nothing saved remains.
        set(false, ascii, "u", &dir, &mut |args| fake.run(args)).unwrap();
        assert!(!saved_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The saved path never hides a task changed elsewhere (another build, Task Scheduler).
    #[test]
    fn a_task_changed_outside_crystal_is_repaired() {
        let dir = scratch("changed");
        let exe = Path::new(r"C:\Crystal\Crystal-Launcher.exe");
        let mut fake = Fake::new(true);
        set(true, exe, "u", &dir, &mut |args| fake.run(args)).unwrap();
        // Another exe re-registered the same task; this build's saved path still names itself.
        let other = Path::new(r"C:\Old\Crystal-Launcher.exe");
        fake.registered = Some(task_xml(other, "u"));
        assert!(ensure(exe, "u", &dir, &mut |args| fake.run(args)).unwrap());
        assert!(matches(fake.registered.as_deref().unwrap(), exe));
        // Its arguments removed.
        fake.registered = Some(task_xml(exe, "u").replace("<Arguments>--background</Arguments>", ""));
        assert!(ensure(exe, "u", &dir, &mut |args| fake.run(args)).unwrap());
        assert!(matches(fake.registered.as_deref().unwrap(), exe));
        // Non-ASCII path: the saved path stands in for the command, but not for the arguments.
        let wide = Path::new(r"C:\Users\山田\Crystal-Launcher.exe");
        fake.lossy = true;
        set(true, wide, "u", &dir, &mut |args| fake.run(args)).unwrap();
        assert!(!ensure(wide, "u", &dir, &mut |args| fake.run(args)).unwrap());
        fake.registered = Some(task_xml(wide, "u").replace("<Arguments>--background</Arguments>", ""));
        assert!(ensure(wide, "u", &dir, &mut |args| fake.run(args)).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
