//! Play is the update, you stay signed in (proposal 09 phase 2, owner-approved 2026-10-13,
//! reconfirmed 2026-10-22).
//!
//! When a Play's W20 says FINAL FANTASY XI needs an update, the account stays signed in at
//! the PlayOnline main menu while its game folder updates, then the game starts through the
//! signed-in start path with no second version check. Square Enix sees what stock POL sends
//! in one signed-in session (capture stock-update-20261013-004932): the W20 → its FIN →
//! ~110 ms → an idle "update?" prompt connection (`updater::PROMPT_HOLD`) → ~105 ms → the file
//! list → one stop-and-wait download connection → no version check → GameStart.
//!
//! One job per game folder (`Job`); other accounts that share it wait at the main menu and
//! start after it. A game running from the folder never closes: the download goes ahead and
//! the install waits for it (`wait_until_free`). A signed-in host that holds a game file is
//! signed out instead (that one account then signs in again after the install).
use crate::i18n::{t, tf};
use crate::operation::{Operation, Stage};
use crate::session_worker::BackgroundSession;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the install looks again for what still uses the folder.
const FREE_POLL: Duration = Duration::from_secs(1);
/// Consecutive failed process scans (a process exiting mid-scan) before the install gives up.
const SCAN_FAILURES: u32 = 30;

/// An account a job knows about: its host and session, so a game that uses the folder can be
/// told from a signed-in host that only holds a file.
#[derive(Clone)]
pub(crate) struct Member {
    pub key: String,
    pub label: String,
    /// The account's game host (loader) PID.
    pub host: Option<u32>,
    pub session: Option<BackgroundSession>,
    /// FFXI was running when the window last looked (the session's own record wins).
    pub playing: bool,
}
impl Member {
    fn playing(&self) -> bool {
        self.session.as_ref().and_then(BackgroundSession::game_running).unwrap_or(self.playing)
    }
}

/// What a job's thread reports to the window (`WorkerEvent::PlayUpdate`).
#[derive(Clone, Debug)]
pub(crate) enum Event {
    /// Downloaded; the install waits for FINAL FANTASY XI to close on this account (`None`:
    /// a program Crystal does not know uses the folder).
    Waiting(Option<String>),
    /// Nothing uses the folder any more: installing.
    Installing,
    /// This account's signed-in host held a game file, so it was signed out: it signs in again
    /// after the install.
    SignedOut(String),
    Finished(Result<Done, String>),
}

#[derive(Clone, Debug)]
pub(crate) struct Done {
    pub summary: String,
    /// The folder's report after the update (the window's FFXI line).
    pub report: Option<crate::patch::PatchReport>,
}

/// An account whose Play waits for this job.
#[derive(Clone, Debug)]
pub(crate) struct Waiter {
    pub key: String,
    pub clicked: Instant,
    /// Its host was signed out (`Event::SignedOut`): it starts with a new sign-in.
    pub fallback: bool,
}

/// One game folder's update.
pub(crate) struct Job {
    pub generation: u64,
    pub folder: PathBuf,
    pub op: Operation,
    /// The account whose Play started it.
    pub owner: String,
    pub official: String,
    /// The owner first, then every account whose W20 asked for the same update.
    pub waiters: Vec<Waiter>,
    /// Every signed-in account of this folder, for the install's wait (shared with the thread).
    pub members: Arc<Mutex<Vec<Member>>>,
    /// Downloaded, and waiting for a game to close (on that account, when known).
    pub blocked: Option<Option<String>>,
    pub installing: bool,
    /// Accounts outside the waiters whose host was signed out for the install.
    pub signed_out: Vec<String>,
    pub cancelled: bool,
    /// Started by maintenance's version check (patch_watch), not a Play: no waiters, and no
    /// game starts after it.
    pub probe: bool,
}
impl Job {
    pub(crate) fn has(&self, key: &str) -> bool { self.waiters.iter().any(|waiter| waiter.key == key) }
}

/// The window's update jobs and what they hand the start path.
#[derive(Default)]
pub(crate) struct Jobs {
    pub jobs: Vec<Job>,
    next: u64,
    /// Work started outside `update` (the PlayOnline update a Play asked for), returned by the
    /// next Tick.
    pub tasks: Vec<crate::effect::Effect<crate::Message>>,
    /// A Play whose PlayOnline update must first sign out this many idle accounts: the row asks.
    pub viewer_confirm: Option<(String, usize)>,
    /// Tests: jobs that would have started (folder, official, W20 FIN).
    #[cfg(test)]
    pub spawned: Vec<(PathBuf, String, Option<Instant>)>,
}

