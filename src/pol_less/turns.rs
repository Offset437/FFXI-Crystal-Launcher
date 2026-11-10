//! Which injector starts may run at the same time: the in-process gate tests give
//! `start_host` (`TurnGate`). Each session worker starts one tool, so production uses the
//! machine-wide lock files instead (`session_worker::windower_turn_at`,
//! `session_worker::ashita_turn_at`). Only the tool start is gated, and only as far as the
//! tools need:
//!
//! - **Windower 4**: its injection handshake writes settings into the fixed-name file mapping
//!   `WindowerMMFSettingsHandler`, creates the fixed-name auto-reset event
//!   `windower.signal-injection`, injects Hook.dll, waits for Hook.dll to signal, disposes both,
//!   and only then resumes the game's main thread. Two handshakes at once could swap settings
//!   blocks or take each other's signal, and Windower.exe shows nothing outside between the
//!   dispose and the resume. The first thing observable after it is the loader's `hello`, which
//!   the loader sends about 1 ms into its `main`. So one Windower start at a time, from spawn
//!   to hello, is the whole handshake, with no margin on top.
//! - Windower also writes its profile's video and sound values into the shared
//!   `HKLM\...\FinalFantasyXI` key before every start. That no longer orders anything: the
//!   launcher sends the same values as `ffxi_settings` and crystal-loader serves them to each
//!   game in-process, so a start with another profile goes
//!   as soon as the previous start's hello, like one with the same profile.
//! - **Ashita v4**: Ashita-cli itself uses no fixed object names and each account has its own
//!   generated boot ini, but `InstallAshita` loads the POL plugins inside every game host's
//!   start-up, and those share files in Ashita's folder: pivot reads and rewrites
//!   `config\pivot\pivot.ini` as it loads. Live 2026-11-10 (and 11-03, 11-09): five hosts started
//!   within the same millisecond, pivot threw "an unknown exception" in two, and those hosts
//!   ended before their loader ran; Ashita-cli still said "injected and installed". Every
//!   loader timeout on record matches such a pivot error in Ashita's log. So one Ashita start at
//!   a time, from spawn to hello (about 0.2 s each), in a turn of its own: Windower and Ashita
//!   starts never wait for each other.
//! - **Vanilla**: each account starts its own host and private pipe directly, with no injector.

use super::Tool;
use crate::i18n::t;
use std::sync::{Condvar, Mutex, MutexGuard};

pub(crate) struct Turns {
    /// The tools with a start between spawn and hello.
    busy: Mutex<Vec<Tool>>,
    changed: Condvar,
}

impl Turns {
    pub(crate) const fn new() -> Self {
        Self { busy: Mutex::new(Vec::new()), changed: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Tool>> {
        self.busy.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait until this start may spawn its tool. Vanilla never waits.
    pub(crate) fn enter(&'static self, tool: Tool, progress: &mut dyn FnMut(&str)) -> Turn {
        if tool == Tool::Vanilla {
            return Turn { turns: None, tool };
        }
        let mut busy = self.lock();
        if busy.contains(&tool) {
            progress(match tool {
                Tool::Ashita => t("Waiting for the previous Ashita start to reach the game..."),
                _ => t("Waiting for the previous Windower start to reach the game..."),
            });
        }
        while busy.contains(&tool) {
            busy = self.changed.wait(busy).unwrap_or_else(|p| p.into_inner());
        }
        busy.push(tool);
        Turn { turns: Some(self), tool }
    }
}

/// The right to run one start's handshake. Dropping it (at the loader's hello, or on a
/// failed start) lets the next start of the same tool go.
pub(crate) struct Turn {
    turns: Option<&'static Turns>,
    tool: Tool,
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(turns) = self.turns.take() {
            turns.lock().retain(|busy| *busy != self.tool);
            turns.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn turns() -> &'static Turns {
        Box::leak(Box::new(Turns::new()))
    }

    /// Enter on another thread; the flag turns true once it has the turn.
    fn enter_later(turns: &'static Turns, tool: Tool) -> (Arc<AtomicBool>, std::thread::JoinHandle<Turn>) {
        let entered = Arc::new(AtomicBool::new(false));
        let flag = entered.clone();
        let handle = std::thread::spawn(move || {
            let turn = turns.enter(tool, &mut |_| {});
            flag.store(true, Ordering::SeqCst);
            turn
        });
        (entered, handle)
    }

    #[test]
    fn vanilla_starts_never_wait_and_ashita_never_waits_for_windower() {
        let turns = turns();
        let _windower = turns.enter(Tool::Windower, &mut |_| {});
        let a = turns.enter(Tool::Vanilla, &mut |_| panic!("must not wait for Windower"));
        let b = turns.enter(Tool::Vanilla, &mut |_| panic!("must not wait for another host"));
        let ashita = turns.enter(Tool::Ashita, &mut |_| panic!("Ashita has a turn of its own"));
        drop((a, b, ashita));
    }

    #[test]
    fn windower_handshakes_run_one_at_a_time_and_end_at_hello() {
        for tool in [Tool::Windower, Tool::Ashita] {
            let turns = turns();
            let mut said = Vec::new();
            let first = turns.enter(tool, &mut |s| said.push(s.to_owned()));
            assert!(said.is_empty(), "the first start doesn't wait");
            let (entered, second) = enter_later(turns, tool);
            std::thread::sleep(Duration::from_millis(150));
            assert!(!entered.load(Ordering::SeqCst), "a second {} start waits for the first", tool.name());
            // The hello: the first start's tool may well still be running.
            drop(first);
            let second = second.join().unwrap();
            assert!(entered.load(Ordering::SeqCst));
            drop(second);
        }
    }
}
