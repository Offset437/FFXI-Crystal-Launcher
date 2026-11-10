//! Settings → System → Open with the game: programs Crystal starts once per Play, right after
//! the first game reports started. A local action only: it touches neither the network nor
//! PlayOnline. A program already running (same image path) is not started again, and a
//! failure never blocks or delays the game launch: the work runs off the UI thread after the
//! game has started and reports back one line per failure.

use crate::i18n::tf;
use crate::Settings;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// One program in the list, as kept in the settings file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Program {
    #[serde(default)]
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Program {
    fn new() -> Self {
        Self { path: String::new(), args: String::new(), enabled: true }
    }
    /// A short name for notices and the log: the file name, else the path as typed.
    pub(crate) fn name(&self) -> String {
        std::path::Path::new(self.path.trim().trim_matches('"')).file_name()
            .map_or_else(|| self.path.trim().to_owned(), |n| n.to_string_lossy().into_owned())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Add,
    Remove(usize),
    Path(usize, String),
    Args(usize, String),
    Enabled(usize, bool),
}

/// Apply a change to the settings; true when they should be saved.
pub(crate) fn apply(settings: &mut Settings, msg: Msg) -> bool {
    let list = &mut settings.open_with_game;
    match msg {
        Msg::Add => list.push(Program::new()),
        Msg::Remove(i) if i < list.len() => { list.remove(i); }
        Msg::Path(i, v) if i < list.len() => list[i].path = v,
        Msg::Args(i, v) if i < list.len() => list[i].args = v,
        Msg::Enabled(i, on) if i < list.len() => list[i].enabled = on,
        _ => return false,
    }
    true
}

// ---- Pure decisions -------------------------------------------------------------------------

/// Once per Play batch: the first game seen while none was active opens the programs; games
/// started later in the same batch (other accounts) do not. The batch is over when no game is
/// active any more.
#[derive(Default)]
pub(crate) struct BatchGate {
    fired: bool,
}

impl BatchGate {
    /// `active`: some game is running; `newly_started`: a game was first seen this tick.
    pub(crate) fn observe(&mut self, active: bool, newly_started: bool) -> bool {
        if !active {
            self.fired = false;
            return false;
        }
        if newly_started && !self.fired {
            self.fired = true;
            return true;
        }
        false
    }
}

/// A running process: its full image path when Windows would tell, and its file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Running {
    pub image: Option<String>,
    pub name: String,
}

/// A path as compared: no quotes, no `\\?\` prefix, one separator, lower case.
pub(crate) fn normalize(path: &str) -> String {
    let p = path.trim().trim_matches('"').trim();
    let p = p.strip_prefix(r"\\?\").unwrap_or(p);
    p.replace('/', r"\").to_lowercase()
}

fn file_name(path: &str) -> String {
    normalize(path).rsplit('\\').next().unwrap_or_default().to_owned()
}

/// Whether this program's executable is already running. A process whose image path could not be
/// read (an elevated one, from a normal Crystal) counts when its file name is the same.
pub(crate) fn is_running(program: &Program, running: &[Running]) -> bool {
    let wanted = normalize(&program.path);
    let name = file_name(&program.path);
    running.iter().any(|r| match &r.image {
        Some(image) => *image == wanted,
        None => !name.is_empty() && r.name.to_lowercase() == name,
    })
}

/// The enabled programs to start: not blank, not running, each path once.
pub(crate) fn to_start<'a>(programs: &'a [Program], running: &[Running]) -> Vec<&'a Program> {
    let mut seen = std::collections::HashSet::new();
    programs.iter()
        .filter(|p| p.enabled && !p.path.trim().is_empty())
        .filter(|p| seen.insert(normalize(&p.path)))
        .filter(|p| !is_running(p, running))
        .collect()
}

/// What a batch of starts came to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub started: usize,
    /// (program name, reason) per failure.
    pub failures: Vec<(String, String)>,
    /// A program was started with Crystal's own (elevated) rights: no normal-rights start worked.
    pub kept_elevated: usize,
}

/// Start what `to_start` picks with `launch`; a failure is recorded and the rest still start.
pub(crate) fn run_with(programs: &[Program], running: &[Running], mut launch: impl FnMut(&Program) -> Result<Level, String>) -> Report {
    let mut report = Report::default();
    for program in to_start(programs, running) {
        match launch(program) {
            Ok(level) => {
                report.started += 1;
                if level == Level::Elevated { report.kept_elevated += 1; }
            }
            Err(reason) => report.failures.push((program.name(), reason)),
        }
    }
    report
}

/// Off the UI thread: read what is running, start the rest.
pub(crate) fn run(programs: Vec<Program>) -> Report {
    if !programs.iter().any(|p| p.enabled && !p.path.trim().is_empty()) {
        return Report::default();
    }
    // Only a process with one of these file names can match; no other process is opened.
    let names: std::collections::HashSet<String> = programs.iter()
        .filter(|p| p.enabled && !p.path.trim().is_empty())
        .map(|p| file_name(&p.path))
        .collect();
    run_with(&programs, &sys::running(&names), sys::launch)
}

