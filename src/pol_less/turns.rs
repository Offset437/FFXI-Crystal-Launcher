//! Which injector starts may run at the same time: the in-process gate tests give
//! `start_host` (`TurnGate`). Each session worker starts one tool, so production uses the
//! machine-wide Windower lock instead (`session_worker::windower_turn_at`). Only the tool start
//! is gated, and only as far as the tools need:
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
//! - **Ashita v4**: Ashita-cli imports no mutex, event or file-mapping APIs and uses no
//!   fixed object names; it hands `InstallAshita` its parameters in the target process's own
//!   memory, and each account has its own generated boot ini. Ashita starts run in parallel.
//! - **Vanilla**: each account starts its own host and private pipe directly, with no injector.

use super::Tool;
use crate::i18n::t;
use std::sync::{Condvar, Mutex, MutexGuard};

pub(crate) struct Turns {
    /// A Windower start is between spawn and hello.
    handshake: Mutex<bool>,
    changed: Condvar,
}

impl Turns {
    pub(crate) const fn new() -> Self {
        Self { handshake: Mutex::new(false), changed: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.handshake.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait until this start may spawn its tool. Ashita and Vanilla never wait.
    pub(crate) fn enter(&'static self, tool: Tool, progress: &mut dyn FnMut(&str)) -> Turn {
        if tool != Tool::Windower {
            return Turn { turns: None };
        }
        let mut busy = self.lock();
        if *busy {
            progress(t("Waiting for the previous Windower start to reach the game..."));
        }
        while *busy {
            busy = self.changed.wait(busy).unwrap_or_else(|p| p.into_inner());
        }
        *busy = true;
        Turn { turns: Some(self) }
    }
}

/// The right to run one Windower start's handshake. Dropping it (at the loader's hello, or on a
/// failed start) lets the next start go.
pub(crate) struct Turn {
    turns: Option<&'static Turns>,
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(turns) = self.turns.take() {
            *turns.lock() = false;
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
    fn ashita_and_vanilla_starts_never_wait() {
        let turns = turns();
        let _windower = turns.enter(Tool::Windower, &mut |_| {});
        for tool in [Tool::Ashita, Tool::Vanilla] {
            let a = turns.enter(tool, &mut |_| panic!("must not wait for Windower"));
            let b = turns.enter(tool, &mut |_| panic!("must not wait for another host"));
            drop((a, b));
        }
    }

    #[test]
    fn windower_handshakes_run_one_at_a_time_and_end_at_hello() {
        let turns = turns();
        let mut said = Vec::new();
        let first = turns.enter(Tool::Windower, &mut |s| said.push(s.to_owned()));
        assert!(said.is_empty(), "the first start doesn't wait");
        let (entered, second) = enter_later(turns, Tool::Windower);
        std::thread::sleep(Duration::from_millis(150));
        assert!(!entered.load(Ordering::SeqCst), "a second handshake waits for the first");
        // The hello: the first start's Windower.exe may well still be running.
        drop(first);
        let second = second.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
        drop(second);
    }
}
