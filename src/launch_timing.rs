//! Launch stage timing: how long each step takes from the Play click to the game running.
//!
//! A `Timeline` records elapsed time (a monotonic `Instant`, one small `Vec` push per mark)
//! for the sign-in stages and for the milestones every launch path shares. It only ever sees
//! stage names and fixed milestone labels, never account fields or session bytes.
//!
//! Output has three shapes:
//! - one compact line per mark for the launch's own log (`unwritten_lines`),
//! - a one-line summary for `launch-history.log` (`summary`): each phase's own duration,
//!   ending with the total from the click, e.g. `sign-in 2.8s, tool 0.4s, hello 1.9s,
//!   game started 9.4s`,
//! - a short line for the account row (`row_summary`), both figures from the click:
//!   `Signed in 2.8s · game started 9.4s`.

use crate::i18n::tf;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The same machine's performance counter, carried only for diagnostic timing.
/// Unlike a serialized elapsed duration, it includes child startup and pipe transit.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct MonotonicStamp { ticks: u64, frequency: u64 }

impl MonotonicStamp {
    pub(crate) fn capture(clicked: Instant) -> Option<Self> {
        let now = Instant::now();
        let (ticks, frequency) = performance_counter()?;
        let elapsed = now.checked_duration_since(clicked)?.as_nanos();
        let passed = u64::try_from(elapsed.checked_mul(frequency as u128)? / 1_000_000_000).ok()?;
        Some(Self { ticks: ticks.checked_sub(passed)?, frequency })
    }

    pub(crate) fn restore(stamp: Option<Self>, fallback_elapsed_ms: u64) -> Instant {
        let now = Instant::now();
        let elapsed = stamp.and_then(|stamp| {
            let (ticks, frequency) = performance_counter()?;
            if stamp.frequency != frequency { return None; }
            let passed = ticks.checked_sub(stamp.ticks)?;
            Some(Duration::new(passed / frequency,
                ((passed % frequency) as u128 * 1_000_000_000 / frequency as u128) as u32))
        }).unwrap_or_else(|| Duration::from_millis(fallback_elapsed_ms));
        now.checked_sub(elapsed).unwrap_or(now)
    }
}

fn performance_counter() -> Option<(u64, u64)> {
    // One counter/frequency shared across local processes.
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut ticks, mut frequency) = (0i64, 0i64);
    // SAFETY: both calls write to live, correctly sized local integers.
    if unsafe { QueryPerformanceCounter(&mut ticks) } == 0
        || unsafe { QueryPerformanceFrequency(&mut frequency) } == 0
        || ticks < 0 || frequency <= 0 { return None; }
    Some((ticks as u64, frequency as u64))
}

/// The points every launch path reports (each path reports the ones that exist for it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    SignInStart,
    SignInDone,
    /// The Ashita or Windower process was started.
    ToolStarted,
    /// crystal-loader connected and its image was verified.
    Hello,
    /// The session line reached the loader.
    SessionSent,
    /// A parked host got its `credentials` line (pol-spec 07d §1.2): replaces
    /// ToolStarted/Hello/SessionSent on the parked path.
    CredentialsSent,
    /// The loader reported `started` (GameStart called).
    GameStarted,
    /// This sign-in's Viewer check (W2U :54000) closed and was answered (pol-spec 07c §4).
    ViewerChecked,
    /// The loader's `native_main_menu`: the Viewer presence publish completed.
    MainMenu,
    /// The FFXI version check (W20 :53001) returned its verdict.
    GameChecked,
}