/// The rights a program was started with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Normal,
    Elevated,
}

/// The log line for a report, or None when nothing happened.
pub(crate) fn log_lines(report: &Report) -> Vec<String> {
    let mut lines = Vec::new();
    if report.started > 0 {
        let mut line = format!("started {} program(s) with the game", report.started);
        if report.kept_elevated > 0 { line += &format!("; {} with Crystal's administrator rights", report.kept_elevated); }
        lines.push(line);
    }
    for (name, reason) in &report.failures {
        lines.push(format!("could not start {name}: {reason}"));
    }
    lines
}

/// The notice for failures: the first one, counted when there are more.
pub(crate) fn notice(report: &Report) -> Option<String> {
    let (name, reason) = report.failures.first()?;
    let first = tf("Could not open {program}: {reason}", &[("program", name), ("reason", reason)]);
    Some(match report.failures.len() {
        1 => first,
        n => format!("{first} (+{})", n - 1),
    })
}

// ---- Windows ----------------------------------------------------------------------------------

pub(crate) mod sys {
    use super::{Level, Program, Running};
    use crate::i18n::t;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ADJUST_DEFAULT,
        TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use crate::win::wide;
    use windows_sys::Win32::System::Threading::{
        CreateProcessWithTokenW, OpenProcess, OpenProcessToken, CREATE_NEW_PROCESS_GROUP, PROCESS_INFORMATION,
        PROCESS_QUERY_INFORMATION, STARTUPINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

    /// The running processes whose file name (lower case) is in `names`, with the image path
    /// where Windows gives it. Other processes are never opened.
    pub(crate) fn running(names: &std::collections::HashSet<String>) -> Vec<Running> {
        crate::win::process::processes()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| names.contains(&entry.name.to_lowercase()))
            .map(|entry| Running {
                image: crate::win::process::image_path(entry.pid).ok().map(|p| super::normalize(&p.to_string_lossy())),
                name: entry.name,
            })
            .collect()
    }

    /// Start `path` with the desktop shell's (normal-rights) token. Err when it cannot be done:
    /// no shell window, a token that will not duplicate, or the Secondary Logon service off.
    fn spawn_with_shell_token(path: &str, args: &str, dir: &str) -> Result<(), u32> {
        unsafe {
            let shell = GetShellWindow();
            if shell.is_null() { return Err(0); }
            let mut pid = 0u32;
            GetWindowThreadProcessId(shell, &mut pid);
            let process = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
            if process.is_null() { return Err(windows_sys::Win32::Foundation::GetLastError()); }
            let mut token = std::ptr::null_mut();
            let opened = OpenProcessToken(process, TOKEN_DUPLICATE, &mut token) != 0;
            CloseHandle(process);
            if !opened { return Err(windows_sys::Win32::Foundation::GetLastError()); }
            let mut primary = std::ptr::null_mut();
            let duplicated = DuplicateTokenEx(token, TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID,
                std::ptr::null(), SecurityImpersonation, TokenPrimary, &mut primary) != 0;
            CloseHandle(token);
            if !duplicated { return Err(windows_sys::Win32::Foundation::GetLastError()); }
            let mut command = wide(&if args.trim().is_empty() { format!("\"{path}\"") } else { format!("\"{path}\" {}", args.trim()) });
            let dir = wide(dir);
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessWithTokenW(primary, 0, std::ptr::null(), command.as_mut_ptr(), CREATE_NEW_PROCESS_GROUP,
                std::ptr::null(), dir.as_ptr(), &startup, &mut info) != 0;
            let error = if ok { 0 } else { windows_sys::Win32::Foundation::GetLastError() };
            CloseHandle(primary);
            if ok {
                CloseHandle(info.hProcess);
                CloseHandle(info.hThread);
                Ok(())
            } else {
                Err(error)
            }
        }
    }

