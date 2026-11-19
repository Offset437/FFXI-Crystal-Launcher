//! The window's logic Tick, delivered only when it can do something. Every message
//! syncs the view and can redraw the window, so an idle launcher that ticked at 1 Hz drew a
//! frame a second for nothing (most of its idle GPU time). While the window reports itself
//! quiet (`set_quiet`: no sign-in, wait, park start or other timed work pending), a Tick is
//! only sent when
//! * a worker event arrived (`EventSender::send` counts them), or
//! * a watched session flag changed (a game or session started or ended), or
//! * the opt-in native foreground check changed, or
//! * the window's next timed work is due (the official status read every few minutes,
//!   parked hosts' fingerprints), or
//! * SAFETY_NET passed since the last one (files another process wrote, a parked host
//!   that ended on its own).
//!
//! Opt-in background mode wakes immediately for worker events and deadlines, with a slower
//! sweep for flags and files that do not send notifications.
use crate::{window_chrome, WorkerEvent};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// The longest a quiet window goes without a Tick.
pub(crate) const SAFETY_NET: Duration = Duration::from_secs(60);
pub(crate) const BACKGROUND_INTERVAL: Duration = Duration::from_secs(2);

static QUIET: AtomicBool = AtomicBool::new(false);
static EVENTS: AtomicU64 = AtomicU64::new(0);
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
struct Watch {
    flags: Vec<(Arc<AtomicBool>, bool)>,
    due: Option<Instant>,
    activation: Option<window_chrome::Activation>,
}
static WATCHED: Mutex<Watch> = Mutex::new(Watch { flags: Vec::new(), due: None, activation: None });

/// After every update: whether the window is quiet, the session flags (with their current
/// values) whose change needs a Tick, when its next timed work is due, and the last
/// native activation state to compare on timer turns (if background savings are on).
pub(crate) fn set_quiet(
    quiet: bool,
    flags: Vec<(Arc<AtomicBool>, bool)>,
    due: Option<Instant>,
    activation: Option<window_chrome::Activation>,
) {
    QUIET.store(quiet, Ordering::Release);
    if let Ok(mut watch) = WATCHED.lock() {
        let deadline_changed = watch.due != due;
        *watch = Watch { flags, due, activation };
        if deadline_changed { WAKE.notify_waiters(); }
    }
}

fn watched_changed() -> bool {
    let Ok(watch) = WATCHED.lock() else { return false };
    if watch.flags.iter().any(|(flag, seen)| flag.load(Ordering::Acquire) != *seen) { return true; }
    let activation = watch.activation;
    drop(watch);
    activation.is_some_and(|seen| window_chrome::activation() != seen)
}

fn next_due() -> Option<Instant> { WATCHED.lock().ok().and_then(|watch| watch.due) }

/// The gate's decision for one timer turn.
#[derive(Debug)]
pub(crate) struct Gate {
    events_seen: u64,
    last: Instant,
}

impl Gate {
    /// A new stream's gate: its first turn always ticks. The host restarts the stream whenever
    /// the tick rate changes, and a worker event sent between the last drain and the restart
    /// is already counted; a gate that started from the live counter would treat it as seen
    /// and leave it queued until the safety net.
    pub(crate) fn new(now: Instant) -> Self { Self { events_seen: u64::MAX, last: now } }

    /// `work`: when the window's next timed work is due; once a Tick went after it, that
    /// time no longer wakes the window (work that did not move it waits for the safety net).
    pub(crate) fn due(&mut self, now: Instant, quiet: bool, events: u64, flags_changed: bool, work: Option<Instant>) -> bool {
        let due = !quiet || events != self.events_seen || flags_changed
            || work.is_some_and(|at| now >= at && self.last < at)
            || now.saturating_duration_since(self.last) >= SAFETY_NET;
        if due {
            self.events_seen = events;
            self.last = now;
        }
        due
    }
}

/// The Tick timer at `every` (the window's tick rate: fast while busy, 1 s idle), on the
/// shared runtime: `deliver` sends one Tick, and the loop ends once it returns false or`alive`
/// does (checked first thing every turn: the front end starts a new loop whenever the rate
/// changes, and the superseded one must not linger). Front-end independent.
pub(crate) async fn run<F, D>(every: Duration, alive: impl Fn() -> bool, mut deliver: F)
where
    F: FnMut() -> D,
    D: std::future::Future<Output = bool>,
{
    let mut gate = Gate::new(Instant::now());
    if every == BACKGROUND_INTERVAL {
        run_background(&mut gate, alive, deliver).await;
        return;
    }
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        if !alive() { return; }
        let quiet = QUIET.load(Ordering::Acquire);
        if gate.due(Instant::now(), quiet, EVENTS.load(Ordering::Acquire), quiet && watched_changed(), if quiet { next_due() } else { None })
            && !deliver().await {
            return;
        }
    }
}

