//! Rows that say "Starting": what each Play click found, every change of a row's start flags
//! (launch history), and the safety net for a start that stopped answering.
//!
//! A row that waits to start a game (`AccountRow::game_start_pending`) cannot be played again,
//! so a start nothing finishes would swallow every later Play. Each Tick compares the rows'
//! flags with the last Tick's and logs what changed; a start with no worker activity past its
//! limit is cleared with an error. A Play clears a queued start whose session is already idle
//! (what `start_queued_plays` would release), and Cancel clears a start by hand.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::i18n::{t, tf};
use crate::{session_worker, App, AccountRow};

/// A queued or batch-pending row whose session is idle: no worker is busy for it.
pub(crate) const STUCK_START_LIMIT: Duration = Duration::from_secs(60);
/// A signed-in start's thread may wait up to 90 s for the main menu, then runs the W20.
const SIGNED_IN_START_LIMIT: Duration = Duration::from_secs(120);
/// A batch waits for its slowest sign-in (180 s) and stragglers (120 s) before it reports.
const BATCH_START_LIMIT: Duration = Duration::from_secs(360);
/// A row's Cancel shows once its start has waited this long.
pub(crate) const CANCEL_AFTER: Duration = Duration::from_secs(10);

/// The flags that decide whether a row can play, as the history logs them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    busy: bool,
    preparing: bool,
    queued: bool,
    batch_pending: bool,
    batch_requested: bool,
    signed_in: bool,
    playing: bool,
}

impl Flags {
    fn of(row: &AccountRow) -> Self {
        Self {
            busy: row.busy, preparing: row.preparing_session, queued: row.queued_play.is_some(),
            batch_pending: row.batch_pending, batch_requested: row.batch_requested,
            signed_in: row.pid.is_some(), playing: row.is_playing(),
        }
    }

    /// "busy 0→1, queued 1→0": only what changed.
    fn changes(self, now: Self) -> String {
        let pairs = [
            ("busy", self.busy, now.busy), ("preparing", self.preparing, now.preparing),
            ("queued", self.queued, now.queued), ("batch_pending", self.batch_pending, now.batch_pending),
            ("batch_requested", self.batch_requested, now.batch_requested),
            ("signed_in", self.signed_in, now.signed_in), ("playing", self.playing, now.playing),
        ];
        pairs.iter().filter(|(_, was, is)| was != is)
            .map(|(name, was, is)| format!("{name} {}→{}", u8::from(*was), u8::from(*is)))
            .collect::<Vec<_>>().join(", ")
    }
}

struct Entry {
    flags: Flags,
    /// Since when the row has said "Starting".
    starting_since: Option<Instant>,
    /// The last event from a worker or thread for this row.
    activity: Option<Instant>,
}

/// Per row: the flags last logged, and its start's clocks.
#[derive(Default)]
pub(crate) struct StartWatch {
    rows: HashMap<String, Entry>,
}

impl StartWatch {
    /// Since when `key` has said "Starting" (as of the last Tick).
    pub(crate) fn starting_since(&self, key: &str) -> Option<Instant> {
        self.rows.get(key).and_then(|entry| entry.starting_since)
    }
}

impl App {
    /// The row's place in Home order, from 1 (the history never names accounts here).
    fn start_place(&self, key: &str) -> usize {
        self.accounts.iter().position(|row| row.key == key).map_or(0, |at| at + 1)
    }

    /// One row's start state for the history: "#2 busy=1 preparing=0 queued=12s batch=0/0
    /// signed_in=1 playing=0 worker=live".
    fn start_state(&self, row: &AccountRow, now: Instant) -> String {
        let worker = if self.preview { "n/a" } else {
            match session_worker::find(&self.settings.config_for(&self.config, &row.account), &row.account) {
                Some(session) if session.supports_batch_start() => "live",
                Some(_) => "live-old",
                None => "none",
            }
        };
        let queued = row.queued_play.map_or_else(|| "-".to_owned(), |at| format!("{}s", now.saturating_duration_since(at).as_secs()));
        format!("#{} busy={} preparing={} queued={queued} batch={}/{} signed_in={} playing={} worker={worker}",
            self.start_place(&row.key), u8::from(row.busy), u8::from(row.preparing_session),
            u8::from(row.batch_pending), u8::from(row.batch_requested), u8::from(row.pid.is_some()),
            u8::from(row.is_playing()))
    }

