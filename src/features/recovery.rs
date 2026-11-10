//! Crash and disconnect recovery: how an ended game ended, and whether Crystal offers a Relaunch
//! or relaunches by itself.
//!
//! A relaunch is an ordinary Play of that account (the interface calls its Play path), so its
//! traffic is exactly a Play's. Automatic relaunch is off by default; when on, it backs off, stops
//! after a few attempts in a window, never repeats a game that keeps failing during startup, and
//! never starts during maintenance or an outage (it becomes an offer instead).

use crate::launch_log::{ExitKind, ExitReport};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Offer Relaunch after a crash or disconnect.
    pub offer: bool,
    /// Relaunch by itself (with backoff and a limit).
    pub auto_relaunch: bool,
    /// Automatic attempts allowed per account within `window_minutes`.
    pub max_attempts: u8,
    pub window_minutes: u16,
}

impl Default for Settings {
    fn default() -> Self { Self { offer: true, auto_relaunch: false, max_attempts: 3, window_minutes: 30 } }
}

/// How a game ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EndKind {
    /// The user quit the game, or asked Crystal to close it or sign out.
    UserClosed,
    /// The game process faulted: an exception exit status, a Windows Error Reporting record, or
    /// a host that vanished without reporting an exit.
    Crash,
    /// The game ended itself with an error code (FFXI returns from GameStart with one when the
    /// connection is lost).
    Disconnect,
    /// Something on Crystal's side ended the session (loader error, worker gone): offered, never
    /// repeated automatically.
    Unknown,
}

/// An exit status Windows uses for an unhandled exception (an NTSTATUS error: 0xC0000000+).
pub(crate) fn is_exception_status(code: i32) -> bool { (code as u32) >= 0xC000_0000 }

/// Classify an ended game. `user_closed`: Crystal's Shutdown or Sign out was used for it.
/// `wer`: Windows Error Reporting recorded a crash of the host since the game started.
pub(crate) fn classify(report: Option<&ExitReport>, user_closed: bool, wer: bool) -> EndKind {
    if user_closed { return EndKind::UserClosed; }
    let Some(report) = report else { return if wer { EndKind::Crash } else { EndKind::Unknown } };
    let exception = report.exit_status.is_some_and(is_exception_status)
        || matches!(report.kind, ExitKind::Code { code } if is_exception_status(code));
    if exception || wer { return EndKind::Crash; }
    match &report.kind {
        ExitKind::Normal => EndKind::UserClosed,
        ExitKind::Code { .. } => EndKind::Disconnect,
        // The host ended without an exit line (or with a plain failure status): it died.
        ExitKind::Other | ExitKind::Startup => EndKind::Crash,
        ExitKind::LoaderError { .. } | ExitKind::WorkerEnded => EndKind::Unknown,
    }
}

/// Whether Windows Error Reporting holds a crash report for `image` (e.g. `pol.exe`) created at
/// or after `since`. Reads two folder listings once, at a game's end; nothing is opened.
pub(crate) fn wer_report_since(image: &str, since: SystemTime) -> bool {
    let root = std::env::var_os("ProgramData").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\ProgramData".into());
    let prefix = format!("appcrash_{}_", image.to_ascii_lowercase());
    ["ReportQueue", "ReportArchive"].iter().any(|folder| {
        std::fs::read_dir(root.join(r"Microsoft\Windows\WER").join(folder)).into_iter().flatten().flatten().any(|entry| {
            entry.file_name().to_string_lossy().to_ascii_lowercase().starts_with(&prefix)
                && entry.metadata().and_then(|m| m.created().or_else(|_| m.modified())).is_ok_and(|at| at >= since)
        })
    })
}

/// Why an automatic relaunch did not happen (the interface offers Relaunch instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// Automatic relaunch is off.
    Off,
    /// FFXI is down for maintenance or an outage, or an Update is running.
    Maintenance,
    /// `max_attempts` automatic relaunches already happened in the window.
    Limit,
    /// The game failed during startup: repeating it would loop.
    Startup,
    /// Crystal-side failures are not repeated by themselves.
    NotAutomatic,
}

/// What to do about an ended game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Nothing (the user closed it, or offers are off).
    None,
    /// Offer a Relaunch button.
    Offer { kind: EndKind, held: Held },
    /// Relaunch at `at` (attempt `attempt` of the window), unless cancelled.
    Relaunch { kind: EndKind, at: Instant, attempt: u8 },
}

/// Waits before automatic attempts 1, 2, 3...: long enough for the server to drop the previous
/// session (an immediate sign-in can be refused as already connected).
const BACKOFF: [Duration; 3] = [Duration::from_secs(20), Duration::from_secs(60), Duration::from_secs(180)];
/// A game that ran this long before ending clears its account's attempt history.
const STABLE: Duration = Duration::from_secs(10 * 60);