impl Jobs {
    pub(crate) fn for_folder(&self, folder: &Path) -> Option<&Job> { self.jobs.iter().find(|job| same_folder(&job.folder, folder)) }
    pub(crate) fn for_folder_mut(&mut self, folder: &Path) -> Option<&mut Job> { self.jobs.iter_mut().find(|job| same_folder(&job.folder, folder)) }
    pub(crate) fn for_key(&self, key: &str) -> Option<&Job> { self.jobs.iter().find(|job| job.has(key)) }
    pub(crate) fn take(&mut self, generation: u64) -> Option<Job> {
        let at = self.jobs.iter().position(|job| job.generation == generation)?;
        Some(self.jobs.remove(at))
    }
    pub(crate) fn get_mut(&mut self, generation: u64) -> Option<&mut Job> { self.jobs.iter_mut().find(|job| job.generation == generation) }

    /// A new job for `folder`, its owner the first waiter (none for a `probe`). Not started (`spawn`).
    pub(crate) fn begin(&mut self, folder: PathBuf, owner: &str, official: String, clicked: Instant, members: Vec<Member>, probe: bool) -> &Job {
        self.next += 1;
        let op = Operation::new();
        op.begin_update();
        op.progress(Stage::Manifest, 0, 1, t("Starting the update"));
        let waiters = if probe { Vec::new() } else { vec![Waiter { key: owner.to_owned(), clicked, fallback: false }] };
        self.jobs.push(Job { generation: self.next, folder, op, owner: owner.to_owned(), official, waiters,
            members: Arc::new(Mutex::new(members)), blocked: None, installing: false, signed_out: Vec::new(), cancelled: false, probe });
        self.jobs.last().unwrap()
    }
}

/// The same game folder, as Windows compares paths.
pub(crate) fn same_folder(a: &Path, b: &Path) -> bool {
    let plain = |path: &Path| crate::backend::plain_windows_path(path).to_string_lossy().trim_end_matches(['\\', '/']).to_ascii_lowercase();
    plain(a) == plain(b)
}

/// What the install does about the processes that use the folder (`users`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Decision {
    /// Signed-in hosts at the main menu that hold a game file: signed out (the fallback).
    pub sign_out: Vec<String>,
    /// A game still runs from the folder: the install waits (on that account when known).
    pub waiting: Option<Option<String>>,
}

pub(crate) fn decide(users: &[u32], members: &[Member]) -> Decision {
    let mut decision = Decision::default();
    for pid in users {
        match members.iter().find(|member| member.host == Some(*pid)) {
            Some(member) if member.playing() => {
                // A named game wins over an unknown program.
                if !matches!(decision.waiting, Some(Some(_))) { decision.waiting = Some(Some(member.label.clone())); }
            }
            Some(member) if member.session.is_some() => {
                if !decision.sign_out.contains(&member.key) { decision.sign_out.push(member.key.clone()); }
            }
            _ => { decision.waiting.get_or_insert(None); }
        }
    }
    decision
}

/// How long a file stays held by a process the scan does not name before the idle signed-in
/// hosts of the folder are signed out (they may hold it; a brief scan by another program ends
/// sooner).
const HELD_SIGN_OUT: u32 = 3;

/// Until nothing uses `root` (`users`, a module or image there) and no file the update replaces
/// is held open (`held`): sign out signed-in hosts that hold a file (`sign_out`), and wait,
/// without closing anything, for games that run from it. Reports each change.
pub(crate) async fn wait_until_free<U, H, S, F>(root: &Path, op: &Operation, members: &Mutex<Vec<Member>>,
    users: U, held: H, sign_out: S, send: &mut dyn FnMut(Event)) -> Result<(), String>