    fn start_history(&self, line: &str) {
        if !self.preview { crate::launch_log::append_history(&self.config.history_path(), "Crystal", line, None); }
    }

    /// The history line for a Play (`source`: main, row, all, batch) and the rows it covers.
    pub(crate) fn log_play_click(&self, source: &str, keys: &[String]) {
        let now = Instant::now();
        let rows: Vec<String> = self.accounts.iter().filter(|row| keys.contains(&row.key))
            .map(|row| self.start_state(row, now)).collect();
        self.start_history(&format!("play click ({source}): {}", if rows.is_empty() { "no accounts".to_owned() } else { rows.join("; ") }));
    }

    /// A worker or thread reported for `key`: its start is not stuck.
    pub(crate) fn note_start_activity(&mut self, key: &str) {
        if let Some(entry) = self.start_watch.rows.get_mut(key) { entry.activity = Some(Instant::now()); }
    }

    /// Whether `row`'s start is waiting for something with its own progress and limit (a
    /// sign-in in flight, an update, a maintenance check, "Wait and play", the first status
    /// read), not for a start that may have stopped answering.
    fn start_waits_legitimately(&self, row: &AccountRow) -> bool {
        (row.busy && row.preparing_session)
            || self.play_update_of(&row.key).is_some()
            || self.patch_watch.in_flight(&row.key)
            || self.folder_updating(row)
            || self.is_waiting(&row.key)
            || self.held_plays.iter().any(|play| play.keys.contains(&row.key))
    }

    /// How long `row`'s start may go without worker activity.
    fn start_limit(row: &AccountRow) -> Duration {
        if row.batch_requested { BATCH_START_LIMIT } else if row.busy { SIGNED_IN_START_LIMIT } else { STUCK_START_LIMIT }
    }

    /// Each Tick: log the rows whose flags changed, and clear a start that stopped answering.
    pub(crate) fn watch_starts(&mut self, now: Instant) {
        let keys: Vec<String> = self.accounts.iter().map(|row| row.key.clone()).collect();
        self.start_watch.rows.retain(|key, _| keys.contains(key));
        let mut stuck = Vec::new();
        for key in keys {
            let Some(row) = self.accounts.iter().find(|row| row.key == key) else { continue };
            let flags = Flags::of(row);
            let starting = row.game_start_pending();
            let waits = self.start_waits_legitimately(row);
            let limit = Self::start_limit(row);
            let place = self.start_place(&key);
            let entry = self.start_watch.rows.entry(key.clone())
                .or_insert(Entry { flags: Flags::default(), starting_since: None, activity: None });
            let previous = std::mem::replace(&mut entry.flags, flags);
            if previous != flags {
                let line = format!("row #{place}: {}", previous.changes(flags));
                if !self.preview { crate::launch_log::append_history(&self.config.history_path(), "Crystal", &line, None); }
            }
            if !starting { entry.starting_since = None; continue; }
            let since = *entry.starting_since.get_or_insert(now);
            let last = entry.activity.map_or(since, |activity| activity.max(since));
            if !waits && now.saturating_duration_since(last) >= limit { stuck.push(key); }
        }
        for key in stuck { self.clear_start(&key, false); }
    }