/// Per-account recovery state. Never saved: a Crystal restart starts fresh.
#[derive(Debug, Default)]
pub(crate) struct Policy {
    /// When each automatic relaunch started, per account.
    attempts: BTreeMap<String, Vec<Instant>>,
    /// Scheduled relaunches.
    pending: BTreeMap<String, (Instant, u8)>,
}

impl Policy {
    /// A game of `account` ended after running for `ran`. `maintenance`: FFXI is down or an
    /// update runs now. Replaces any relaunch already pending for the account.
    pub(crate) fn on_end(&mut self, account: &str, kind: EndKind, during_startup: bool, ran: Duration, now: Instant,
        maintenance: bool, settings: &Settings) -> Decision {
        self.pending.remove(account);
        if kind == EndKind::UserClosed { return Decision::None; }
        let window = Duration::from_secs(u64::from(settings.window_minutes.max(1)) * 60);
        let attempts = self.attempts.entry(account.to_owned()).or_default();
        if ran >= STABLE { attempts.clear(); }
        attempts.retain(|at| now.saturating_duration_since(*at) < window);
        let held = if !settings.auto_relaunch { Some(Held::Off) }
            else if !matches!(kind, EndKind::Crash | EndKind::Disconnect) { Some(Held::NotAutomatic) }
            else if during_startup { Some(Held::Startup) }
            else if attempts.len() >= usize::from(settings.max_attempts) { Some(Held::Limit) }
            else if maintenance { Some(Held::Maintenance) }
            else { None };
        match held {
            Some(held) if settings.offer || held != Held::Off => Decision::Offer { kind, held },
            Some(_) => Decision::None,
            None => {
                let attempt = attempts.len() as u8 + 1;
                let at = now + BACKOFF[attempts.len().min(BACKOFF.len() - 1)];
                self.pending.insert(account.to_owned(), (at, attempt));
                Decision::Relaunch { kind, at, attempt }
            }
        }
    }

    /// The relaunches due at `now`, removed from the schedule. During maintenance they are
    /// returned as held (offer instead) and nothing is counted.
    pub(crate) fn due(&mut self, now: Instant, maintenance: bool) -> Vec<(String, Result<u8, Held>)> {
        let due: Vec<String> = self.pending.iter().filter(|(_, (at, _))| *at <= now).map(|(k, _)| k.clone()).collect();
        due.into_iter().filter_map(|key| {
            let (_, attempt) = self.pending.remove(&key)?;
            if maintenance { return Some((key, Err(Held::Maintenance))); }
            self.attempts.entry(key.clone()).or_default().push(now);
            Some((key, Ok(attempt)))
        }).collect()
    }

    /// The next scheduled relaunch, for a one-shot wake.
    pub(crate) fn next_due(&self) -> Option<Instant> { self.pending.values().map(|(at, _)| *at).min() }

    pub(crate) fn pending(&self, account: &str) -> Option<(Instant, u8)> { self.pending.get(account).copied() }