where U: Fn(&Path) -> Result<Vec<u32>, String>, H: Fn() -> bool, S: Fn(BackgroundSession) -> F,
    F: std::future::Future<Output = Result<(), String>> {
    let mut shown: Option<Option<String>> = None;
    let mut signed_out = BTreeSet::new();
    let mut failures = 0;
    let mut held_for = 0;
    loop {
        op.check_cancelled()?;
        let found = match users(root) {
            Ok(found) => { failures = 0; found }
            Err(error) => {
                failures += 1;
                if failures >= SCAN_FAILURES { return Err(error); }
                pause(op).await?;
                continue;
            }
        };
        let open = found.is_empty() && held();
        if found.is_empty() && !open { break; }
        let known = members.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut decision = decide(&found, &known);
        // A file held open with no process named: after a moment, the folder's idle signed-in
        // hosts (they may hold it) sign out; whatever else holds it, the install waits for.
        held_for = if open { held_for + 1 } else { 0 };
        if open {
            if held_for >= HELD_SIGN_OUT {
                decision.sign_out = known.iter().filter(|member| member.session.is_some() && !member.playing())
                    .map(|member| member.key.clone()).collect();
            }
            decision.waiting.get_or_insert(None);
        }
        for key in decision.sign_out {
            if !signed_out.insert(key.clone()) { continue; }
            let Some(session) = known.iter().find(|member| member.key == key).and_then(|member| member.session.clone()) else { continue };
            op.progress(Stage::SignOut, 0, 1, t("Signing out to install the update"));
            sign_out(session).await?;
            send(Event::SignedOut(key));
        }
        if let Some(waiting) = decision.waiting {
            if shown.as_ref() != Some(&waiting) {
                op.progress(Stage::Waiting, 0, 0, t("Waiting for FINAL FANTASY XI to close"));
                send(Event::Waiting(waiting.clone()));
                shown = Some(waiting);
            }
        }
        pause(op).await?;
    }
    if shown.is_some() || !signed_out.is_empty() { send(Event::Installing); }
    Ok(())
}

async fn pause(op: &Operation) -> Result<(), String> {
    tokio::select! {
        _ = tokio::time::sleep(FREE_POLL) => op.check_cancelled(),
        cancelled = op.cancelled_wait() => Err(cancelled),
    }
}

/// The job's thread: download (the prompt connection, the list, the downloads), wait for the
/// folder to be free, install. `closed`: the FIN of the W20 that asked, when another process
/// ran it (the window's own W20s note theirs in `patch::check_before_launch_timed`).
#[cfg_attr(test, allow(dead_code))] // tests record jobs instead of starting them
pub(crate) fn spawn(generation: u64, folder: PathBuf, official: String, closed: Option<Instant>, op: Operation,
    members: Arc<Mutex<Vec<Member>>>, sender: crate::tick_gate::EventSender) -> Result<(), String> {
    std::thread::Builder::new().name("play-update".into()).spawn(move || {
        let send = |event: Event| { let _ = sender.send(crate::WorkerEvent::PlayUpdate(generation, event)); };
        let mut report = send;
        let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime.block_on(run(folder, official, closed, &op, &members, &mut report)),
            Err(error) => Err(error.to_string()),
        };
        send(Event::Finished(result));
    }).map(|_| ()).map_err(|_| t("Could not start the update.").to_owned())
}

#[cfg_attr(test, allow(dead_code))]
async fn run(folder: PathBuf, official: String, closed: Option<Instant>, op: &Operation, members: &Mutex<Vec<Member>>,
    send: &mut dyn FnMut(Event)) -> Result<Done, String> {
    if let Some(at) = closed { crate::updater::note_patch_closed(crate::updater::Product::Ffxi, at); }
    let staged = crate::updater::stage_after_play(folder.clone(), official, op).await?;
    let root = staged.root().to_path_buf();
    let sign_out_op = op.clone();
    wait_until_free(&root, op, members, crate::updater::folder_users, || !staged.held_files().is_empty(), |session| {
        let op = sign_out_op.clone();
        async move { crate::session_worker::sign_out_for_update(vec![session], &op).await }
    }, send).await?;
    // USER data first, once the folder is free and before any file changes (features::backups).
    let install = folder.clone();
    let _ = tokio::task::spawn_blocking(move || crate::features::runtime::backup_before_maintenance_logged(
        &crate::backend::Config::default(), &install, crate::features::backups::Maintenance::Update)).await;
    let report = crate::updater::install_staged(staged, op).await?;
    let summary = report.summary.clone();
    let shown = tokio::task::spawn_blocking(move || crate::patch::report_after_operation(&folder, &report, crate::updater::Mode::Update)).await.ok();
    Ok(Done { summary, report: shown })
}

