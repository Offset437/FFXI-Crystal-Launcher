//! Launch diagnostics: why a game client ended, read from the tail of its stderr log,
//! plus the one-line-per-launch history file. Nothing here sees passwords or session
//! bytes; it only reads the client's own log text and exit status.

use crate::i18n::{t, t_in, tf_in, Lang};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// A client that ends sooner than this after it was spawned counts as a startup failure.
const STARTUP_WINDOW: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExitKind {
    /// A clean game exit or an acknowledged native sign-out.
    Normal,
    /// `GameStart returned ...; exit code N`.
    Code { code: i32 },
    /// Ended soon after spawn with nothing recognisable in the log.
    Startup,
    /// Anything else.
    Other,
    /// The POL-less loader reported an error (its message, one line).
    LoaderError { message: String },
    /// The background session worker process went away without a report.
    WorkerEnded,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExitReport {
    pub kind: ExitKind,
    /// The process exit status, when Windows gave one.
    pub exit_status: Option<i32>,
    /// Whether the client ended inside the startup window.
    pub during_startup: bool,
    /// One English line for the history file and logs; the Accounts page shows
    /// `message()`, the same line in the current language.
    pub summary: String,
}

impl ExitReport {
    /// Everything except a clean quit is worth the user's attention.
    pub fn is_problem(&self) -> bool {
        self.kind != ExitKind::Normal
    }

    /// `summary` in the current language, worded from the report's fields.
    pub fn message(&self) -> String {
        self.message_in(crate::i18n::current())
    }

    /// The report's text in `lang`; `summary` is this in English.
    fn message_in(&self, lang: Lang) -> String {
        let t = |en: &'static str| crate::i18n::t_in(lang, en);
        let tf = |en: &'static str, args: &[(&str, &dyn std::fmt::Display)]| tf_in(lang, en, args);
        let startup = self.during_startup;
        let process = self.exit_status.filter(|code| *code != 0);
        match &self.kind {
            ExitKind::Normal => t("Game closed normally").into(),
            ExitKind::Code { code } if startup => tf("Game exited during startup with code {code}", &[("code", code)]),
            ExitKind::Code { code } => tf("Game exited with code {code}", &[("code", code)]),
            ExitKind::Startup => match process {
                Some(code) => tf("Game exited during startup (process exit {code})", &[("code", &code)]),
                None => t("Game exited during startup").into(),
            },
            ExitKind::Other => match process {
                Some(code) => tf("Game closed unexpectedly (process exit {code})", &[("code", &code)]),
                None => t("Game closed unexpectedly").into(),
            },
            ExitKind::LoaderError { message } if startup => {
                tf("Game loader error during startup: {message}", &[("message", message)])
            }
            ExitKind::LoaderError { message } => tf("Game loader error: {message}", &[("message", message)]),
            ExitKind::WorkerEnded => {
                t("The background session worker ended unexpectedly. Check the game's connection.").into()
            }
        }
    }

    /// A report whose `summary` is its English message.
    fn new(kind: ExitKind, exit_status: Option<i32>, during_startup: bool) -> Self {
        let mut report = ExitReport { kind, exit_status, during_startup, summary: String::new() };
        report.summary = report.message_in(Lang::En);
        report
    }

    /// The report for a session worker that ended without writing one.
    pub fn worker_ended() -> Self {
        Self::new(ExitKind::WorkerEnded, None, false)
    }

    /// The report for a session that ended with its own failure message: an unexpected end.
    pub fn other(exit_status: Option<i32>) -> Self {
        Self::new(ExitKind::Other, exit_status, false)
    }
}

/// How a POL-less session (crystal-loader under Ashita or Windower) ended, as its session
/// pipe reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoaderEnd {
    /// `{"type":"exit","code":N}`.
    Exit(i32),
    /// Exit 0 after the native idle session accepted the user's sign-out.
    SignedOut,
    /// `{"type":"error","message":...}` (already flattened to one line).
    Error(String),
    /// The pipe closed (or failed) without an exit line.
    Closed,
}