async fn run_background<F, D>(gate: &mut Gate, alive: impl Fn() -> bool, mut deliver: F)
where
    F: FnMut() -> D,
    D: std::future::Future<Output = bool>,
{
    let mut sweep = Instant::now() + BACKGROUND_INTERVAL;
    loop {
        let wake = WAKE.notified();
        if !alive() { return; }
        let now = Instant::now();
        let events = EVENTS.load(Ordering::Acquire);
        let deadline = next_due().filter(|at| *at > gate.last);
        if events != gate.events_seen || now >= sweep || deadline.is_some_and(|at| now >= at) {
            sweep = now + BACKGROUND_INTERVAL;
            if gate.due(now, QUIET.load(Ordering::Acquire), events, watched_changed(), deadline)
                && !deliver().await {
                return;
            }
        } else {
            let until = deadline.map_or(sweep, |at| at.min(sweep));
            tokio::select! {
                _ = wake => {},
                _ = tokio::time::sleep_until(until.into()) => {},
            }
        }
    }
}

pub(crate) fn notify_session_change() {
    EVENTS.fetch_add(1, Ordering::AcqRel);
    WAKE.notify_waiters();
}

/// The worker-event channel's sending half: also wakes a quiet window's next Tick.
#[derive(Clone)]
pub(crate) struct EventSender(mpsc::Sender<WorkerEvent>);

impl EventSender {
    pub(crate) fn new(sender: mpsc::Sender<WorkerEvent>) -> Self { Self(sender) }
    pub(crate) fn send(&self, event: WorkerEvent) -> Result<(), mpsc::SendError<WorkerEvent>> {
        let result = self.0.send(event);
        if result.is_ok() { notify_session_change(); }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_window_ticks_every_turn() {
        let start = Instant::now();
        let mut gate = Gate::new(start);
        for turn in 1..5 { assert!(gate.due(start + Duration::from_secs(turn), false, 0, false, None)); }
    }

    #[test]
    fn a_restarted_stream_ticks_once_even_when_quiet() {
        let start = Instant::now();
        let mut gate = Gate::new(start);
        let events = EVENTS.load(Ordering::Acquire);
        assert!(gate.due(start + Duration::from_secs(1), true, events, false, None), "drains an event counted before the restart");
        assert!(!gate.due(start + Duration::from_secs(2), true, events, false, None));
    }

    #[test]
    fn a_quiet_window_ticks_on_events_flag_changes_and_the_safety_net_only() {
        let start = Instant::now();
        let mut gate = Gate { events_seen: 7, last: start };
        let at = |s: u64| start + Duration::from_secs(s);
        assert!(!gate.due(at(1), true, 7, false, None));
        assert!(!gate.due(at(2), true, 7, false, None));
        assert!(gate.due(at(3), true, 8, false, None), "a worker event");
        assert!(!gate.due(at(4), true, 8, false, None), "seen");
        assert!(gate.due(at(5), true, 8, true, None), "a game ended");
        assert!(!gate.due(at(6), true, 8, false, None));
        let net = SAFETY_NET.as_secs();
        assert!(!gate.due(at(4 + net), true, 8, false, None));
        assert!(gate.due(at(5 + net), true, 8, false, None), "the safety net, SAFETY_NET after the last Tick");
        assert!(!gate.due(at(6 + net), true, 8, false, None));
        assert!(gate.due(at(7 + net), false, 8, false, None), "busy again");
    }

    #[test]
    fn timed_work_wakes_a_quiet_window_once_when_due() {
        let start = Instant::now();
        let mut gate = Gate { events_seen: 0, last: start };
        let at = |s: u64| start + Duration::from_secs(s);
        let work = Some(at(5));
        assert!(!gate.due(at(4), true, 0, false, work));
        assert!(gate.due(at(5), true, 0, false, work), "the status read is due");
        // The work did not move its time (it could not start yet): no Tick a second.
        assert!(!gate.due(at(6), true, 0, false, work));
        assert!(!gate.due(at(7), true, 0, false, work));
        assert!(gate.due(at(9), true, 0, false, Some(at(9))), "the next one");
    }

    #[tokio::test]
    async fn a_worker_event_wakes_background_ticks_before_the_next_sweep() {
        let (events, _receiver) = mpsc::channel();
        let sender = EventSender::new(events);
        let (first, ready) = tokio::sync::oneshot::channel();
        let mut first = Some(first);
        let task = tokio::spawn(run(BACKGROUND_INTERVAL, || true, move || {
            let first = first.take();
            async move { first.is_some_and(|first| first.send(()).is_ok()) }
        }));
        ready.await.unwrap();
        sender.send(WorkerEvent::Notice("background wake".into())).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task).await.expect("worker wake before the two-second sweep").unwrap();
    }

    #[test]
    fn sending_an_event_counts_it() {
        let (sender, receiver) = mpsc::channel();
        let sender = EventSender::new(sender);
        let before = EVENTS.load(Ordering::Acquire);
        sender.send(WorkerEvent::Notice("x".into())).unwrap();
        assert!(EVENTS.load(Ordering::Acquire) > before);
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn a_watched_flag_that_changed_is_seen() {
        let flag = Arc::new(AtomicBool::new(true));
        // Only this test sets the watch list; other tests read the counters only.
        set_quiet(false, vec![(flag.clone(), true)], None, None);
        assert!(!watched_changed());
        flag.store(false, Ordering::Release);
        assert!(watched_changed());
        set_quiet(false, Vec::new(), None, None);
    }
}