pub(crate) fn remaining_time(duration: std::time::Duration) -> String {
    // Round up without overflowing even Duration::MAX or showing zero for a fraction of a second.
    let seconds = duration.as_secs() as u128 + u128::from(duration.subsec_nanos() != 0);
    if seconds < 60 {
        return tf("{seconds} sec", &[("seconds", &seconds)]);
    }
    let minutes = seconds.div_ceil(60);
    if minutes < 60 {
        return tf("{minutes} min", &[("minutes", &minutes)]);
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if minutes == 0 {
        tf("{hours} hr", &[("hours", &hours)])
    } else {
        tf("{hours} hr {minutes} min", &[("hours", &hours), ("minutes", &minutes)])
    }
}
/// Percent done of the running file operation: the whole update's estimate when there is
/// one, else the current step's.
pub(crate) fn maintenance_percent(progress: &crate::operation::Progress) -> Option<u8> {
    match &progress.overall {
        Some(overall) => overall.remaining_percent().map(|left| 100 - left),
        None => progress.remaining_percent().map(|left| 100 - left),
    }
}
/// What a row (and the Play button) says about the update its Play runs in its session.
pub(crate) struct Line {
    pub headline: String,
    /// The job's generation, for its Cancel link.
    pub cancel: Option<u64>,
    /// Short, for the Play button: "Updating FINAL FANTASY XI · 43%".
    pub label: String,
    /// The update's progress, for the account row's bar.
    pub fraction: Option<f32>,
}

/// "Updating FINAL FANTASY XI · 43% · 40 sec left", "Update ready · waiting for FINAL FANTASY XI
/// to close on Alice", or, for a Play that waits for another account's update of its folder,
/// "Waiting for the update · 43%".
pub(crate) fn line(app: &crate::App, key: &str) -> Option<Line> {
    let (job, waiter) = app.play_update_of(key)?;
    let progress = job.op.snapshot();
    let percent = maintenance_percent(&progress);
    let label = match percent {
        Some(percent) => tf("{label} · {percent}%", &[("label", &t("Updating FINAL FANTASY XI")), ("percent", &percent)]),
        None => t("Updating FINAL FANTASY XI").to_owned(),
    };
    if !waiter {
        let headline = match percent {
            Some(percent) => tf("Waiting for the update · {percent}%", &[("percent", &percent)]),
            None => t("Waiting for the update…").to_owned(),
        };
        return Some(Line { headline, cancel: None, label, fraction: percent.map(|p| p as f32 / 100.0) });
    }
    let headline = match &job.blocked {
        Some(Some(name)) => tf("Update ready · waiting for FINAL FANTASY XI to close on {name}", &[("name", name)]),
        Some(None) => t("Update ready · waiting for FINAL FANTASY XI to close").to_owned(),
        None if job.cancelled => t("Pausing the update…").to_owned(),
        None => {
            let mut parts = vec![label.clone()];
            parts.extend(time_left(&progress));
            parts.join(" · ")
        }
    };

    let label = if job.blocked.is_some() { t("Update ready").to_owned() } else { label };
    let fraction = job.blocked.is_none().then(|| percent.map(|p| p as f32 / 100.0)).flatten();
    Some(Line { headline, cancel: (!job.cancelled).then_some(job.generation), label, fraction })
}
/// What maintenance's update (patch_watch) says for `key`'s game folder: its version check,
/// its download with progress (and Cancel), or that it installed while the worlds stay closed.
pub(crate) fn watch_line(app: &crate::App, key: &str) -> Option<Line> {
    // Nothing tracked (most of a maintenance): no folder lookup per row per frame.
    if !(app.patch_watch.active() && app.patch_watch.tracking()) && !app.play_updates.jobs.iter().any(|job| job.probe) { return None; }
    let folder = app.play_folder(app.accounts.iter().find(|row| row.key == key)?)?;
    if let Some(job) = app.play_updates.for_folder(&folder).filter(|job| job.probe) {
        let percent = maintenance_percent(&job.op.snapshot());
        let label = match (&job.blocked, percent) {
            (Some(_), _) => t("Update ready · waiting for FINAL FANTASY XI to close").to_owned(),
            (None, Some(percent)) => tf("Downloading the FFXI update · {percent}%", &[("percent", &percent)]),
            (None, None) => t("Downloading the FFXI update").to_owned(),
        };
        let fraction = job.blocked.is_none().then(|| percent.map(|p| p as f32 / 100.0)).flatten();
        return Some(Line { headline: label.clone(), cancel: (!job.cancelled).then_some(job.generation), label, fraction });
    }
    let label = if app.patch_watch.checking(&folder) { t("Checking for the FFXI update") }
        else if app.patch_watch.installed(&folder) { t("Update installed · waiting for servers") }
        else { return None };
    Some(Line { headline: label.to_owned(), cancel: None, label: label.to_owned(), fraction: None })
}

/// "3 min left" for the whole update when it has a total, else this step; none at zero.
fn time_left(progress: &crate::operation::Progress) -> Option<String> {
    progress.overall.map_or(progress.remaining, |overall| overall.remaining)
        .filter(|left| !left.is_zero())
        .map(|left| tf("{time} left", &[("time", &remaining_time(left))]))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn member(key: &str, host: u32, playing: bool) -> Member {
        Member { key: key.into(), label: key.to_uppercase(), host: Some(host), session: None, playing }
    }

    #[test]
    fn a_game_waits_a_held_file_signs_its_host_out_and_unknown_programs_wait() {
        let members = [member("alice", 10, true), member("bob", 20, false)];
        assert_eq!(decide(&[], &members), Decision::default());
        assert_eq!(decide(&[10], &members).waiting, Some(Some("ALICE".into())));
        // Bob's idle host has no session record here, so nothing can sign it out: it waits.
        assert_eq!(decide(&[20], &members).waiting, Some(None));
        assert_eq!(decide(&[99, 10], &members).waiting, Some(Some("ALICE".into())), "a named game wins");
        assert_eq!(decide(&[10, 99], &members).waiting, Some(Some("ALICE".into())));
        assert!(decide(&[10], &members).sign_out.is_empty(), "never close a game");
    }

    #[test]
    fn folders_compare_like_windows_paths() {
        assert!(same_folder(Path::new(r"C:\Games\FFXI\"), Path::new(r"c:\games\ffxi")));
        assert!(!same_folder(Path::new(r"C:\Games\FFXI"), Path::new(r"C:\Games\FFXI2")));
    }

    /// An account already in game from the folder: the update has downloaded, the install waits
    /// for that game (named) and goes on by itself once it closed. Nothing is closed.
    #[tokio::test]
    async fn the_install_waits_for_a_running_game_and_goes_on_once_it_closes() {
        let op = Operation::new();
        let members = Mutex::new(vec![member("alice", 10, true)]);
        let scans = std::sync::atomic::AtomicUsize::new(0);
        let users = |_: &Path| Ok(if scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 { vec![10] } else { vec![] });
        let mut events = Vec::new();
        let mut send = |event: Event| events.push(format!("{event:?}"));
        let started = Instant::now();
        wait_until_free(Path::new("."), &op, &members, users, || false, |_| async { Err::<(), String>("never signs a game out".into()) }, &mut send).await.unwrap();
        assert!(started.elapsed() >= FREE_POLL * 2, "it looked again only after a pause");
        assert_eq!(events, [format!("{:?}", Event::Waiting(Some("ALICE".into()))), format!("{:?}", Event::Installing)]);
    }

    /// A file the update replaces is held open by something the process scan does not name (a
    /// plain open data file): the install waits for it, then goes on.
    #[tokio::test]
    async fn a_held_file_holds_the_install_back_until_it_is_released() {
        let op = Operation::new();
        let members = Mutex::new(vec![member("alice", 10, false)]);
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let held = || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 1;
        let mut events = Vec::new();
        let mut send = |event: Event| events.push(format!("{event:?}"));
        wait_until_free(Path::new("."), &op, &members, |_| Ok(vec![]), held, |_| async { Ok(()) }, &mut send).await.unwrap();
        assert_eq!(events, [format!("{:?}", Event::Waiting(None)), format!("{:?}", Event::Installing)]);
    }

    #[tokio::test]
    async fn a_cancel_while_waiting_keeps_the_download_and_stops() {
        let op = Operation::new();
        let members = Mutex::new(vec![member("alice", 10, true)]);
        let cancel = op.clone();
        let mut send = |event: Event| { if matches!(event, Event::Waiting(_)) { cancel.cancel(); } };
        let error = wait_until_free(Path::new("."), &op, &members, |_| Ok(vec![10]), || false, |_| async { Ok(()) }, &mut send).await.unwrap_err();
        assert_eq!(error, t("Cancelled. Verified downloads are kept for resuming."));
    }

    #[tokio::test]
    async fn a_scan_that_keeps_failing_stops_the_install_but_one_failure_does_not() {
        let op = Operation::new();
        let members = Mutex::new(Vec::new());
        let scans = std::sync::atomic::AtomicUsize::new(0);
        let users = |_: &Path| if scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { Err("busy".to_owned()) } else { Ok(vec![]) };
        let mut send = |_: Event| {};
        wait_until_free(Path::new("."), &op, &members, users, || false, |_| async { Ok(()) }, &mut send).await.unwrap();
    }
}