/// Classify a loader session's end. `ran` is measured from when the session was handed over.
pub fn classify_loader_end(end: &LoaderEnd, ran: Duration) -> ExitReport {
    let during_startup = ran < STARTUP_WINDOW && !matches!(end, LoaderEnd::SignedOut);
    let (kind, exit_status) = match end {
        LoaderEnd::SignedOut => (ExitKind::Normal, Some(0)),
        LoaderEnd::Exit(0) if !during_startup => (ExitKind::Normal, Some(0)),
        LoaderEnd::Exit(code) => (ExitKind::Code { code: *code }, Some(*code)),
        LoaderEnd::Error(message) => {
            (ExitKind::LoaderError { message: message.chars().take(160).collect() }, None)
        }
        LoaderEnd::Closed if during_startup => (ExitKind::Startup, None),
        LoaderEnd::Closed => (ExitKind::Other, None),
    };
    ExitReport::new(kind, exit_status, during_startup)
}

/// What a supervisor needs to describe and record one launch when it ends.
#[derive(Clone)]
pub struct Session {
    pub history: PathBuf,
    pub label: String,
    pub log: PathBuf,
}

impl Session {
    pub fn record(&self, outcome: &str) {
        append_history(&self.history, &self.label, outcome, Some(&self.log));
    }
}