    /// The user cancelled, played, or the account went away.
    pub(crate) fn cancel(&mut self, account: &str) -> bool { self.pending.remove(account).is_some() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(kind: ExitKind, status: Option<i32>) -> ExitReport {
        ExitReport { kind, exit_status: status, during_startup: false, summary: String::new() }
    }

    #[test]
    fn ends_are_classified_by_exit_status_kind_and_wer() {
        assert_eq!(classify(Some(&report(ExitKind::Normal, Some(0))), false, false), EndKind::UserClosed);
        assert_eq!(classify(Some(&report(ExitKind::Other, None)), true, true), EndKind::UserClosed, "Crystal's own shutdown wins");
        // 0xC0000005 access violation as the host's exit status, or as GameStart's code.
        let av = 0xC000_0005u32 as i32;
        assert_eq!(classify(Some(&report(ExitKind::Other, Some(av))), false, false), EndKind::Crash);
        assert_eq!(classify(Some(&report(ExitKind::Code { code: av }, Some(av))), false, false), EndKind::Crash);
        assert_eq!(classify(Some(&report(ExitKind::Code { code: 3 }, Some(3))), false, false), EndKind::Disconnect);
        assert_eq!(classify(Some(&report(ExitKind::Code { code: 3 }, Some(3))), false, true), EndKind::Crash, "a WER record is a crash");
        assert_eq!(classify(Some(&report(ExitKind::Other, None)), false, false), EndKind::Crash);
        assert_eq!(classify(Some(&report(ExitKind::Startup, None)), false, false), EndKind::Crash);
        assert_eq!(classify(Some(&report(ExitKind::LoaderError { message: "x".into() }, None)), false, false), EndKind::Unknown);
        assert_eq!(classify(Some(&ExitReport::worker_ended()), false, false), EndKind::Unknown);
        assert_eq!(classify(None, false, false), EndKind::Unknown);
        assert!(!is_exception_status(1) && !is_exception_status(0) && is_exception_status(-1073741819));
    }

    #[test]
    fn offers_by_default_and_never_relaunches_by_itself_unless_enabled() {
        let mut policy = Policy::default();
        let now = Instant::now();
        let settings = Settings::default();
        assert_eq!(policy.on_end("a", EndKind::Crash, false, Duration::from_secs(60), now, false, &settings),
            Decision::Offer { kind: EndKind::Crash, held: Held::Off });
        assert_eq!(policy.on_end("a", EndKind::UserClosed, false, Duration::ZERO, now, false, &settings), Decision::None);
        let quiet = Settings { offer: false, ..Settings::default() };
        assert_eq!(policy.on_end("a", EndKind::Crash, false, Duration::ZERO, now, false, &quiet), Decision::None);
        assert_eq!(policy.next_due(), None);
    }

    #[test]
    fn automatic_relaunch_backs_off_and_stops_at_the_limit() {
        let mut policy = Policy::default();
        let settings = Settings { auto_relaunch: true, ..Settings::default() };
        let mut now = Instant::now();
        let mut waits = Vec::new();
        for attempt in 1..=3u8 {
            let Decision::Relaunch { at, attempt: n, .. } = policy.on_end("a", EndKind::Disconnect, false, Duration::from_secs(90), now, false, &settings) else { panic!("attempt {attempt}") };
            assert_eq!(n, attempt);
            waits.push(at - now);
            assert_eq!(policy.next_due(), Some(at));
            assert!(policy.due(at - Duration::from_millis(1), false).is_empty());
            assert_eq!(policy.due(at, false), vec![("a".to_owned(), Ok(attempt))]);
            now = at + Duration::from_secs(5);
        }
        assert_eq!(waits, BACKOFF.to_vec());
        // The fourth failure inside the window only offers.
        assert_eq!(policy.on_end("a", EndKind::Crash, false, Duration::from_secs(90), now, false, &settings),
            Decision::Offer { kind: EndKind::Crash, held: Held::Limit });
        // A game that then ran stably clears the history.
        assert!(matches!(policy.on_end("a", EndKind::Crash, false, STABLE, now, false, &settings), Decision::Relaunch { attempt: 1, .. }));
        // Other accounts have their own history.
        assert!(matches!(policy.on_end("b", EndKind::Crash, false, Duration::ZERO, now, false, &settings), Decision::Relaunch { attempt: 1, .. }));
    }

    #[test]
    fn no_automatic_relaunch_in_maintenance_after_startup_failures_or_crystal_errors() {
        let mut policy = Policy::default();
        let settings = Settings { auto_relaunch: true, ..Settings::default() };
        let now = Instant::now();
        assert_eq!(policy.on_end("a", EndKind::Crash, false, Duration::ZERO, now, true, &settings), Decision::Offer { kind: EndKind::Crash, held: Held::Maintenance });
        assert_eq!(policy.on_end("a", EndKind::Crash, true, Duration::ZERO, now, false, &settings), Decision::Offer { kind: EndKind::Crash, held: Held::Startup });
        assert_eq!(policy.on_end("a", EndKind::Unknown, false, Duration::ZERO, now, false, &settings), Decision::Offer { kind: EndKind::Unknown, held: Held::NotAutomatic });
        // Maintenance that began while a relaunch waited holds it and counts nothing.
        let Decision::Relaunch { at, .. } = policy.on_end("a", EndKind::Crash, false, Duration::ZERO, now, false, &settings) else { panic!() };
        assert_eq!(policy.due(at, true), vec![("a".to_owned(), Err(Held::Maintenance))]);
        assert!(matches!(policy.on_end("a", EndKind::Crash, false, Duration::ZERO, at, false, &settings), Decision::Relaunch { attempt: 1, .. }));
        // A user's own action cancels the pending relaunch.
        assert!(policy.cancel("a") && !policy.cancel("a") && policy.next_due().is_none());
    }

    #[test]
    fn wer_lookup_reads_nothing_it_cannot_and_old_files_load() {
        // A process name with no reports, and a time in the future: never a match.
        assert!(!wer_report_since("crystal-feature-test-no-such.exe", SystemTime::now()));
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert!(s.offer && !s.auto_relaunch);
    }
}
