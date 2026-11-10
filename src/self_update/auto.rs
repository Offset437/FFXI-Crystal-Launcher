//! When Crystal restarts by itself to install a downloaded update: the setting is on, the
//! update is ready, Crystal could restart without losing work, no game runs or is starting,
//! no dialog is open, and nobody is using the window (hidden, unfocused, or no input for
//! IDLE_AFTER). The restart is announced for COUNTDOWN first; "Not now" holds it off for
//! POSTPONE or until the next start. Plain data and decisions: `App` gathers the conditions on
//! its existing events and runs the step.
use std::time::{Duration, Instant};

pub(crate) const COUNTDOWN: Duration = Duration::from_secs(10);
pub(crate) const IDLE_AFTER: Duration = Duration::from_secs(60);
pub(crate) const POSTPONE: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Conditions {
    /// Settings → System → "Restart automatically to install updates".
    pub enabled: bool,
    /// A verified update is downloaded (`State::Ready`).
    pub ready: bool,
    /// `App::can_restart_launcher`: no sign-in, operation, update or launch work in flight.
    pub can_restart: bool,
    /// A game runs or is starting.
    pub game_running: bool,
    /// A dialog or the account sheet is open (its edits would be lost).
    pub dialog: bool,
    /// The window is shown and focused and had input within IDLE_AFTER.
    pub user_active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Wait,
    /// The countdown started.
    Scheduled,
    /// A running countdown stopped: a game started, work began or the update went away.
    Cancelled,
    /// The countdown ran out: install and restart now.
    Apply,
}

#[derive(Debug, Default)]
pub(crate) struct AutoRestart {
    deadline: Option<Instant>,
    postponed_until: Option<Instant>,
    /// The window has the keyboard focus (winit's Focused events).
    pub focused: bool,
    /// The last keyboard or pointer input in the window (`Message::UserInput`).
    pub last_input: Option<Instant>,
}

impl AutoRestart {
    pub(crate) fn user_active(&self, shown: bool, now: Instant) -> bool {
        shown && self.focused && self.last_input.is_some_and(|at| now.saturating_duration_since(at) < IDLE_AFTER)
    }

    pub(crate) fn postponed(&self, now: Instant) -> bool {
        self.postponed_until.is_some_and(|until| now < until)
    }

    /// Whole seconds left on the countdown, while it runs.
    pub(crate) fn remaining(&self, now: Instant) -> Option<u64> {
        self.deadline.map(|at| at.saturating_duration_since(now).as_millis().div_ceil(1000) as u64)
    }

    /// Input does not stop a countdown already shown (the pointer moving to its "Not now" is
    /// input); it only keeps a new one from starting.
    pub(crate) fn step(&mut self, c: Conditions, now: Instant) -> Step {
        let possible = c.enabled && c.ready && c.can_restart && !c.game_running && !c.dialog && !self.postponed(now);
        match self.deadline {
            None if possible && !c.user_active => {
                self.deadline = Some(now + COUNTDOWN);
                Step::Scheduled
            }
            None => Step::Wait,
            Some(_) if !possible => {
                self.deadline = None;
                Step::Cancelled
            }
            Some(at) if now >= at => {
                self.deadline = None;
                Step::Apply
            }
            Some(_) => Step::Wait,
        }
    }

    /// "Not now": no restart until the next start, or POSTPONE from now.
    pub(crate) fn postpone(&mut self, now: Instant) {
        self.deadline = None;
        self.postponed_until = Some(now + POSTPONE);
    }

