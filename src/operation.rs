//! Shared progress and cancellation for disk/network work outside the UI thread.
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A step of a file operation. Never shown as is: the Downloads page names each
/// stage in the current language; `as_str` is its id for logs and benches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// No step named yet: the page names the operation.
    #[default]
    None,
    Manifest,
    SignOut,
    Waiting,
    Check,
    Download,
    Verify,
    Apply,
    Finish,
    InstallerMetadata,
    InstallerDownload,
}
impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Manifest => "Manifest",
            Self::SignOut => "SignOut",
            Self::Waiting => "Waiting",
            Self::Check => "Check",
            Self::Download => "Download",
            Self::Verify => "Verify",
            Self::Apply => "Apply",
            Self::Finish => "Finish",
            Self::InstallerMetadata => "installer-metadata",
            Self::InstallerDownload => "installer-download",
        }
    }
}
impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
    pub detail: String,
    pub bytes: bool,
    /// Estimate for this stage only, based on recent observed progress.
    pub remaining: Option<Duration>,
    pub overall: Option<OverallProgress>,
    /// The official installer volume this step is about, kept as data so the
    /// page draws its per-part list and words it in the current language.
    pub part: Option<Part>,
}

impl Progress {
    pub fn remaining_percent(&self) -> Option<u8> {
        (self.total > 0).then(|| {
            let left = self.total.saturating_sub(self.done) as u128;
            // Keep 1% visible until the stage really completes; avoid overflow
            // even for the largest byte counts.
            (left * 100).div_ceil(self.total as u128) as u8
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OverallProgress {
    /// An estimate for the whole run, held across stage changes and stalls.
    pub fraction: Option<f64>,
    pub remaining: Option<Duration>,
    pub complete: bool,
}
impl OverallProgress {
    pub fn remaining_percent(&self) -> Option<u8> {
        if self.complete { return Some(0); }
        self.fraction.filter(|fraction| fraction.is_finite()).map(|fraction| {
            100 - (fraction.clamp(0.0, 0.99) * 100.0).floor() as u8
        })
    }
}

#[derive(Clone, Copy)]
pub enum UpdateWork { Staging, Apply, Verify }

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateStage { Preparing, Download, Apply, Verify, Finish, Complete }

#[derive(Default)]
struct Rate {
    units: f64,
    seconds: f64,
}
impl Rate {
    fn record(&mut self, units: u64, elapsed: Duration) {
        if units == 0 || elapsed.is_zero() { return; }
        // Bound old observations so a warm cache or changing disk load can
        // replace the initial scan's speed instead of dominating indefinitely.
        if self.seconds > 20.0 { self.units *= 0.5; self.seconds *= 0.5; }
        self.units += units as f64;
        self.seconds += elapsed.as_secs_f64();
    }
    fn duration(&self, units: u64) -> Option<Duration> {
        if units == 0 { return Some(Duration::ZERO); }
        if self.units == 0.0 { return None; }
        Duration::try_from_secs_f64(units as f64 * self.seconds / self.units).ok()
    }
}

struct WholeUpdate {
    started: Instant,
    stage: UpdateStage,
    network: u64,
    pending: [u64; 3],
    disk: Rate,
    apply: Rate,
    fraction: Option<f64>,
    revision: u64,
    displayed_revision: u64,
    last_activity: Instant,
}
impl WholeUpdate {
    fn new(now: Instant) -> Self {
        Self { started: now, stage: UpdateStage::Preparing, network: 0, pending: [0; 3],
            disk: Rate::default(), apply: Rate::default(), fraction: None, revision: 1, displayed_revision: 0, last_activity: now }
    }
    fn observe(&mut self, stage: Stage, advanced: bool, now: Instant) {
        let next = match stage {
            Stage::Download if self.stage != UpdateStage::Preparing => UpdateStage::Download,
            Stage::Apply => UpdateStage::Apply,
            Stage::Verify => UpdateStage::Verify,
            Stage::Finish => UpdateStage::Finish,
            _ => self.stage,
        };
        if advanced || next != self.stage { self.revision += 1; self.last_activity = now; }
        self.stage = next;
    }
    fn plan(&mut self, network: u64, staging: u64, apply: u64, verify: u64, now: Instant) {
        self.network = network;
        self.pending = [staging, apply, verify];
        self.stage = UpdateStage::Download;
        self.revision += 1;
        self.last_activity = now;
    }
    fn credit(&mut self, work: UpdateWork, units: u64, elapsed: Option<Duration>, now: Instant) {
        let index = work as usize;
        self.pending[index] = self.pending[index].saturating_sub(units);
        if matches!(work, UpdateWork::Apply) {
            if let Some(elapsed) = elapsed { self.apply.record(units, elapsed); }
        }
        self.revision += 1;
        self.last_activity = now;
    }
    fn remaining(&self, progress: &Progress, step_eta: Option<Duration>) -> Option<Duration> {
        match self.stage {
            UpdateStage::Preparing => None,
            UpdateStage::Complete | UpdateStage::Finish => Some(Duration::ZERO),
            UpdateStage::Verify => self.apply.duration(self.pending[1])
                .or_else(|| self.disk.duration(self.pending[1]))?
                .checked_add(self.disk.duration(self.pending[2])?),
            UpdateStage::Apply | UpdateStage::Download => {
                let apply = self.apply.duration(self.pending[1]).or_else(|| self.disk.duration(self.pending[1]))?;
                let verify = self.disk.duration(self.pending[2])?;
                let disk = if self.stage == UpdateStage::Download {
                    self.disk.duration(self.pending[0])?
                } else { Duration::ZERO };
                let network = if self.stage == UpdateStage::Download && self.network > 0 {
                    if progress.stage != Stage::Download || !progress.bytes { return None; }
                    step_eta?
                } else { Duration::ZERO };
                // Staging and six network workers overlap; applying and the
                // full installation verification run after those workers.
                network.max(disk).checked_add(apply)?.checked_add(verify)
            }
        }
    }
    fn snapshot(&mut self, now: Instant, progress: &Progress, step_eta: Option<Duration>) -> OverallProgress {
        let stalled = !matches!(self.stage, UpdateStage::Finish | UpdateStage::Complete)
            && now.duration_since(self.last_activity) > Duration::from_secs(10);
        let remaining = if stalled { None } else { self.remaining(progress, step_eta) };
        let complete = self.stage == UpdateStage::Complete;
        if complete {
            self.fraction = Some(1.0);
        } else if self.revision != self.displayed_revision {
            if let Some(remaining) = remaining {
                let elapsed = now.duration_since(self.started).as_secs_f64();
                let fraction = if remaining.is_zero() { 0.99 }
                    else { (elapsed / (elapsed + remaining.as_secs_f64())).min(0.99) };
                self.fraction = Some(self.fraction.unwrap_or(0.0).max(fraction));
                self.displayed_revision = self.revision;
            }
        }
        OverallProgress { fraction: self.fraction, remaining, complete }
    }
}

#[derive(Default)]
struct State {
    progress: Progress,
    samples: VecDeque<(Instant, u64)>,
    last_advance: Option<Instant>,
    update: Option<WholeUpdate>,
}

impl State {
    fn publish(&mut self, progress: Progress, now: Instant, rebase: bool) {
        let reset = rebase || self.samples.is_empty()
            || self.progress.stage != progress.stage
            || self.progress.total != progress.total
            || self.progress.bytes != progress.bytes
            || self.progress.part.map(|part| part.number) != progress.part.map(|part| part.number)
            || progress.done < self.progress.done;
        if reset {
            self.samples.clear();
            self.samples.push_back((now, progress.done));
            self.last_advance = Some(now);
        } else if progress.done > self.progress.done {
            self.last_advance = Some(now);
            if self.samples.back().is_some_and(|(at, _)| now.duration_since(*at) >= Duration::from_millis(500)) {
                self.samples.push_back((now, progress.done));
            }
        }
        if let Some(update) = &mut self.update {
            update.observe(progress.stage, progress.done > self.progress.done, now);
        }
        self.progress = progress;
        // A bounded recent window adapts to changing network/disk speed. Keep
        // one sample before the window so sparse file completions still work.
        while self.samples.len() > 2 && self.samples.get(1)
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(20)) {
            self.samples.pop_front();
        }
    }

    fn estimate(&self, now: Instant) -> Option<Duration> {
        if self.progress.total == 0 { return None; }
        if self.progress.done >= self.progress.total { return Some(Duration::ZERO); }
        let (started, initial) = *self.samples.front()?;
        let elapsed = now.duration_since(started);
        if elapsed < Duration::from_secs(2)
            || now.duration_since(self.last_advance?) > Duration::from_secs(10) {
            return None;
        }
        let advanced = self.progress.done.checked_sub(initial)?;
        if advanced == 0 { return None; }
        let left = self.progress.total - self.progress.done;
        Duration::try_from_secs_f64(left as f64 * elapsed.as_secs_f64() / advanced as f64).ok()
    }
}

/// One official installer volume's step (`number` is 1-based).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub number: u8,
    pub name: &'static str,
    pub state: PartState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartState {
    /// Reading the volume's official metadata.
    Checking,
    /// Transferring: `done` of `expected` bytes are on disk.
    Downloading { done: u64, expected: u64 },
    /// A verified copy from an earlier run was kept.
    AlreadyComplete,
    /// Downloaded and verified in this run.
    Complete,
    /// The kept copy no longer matches; it is being downloaded again.
    Changed,
}

impl Part {
    /// English, for console progress and anywhere the page is not drawing it.
    pub fn english(&self) -> String {
        let name = self.name;
        match self.state {
            PartState::Checking => format!("Checking {name}"),
            PartState::Downloading { done, expected } => format!("Downloading {name}: {done} / {expected} bytes"),
            PartState::AlreadyComplete => format!("Already complete: {name}"),
            PartState::Complete => format!("Complete: {name}"),
            PartState::Changed => format!("{name} changed; downloading the official file again"),
        }
    }
}

#[derive(Clone, Default)]
pub struct Operation {
    cancelled: Arc<AtomicBool>,
    /// Wakes `cancelled_wait` at `cancel` (every clone shares it).
    cancel_wake: Arc<tokio::sync::Notify>,
    state: Arc<Mutex<State>>,
    component: Option<&'static str>,
}

impl Operation {
    pub fn new() -> Self { Self::default() }
    pub fn component(&self, name: &'static str) -> Self { Self { component: Some(name), ..self.clone() } }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_wake.notify_waiters();
    }
    pub fn cancelled(&self) -> bool { self.cancelled.load(Ordering::Acquire) }
    pub fn check_cancelled(&self) -> Result<(), String> {
        if self.cancelled() { Err(Self::cancel_message()) } else { Ok(()) }
    }
    fn cancel_message() -> String { crate::i18n::t("Cancelled. Verified downloads are kept for resuming.").into() }
    /// Resolves once the operation is cancelled (at once if it already is), with
    /// `check_cancelled`'s error, so a wait can race it instead of polling.
    pub async fn cancelled_wait(&self) -> String {
        let woken = self.cancel_wake.notified();
        tokio::pin!(woken);
        // Registered before the flag is read, so a cancel in between still wakes it.
        woken.as_mut().enable();
        if !self.cancelled() { woken.await; }
        Self::cancel_message()
    }
    pub fn snapshot(&self) -> Progress {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let mut progress = state.progress.clone();
        progress.remaining = state.estimate(now);
        progress.overall = state.update.as_mut().map(|update| update.snapshot(now, &progress, progress.remaining));
        progress
    }
    pub fn begin_update(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.update.is_none() { state.update = Some(WholeUpdate::new(Instant::now())); }
    }
    pub fn plan_update(&self, network: u64, staging: u64, apply: u64, verify: u64) {
        if let Some(update) = &mut self.state.lock().unwrap_or_else(|e| e.into_inner()).update {
            update.plan(network, staging, apply, verify, Instant::now());
        }
    }
    pub fn record_disk_io(&self, bytes: u64, elapsed: Duration) {
        if let Some(update) = &mut self.state.lock().unwrap_or_else(|e| e.into_inner()).update {
            update.disk.record(bytes, elapsed);
            update.last_activity = Instant::now();
        }
    }
    pub fn complete_update_work(&self, work: UpdateWork, bytes: u64, elapsed: Option<Duration>) {
        if let Some(update) = &mut self.state.lock().unwrap_or_else(|e| e.into_inner()).update {
            update.credit(work, bytes, elapsed, Instant::now());
        }
    }
    pub fn finish_update(&self) {
        if let Some(update) = &mut self.state.lock().unwrap_or_else(|e| e.into_inner()).update {
            update.stage = UpdateStage::Complete;
            update.revision += 1;
        }
    }
    pub fn progress(&self, stage: Stage, done: u64, total: u64, detail: impl Into<String>) {
        self.publish(Progress {
            stage, done, total, detail: detail.into(), part: None,
            ..Progress::default()
        }, false);
    }
    pub fn progress_bytes(&self, stage: Stage, done: u64, total: u64, detail: impl Into<String>) {
        self.publish(Progress {
            stage, done, total, detail: detail.into(), bytes: true,
            ..Progress::default()
        }, false);
    }
    /// Reused files advance the fraction without pretending their bytes were
    /// transferred at network speed.
    pub fn progress_bytes_rebased(&self, stage: Stage, done: u64, total: u64, detail: impl Into<String>) {
        self.publish(Progress {
            stage, done, total, detail: detail.into(), bytes: true,
            ..Progress::default()
        }, true);
    }
    /// `progress` for one installer volume; the detail is its English line.
    pub fn progress_part(&self, stage: Stage, done: u64, total: u64, part: Part) {
        self.publish(Progress {
            stage, done, total, detail: part.english(), part: Some(part),
            bytes: stage == Stage::InstallerDownload,
            ..Progress::default()
        }, matches!(part.state, PartState::AlreadyComplete));
    }
    fn publish(&self, mut progress: Progress, rebase: bool) {
        if let Some(name) = self.component { progress.detail = format!("{name}: {}", progress.detail); }
        self.state.lock().unwrap_or_else(|e| e.into_inner()).publish(progress, Instant::now(), rebase);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(stage: Stage, done: u64, total: u64) -> Progress {
        Progress { stage, done, total, ..Progress::default() }
    }

    /// A wait raced against `cancelled_wait` ends at the cancel (from any clone, even one made
    /// for a component), not on a poll, and a cancel before the wait starts is not missed.
    #[tokio::test]
    async fn cancelled_wait_wakes_at_the_cancel_and_never_misses_an_earlier_one() {
        let op = Operation::new();
        let canceller = op.component("PlayOnline");
        let started = Instant::now();
        let waiting = tokio::spawn(async move { op.cancelled_wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        canceller.cancel();
        let message = tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
        assert!(message.contains("Cancelled") && started.elapsed() < Duration::from_millis(500));
        let already = Operation::new();
        already.cancel();
        assert!(tokio::time::timeout(Duration::from_millis(50), already.cancelled_wait()).await.is_ok());
    }

    #[test]
    fn percentage_waits_for_a_total_and_does_not_round_to_zero_early() {
        assert_eq!(progress(Stage::Download, 0, 0).remaining_percent(), None);
        assert_eq!(progress(Stage::Download, 1, 3).remaining_percent(), Some(67));
        assert_eq!(progress(Stage::Download, 999, 1000).remaining_percent(), Some(1));
        assert_eq!(progress(Stage::Download, u64::MAX - 1, u64::MAX).remaining_percent(), Some(1));
        assert_eq!(progress(Stage::Download, 1001, 1000).remaining_percent(), Some(0));
    }

    #[test]
    fn estimates_observed_progress_and_withdraws_the_estimate_when_stalled() {
        let start = Instant::now();
        let mut state = State::default();
        state.publish(progress(Stage::Check, 0, 100), start, false);
        state.publish(progress(Stage::Check, 10, 100), start + Duration::from_secs(1), false);
        assert_eq!(state.estimate(start + Duration::from_secs(1)), None);
        state.publish(progress(Stage::Check, 40, 100), start + Duration::from_secs(4), false);
        assert_eq!(state.estimate(start + Duration::from_secs(4)), Some(Duration::from_secs(6)));
        assert_eq!(state.estimate(start + Duration::from_secs(15)), None);
        state.publish(progress(Stage::Check, 100, 100), start + Duration::from_secs(16), false);
        assert_eq!(state.estimate(start + Duration::from_secs(16)), Some(Duration::ZERO));
    }

    #[test]
    fn changing_steps_totals_or_reusing_bytes_restarts_the_estimate() {
        let start = Instant::now();
        let mut state = State::default();
        state.publish(progress(Stage::Check, 0, 100), start, false);
        state.publish(progress(Stage::Check, 40, 100), start + Duration::from_secs(4), false);
        assert!(state.estimate(start + Duration::from_secs(4)).is_some());
        state.publish(progress(Stage::Download, 0, 100), start + Duration::from_secs(4), false);
        assert_eq!(state.estimate(start + Duration::from_secs(4)), None);
        state.publish(progress(Stage::Download, 50, 100), start + Duration::from_secs(8), true);
        assert_eq!(state.estimate(start + Duration::from_secs(8)), None);
        state.publish(progress(Stage::Download, 70, 100), start + Duration::from_secs(10), false);
        assert_eq!(state.estimate(start + Duration::from_secs(10)), Some(Duration::from_secs(3)));
        state.publish(progress(Stage::Download, 70, 200), start + Duration::from_secs(11), false);
        assert_eq!(state.estimate(start + Duration::from_secs(11)), None);
    }

    #[test]
    fn resumed_installer_bytes_are_not_measured_as_network_throughput() {
        let start = Instant::now();
        let volume = |number, done, total_done| Progress {
            stage: Stage::InstallerDownload, done: total_done, total: 200, bytes: true,
            part: Some(Part { number, name: "volume", state: PartState::Downloading { done, expected: 100 } }),
            ..Progress::default()
        };
        let mut state = State::default();
        state.publish(volume(1, 0, 0), start, false);
        state.publish(volume(1, 100, 100), start + Duration::from_secs(10), false);
        assert_eq!(state.estimate(start + Duration::from_secs(10)), Some(Duration::from_secs(10)));
        // The next volume resumes 90 bytes from disk, without transferring them.
        state.publish(volume(2, 90, 190), start + Duration::from_secs(10), false);
        assert_eq!(state.estimate(start + Duration::from_secs(10)), None);
        state.publish(volume(2, 95, 195), start + Duration::from_secs(12), false);
        assert_eq!(state.estimate(start + Duration::from_secs(12)), Some(Duration::from_secs(2)));
    }

    #[test]
    fn whole_update_includes_applying_and_all_verification_after_downloads_finish() {
        let start = Instant::now();
        let mut update = WholeUpdate::new(start);
        update.disk.record(100, Duration::from_secs(10));
        update.plan(100, 20, 40, 100, start);
        let downloaded = Progress { stage: Stage::Download, done: 100, total: 100, bytes: true, ..Progress::default() };
        update.observe(Stage::Download, true, start + Duration::from_secs(5));
        let overall = update.snapshot(start + Duration::from_secs(5), &downloaded, Some(Duration::ZERO));
        assert_eq!(overall.remaining, Some(Duration::from_secs(16)));
        assert!(!overall.complete && overall.remaining_percent().unwrap() > 0);
        let downloaded_fraction = overall.fraction.unwrap();
        update.credit(UpdateWork::Staging, 20, None, start + Duration::from_secs(6));
        update.observe(Stage::Apply, false, start + Duration::from_secs(6));
        let applying = update.snapshot(start + Duration::from_secs(6), &downloaded, None);
        assert_eq!(applying.remaining, Some(Duration::from_secs(14)));
        assert!(applying.fraction.unwrap() >= downloaded_fraction);
        update.credit(UpdateWork::Apply, 40, Some(Duration::from_secs(8)), start + Duration::from_secs(14));
        update.observe(Stage::Verify, false, start + Duration::from_secs(14));
        assert_eq!(update.snapshot(start + Duration::from_secs(14), &downloaded, None).remaining, Some(Duration::from_secs(10)));
        update.credit(UpdateWork::Verify, 100, None, start + Duration::from_secs(24));
        update.observe(Stage::Finish, false, start + Duration::from_secs(24));
        let finishing = update.snapshot(start + Duration::from_secs(24), &downloaded, None);
        assert_eq!(finishing.remaining_percent(), Some(1));
        assert!(!finishing.complete);
        update.stage = UpdateStage::Complete;
        let complete = update.snapshot(start + Duration::from_secs(25), &downloaded, None);
        assert_eq!(complete.remaining_percent(), Some(0));
        assert_eq!(complete.fraction, Some(1.0));
    }

    #[test]
    fn first_component_verification_keeps_second_component_work_in_the_total() {
        let start = Instant::now();
        let mut update = WholeUpdate::new(start);
        update.disk.record(100, Duration::from_secs(10));
        update.plan(0, 0, 60, 200, start);
        update.credit(UpdateWork::Apply, 20, Some(Duration::from_secs(4)), start);
        update.credit(UpdateWork::Verify, 50, None, start);
        update.observe(Stage::Verify, true, start);
        let progress = progress(Stage::Verify, 50, 100);
        let first = update.snapshot(start, &progress, None);
        assert_eq!(first.remaining, Some(Duration::from_secs(23))); // 8 apply + 15 verify
        assert!(!first.complete);
        update.observe(Stage::Apply, true, start);
        assert_eq!(update.snapshot(start, &progress, None).remaining, first.remaining);
    }

    #[test]
    fn stalled_snapshots_do_not_advance_the_overall_bar() {
        let start = Instant::now();
        let mut update = WholeUpdate::new(start);
        update.disk.record(100, Duration::from_secs(10));
        update.plan(0, 0, 0, 1000, start);
        update.observe(Stage::Verify, false, start + Duration::from_secs(5));
        let progress = progress(Stage::Verify, 0, 100);
        let before = update.snapshot(start + Duration::from_secs(5), &progress, None);
        let waiting = update.snapshot(start + Duration::from_secs(9), &progress, None);
        assert_eq!(waiting.fraction, before.fraction);
        let stalled = update.snapshot(start + Duration::from_secs(16), &progress, None);
        assert_eq!(stalled.remaining, None);
        assert_eq!(stalled.fraction, before.fraction);
        update.credit(UpdateWork::Verify, 100, None, start + Duration::from_secs(20));
        let resumed = update.snapshot(start + Duration::from_secs(20), &progress, None);
        assert!(resumed.fraction.unwrap() > before.fraction.unwrap());
        assert!(resumed.remaining.is_some());
    }

    #[test]
    fn cached_or_empty_download_plans_need_no_network_speed_sample() {
        let start = Instant::now();
        for network in [0, 100] {
            let mut update = WholeUpdate::new(start);
            update.disk.record(100, Duration::from_secs(1));
            update.plan(network, 0, 0, 100, start);
            let progress = Progress { stage: Stage::Download, done: network, total: network, bytes: true, ..Progress::default() };
            let overall = update.snapshot(start + Duration::from_secs(2), &progress, Some(Duration::ZERO));
            assert_eq!(overall.remaining, Some(Duration::from_secs(1)));
            assert!(!overall.complete);
        }
    }

    #[test]
    fn fallback_extends_total_time_without_resetting_overall_progress() {
        let start = Instant::now();
        let mut update = WholeUpdate::new(start);
        update.disk.record(100, Duration::from_secs(10));
        update.plan(100, 0, 0, 100, start);
        let mut progress = Progress { stage: Stage::Download, done: 50, total: 100, bytes: true, ..Progress::default() };
        update.observe(Stage::Download, true, start + Duration::from_secs(5));
        let before = update.snapshot(start + Duration::from_secs(5), &progress, Some(Duration::from_secs(10)));
        progress.total = 600;
        let fallback = update.snapshot(start + Duration::from_secs(5), &progress, Some(Duration::from_secs(110)));
        assert!(fallback.remaining.unwrap() > before.remaining.unwrap());
        assert_eq!(fallback.fraction, before.fraction);
    }

    #[test]
    fn overall_estimate_waits_for_real_disk_and_network_measurements() {
        let start = Instant::now();
        let mut update = WholeUpdate::new(start);
        let progress = Progress { stage: Stage::Download, done: 50, total: 100, bytes: true, ..Progress::default() };
        assert_eq!(update.snapshot(start, &progress, None).remaining, None);
        update.plan(100, 20, 40, 100, start);
        assert_eq!(update.snapshot(start, &progress, Some(Duration::from_secs(5))).remaining, None);
        update.disk.record(100, Duration::from_secs(1));
        assert_eq!(update.snapshot(start, &progress, None).remaining, None);
        let measured = update.snapshot(start + Duration::from_secs(2), &progress, Some(Duration::from_secs(5)));
        // Network and staging overlap: max(5, 0.2) + 0.4 apply + 1 verification.
        assert_eq!(measured.remaining, Some(Duration::from_millis(6400)));
    }
}
