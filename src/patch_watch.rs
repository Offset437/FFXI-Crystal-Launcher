//! The FFXI update as soon as it is out (owner, 1.1.0: "I want it to update as soon as update
//! is available").
//!
//! During maintenance the stock FFXI page turns Play into "Check Version" while
//! `$playbutton_enable` is 0 and `$patch_enable` is 1 and this version's update is not done yet
//! (index.pml, capture pml-20261013). A signed-in user who presses it sends the W20 to
//! pc001W2U.pol.com:53001 and, when it asks for one, downloads the update before the worlds
//! open (capture stock-update-20261013-004932: W20s at 00:50:19 and 00:54:49, then the file list
//! and the downloads, all before the flag flipped at 01:17:30). Crystal does the same from a
//! session held at the main menu, with no game start after the W20.
//!
//! This is the schedule; main.rs runs it (`tend_patch_watch`). Checks run only while FFXI is in
//! maintenance and the flag's `$patch_enable` is a clear 1, one per game folder per trigger:
//! the first sight of that, its 0 → 1 flip, and every RECHECK ± JITTER after an answer of
//! "current".
use crate::play_update::same_folder;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Between checks of a folder whose last answer was "current" (the stock page's own reload).
pub(crate) const RECHECK: Duration = Duration::from_secs(300);
/// RECHECK varies by up to this either way.
pub(crate) const JITTER: Duration = Duration::from_secs(30);
/// An official "up" with no fresh flag ends the watch only once it has held this long.
pub(crate) const END_AFTER: Duration = Duration::from_secs(300);

/// What the window knows of FFXI's state, for `observe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// In maintenance: a fresh flag of 0, or the official status without a fresh flag.
    Closed,
    /// A fresh flag of 1: maintenance is over.
    Open,
    /// The official status says up, with no fresh flag: over once it holds for END_AFTER.
    Up,
    /// Nothing known (stale or failed reads): no change.
    Unknown,
}

/// What `observe` saw change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// Maintenance began (or was first seen).
    Began,
    /// `$patch_enable` went from 0 to 1 during it.
    PatchReady,
    /// Maintenance is over.
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// A W20 is out.
    Checking,
    /// The last answer was "current" (or none came): checked again at `next`.
    Current { next: Instant },
    /// The update runs.
    Updating,
    /// Installed: nothing more until the next flip.
    Installed,
    /// The update stopped (cancelled or failed): nothing more until the next flip.
    Stopped,
}

#[derive(Debug, Default)]
pub(crate) struct Watch {
    active: bool,
    patch: Option<bool>,
    /// The last clear `$patch_enable` of this maintenance, for the flip.
    was: Option<bool>,
    /// Accounts signed in (or tried) since the last trigger: one sign-in each per trigger.
    signed: BTreeSet<String>,
    folders: Vec<(PathBuf, State)>,
    /// When each folder's last W20 answered (or failed): kept through a reset, so no check comes
    /// sooner than RECHECK - JITTER after the last, except at a real 0 → 1 flip.
    last_w20: Vec<(PathBuf, Instant)>,
    /// Accounts whose session has a check out: kept through a reset, so a Play there waits for
    /// it instead of sending a second W20 on the same session.
    in_flight: BTreeSet<String>,
    /// `ready()`, shared with the check threads: one that waited for its main menu sends only
    /// while it is still true.
    gate: Arc<AtomicBool>,
    /// When an official "up" with no fresh flag was first seen (`Seen::Up`).
    up_since: Option<Instant>,
    /// Tests: checks that would have started (folder, account key).
    #[cfg(test)]
    pub checks: Vec<(PathBuf, String)>,
}

impl Watch {
    /// FFXI is in maintenance.
    pub(crate) fn active(&self) -> bool { self.active }
    /// The last clear `$patch_enable` of this maintenance.
    pub(crate) fn last_patch(&self) -> Option<bool> { self.was }
    /// Checks (and the sign-ins they need) may run: maintenance, and a clear 1 now.
    pub(crate) fn ready(&self) -> bool { self.active && self.patch == Some(true) }
    /// `ready()` as the check threads see it.
    pub(crate) fn gate(&self) -> Arc<AtomicBool> { self.gate.clone() }

    /// Each Tick and flag read: what is known of FFXI (`seen`, `now`), and the flag's
    /// `$patch_enable` from a fresh read (`None` when unknown). Only an affirmative end stops the
    /// watch; a new maintenance or a flip starts a new cycle: every account may sign in once more
    /// and every folder is checked again.
    pub(crate) fn observe(&mut self, seen: Seen, patch: Option<bool>, now: Instant) -> Option<Change> {
        let change = self.observe_inner(seen, patch, now);
        self.gate.store(self.ready(), Ordering::SeqCst);
        change
    }