impl Milestone {
    /// The words used in the log lines.
    fn log_label(self) -> &'static str {
        match self {
            Self::SignInStart => "sign-in start",
            Self::SignInDone => "sign-in done",
            Self::ToolStarted => "tool process started",
            Self::Hello => "loader hello",
            Self::SessionSent => "session delivered",
            Self::CredentialsSent => "credentials delivered",
            Self::GameStarted => "game started",
            Self::ViewerChecked => "W2U done",
            Self::MainMenu => "main menu reached",
            Self::GameChecked => "W20 done",
        }
    }
    /// The account row's text for a terminal milestone reached `at` after the
    /// click: capitalised when it starts the line, lower-case after
    /// "Signed in ...". Shown only, never stored in a file, so it is translated
    /// (the history summary above keeps its English words).
    fn row_text(self, at: Duration, first: bool) -> String {
        let time = secs(at, 1);
        let args: &[(&str, &dyn std::fmt::Display)] = &[("time", &time)];
        match (self, first) {
            (Self::GameStarted, true) => tf("Game started {time}", args),
            (Self::GameStarted, false) => tf("game started {time}", args),
            // Only `TERMINAL` reaches the row; any other milestone would show
            // its time alone rather than an untranslated (history) label.
            _ => time,
        }
    }
    /// The words used in the history summary.
    fn summary_label(self) -> &'static str {
        match self {
            Self::SignInStart => "start",
            Self::SignInDone => "sign-in",
            Self::ToolStarted => "tool",
            Self::Hello => "hello",
            Self::SessionSent => "session",
            Self::CredentialsSent => "credentials",
            Self::GameStarted => "game started",
            Self::ViewerChecked => "W2U",
            Self::MainMenu => "main menu",
            Self::GameChecked => "W20",
        }
    }
}

/// What started a launch (pol-spec 07d §4.2): one `launch trigger: <name>` line per launch
/// log, right after `launch clock`, so measurements can be split by path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaunchTrigger {
    /// The Play button on an account that is not signed in (cold).
    #[default]
    Play,
    /// "Sign accounts in when Crystal opens" (Settings → System), for each account known to
    /// sign in without a one-time code.
    StartupSignIn,
    /// The one retry of a startup sign-in that failed on the network (07d §5.5).
    StartupRetry,
    /// The explicit sign-in ("Connect").
    Connect,
    /// A Play that adopted this account's parked host.
    ParkedPlay,
    /// An explicit sign-in that adopted this account's parked host.
    ParkedConnect,
    /// A Play on a session already at its main menu (W20 + StartGame only).
    PreSignedPlay,
    /// A Play queued during a pre-sign-in (started together once its main menu is idle).
    QueuedPlay,
    /// A multi-account Play (the batch coordinator).
    BatchPlay,
    /// A queued Play re-issued cold after its pre-sign-in failed (07d §5.4).
    ReissuedPlay,
    /// A pre-signed account signed back in after its game closed (owner, 2026-10-12).
    SignInAfterGame,
    /// The one retry of that sign-in after it failed on the network.
    SignInAfterGameRetry,
    /// A sign-in held at the main menu during maintenance for the FFXI version check
    /// (patch_watch.rs), as a stock user signs in to press "Check Version".
    MaintenanceSignIn,
}
impl LaunchTrigger {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::StartupSignIn => "startup sign-in",
            Self::StartupRetry => "startup sign-in retry",
            Self::Connect => "Connect",
            Self::ParkedPlay => "parked Play",
            Self::ParkedConnect => "parked Connect",
            Self::PreSignedPlay => "pre-signed Play",
            Self::QueuedPlay => "queued Play",
            Self::BatchPlay => "batch Play",
            Self::ReissuedPlay => "re-issued Play",
            Self::SignInAfterGame => "sign-in after game",
            Self::SignInAfterGameRetry => "sign-in after game retry",
            Self::MaintenanceSignIn => "maintenance sign-in",
        }
    }
    /// The launch-log line.
    pub(crate) fn line(self) -> String { format!("launch trigger: {}", self.name()) }
    /// A sign-in Crystal starts by itself, without a code (owner decision 2026-10-13): at its
    /// start, after a game closed, or the one network retry of either.
    pub(crate) fn is_presign(self) -> bool {
        matches!(self, Self::StartupSignIn | Self::StartupRetry | Self::SignInAfterGame | Self::SignInAfterGameRetry | Self::MaintenanceSignIn)
    }
}

/// TCP timer quanta that a lost packet adds to one step (08 §1 row 1): Windows' SYN (or
/// DNS) retry, Square Enix's SYN-ACK retransmission, its data retransmission, and that
/// one doubled. (low, high) in seconds.
const LOSS_QUANTA: [(f64, f64, &str); 4] = [
    (1.0, 1.0, "1.0 s SYN/DNS retry"),
    (3.36, 3.36, "3.36 s SYN-ACK retransmission"),
    (4.1, 4.55, "4.1-4.55 s server retransmission"),
    (8.2, 9.1, "8.2-9.1 s doubled server retransmission"),
];
const LOSS_SLACK: f64 = 0.15;

