//! The controller's navigation commands: D-pad and stick moves with repeat, and the face,
//! shoulder and Start buttons, queued for the window to carry out (slint_host::pad).
//! No global mouse/key injection and no game input hooks.
use crate::gamepad::detect::{Btn, Input, Pad};
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
    Left,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Move(Direction),
    Select,
    Back,
    Page(i8),
    Play,
}
#[derive(Default)]
struct InputState {
    device: Option<usize>,
    previous: Input,
    direction: Option<Direction>,
    repeat_at: Option<Instant>,
}
#[derive(Default)]
pub(crate) struct State {
    /// Commands not carried out yet, oldest first.
    pending: Mutex<VecDeque<Command>>,
    input: InputState,
}
impl State {
    fn pending(&self) -> std::sync::MutexGuard<'_, VecDeque<Command>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn clear(&mut self) {
        self.pending().clear();
        self.input.repeat_at = None;
    }
    pub(crate) fn repeating(&self) -> bool {
        self.input.repeat_at.is_some()
    }
    pub(crate) fn poll(&mut self, pad: Option<&Pad>, allowed: bool, now: Instant) -> bool {
        let commands = self.input.poll(pad.map(|p| (p.id, p.input)), allowed, now);
        let active = !commands.is_empty();
        let mut pending = self.pending();
        if !allowed || pad.is_none() {
            pending.clear();
        }
        for command in commands {
            if pending.len() < 8 {
                pending.push_back(command);
            }
        }
        active
    }
    /// The queued commands, oldest first.
    pub(crate) fn take_pending(&self) -> Vec<Command> {
        self.pending().drain(..).collect()
    }
    pub(crate) fn preview_focus(&mut self) {
        self.pending().push_back(Command::Move(Direction::Down));
    }
}
fn direction(input: Input, previous: Option<Direction>) -> Option<Direction> {
    if input.pressed(Btn::DUp) {
        return Some(Direction::Up);
    }
    if input.pressed(Btn::DDown) {
        return Some(Direction::Down);
    }
    if input.pressed(Btn::DLeft) {
        return Some(Direction::Left);
    }
    if input.pressed(Btn::DRight) {
        return Some(Direction::Right);
    }
    let threshold = if previous.is_some() { 0.35 } else { 0.55 };
    if input.lx.abs().max(input.ly.abs()) < threshold {
        return None;
    }
    if input.lx.abs() > input.ly.abs() {
        Some(if input.lx < 0.0 {
            Direction::Left
        } else {
            Direction::Right
        })
    } else {
        Some(if input.ly > 0.0 {
            Direction::Up
        } else {
            Direction::Down
        })
    }
}
impl InputState {
    fn poll(&mut self, pad: Option<(usize, Input)>, allowed: bool, now: Instant) -> Vec<Command> {
        let Some((device, input)) = pad else {
            *self = Self::default();
            return Vec::new();
        };
        if !allowed || self.device != Some(device) {
            self.device = Some(device);
            self.previous = input;
            self.direction = direction(input, None);
            self.repeat_at = None;
            return Vec::new();
        }
        let mut output = Vec::new();
        let pressed = input.buttons & !self.previous.buttons;
        self.previous = input;
        for (button, command) in [
            (Btn::South, Command::Select),
            (Btn::East, Command::Back),
            (Btn::LB, Command::Page(-1)),
            (Btn::RB, Command::Page(1)),
            (Btn::Start, Command::Play),
        ] {
            if pressed & button.bit() != 0 {
                output.push(command);
            }
        }
        let next = direction(input, self.direction);
        if next != self.direction {
            self.direction = next;
            self.repeat_at = next.map(|_| now + Duration::from_millis(360));
            if let Some(next) = next {
                output.push(Command::Move(next));
            }
        } else if let (Some(next), Some(at)) = (next, self.repeat_at) {
            if now >= at {
                output.push(Command::Move(next));
                self.repeat_at = Some(now + Duration::from_millis(100));
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn held_buttons_on_connect_or_focus_gain_never_activate_controls() {
        let mut state = InputState::default();
        let now = Instant::now();
        let held = Input {
            buttons: Btn::South.bit(),
            ..Default::default()
        };
        assert!(state.poll(Some((1, held)), true, now).is_empty());
        assert!(state.poll(Some((1, held)), true, now).is_empty());
        state.poll(Some((1, Input::default())), true, now);
        assert_eq!(state.poll(Some((1, held)), true, now), [Command::Select]);
        assert!(state.poll(Some((1, held)), false, now).is_empty());
        assert!(state.poll(Some((1, held)), true, now).is_empty());
    }
    #[test]
    fn only_directional_input_repeats_and_stick_noise_is_ignored() {
        let mut state = InputState::default();
        let now = Instant::now();
        state.poll(Some((1, Input::default())), true, now);
        let held = Input {
            buttons: Btn::DDown.bit() | Btn::South.bit(),
            ..Default::default()
        };
        assert_eq!(
            state.poll(Some((1, held)), true, now),
            [Command::Select, Command::Move(Direction::Down)]
        );
        assert!(state
            .poll(Some((1, held)), true, now + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            state.poll(Some((1, held)), true, now + Duration::from_millis(400)),
            [Command::Move(Direction::Down)]
        );
        assert_eq!(
            direction(
                Input {
                    lx: 0.2,
                    ly: -0.1,
                    ..Default::default()
                },
                None
            ),
            None
        );
        assert_eq!(
            direction(
                Input {
                    lx: -0.8,
                    ..Default::default()
                },
                None
            ),
            Some(Direction::Left)
        );
    }

    #[test]
    fn a_disconnected_controller_drops_its_pending_input() {
        let mut state = State::default();
        state.pending().push_back(Command::Select);
        assert!(!state.poll(None, true, Instant::now()));
        assert!(state.take_pending().is_empty());
    }
}