/// `<time> | <label> | <outcome> | <log path>`. Fields are flattened to one line so a label
/// or message can never forge or split a history entry.
pub fn history_line(time: &str, label: &str, outcome: &str, log: Option<&Path>) -> String {
    let flat = |s: &str| {
        s.chars()
            .map(|c| if c.is_control() || c == '|' { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let log = log.map_or_else(|| "-".to_owned(), |p| flat(&p.to_string_lossy()));
    format!("{time} | {} | {} | {log}", flat(label), flat(outcome))
}

/// Best-effort append; diagnostics must never fail a launch.
pub fn append_history(history: &Path, label: &str, outcome: &str, log: Option<&Path>) {
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = history_line(&time, label, outcome, log);
    if let Some(parent) = history.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(history) {
        // One write per line: the UI, the prefetch thread and every worker append at once.
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

/// A launch that ended in `error`: its message (at most 240 characters) and its log path.
pub fn record_failure(history: &Path, label: &str, error: &str) {
    let (message, log) = split_log(error);
    let message: String = message.chars().take(240).collect();
    append_history(history, label, &message, log.as_deref());
}

/// Append `[time] [launcher] <line>` lines to an existing launch log (the pre-signed and
/// batch paths add their trigger and W20 lines to the session's log; 07d §4.2, 08 §4
/// item 6). One append per line, flattened, like `SessionLog::line`; best effort.
pub fn append_launcher_lines(log: &Path, lines: &[String]) {
    append_launcher_lines_to(log, lines, false);
}

/// `append_launcher_lines`; with `create`, the log (and its folder) is made when missing.
pub fn append_launcher_lines_to(log: &Path, lines: &[String], create: bool) {
    if lines.is_empty() { return; }
    if create {
        if let Some(parent) = log.parent() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(create).append(true).open(log) {
        let time = chrono::Local::now().format("%H:%M:%S");
        for line in lines {
            let flat: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            let _ = file.write_all(format!("[{time}] [launcher] {flat}\n").as_bytes());
        }
    }
}

/// A sign-in that failed because the network was not there (07d §5.5): the W2U's
/// "network not ready" verdict (any language), or polcore's -19 during the member login,
/// as at 18:58 when DNS failed during a VPN start (08 §1 row 8). Such a failure never
/// reached a password check, so retrying once the network is back costs no extra login.
pub fn network_failure(message: &str) -> bool {
    crate::viewer::is_network_not_ready_message(message) || message.contains("POL result -19)")
}

/// A sign-in that PlayOnline's member login refused: a verdict on the account, such as its
/// password, or a one-time code it asked for and was not given. The loader's words are the
/// same for both (only POL's result number differs, and none is known to mean "a code is
/// needed"), so a refused sign-in that went without a code may be an account that now uses one.
/// Only the loader's member-login stage counts: its network-level connection fails before any
/// password or code is sent, a cancelled login gave no verdict, and a network failure (-19) is
/// none either. An older loader names no stage, so none of its failures count.
pub fn sign_in_refused(message: &str) -> bool {
    message.contains("Native PlayOnline member login failed (state ") && !network_failure(message)
}

/// A sign-in that ended because its worker or game host went away before reporting a
/// result (no password, OTP or version verdict was given), so the same Play can be started
/// again (07d §5.4).
pub fn transient_failure(message: &str) -> bool {
    const ENDED: [&str; 4] = [
        "The background session worker ended before starting the game.",
        "The background session worker ended.",
        "The background session worker ended unexpectedly. Check the game's connection.",
        "The background session ended before starting the game.",
    ];
    Lang::ALL.into_iter().any(|lang| {
        ENDED.iter().any(|text| message.starts_with(t_in(lang, text)))
            || exited_during_startup(lang, message)
    })
}

/// "Game exited during startup", bare or with its process exit, in `lang`, with or without
/// the worker's "({tool} via crystal-loader)." wrapper; never "… with code N" or a loader error.
fn exited_during_startup(lang: Lang, message: &str) -> bool {
    let wrapper = t_in(lang, "{summary} ({tool} via crystal-loader).");
    let (prefix, rest) = wrapper.split_once("{summary}").unwrap_or(("", wrapper));
    let opening = rest.split_once("{tool}").map_or(rest, |(before, _)| before);
    let message = message.strip_prefix(prefix).unwrap_or(message);
    let summary = if opening.is_empty() {
        message
    } else {
        message.find(opening).map_or(message, |at| &message[..at])
    };
    let summary = summary.trim_end().trim_end_matches(['.', '。']);
    if summary == t_in(lang, "Game exited during startup") {
        return true;
    }
    let process = t_in(lang, "Game exited during startup (process exit {code})");
    process.split_once("{code}").is_some_and(|(before, after)| {
        summary
            .strip_prefix(before)
            .and_then(|code| code.strip_suffix(after))
            .is_some_and(|code| code.parse::<i32>().is_ok())
    })
}

/// Split a launch error such as `"Client exited (...). Log: C:\x\a.stderr.log"` into the
/// message and the log path it points at.
pub fn split_log(error: &str) -> (&str, Option<PathBuf>) {
    match error.rsplit_once("Log: ") {
        Some((message, path)) if !path.trim().is_empty() && !path.contains('\n') => {
            (message.trim_end(), Some(PathBuf::from(path.trim())))
        }
        _ => (error.trim_end(), None),
    }
}

/// Check that `target` is inside `logs_dir` (after resolving `..` and links) and, for a file,
/// is a `.log` file. Returns the resolved path to hand to the shell.
pub fn resolve_log_target(logs_dir: &Path, target: &Path) -> Result<PathBuf, String> {
    let dir = fs::canonicalize(logs_dir).map_err(|_| t("The logs folder does not exist yet."))?;
    let resolved = fs::canonicalize(target).map_err(|_| t("That log file no longer exists."))?;
    if !resolved.starts_with(&dir) {
        return Err(t("That file is not in this account's logs folder.").into());
    }
    if resolved.is_file() && resolved.extension().and_then(|e| e.to_str()) != Some("log") {
        return Err(t("Only .log files can be opened from here.").into());
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loader_session_ends_are_classified() {
        let long = Duration::from_secs(600);
        let short = Duration::from_secs(5);
        let normal = classify_loader_end(&LoaderEnd::Exit(0), long);
        assert_eq!(normal.kind, ExitKind::Normal);
        assert_eq!(normal.summary, "Game closed normally");
        assert!(!normal.is_problem());
        assert_eq!(classify_loader_end(&LoaderEnd::Exit(0), short).summary, "Game exited during startup with code 0");
        let code = classify_loader_end(&LoaderEnd::Exit(-14), long);
        assert_eq!(code.kind, ExitKind::Code { code: -14 });
        assert_eq!(code.exit_status, Some(-14));
        let error = classify_loader_end(&LoaderEnd::Error("FFXiMain.dll not found".into()), short);
        assert_eq!(error.summary, "Game loader error during startup: FFXiMain.dll not found");
        assert!(error.is_problem());
        assert_eq!(classify_loader_end(&LoaderEnd::Closed, short).kind, ExitKind::Startup);
        assert_eq!(classify_loader_end(&LoaderEnd::Closed, long).summary, "Game closed unexpectedly");
    }

    #[test]
    fn acknowledged_native_sign_out_is_clean_even_inside_the_startup_window() {
        let short = Duration::from_secs(5);
        let signed_out = classify_loader_end(&LoaderEnd::SignedOut, short);
        assert_eq!(signed_out.kind, ExitKind::Normal);
        assert_eq!(signed_out.exit_status, Some(0));
        assert!(!signed_out.during_startup);
        assert!(!signed_out.is_problem());
        // Time and a zero exit alone never prove an intentional sign-out.
        for end in [LoaderEnd::Exit(0), LoaderEnd::Exit(-14), LoaderEnd::Closed,
            LoaderEnd::Error("sign-out failed".into())] {
            let report = classify_loader_end(&end, short);
            assert!(report.is_problem(), "{end:?}");
            assert!(report.during_startup, "{end:?}");
        }
    }

    /// The row's text is the history summary, worded from the report's fields
    /// in the current language, so a language change reaches it too.
    #[test]
    fn shown_message_matches_the_english_summary_for_every_kind() {
        crate::i18n::set(crate::i18n::Lang::En);
        let long = Duration::from_secs(600);
        let short = Duration::from_secs(5);
        let reports = [
            classify_loader_end(&LoaderEnd::Exit(0), long),
            classify_loader_end(&LoaderEnd::Exit(-14), short),
            classify_loader_end(&LoaderEnd::Error("FFXiMain.dll not found".into()), short),
            classify_loader_end(&LoaderEnd::Error("pipe broke".into()), long),
            classify_loader_end(&LoaderEnd::Closed, short),
            classify_loader_end(&LoaderEnd::Closed, long),
            ExitReport::worker_ended(),
            classify_loader_end(&LoaderEnd::SignedOut, short),
            ExitReport::other(Some(1)),
            ExitReport::other(None),
        ];
        for report in &reports {
            assert_eq!(report.message(), report.summary, "{report:?}");
        }
        // The worker's record format still reads back (new kinds included).
        let encoded = serde_json::to_string(&reports[2]).unwrap();
        assert_eq!(serde_json::from_str::<ExitReport>(&encoded).unwrap(), reports[2]);
    }

    #[test]
    fn a_failed_session_is_an_unexpected_end() {
        crate::i18n::set(crate::i18n::Lang::En);
        let report = ExitReport::other(Some(1));
        assert_eq!(report.kind, ExitKind::Other);
        assert!(!report.during_startup && report.is_problem());
        assert_eq!(report.summary, "Game closed unexpectedly (process exit 1)");
        assert_eq!(ExitReport::other(None).summary, "Game closed unexpectedly");
    }

    #[test]
    fn history_lines_are_single_line_and_carry_all_fields() {
        let line = history_line(
            "2026-10-03 12:00:00",
            "Main | account\n",
            "Game crashed\r\n(no translation at 102dbbc4)",
            Some(Path::new(r"C:\data\logs\a.stderr.log")),
        );
        assert_eq!(
            line,
            r"2026-10-03 12:00:00 | Main   account | Game crashed  (no translation at 102dbbc4) | C:\data\logs\a.stderr.log"
        );
        assert!(!line.contains('\n') && !line.contains('\r'));
        assert!(history_line("t", "a", "b", None).ends_with("| b | -"));
    }

    /// 07d §5.4-5.5: only a missing network or a host that went away without a verdict
    /// may start the same sign-in again; a refused password, an OTP, an update or a
    /// loader error never does.
    #[test]
    fn retryable_failures_are_network_or_a_vanished_host_only() {
        crate::i18n::set(crate::i18n::Lang::En);
        assert!(network_failure("Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader)."));
        assert!(!network_failure("Game loader error during startup: Native PlayOnline login failed (state 39, POL result -1036). (Windower via crystal-loader)."));
        assert!(!network_failure("Game loader error during startup: Native PlayOnline login failed (state 39, POL result -190)."));
        for message in ["The background session worker ended before starting the game.", "The background session worker ended.",
            "Game exited during startup (Windower via crystal-loader).", "The background session ended before starting the game."] {
            assert!(transient_failure(message), "{message}");
        }
        for message in ["Game exited during startup with code 0 (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline login failed (state -1036, POL result -1036). (Windower via crystal-loader).",
            "Enter a six-digit one-time code, or leave it empty.", "Sign-in failed"] {
            assert!(!transient_failure(message), "{message}");
        }
        // Workers write in the window's language: full-width brackets, 。 and no space.
        for message in ["起動中にゲームが終了しました（crystal-loader経由のWindower）。",
            "起動中にゲームが終了しました（crystal-loader経由のAshita）。 Log: C:\\d\\a.log",
            "起動中にゲームが終了しました（終了コード 3）（crystal-loader経由のWindower）。",
            "起動中にゲームが終了しました", "Game exited during startup (process exit 3) (Windower via crystal-loader).",
            "ゲームの起動前にバックグラウンドのセッション処理が終了しました。"] {
            assert!(transient_failure(message), "{message}");
        }
        for message in ["起動中にゲームが終了しました（コード 0）（crystal-loader経由のWindower）。",
            "起動中にゲームのローダーでエラーが発生しました：x（crystal-loader経由のWindower）。"] {
            assert!(!transient_failure(message), "{message}");
        }
    }

    #[test]
    fn launcher_lines_can_create_their_log() {
        let dir = std::env::temp_dir().join(format!("xi-launch-create-{}", crate::backend::unique_id().unwrap()));
        let log = dir.join("nested").join("a.crystal.log");
        append_launcher_lines_to(&log, &["first".into()], true);
        append_launcher_lines_to(&log, &["second\tline".into()], true);
        let text = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("[launcher] first"));
        assert!(lines[1].ends_with("[launcher] second line"));
        // The plain append never creates a session log; it flattens lines into an existing one.
        let existing = dir.join("existing.crystal.log");
        append_launcher_lines(&existing, &["never created".into()]);
        assert!(!existing.exists(), "only an existing session log is appended to");
        fs::write(&existing, "[00:00:00] [launcher] first\n").unwrap();
        append_launcher_lines(&existing, &["launch trigger: pre-signed Play".into(), "two\nlines".into()]);
        let text = fs::read_to_string(&existing).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().nth(1).unwrap().ends_with("[launcher] launch trigger: pre-signed Play"));
        assert!(text.lines().nth(2).unwrap().ends_with("[launcher] two lines"));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Owner decision 2026-10-13: a code-less sign-in the member login refused may be an account
    /// that now uses a one-time code. A network failure, a vanished host, a login that never
    /// started or a loader error is no refusal. Review finding: neither is a failure of the
    /// network-level connection before any password is sent (POL down or unreachable), a
    /// cancelled login, nor an older loader's unnamed stage.
    #[test]
    fn member_login_refusals_are_told_apart_from_other_failures() {
        for message in ["Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -1036). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline member login failed (state -1036, POL result -1036). (Windower via crystal-loader)."] {
            assert!(sign_in_refused(message), "{message}");
        }
        for message in ["Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -19). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline network connection failed (state -1025, POL result -1036). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline network connection failed (state 30, POL result 0). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline login was cancelled at state -1 (POL result -1036). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -1036). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline login could not start (mode 1, POL result -3).",
            "Game loader error during startup: Native PlayOnline login timed out at state 23.",
            "The background session worker ended before starting the game.",
            "Game loader error during startup: FFXiMain.dll not found", "Sign-in failed"] {
            assert!(!sign_in_refused(message), "{message}");
        }
        // Either stage's -19 is still the network failure that is retried.
        assert!(network_failure("Game loader error during startup: Native PlayOnline network connection failed (state -19, POL result -19). (Windower via crystal-loader)."));
        assert!(network_failure("Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -19). (Windower via crystal-loader)."));
    }

    #[test]
    fn launch_errors_split_into_message_and_log_path() {
        let (message, log) = split_log(r"Client exited during login/startup (exit code: 1). Log: C:\d\logs\a.stderr.log");
        assert_eq!(message, "Client exited during login/startup (exit code: 1).");
        assert_eq!(log, Some(PathBuf::from(r"C:\d\logs\a.stderr.log")));
        assert_eq!(split_log("Retail authentication failed: bad").1, None);
    }

    #[test]
    fn history_append_round_trip() {
        let dir = std::env::temp_dir().join(format!("xi-launch-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("logs")).unwrap();
        let log = dir.join("logs").join("a.stderr.log");
        let history = dir.join("iced-launcher").join("launch-history.log");
        let session = Session { history: history.clone(), label: "Main".into(), log: log.clone() };
        let report = classify_loader_end(&LoaderEnd::Error("FFXiMain.dll not found".into()), Duration::from_secs(3));
        session.record(&report.summary);
        session.record("second");
        let text = fs::read_to_string(&history).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("| Main | Game loader error during startup: FFXiMain.dll not found |"));
        assert!(lines[0].ends_with("a.stderr.log"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn opener_stays_inside_the_logs_folder() {
        let dir = std::env::temp_dir().join(format!("xi-launch-open-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("a.stderr.log"), "x").unwrap();
        fs::write(logs.join("a.exe"), "x").unwrap();
        fs::write(dir.join("secret.log"), "x").unwrap();
        assert!(resolve_log_target(&logs, &logs.join("a.stderr.log")).is_ok());
        assert!(resolve_log_target(&logs, &logs).is_ok());
        assert!(resolve_log_target(&logs, &logs.join("a.exe")).is_err());
        assert!(resolve_log_target(&logs, &dir.join("secret.log")).is_err());
        assert!(resolve_log_target(&logs, &logs.join("..").join("secret.log")).is_err());
        assert!(resolve_log_target(&logs, &logs.join("missing.log")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