/// A "possible packet loss" note when `actual` exceeds the step's `clean` value by one
/// quantum, or by two, ±0.15 s (07d §4.5). The same format is written by the loader
/// for its own request steps. `None` when it does not fit.
pub(crate) fn possible_loss(step: &str, actual: Duration, clean: Duration) -> Option<String> {
    let excess = actual.as_secs_f64() - clean.as_secs_f64();
    let fits = |low: f64, high: f64| excess >= low - LOSS_SLACK && excess <= high + LOSS_SLACK;
    let label = LOSS_QUANTA.iter().find(|(low, high, _)| fits(*low, *high)).map(|(.., name)| (*name).to_owned())
        .or_else(|| LOSS_QUANTA.iter().flat_map(|a| LOSS_QUANTA.iter().map(move |b| (a, b)))
            .find(|(a, b)| fits(a.0 + b.0, a.1 + b.1)).map(|(a, b)| format!("{} + {}", a.2, b.2)))?;
    Some(format!("possible packet loss: {step} took {:.2} s, {excess:+.2} s over its clean {:.2} s ({label})",
        actual.as_secs_f64(), clean.as_secs_f64()))
}

/// Clean W2U/W20 step durations on this PC (07d §4.5 table: resolve warm, connect and
/// reply one RTT each).
const CHECK_CLEAN: [(&str, Duration); 3] = [
    ("resolve", Duration::from_millis(3)),
    ("connect", Duration::from_millis(180)),
    ("reply", Duration::from_millis(170)),
];

/// The launch-log lines for one stock version check on any path (07d §2.4, §4.5):
/// `<check>: <phases>.` (when there are phases), then a loss note for each attempt step
/// that overshoots its clean value by a TCP timer quantum. `check` is
/// `Viewer check (W2U)` or `FFXI check (W20)`.
pub(crate) fn check_lines(check: &str, timing: &crate::patch::CheckTiming) -> Vec<String> {
    let mut lines: Vec<String> = timing.phases().map(|phases| format!("{check}: {phases}.")).into_iter().collect();
    let short = check.rsplit_once('(').map_or(check, |(_, name)| name.trim_end_matches(')'));
    let several = timing.attempts.len() > 1;
    for (index, attempt) in timing.attempts.iter().enumerate() {
        for ((name, clean), actual) in CHECK_CLEAN.iter().zip([attempt.resolve, attempt.connect, attempt.reply]) {
            let Some(actual) = actual else { continue };
            let step = if several { format!("{short} attempt {} {name}", index + 1) } else { format!("{short} {name}") };
            lines.extend(possible_loss(&step, actual, *clean));
        }
    }
    lines
}

/// Phases reported as their own duration. Native POL signs in after the tool
/// starts; older routes sign in first, so their actual timestamps set the order.
/// The stock checks and the main menu (pol-spec 07c §5.1) split the native sign-in:
/// W2U before the member login, the main-menu publish after it, then W20.
const PHASES: [Milestone; 8] = [
    Milestone::SignInDone,
    Milestone::ToolStarted,
    Milestone::Hello,
    Milestone::SessionSent,
    Milestone::CredentialsSent,
    Milestone::ViewerChecked,
    Milestone::MainMenu,
    Milestone::GameChecked,
];
/// A launch that got this far is reported as a total from the click.
const TERMINAL: Milestone = Milestone::GameStarted;

#[derive(Clone, Debug)]
struct Mark {
    at: Duration,
    milestone: Option<Milestone>,
    label: String,
}

#[derive(Clone, Debug)]
pub struct Timeline {
    start: Instant,
    marks: Vec<Mark>,
    /// How many marks `unwritten_lines` has already handed out.
    written: usize,
}

impl Timeline {
    /// Start counting now.
    #[cfg(test)]
    pub fn begin() -> Self {
        Self::begin_at(Instant::now())
    }
    /// Start counting from an earlier instant, such as the Play click.
    pub fn begin_at(start: Instant) -> Self {
        Self { start, marks: Vec::new(), written: 0 }
    }