    fn os_reason(error: &std::io::Error) -> String {
        if error.kind() == std::io::ErrorKind::NotFound { t("file not found").into() }
        else if error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) { t("access denied").into() }
        else { error.to_string() }
    }

    /// Start one program: its own folder as the working directory, detached, not waited for.
    /// Crystal at normal rights starts it directly; elevated, through the shell's token so the
    /// program does not inherit administrator rights, and as itself only if that fails.
    pub(crate) fn launch(program: &Program) -> Result<Level, String> {
        let path = program.path.trim().trim_matches('"').trim();
        let file = std::path::Path::new(path);
        if !file.is_absolute() || !file.is_file() { return Err(t("file not found").into()); }
        let dir = file.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let mut level = Level::Normal;
        if crate::win::process::is_elevated() {
            match spawn_with_shell_token(path, &program.args, &dir) {
                Ok(()) => return Ok(Level::Normal),
                Err(_) => level = Level::Elevated,
            }
        }
        let mut command = std::process::Command::new(file);
        if !program.args.trim().is_empty() { command.raw_arg(program.args.trim()); }
        command.current_dir(&dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()).creation_flags(CREATE_NEW_PROCESS_GROUP);
        command.spawn().map(|_child| level).map_err(|e| os_reason(&e))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn program(path: &str) -> Program {
        Program { path: path.into(), args: String::new(), enabled: true }
    }
    fn image(path: &str) -> Running {
        Running { image: Some(normalize(path)), name: path.rsplit('\\').next().unwrap().into() }
    }

    #[test]
    fn the_list_round_trips_and_older_files_have_none() {
        let mut settings = Settings::default();
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("open_with_game"), "an empty list adds nothing to the file");
        let old: Settings = serde_json::from_str(r#"{"install_path":"C:\\FFXI"}"#).unwrap();
        assert!(old.open_with_game.is_empty());
        settings.open_with_game = vec![
            Program { path: r"C:\Tools\Voice.exe".into(), args: "--minimized".into(), enabled: false },
            program(r"D:\Overlay\overlay.exe"),
        ];
        let back: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(back.open_with_game, settings.open_with_game);
        // A program entry missing its optional fields reads as enabled with no arguments.
        let p: Program = serde_json::from_str(r#"{"path":"C:\\a.exe"}"#).unwrap();
        assert!(p.enabled && p.args.is_empty());
    }

    #[test]
    fn edits_apply_by_index_and_ignore_a_stale_one() {
        let mut settings = Settings::default();
        assert!(apply(&mut settings, Msg::Add));
        assert!(apply(&mut settings, Msg::Path(0, r"C:\a.exe".into())));
        assert!(apply(&mut settings, Msg::Args(0, "-x".into())));
        assert!(apply(&mut settings, Msg::Enabled(0, false)));
        assert_eq!(settings.open_with_game, vec![Program { path: r"C:\a.exe".into(), args: "-x".into(), enabled: false }]);
        assert!(!apply(&mut settings, Msg::Remove(3)));
        assert!(apply(&mut settings, Msg::Remove(0)));
        assert!(settings.open_with_game.is_empty());
    }

    #[test]
    fn programs_open_once_per_play_batch() {
        let mut gate = BatchGate::default();
        assert!(!gate.observe(false, false));
        assert!(gate.observe(true, true), "the first game opens them");
        assert!(!gate.observe(true, false));
        assert!(!gate.observe(true, true), "a second account's game in the same batch does not");
        assert!(!gate.observe(false, false), "every game closed: the batch is over");
        assert!(gate.observe(true, true), "the next Play opens them again");
    }

    #[test]
    fn a_running_executable_is_matched_by_full_path_and_skipped() {
        let programs = [program(r"C:\Tools\Voice.exe"), program(r"D:\Other\Voice.exe"), program(r"C:\Tools\Overlay.exe")];
        let running = [image(r"c:\tools\voice.exe")];
        let start: Vec<&str> = to_start(&programs, &running).iter().map(|p| p.path.as_str()).collect();
        assert_eq!(start, [r"D:\Other\Voice.exe", r"C:\Tools\Overlay.exe"], "same name, other folder, still starts");
        // An unreadable image (elevated process) matches by file name instead.
        let hidden = [Running { image: None, name: "Overlay.exe".into() }];
        let start: Vec<&str> = to_start(&programs, &hidden).iter().map(|p| p.path.as_str()).collect();
        assert_eq!(start, [r"C:\Tools\Voice.exe", r"D:\Other\Voice.exe"]);
    }

    #[test]
    fn disabled_blank_and_repeated_entries_do_not_start() {
        let mut off = program(r"C:\off.exe");
        off.enabled = false;
        let programs = [off, program("  "), program(r"C:\A.exe"), program(r#""c:/a.exe""#)];
        let start = to_start(&programs, &[]);
        assert_eq!(start.len(), 1);
        assert_eq!(start[0].path, r"C:\A.exe");
        assert_eq!(normalize(r"\\?\C:\Tools/X.EXE"), r"c:\tools\x.exe");
    }

    #[test]
    fn a_failure_is_reported_and_the_others_still_start() {
        let programs = [program(r"C:\missing.exe"), program(r"C:\ok.exe"), program(r"C:\denied.exe")];
        let mut tried = Vec::new();
        let report = run_with(&programs, &[], |p| {
            tried.push(p.path.clone());
            match p.path.as_str() {
                r"C:\missing.exe" => Err("file not found".into()),
                r"C:\denied.exe" => Err("access denied".into()),
                _ => Ok(Level::Normal),
            }
        });
        assert_eq!(tried.len(), 3, "a failure does not stop the rest");
        assert_eq!(report.started, 1);
        assert_eq!(report.failures, [("missing.exe".to_owned(), "file not found".to_owned()), ("denied.exe".to_owned(), "access denied".to_owned())]);
        assert_eq!(log_lines(&report).len(), 3);
        assert!(notice(&report).unwrap().ends_with("(+1)"));
        assert!(notice(&Report::default()).is_none());
    }

    #[test]
    fn a_missing_file_fails_at_once_without_starting_anything() {
        let started = std::time::Instant::now();
        let report = run(vec![program(r"C:\definitely\not\here.exe")]);
        assert_eq!(report.started, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