    fn observe_inner(&mut self, seen: Seen, patch: Option<bool>, now: Instant) -> Option<Change> {
        if seen != Seen::Closed {
            if !self.active { self.up_since = None; return None; }
            let ended = match seen {
                Seen::Open => true,
                Seen::Up => now.saturating_duration_since(*self.up_since.get_or_insert(now)) >= END_AFTER,
                _ => false,
            };
            if !ended {
                // No fresh flag: nothing is ready, and nothing is forgotten.
                self.patch = None;
                return None;
            }
            self.reset();
            return Some(Change::Ended);
        }
        self.up_since = None;
        let began = !self.active;
        if began {
            self.reset();
            self.active = true;
        }
        self.patch = patch;
        let flipped = patch == Some(true) && self.was == Some(false);
        if patch.is_some() { self.was = patch; }
        if flipped {
            self.signed.clear();
            self.last_w20.clear();
            // A W20 or an update already out goes on.
            self.folders.retain(|(_, state)| matches!(state, State::Checking | State::Updating));
            return Some(Change::PatchReady);
        }
        began.then_some(Change::Began)
    }

    /// A new cycle. A W20 or an update already out, and when each folder was last checked,
    /// carry over.
    fn reset(&mut self) {
        let mut folders = std::mem::take(&mut self.folders);
        folders.retain(|(_, state)| matches!(state, State::Checking | State::Updating));
        let kept = Self { folders, last_w20: std::mem::take(&mut self.last_w20), in_flight: std::mem::take(&mut self.in_flight),
            gate: self.gate.clone(), #[cfg(test)] checks: std::mem::take(&mut self.checks), ..Self::default() };
        *self = kept;
    }

    /// Whether `key` may be signed in now: once per cycle.
    pub(crate) fn sign_in_once(&mut self, key: &str) -> bool { self.signed.insert(key.to_owned()) }
    /// `key` has signed in (or tried) this cycle.
    pub(crate) fn signed(&self, key: &str) -> bool { self.signed.contains(key) }
    /// `key` may sign in again this cycle (its host was signed out for the install).
    pub(crate) fn forget_signed(&mut self, key: &str) { self.signed.remove(key); }
    /// Any folder has a state to show.
    pub(crate) fn tracking(&self) -> bool { !self.folders.is_empty() }
    /// `key`'s session has a check out.
    pub(crate) fn in_flight(&self, key: &str) -> bool { self.in_flight.contains(key) }

    fn state(&self, folder: &Path) -> Option<State> {
        self.folders.iter().find(|(known, _)| same_folder(known, folder)).map(|(_, state)| *state)
    }

    fn set(&mut self, folder: &Path, state: State) {
        match self.folders.iter_mut().find(|(known, _)| same_folder(known, folder)) {
            Some(entry) => entry.1 = state,
            None => self.folders.push((folder.to_path_buf(), state)),
        }
    }

    /// When `folder` may next be checked after its last W20.
    fn spaced_until(&self, folder: &Path) -> Option<Instant> {
        self.last_w20.iter().find(|(known, _)| same_folder(known, folder)).map(|(_, at)| *at + RECHECK - JITTER)
    }

    /// Whether `folder` is checked now.
    pub(crate) fn due(&self, folder: &Path, now: Instant) -> bool {
        self.ready() && self.spaced_until(folder).is_none_or(|at| now >= at) && match self.state(folder) {
            None => true,
            Some(State::Current { next }) => now >= next,
            Some(_) => false,
        }
    }

    /// A check of `folder` goes out from `key`'s session.
    pub(crate) fn begin_check(&mut self, folder: &Path, key: &str) {
        self.set(folder, State::Checking);
        self.in_flight.insert(key.to_owned());
    }

    /// `key`'s check came back (answered or not).
    pub(crate) fn answered(&mut self, key: &str) { self.in_flight.remove(key); }

    /// A check of `folder` answered: an update starts (`updating`), or it is checked again in
    /// RECHECK, varied by `jitter` (0..=2 × JITTER, centred on RECHECK).
    pub(crate) fn checked(&mut self, folder: &Path, updating: bool, now: Instant, jitter: Duration) {
        let state = if updating { State::Updating } else {
            State::Current { next: now + RECHECK - JITTER + jitter.min(JITTER * 2) }
        };
        self.set(folder, state);
        match self.last_w20.iter_mut().find(|(known, _)| same_folder(known, folder)) {
            Some(entry) => entry.1 = now,
            None => self.last_w20.push((folder.to_path_buf(), now)),
        }
    }

    /// `folder`'s update installed (`true`) or stopped: nothing more until the next flip.
    pub(crate) fn updated(&mut self, folder: &Path, installed: bool) {
        self.set(folder, if installed { State::Installed } else { State::Stopped });
    }

    pub(crate) fn checking(&self, folder: &Path) -> bool { self.state(folder) == Some(State::Checking) }
    pub(crate) fn installed(&self, folder: &Path) -> bool { self.active && self.state(folder) == Some(State::Installed) }

    /// When the next check falls due, for a quiet window's timer.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        if !self.ready() { return None; }
        let now = Instant::now();
        self.folders.iter().filter_map(|(_, state)| match state { State::Current { next } => Some(*next), _ => None })
            .chain(self.last_w20.iter().filter(|(folder, _)| self.state(folder).is_none())
                .filter_map(|(folder, _)| self.spaced_until(folder)).filter(|at| *at > now))
            .min()
    }
}