    /// When counting started (the Play click).
    pub(crate) fn started(&self) -> Instant {
        self.start
    }

    /// Matches the game host's read-only title timestamp on the same PC.
    pub(crate) fn start_host_micros(&self) -> Option<u64> {
        let stamp = MonotonicStamp::capture(self.start)?;
        u64::try_from(stamp.ticks as u128 * 1_000_000 / stamp.frequency as u128).ok()
    }

    /// Record a milestone now.
    pub fn mark(&mut self, milestone: Milestone) {
        self.push(self.start.elapsed(), Some(milestone), milestone.log_label().to_owned());
    }
    /// Record a milestone reached at `at` on another thread (W20 finishes on its own).
    pub fn mark_at(&mut self, milestone: Milestone, at: Instant) {
        self.push(at.saturating_duration_since(self.start), Some(milestone), milestone.log_label().to_owned());
    }
    /// Record a sign-in stage now. The name comes from the sign-in library's progress text;
    /// it is flattened and capped so nothing odd can reach a log line.
    pub fn stage(&mut self, name: &str) {
        let flat: String = name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(80)
            .collect();
        self.push(self.start.elapsed(), None, format!("sign-in stage: {}", flat.trim()));
    }
    fn push(&mut self, at: Duration, milestone: Option<Milestone>, label: String) {
        self.marks.push(Mark { at, milestone, label });
    }

    /// When the milestone was first reached, counted from the start.
    pub fn elapsed_at(&self, milestone: Milestone) -> Option<Duration> {
        self.marks.iter().find(|m| m.milestone == Some(milestone)).map(|m| m.at)
    }

    /// The log lines for marks not yet handed out, one per mark:
    /// `timing +3.20s (+0.40s) tool process started`.
    pub fn unwritten_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for index in self.written..self.marks.len() {
            let previous = index.checked_sub(1).map_or(Duration::ZERO, |i| self.marks[i].at);
            let mark = &self.marks[index];
            lines.push(format!(
                "timing +{} (+{}) {}",
                secs(mark.at, 2),
                secs(mark.at.saturating_sub(previous), 2),
                mark.label
            ));
        }
        self.written = self.marks.len();
        lines
    }

    /// The history-line summary; empty when no milestone was reached.
    pub fn summary(&self) -> String {
        let terminal = self.terminal();
        let mut parts = Vec::new();
        let mut previous = Duration::ZERO;
        let mut phases: Vec<_> = PHASES.into_iter()
            .filter_map(|phase| self.elapsed_at(phase).map(|at| (phase, at)))
            .collect();
        phases.sort_by_key(|(_, at)| *at);
        for (phase, at) in phases {
            if Some(phase) == terminal {
                continue;
            }
            parts.push(format!("{} {}", phase.summary_label(), secs(at.saturating_sub(previous), 1)));
            previous = at;
        }
        if let Some(milestone) = terminal {
            if let Some(at) = self.elapsed_at(milestone) {
                parts.push(format!("{} {}", milestone.summary_label(), secs(at, 1)));
            }
        }
        parts.join(", ")
    }

    /// The account row's line, both figures from the click; empty when there is nothing to show.
    pub fn row_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(at) = self.elapsed_at(Milestone::SignInDone) {
            parts.push(tf("Signed in {time}", &[("time", &secs(at, 1))]));
        }
        if let Some(milestone) = self.terminal() {
            if let Some(at) = self.elapsed_at(milestone) {
                parts.push(milestone.row_text(at, parts.is_empty()));
            }
        } else if let Some(at) = self.elapsed_at(Milestone::SessionSent) {
            parts.push(tf("session delivered {time}", &[("time", &secs(at, 1))]));
        }
        parts.join(" · ")
    }

    fn terminal(&self) -> Option<Milestone> {
        self.elapsed_at(TERMINAL).map(|_| TERMINAL)
    }

    /// Append every unwritten mark to `path` (best effort: timing never fails a launch).
    pub fn append_to(&mut self, path: &Path) {
        let lines = self.unwritten_lines();
        crate::launch_log::append_launcher_lines_to(path, &lines, true);
    }
}

/// `2.8s`; `digits` decimal places.
pub fn secs(duration: Duration, digits: usize) -> String {
    format!("{:.*}s", digits, duration.as_secs_f64())
}

