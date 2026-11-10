//! The window's logic Tick, delivered only when it can do something. Every message
//! syncs the view and can redraw the window, so an idle launcher that ticked at 1 Hz drew a
//! frame a second for nothing (most of its idle GPU time). While the window reports itself
//! quiet (`set_quiet`: no sign-in, wait, park start or other timed work pending), the timer
//! still runs at its usual rate but a Tick is only sent when
//! * a worker event arrived (`EventSender::send` counts them), or
//! * a watched session flag changed (a game or session started or ended), or
//! * the window's next timed work is due (the official status read every few minutes,
//!   parked hosts' fingerprints), or
//! * SAFETY_NET passed since the last one (files another process wrote, a parked host
//!   that ended on its own).
//!
//! Nothing that ran on a Tick before is skipped for good, and a busy window ticks exactly as
//! it did.
use crate::WorkerEvent;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// The longest a quiet window goes without a Tick.
pub(crate) const SAFETY_NET: Duration = Duration::from_secs(60);

static QUIET: AtomicBool = AtomicBool::new(false);
static EVENTS: AtomicU64 = AtomicU64::new(0);
struct Watch { flags: Vec<(Arc<AtomicBool>, bool)>, due: Option<Instant> }
static WATCHED: Mutex<Watch> = Mutex::new(Watch { flags: Vec::new(), due: None });

/// After every update: whether the window is quiet, the session flags (with their current
/// values) whose change needs a Tick at once, and when its next timed work is due.
pub(crate) fn set_quiet(quiet: bool, flags: Vec<(Arc<AtomicBool>, bool)>, due: Option<Instant>) {
    QUIET.store(quiet, Ordering::Release);
    if let Ok(mut watch) = WATCHED.lock() { *watch = Watch { flags, due }; }
}

fn watched_changed() -> bool {
    WATCHED.lock().is_ok_and(|watch| watch.flags.iter().any(|(flag, seen)| flag.load(Ordering::Acquire) != *seen))
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
/// shared runtime: `deliver` sends one Tick, and the loop ends once it returns false (the
/// front end restarts it whenever the rate changes). Front-end independent.
pub(crate) async fn run<F, D>(every: Duration, mut deliver: F)
where
    F: FnMut() -> D,
    D: std::future::Future<Output = bool>,
{
    let mut gate = Gate::new(Instant::now());
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        let quiet = QUIET.load(Ordering::Acquire);
        if gate.due(Instant::now(), quiet, EVENTS.load(Ordering::Acquire), quiet && watched_changed(), if quiet { next_due() } else { None })
            && !deliver().await {
            return;
        }
    }
}

/// The worker-event channel's sending half: also wakes a quiet window's next Tick.
#[derive(Clone)]
pub(crate) struct EventSender(mpsc::Sender<WorkerEvent>);

impl EventSender {
    pub(crate) fn new(sender: mpsc::Sender<WorkerEvent>) -> Self { Self(sender) }
    pub(crate) fn send(&self, event: WorkerEvent) -> Result<(), mpsc::SendError<WorkerEvent>> {
        let result = self.0.send(event);
        EVENTS.fetch_add(1, Ordering::AcqRel);
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
        set_quiet(false, vec![(flag.clone(), true)], None);
        assert!(!watched_changed());
        flag.store(false, Ordering::Release);
        assert!(watched_changed());
        set_quiet(false, Vec::new(), None);
    }
}