/// A uniformly spread 0..=2 × JITTER, for `Watch::checked`.
pub(crate) fn jitter() -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let bits = std::collections::hash_map::RandomState::new().build_hasher().finish();
    Duration::from_millis(bits % (JITTER.as_millis() as u64 * 2 + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> PathBuf { PathBuf::from(r"C:\Games\FINAL FANTASY XI") }

    #[test]
    fn no_checks_outside_maintenance_or_while_patch_enable_is_not_1() {
        let now = Instant::now();
        let mut watch = Watch::default();
        assert_eq!(watch.observe(Seen::Open, Some(true), now), None);
        assert!(!watch.due(&folder(), now), "outside maintenance");
        assert_eq!(watch.observe(Seen::Closed, Some(false), now), Some(Change::Began));
        assert!(!watch.due(&folder(), now), "patch_enable 0");
        assert_eq!(watch.observe(Seen::Closed, None, now), None);
        assert!(!watch.due(&folder(), now), "unknown is never ready");
        assert_eq!(watch.next_due(), None);
    }

    #[test]
    fn first_sight_checks_at_once_then_every_five_minutes_with_jitter() {
        let now = Instant::now();
        let mut watch = Watch::default();
        assert_eq!(watch.observe(Seen::Closed, Some(true), now), Some(Change::Began));
        assert!(watch.due(&folder(), now), "first sight with patch_enable 1");
        watch.begin_check(&folder(), "alice");
        assert!(watch.checking(&folder()) && !watch.due(&folder(), now), "one check at a time");
        // The same folder written another way is the same folder.
        assert!(!watch.due(Path::new(r"c:\games\final fantasy xi\"), now));
        for jitter in [Duration::ZERO, JITTER, JITTER * 2, JITTER * 9] {
            watch.checked(&folder(), false, now, jitter);
            let next = now + RECHECK - JITTER + jitter.min(JITTER * 2);
            assert!(next >= now + RECHECK - JITTER && next <= now + RECHECK + JITTER);
            assert!(!watch.due(&folder(), next - Duration::from_millis(1)));
            assert!(watch.due(&folder(), next));
            assert_eq!(watch.next_due(), Some(next));
        }
        for _ in 0..50 { assert!(jitter() <= JITTER * 2); }
        // patch_enable drops to 0: no more checks.
        watch.observe(Seen::Closed, Some(false), now);
        assert!(!watch.due(&folder(), now + RECHECK * 10));
    }

    #[test]
    fn the_flip_checks_at_once_and_starts_a_new_sign_in_cycle() {
        let now = Instant::now();
        let mut watch = Watch::default();
        watch.observe(Seen::Closed, Some(false), now);
        assert!(watch.sign_in_once("alice") && !watch.sign_in_once("alice"), "once per cycle");
        assert_eq!(watch.observe(Seen::Closed, Some(true), now), Some(Change::PatchReady));
        assert!(watch.sign_in_once("alice"), "the flip is a new trigger");
        assert!(watch.due(&folder(), now));
        watch.begin_check(&folder(), "alice");
        watch.checked(&folder(), false, now, JITTER);
        assert!(!watch.due(&folder(), now));
        // A read that fails (unknown) and a 1 again is no flip.
        assert_eq!(watch.observe(Seen::Closed, None, now), None);
        assert_eq!(watch.observe(Seen::Closed, Some(true), now), None);
        assert!(!watch.due(&folder(), now) && !watch.sign_in_once("alice"));
        // 1 → 0 → 1 is a flip: checked at once.
        watch.observe(Seen::Closed, Some(false), now);
        assert_eq!(watch.observe(Seen::Closed, Some(true), now), Some(Change::PatchReady));
        assert!(watch.due(&folder(), now));
    }

    #[test]
    fn an_update_holds_the_folder_until_maintenance_ends() {
        let now = Instant::now();
        let mut watch = Watch::default();
        watch.observe(Seen::Closed, Some(true), now);
        watch.begin_check(&folder(), "alice");
        watch.checked(&folder(), true, now, JITTER);
        assert!(!watch.due(&folder(), now + RECHECK * 10), "updating");
        watch.updated(&folder(), true);
        assert!(watch.installed(&folder()) && !watch.due(&folder(), now + RECHECK * 10));
        assert_eq!(watch.observe(Seen::Open, Some(true), now), Some(Change::Ended));
        assert!(!watch.installed(&folder()) && !watch.active() && watch.last_patch().is_none());
        // A stopped update is not retried within the cycle.
        watch.observe(Seen::Closed, Some(true), now);
        watch.begin_check(&folder(), "alice");
        watch.checked(&folder(), true, now, JITTER);
        watch.updated(&folder(), false);
        assert!(!watch.due(&folder(), now + RECHECK * 10));
        // A check that answers after maintenance ended changes nothing.
        watch.observe(Seen::Open, None, now);
        watch.checked(&folder(), false, now, JITTER);
        assert!(watch.next_due().is_none() && !watch.checking(&folder()));
    }

    /// Review findings: stale or failed reads (Unknown) and an official "up" that has not held
    /// change nothing; after a real end the last W20 still spaces the next one, except at a flip.
    #[test]
    fn unknown_changes_nothing_and_the_last_w20_spaces_checks_across_a_reset() {
        let now = Instant::now();
        let mut watch = Watch::default();
        watch.observe(Seen::Closed, Some(true), now);
        assert!(watch.sign_in_once("alice"));
        watch.begin_check(&folder(), "alice");
        watch.checked(&folder(), false, now, JITTER);
        let gate = watch.gate();
        assert!(gate.load(Ordering::SeqCst), "ready");
        for seen in [Seen::Unknown, Seen::Up] {
            assert_eq!(watch.observe(seen, None, now + Duration::from_secs(60)), None);
            assert!(watch.active() && !watch.ready() && !gate.load(Ordering::SeqCst), "not ready, not over");
        }
        assert_eq!(watch.observe(Seen::Closed, Some(true), now + Duration::from_secs(90)), None, "no new cycle");
        assert!(!watch.sign_in_once("alice") && !watch.due(&folder(), now + Duration::from_secs(90)) && gate.load(Ordering::SeqCst));
        // An official "up" ends the watch only once it has held for END_AFTER.
        watch.observe(Seen::Up, None, now + Duration::from_secs(100));
        assert_eq!(watch.observe(Seen::Up, None, now + Duration::from_secs(100) + END_AFTER), Some(Change::Ended));
        // A new maintenance at once: the last W20 still spaces the folder's next.
        assert_eq!(watch.observe(Seen::Closed, Some(true), now + Duration::from_secs(120) + END_AFTER), Some(Change::Began));
        assert!(!watch.due(&folder(), now + RECHECK - JITTER - Duration::from_millis(1)));
        assert!(watch.due(&folder(), now + RECHECK - JITTER));
        assert_eq!(watch.next_due(), (now + RECHECK - JITTER > Instant::now()).then_some(now + RECHECK - JITTER));
        // A real 0 → 1 flip checks at once.
        watch.observe(Seen::Closed, Some(false), now);
        assert!(!gate.load(Ordering::SeqCst));
        watch.observe(Seen::Closed, Some(true), now);
        assert!(watch.due(&folder(), now));
    }

    /// Review finding: a check out on a session, and its folder's Checking, survive the end of
    /// the watch, so no second W20 starts there until it comes back.
    #[test]
    fn a_check_out_survives_a_reset() {
        let now = Instant::now();
        let mut watch = Watch::default();
        watch.observe(Seen::Closed, Some(true), now);
        watch.begin_check(&folder(), "alice");
        assert_eq!(watch.observe(Seen::Open, Some(true), now), Some(Change::Ended));
        assert!(watch.in_flight("alice"));
        watch.observe(Seen::Closed, Some(true), now);
        assert!(watch.checking(&folder()) && !watch.due(&folder(), now + RECHECK * 10));
        watch.answered("alice");
        watch.checked(&folder(), false, now, JITTER);
        assert!(!watch.in_flight("alice") && !watch.checking(&folder()));
    }
}