/// The history outcome with the launch's timings after it; unchanged when there are none.
pub fn noted(outcome: &str, summary: &str) -> String {
    if summary.is_empty() {
        outcome.to_owned()
    } else {
        format!("{outcome} (launch: {summary})")
    }
}

/// Where a native client's timing lines go: beside its stderr log, never inside it (the
/// client owns that file and the exit classifier reads its tail).
pub fn sibling_path(stderr_log: &Path) -> PathBuf {
    let name = stderr_log.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.strip_suffix(".stderr.log").unwrap_or_else(|| name.strip_suffix(".log").unwrap_or(&name));
    stderr_log.with_file_name(format!("{stem}.timing.log"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn missing_or_invalid_monotonic_stamp_keeps_legacy_timing() {
        assert!(MonotonicStamp::restore(None, 250).elapsed() >= Duration::from_millis(250));
        assert!(MonotonicStamp::restore(Some(MonotonicStamp { ticks: u64::MAX, frequency: 0 }), 250)
            .elapsed() >= Duration::from_millis(250));
    }

    #[test]
    #[ignore = "offline timing subprocess only"]
    fn clock_fixture() {
        use std::io::Read;
        // The message was already serialized before this delay. A duration-only
        // handoff would lose it when the child reconstructed the click.
        std::thread::sleep(Duration::from_millis(50));
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        let stamp: MonotonicStamp = serde_json::from_slice(&bytes).unwrap();
        println!("TIMING-ELAPSED {}", MonotonicStamp::restore(Some(stamp), 0).elapsed().as_millis());
    }

    #[test]
    fn monotonic_click_includes_subprocess_startup_and_request_transit() {
        use std::process::{Command, Stdio};
        let clicked = Instant::now();
        let bytes = serde_json::to_vec(&MonotonicStamp::capture(clicked).unwrap()).unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::clock_fixture"), "--ignored", "--nocapture"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let elapsed: u128 = stdout.lines().find_map(|line| line.strip_prefix("TIMING-ELAPSED "))
            .expect("child timing result").parse().unwrap();
        assert!(elapsed >= 50, "child startup/request delay was lost: {elapsed}ms");
        assert!(elapsed <= clicked.elapsed().as_millis(), "cross-process timing exceeds the observed duration");
    }

    fn timeline(marks: &[(u64, Milestone)]) -> Timeline {
        let mut t = Timeline::begin();
        for (ms, m) in marks {
            t.push(Duration::from_millis(*ms), Some(*m), m.log_label().to_owned());
        }
        t
    }

    fn pol_less() -> Timeline {
        timeline(&[
            (10, Milestone::SignInStart),
            (2800, Milestone::SignInDone),
            (3200, Milestone::ToolStarted),
            (5100, Milestone::Hello),
            (5200, Milestone::SessionSent),
            (9400, Milestone::GameStarted),
        ])
    }

    #[test]
    fn seconds_format_with_requested_digits() {
        assert_eq!(secs(Duration::from_millis(2840), 1), "2.8s");
        assert_eq!(secs(Duration::from_millis(2846), 2), "2.85s");
        assert_eq!(secs(Duration::ZERO, 1), "0.0s");
        assert_eq!(secs(Duration::from_secs(125), 1), "125.0s");
    }

    #[test]
    fn summary_gives_each_phase_then_the_total() {
        assert_eq!(
            pol_less().summary(),
            "sign-in 2.8s, tool 0.4s, hello 1.9s, session 0.1s, game started 9.4s"
        );
    }

    #[test]
    fn native_pol_summary_measures_authentication_after_the_tool_handshake() {
        let native = timeline(&[
            (220, Milestone::ToolStarted),
            (2800, Milestone::Hello),
            (2810, Milestone::SessionSent),
            (10350, Milestone::SignInDone),
            (13000, Milestone::GameStarted),
        ]);
        assert_eq!(native.summary(), "tool 0.2s, hello 2.6s, session 0.0s, sign-in 7.5s, game started 13.0s");
    }

    #[test]
    fn native_play_summary_shows_the_stock_checks_and_the_main_menu() {
        let mut play = timeline(&[
            (200, Milestone::ToolStarted),
            (2800, Milestone::Hello),
            (2810, Milestone::SessionSent),
            (4000, Milestone::ViewerChecked),
            (10000, Milestone::SignInDone),
            (11500, Milestone::MainMenu),
            (11900, Milestone::GameChecked),
            (12400, Milestone::GameStarted),
        ]);
        assert_eq!(play.summary(), "tool 0.2s, hello 2.6s, session 0.0s, W2U 1.2s, sign-in 6.0s, main menu 1.5s, W20 0.4s, game started 12.4s");
        assert_eq!(play.row_summary(), "Signed in 10.0s · game started 12.4s");
        let lines = play.unwritten_lines();
        for label in ["W2U done", "main menu reached", "W20 done"] {
            assert!(lines.iter().any(|line| line.ends_with(label)), "{label}: {lines:?}");
        }
        // A check finished on its own thread keeps its own instant.
        let mut t = Timeline::begin_at(Instant::now() - Duration::from_secs(2));
        t.mark_at(Milestone::GameChecked, t.start + Duration::from_millis(1500));
        assert_eq!(t.elapsed_at(Milestone::GameChecked), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn summary_stops_where_the_launch_stopped() {
        let unconfirmed = timeline(&[(2800, Milestone::SignInDone), (3200, Milestone::ToolStarted), (5100, Milestone::Hello)]);
        assert_eq!(unconfirmed.summary(), "sign-in 2.8s, tool 0.4s, hello 1.9s");
        assert_eq!(Timeline::begin().summary(), "");
        assert_eq!(Timeline::begin().row_summary(), "");
    }

    #[test]
    fn row_summary_uses_totals_from_the_click() {
        assert_eq!(pol_less().row_summary(), "Signed in 2.8s · game started 9.4s");
        let unconfirmed = timeline(&[(2800, Milestone::SignInDone), (8000, Milestone::SessionSent)]);
        assert_eq!(unconfirmed.row_summary(), "Signed in 2.8s · session delivered 8.0s");
    }

    #[test]
    fn log_lines_show_total_and_step_and_are_handed_out_once() {
        let mut t = timeline(&[(2800, Milestone::SignInDone), (3200, Milestone::ToolStarted)]);
        t.push(Duration::from_millis(3300), None, "sign-in stage: Logging in".into());
        assert_eq!(
            t.unwritten_lines(),
            vec![
                "timing +2.80s (+2.80s) sign-in done",
                "timing +3.20s (+0.40s) tool process started",
                "timing +3.30s (+0.10s) sign-in stage: Logging in",
            ]
        );
        assert!(t.unwritten_lines().is_empty());
        t.push(Duration::from_millis(5100), Some(Milestone::Hello), "loader hello".into());
        assert_eq!(t.unwritten_lines(), vec!["timing +5.10s (+1.80s) loader hello"]);
    }

    #[test]
    fn stage_names_are_flattened_and_capped() {
        let mut t = Timeline::begin();
        t.stage("Line one\r\nline two");
        t.stage(&"x".repeat(500));
        let lines = t.unwritten_lines();
        assert!(lines[0].ends_with("sign-in stage: Line one  line two"));
        assert!(!lines[0].contains('\n') && !lines[0].contains('\r'));
        assert!(lines[1].len() < 130);
        assert_eq!(t.summary(), "");
    }

    #[test]
    fn marks_from_a_real_clock_never_go_backwards() {
        let mut t = Timeline::begin();
        t.mark(Milestone::SignInStart);
        t.mark(Milestone::SignInDone);
        assert!(t.elapsed_at(Milestone::SignInDone) >= t.elapsed_at(Milestone::SignInStart));
        assert_eq!(t.unwritten_lines().len(), 2);
    }

    #[test]
    fn history_note_is_appended_only_when_there_is_one() {
        assert_eq!(noted("Game closed normally", ""), "Game closed normally");
        assert_eq!(
            noted("Game closed normally", "sign-in 2.8s, game started 9.4s"),
            "Game closed normally (launch: sign-in 2.8s, game started 9.4s)"
        );
    }

    #[test]
    fn native_timing_file_sits_beside_the_stderr_log() {
        assert_eq!(
            sibling_path(Path::new(r"C:\d\logs\abc.stderr.log")),
            PathBuf::from(r"C:\d\logs\abc.timing.log")
        );
        assert_eq!(sibling_path(Path::new(r"C:\d\logs\abc.log")), PathBuf::from(r"C:\d\logs\abc.timing.log"));
    }

    #[test]
    fn append_writes_each_mark_once() {
        let dir = std::env::temp_dir().join(format!("xi-launch-timing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("a.timing.log");
        let mut t = timeline(&[(2800, Milestone::SignInDone)]);
        t.append_to(&path);
        t.append_to(&path);
        t.push(Duration::from_millis(3100), Some(Milestone::ToolStarted), "tool process started".into());
        t.append_to(&path);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().unwrap().contains("[launcher] timing +2.80s (+2.80s) sign-in done"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// pol-spec 07d §4.5: the excesses measured on 2026-10-12 (08 §1 row 1).
    #[test]
    fn possible_loss_matches_tcp_timer_quanta_only() {
        let s = Duration::from_secs_f64;
        assert_eq!(possible_loss("status", s(5.25), s(1.15)).as_deref(),
            Some("possible packet loss: status took 5.25 s, +4.10 s over its clean 1.15 s (4.1-4.55 s server retransmission)"));
        assert!(possible_loss("member connect", s(4.40), s(1.04)).unwrap().ends_with("(3.36 s SYN-ACK retransmission)"));
        assert!(possible_loss("W2U connect", s(1.18), s(0.18)).unwrap().ends_with("(1.0 s SYN/DNS retry)"));
        // 18:59 characters +12.1 s = 4.05 + 8.1: two quanta (the pair named is the first that fits).
        assert!(possible_loss("characters", s(13.25), s(1.1)).unwrap().contains(" + 8.2-9.1 s doubled server retransmission)"));
        for (actual, clean) in [(1.30, 1.15), (2.6, 1.15), (2.9, 1.15), (7.0, 1.15)] {
            assert_eq!(possible_loss("lists", s(actual), s(clean)), None, "{actual}");
        }
    }

    /// 07d §2.4, §4.5: the phases line (whatever `phases()` words) and then one loss note per
    /// overshooting step; attempts are numbered only when there were several.
    #[test]
    fn check_lines_give_the_phases_then_loss_notes_per_step() {
        use crate::patch::{Attempt, CheckTiming};
        let ms = Duration::from_millis;
        let clean = Attempt { resolve: Some(ms(3)), connect: Some(ms(180)), reply: Some(ms(164)), close: Some(ms(104)), ..Default::default() };
        let lost = Attempt { connect: Some(ms(1180)), ..clean.clone() };
        let one = CheckTiming { attempts: vec![lost.clone()], ..Default::default() };
        let phases = |timing: &CheckTiming, check: &str| timing.phases().map(|p| format!("{check}: {p}.")).into_iter().collect::<Vec<_>>();
        let mut expected = phases(&one, "Viewer check (W2U)");
        expected.push("possible packet loss: W2U connect took 1.18 s, +1.00 s over its clean 0.18 s (1.0 s SYN/DNS retry)".into());
        assert_eq!(check_lines("Viewer check (W2U)", &one), expected);
        let two = CheckTiming { attempts: vec![clean.clone(), Attempt { reply: Some(ms(4440)), ..clean.clone() }], ..Default::default() };
        let lines = check_lines("FFXI check (W20)", &two);
        assert_eq!(lines.last().unwrap(), "possible packet loss: W20 attempt 2 reply took 4.44 s, +4.27 s over its clean 0.17 s (4.1-4.55 s server retransmission)");
        assert_eq!(lines.len(), phases(&two, "FFXI check (W20)").len() + 1);
        assert_eq!(check_lines("FFXI check (W20)", &CheckTiming { attempts: vec![clean], ..Default::default() }).iter()
            .filter(|line| line.starts_with("possible packet loss")).count(), 0);
    }

    #[test]
    fn launch_triggers_have_fixed_names_and_default_to_play() {
        assert_eq!(LaunchTrigger::default(), LaunchTrigger::Play);
        assert_eq!(LaunchTrigger::ParkedPlay.line(), "launch trigger: parked Play");
        assert_eq!(serde_json::to_string(&LaunchTrigger::StartupSignIn).unwrap(), "\"startup_sign_in\"");
    }
}