    /// Clears `key`'s start: its queue, its batch claim and the busy flag of a start no thread
    /// answers for. `cancelled`: the row's Cancel; otherwise the watch found it stuck.
    pub(crate) fn clear_start(&mut self, key: &str, cancelled: bool) {
        let Some(row) = self.accounts.iter().find(|row| row.key == key) else { return };
        if !row.game_start_pending() { return; }
        let before = self.start_state(row, Instant::now());
        let config = self.settings.config_for(&self.config, &row.account);
        let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) else { return };
        row.queued_play = None;
        if row.batch_requested {
            row.batch_requested = false;
            row.batch_pending = session_worker::batch_pending(&config, &row.account);
        }
        if row.busy && !row.preparing_session { row.busy = false; }
        row.play_timing = None;
        row.problem = !cancelled;
        row.status = if cancelled { t("Start cancelled.") } else { t("FFXI did not start: nothing answered for a minute. Press Play to try again.") }.into();
        if let Some(entry) = self.start_watch.rows.get_mut(key) { entry.starting_since = None; }
        self.start_history(&format!("start cleared ({}): {before}", if cancelled { "cancel" } else { "no answer" }));
    }

    /// Whether `key`'s row offers Cancel for its start: it has waited CANCEL_AFTER and is not
    /// an update or a wait with its own Cancel.
    pub(crate) fn start_cancellable(&self, key: &str) -> bool {
        self.accounts.iter().find(|row| row.key == key).is_some_and(|row| row.game_start_pending()
            && !self.start_waits_legitimately(row)
            && self.start_watch.starting_since(key).is_some_and(|since| since.elapsed() >= CANCEL_AFTER))
    }

    /// A Play on `keys`: a queued start whose session is already idle is released here (what
    /// `start_queued_plays` does), so it plays now instead of being skipped.
    fn release_stale_queues(&mut self, keys: &[String]) {
        let stale: Vec<String> = self.accounts.iter().filter(|row| keys.contains(&row.key)
            && row.queued_play.is_some() && !row.busy && row.pid.is_some() && !row.is_playing()
            && !self.start_waits_legitimately(row)).map(|row| row.key.clone()).collect();
        for key in stale {
            if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) { row.queued_play = None; }
            self.start_history(&format!("queued start released by Play: #{}", self.start_place(&key)));
        }
    }

    /// Home's Play (`selected_only`) or Play all: logs what it found, releases stale queues,
    /// then plays; with nothing it can start, says how many accounts are still starting.
    pub(crate) fn play_from_home(&mut self, source: &str, selected_only: bool) {
        let candidates: Vec<String> = self.accounts.iter().filter(|row| !selected_only || row.selected).map(|row| row.key.clone()).collect();
        self.log_play_click(source, &candidates);
        self.release_stale_queues(&candidates);
        let keys = self.launch_keys(selected_only);
        if keys.is_empty() { self.note_still_starting(&candidates); return; }
        self.play(keys);
    }

    /// A row's own Play.
    pub(crate) fn play_row(&mut self, key: String) {
        let keys = vec![key];
        self.log_play_click("row", &keys);
        self.release_stale_queues(&keys);
        if self.accounts.iter().any(|row| row.key == keys[0] && row.can_play()) || self.launch_blocked() {
            self.play(keys);
        } else {
            self.note_still_starting(&keys);
        }
    }

    /// Nothing could start: the status says how many of `keys` are still starting.
    fn note_still_starting(&mut self, keys: &[String]) {
        let count = self.accounts.iter().filter(|row| keys.contains(&row.key) && row.game_start_pending()).count();
        if count > 0 { self.status = tf("{count} accounts are still starting.", &[("count", &count)]); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicBool, Arc};

    fn signed_in(app: &mut App, index: usize) -> String {
        let row = &mut app.accounts[index];
        (row.pid, row.alive, row.game_running) = (Some(200 + index as u32), Some(Arc::new(AtomicBool::new(true))), Some(Arc::new(AtomicBool::new(false))));
        row.busy = false;
        row.preparing_session = false;
        row.key.clone()
    }

    /// A queued start on an idle signed-in session that nothing releases (the 1.2.7 live
    /// report: rows said "Starting" and every later Play was skipped) is cleared after the
    /// limit with an error, and the row can play again.
    #[test]
    fn a_start_that_stopped_answering_is_cleared_and_the_row_plays_again() {
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        let key = signed_in(&mut app, 0);
        app.accounts[0].queued_play = Some(Instant::now());
        let start = Instant::now();
        app.watch_starts(start);
        assert!(!app.accounts[0].can_play() && app.accounts[0].game_start_pending());
        app.watch_starts(start + STUCK_START_LIMIT - Duration::from_secs(1));
        assert!(app.accounts[0].queued_play.is_some(), "not before the limit");
        // Worker activity restarts the clock.
        app.note_start_activity(&key);
        app.watch_starts(Instant::now() + STUCK_START_LIMIT - Duration::from_secs(1));
        assert!(app.accounts[0].queued_play.is_some());
        app.watch_starts(Instant::now() + STUCK_START_LIMIT + Duration::from_secs(1));
        let row = &app.accounts[0];
        assert!(row.queued_play.is_none() && row.problem && row.can_play(), "{}", row.status);
        assert_eq!(row.status, t("FFXI did not start: nothing answered for a minute. Press Play to try again."));
    }

    /// A batch claim (busy, batch_pending, batch_requested) whose batch never reports is cleared
    /// after the batch's own limits, never before.
    #[test]
    fn a_batch_that_never_reports_is_released_after_its_limit() {
        let mut app = crate::tests::app();
        signed_in(&mut app, 1);
        {
            let row = &mut app.accounts[1];
            (row.busy, row.batch_pending, row.batch_requested) = (true, true, true);
        }
        let start = Instant::now();
        app.watch_starts(start);
        app.watch_starts(start + SIGNED_IN_START_LIMIT);
        assert!(app.accounts[1].batch_requested, "a batch may wait for its slowest sign-in");
        app.watch_starts(start + BATCH_START_LIMIT + Duration::from_secs(1));
        let row = &app.accounts[1];
        assert!(!row.busy && !row.batch_requested && !row.batch_pending && row.can_play());
    }

    /// A sign-in still in flight, and a Play waiting for an update or a maintenance check, have
    /// their own limits: the watch leaves them alone.
    #[test]
    fn waits_with_their_own_limits_are_never_cleared() {
        let mut app = crate::tests::app();
        {
            let row = &mut app.accounts[0];
            (row.busy, row.preparing_session, row.queued_play) = (true, true, Some(Instant::now()));
        }
        let start = Instant::now();
        app.watch_starts(start);
        app.watch_starts(start + BATCH_START_LIMIT * 2);
        assert!(app.accounts[0].queued_play.is_some() && app.accounts[0].busy);
    }

    /// Play on rows whose queued start sits on an idle session plays them now; Play with every
    /// row still starting says so instead of doing nothing; Cancel clears a start.
    #[test]
    fn play_releases_stale_queues_reports_starting_rows_and_cancel_clears() {
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        app.launches = Some(Vec::new());
        let key = signed_in(&mut app, 0);
        for row in &mut app.accounts { row.selected = row.key == key; }
        app.accounts[0].queued_play = Some(Instant::now());
        app.play_from_home("main", true);
        assert!(app.accounts[0].queued_play.is_none());
        assert_eq!(app.launches.as_ref().unwrap().len(), 1, "the stale queue played");
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key.clone()]);
        // A batch claim is not stale: Play says the account is still starting.
        {
            let row = &mut app.accounts[0];
            (row.busy, row.batch_pending, row.batch_requested) = (true, true, true);
        }
        app.status.clear();
        app.play_from_home("main", true);
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
        assert_eq!(app.status, tf("{count} accounts are still starting.", &[("count", &1)]));
        app.play_row(key.clone());
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
        // Cancel shows once the start has waited, and clears it.
        app.watch_starts(Instant::now());
        assert!(!app.start_cancellable(&key));
        if let Some(entry) = app.start_watch.rows.get_mut(&key) { entry.starting_since = Some(Instant::now() - CANCEL_AFTER); }
        assert!(app.start_cancellable(&key));
        app.clear_start(&key, true);
        let row = &app.accounts[0];
        assert!(row.can_play() && !row.problem && row.status == t("Start cancelled."));
    }

    #[test]
    fn flag_changes_name_only_what_changed() {
        let before = Flags { busy: true, preparing: true, ..Flags::default() };
        let after = Flags { busy: true, queued: true, ..Flags::default() };
        assert_eq!(before.changes(after), "preparing 1→0, queued 0→1");
    }
}