    /// When the decision can change with no event to say so: the countdown's next second (its
    /// label counts down), a postponement's end, or the end of the input grace. `None` with no
    /// update pending, so an idle Crystal keeps no timer for this.
    pub(crate) fn next_due(&self, c: Conditions, now: Instant) -> Option<Instant> {
        if !(c.enabled && c.ready) {
            return None;
        }
        if let Some(at) = self.deadline {
            return Some(at.min(now + Duration::from_secs(1)));
        }
        if let Some(until) = self.postponed_until.filter(|until| now < *until) {
            return Some(until);
        }
        if c.user_active {
            return self.last_input.map(|at| at + IDLE_AFTER);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Conditions {
        Conditions { enabled: true, ready: true, can_restart: true, game_running: false, dialog: false, user_active: false }
    }

    #[test]
    fn ready_with_no_game_and_nobody_at_the_window_schedules_then_applies() {
        let now = Instant::now();
        let mut auto = AutoRestart::default();
        assert_eq!(auto.step(idle(), now), Step::Scheduled);
        assert_eq!(auto.remaining(now), Some(10));
        assert_eq!(auto.next_due(idle(), now), Some(now + Duration::from_secs(1)));
        assert_eq!(auto.step(idle(), now + Duration::from_secs(5)), Step::Wait);
        assert_eq!(auto.remaining(now + Duration::from_millis(5500)), Some(5));
        assert_eq!(auto.step(idle(), now + COUNTDOWN), Step::Apply);
        assert_eq!(auto.remaining(now + COUNTDOWN), None);
    }

    #[test]
    fn a_running_game_waits_and_its_end_schedules() {
        let now = Instant::now();
        let mut auto = AutoRestart::default();
        let playing = Conditions { game_running: true, ..idle() };
        assert_eq!(auto.step(playing, now), Step::Wait);
        assert_eq!(auto.next_due(playing, now), None, "no timer while a game runs: its end is an event");
        assert_eq!(auto.step(idle(), now + Duration::from_secs(3600)), Step::Scheduled, "the game ended");
        // A game that starts during the countdown stops it.
        assert_eq!(auto.step(playing, now + Duration::from_secs(3601)), Step::Cancelled);
        assert_eq!(auto.remaining(now), None);
        for busy in [Conditions { can_restart: false, ..idle() }, Conditions { dialog: true, ..idle() }] {
            let mut auto = AutoRestart::default();
            assert_eq!(auto.step(busy, now), Step::Wait);
        }
    }

    #[test]
    fn an_active_user_waits_until_idle_but_input_does_not_stop_a_shown_countdown() {
        let now = Instant::now();
        let mut auto = AutoRestart { focused: true, last_input: Some(now), ..Default::default() };
        assert!(auto.user_active(true, now));
        assert!(!auto.user_active(false, now), "hidden in the tray is idle");
        let active = Conditions { user_active: true, ..idle() };
        assert_eq!(auto.step(active, now), Step::Wait);
        assert_eq!(auto.next_due(active, now), Some(now + IDLE_AFTER));
        let later = now + IDLE_AFTER;
        assert!(!auto.user_active(true, later), "no input for a minute");
        auto.focused = false;
        assert!(!auto.user_active(true, now), "unfocused or minimised");
        assert_eq!(auto.step(idle(), later), Step::Scheduled);
        assert_eq!(auto.step(active, later + Duration::from_secs(2)), Step::Wait, "moving to Not now is input");
        assert_eq!(auto.step(active, later + COUNTDOWN), Step::Apply);
    }

    #[test]
    fn not_now_postpones_for_four_hours() {
        let now = Instant::now();
        let mut auto = AutoRestart::default();
        assert_eq!(auto.step(idle(), now), Step::Scheduled);
        auto.postpone(now + Duration::from_secs(2));
        assert_eq!(auto.remaining(now), None);
        assert_eq!(auto.step(idle(), now + Duration::from_secs(3)), Step::Wait);
        let until = now + Duration::from_secs(2) + POSTPONE;
        assert_eq!(auto.next_due(idle(), now + Duration::from_secs(3)), Some(until));
        assert_eq!(auto.step(idle(), until - Duration::from_secs(1)), Step::Wait);
        assert_eq!(auto.step(idle(), until), Step::Scheduled);
        // The next start begins with a fresh state: the postponement is not saved.
        assert!(!AutoRestart::default().postponed(now));
    }

    #[test]
    fn the_setting_off_never_schedules() {
        let now = Instant::now();
        let mut auto = AutoRestart::default();
        let off = Conditions { enabled: false, ..idle() };
        assert_eq!(auto.step(off, now), Step::Wait);
        assert_eq!(auto.next_due(off, now), None);
        assert_eq!(auto.step(idle(), now), Step::Scheduled);
        assert_eq!(auto.step(off, now + Duration::from_secs(1)), Step::Cancelled, "turned off during the countdown");
        let none = Conditions { ready: false, ..idle() };
        assert_eq!(auto.step(none, now), Step::Wait);
    }
}
