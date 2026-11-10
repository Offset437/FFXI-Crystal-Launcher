//! The core's background jobs: Windows components detection and installs (`prereqs::run`) and
//! the dgVoodoo download (`dgvoodoo::run`).
//! After each batch the host lists what the core wants running (`wanted`); `Jobs::set` starts
//! each new one on the shared runtime and stops each one no longer listed,
//! and every message they produce goes back to the UI thread in order.
use crate::{App, Message};
use futures_util::StreamExt;
use std::collections::HashMap;
use tokio::task::AbortHandle;

/// One job, by the identity that keeps it running across batches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Key {
    Prereqs(crate::prereqs::Work),
    Dgvoodoo(u64),
}

/// What the core wants running now.
pub(super) fn wanted(app: &App) -> Vec<Key> {
    let mut keys: Vec<Key> = crate::prereqs::work(&app.prereqs).into_iter().map(Key::Prereqs).collect();
    keys.extend(crate::dgvoodoo::work(&app.dgvoodoo).map(Key::Dgvoodoo));
    keys
}

/// The running jobs.
#[derive(Default)]
pub(super) struct Jobs {
    running: HashMap<Key, AbortHandle>,
}

impl Jobs {
    /// Starts each wanted job not running yet (with `start`) and stops each one no longer
    /// wanted. A job that finished on its own stays listed until it is no longer wanted, so it
    /// is not started again.
    pub(super) fn set(&mut self, wanted: &[Key], mut start: impl FnMut(Key) -> AbortHandle) {
        self.running.retain(|key, job| {
            let keep = wanted.contains(key);
            if !keep { job.abort(); }
            keep
        });
        for key in wanted {
            self.running.entry(*key).or_insert_with(|| start(*key));
        }
    }

    /// Stops every job (the core is exiting).
    pub(super) fn stop(&mut self) { self.set(&[], |_| unreachable!()); }
}

/// Runs `key` on the shared runtime; `post` takes each message it produces, in order.
pub(super) fn start(key: Key, post: fn(Message)) -> AbortHandle {
    crate::runtime::get()
        .spawn(async move {
            match key {
                Key::Prereqs(work) => {
                    let mut messages = crate::prereqs::run(work);
                    while let Some(message) = messages.next().await { post(Message::Prereqs(message)); }
                }
                Key::Dgvoodoo(id) => {
                    let mut messages = std::pin::pin!(crate::dgvoodoo::run(id));
                    while let Some(message) = messages.next().await { post(message); }
                }
            }
        })
        .abort_handle()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prereqs::Work;

    fn idle() -> AbortHandle { crate::runtime::get().spawn(std::future::pending::<()>()).abort_handle() }

    #[test]
    fn each_job_starts_once_and_stops_when_no_longer_wanted() {
        let mut jobs = Jobs::default();
        let mut started = Vec::new();
        let detect = Key::Prereqs(Work::Detect(1));
        let download = Key::Dgvoodoo(4);
        jobs.set(&[detect, download], |key| { started.push(key); idle() });
        jobs.set(&[detect, download], |key| { started.push(key); idle() });
        assert_eq!(started, vec![detect, download], "a running job is not started again");
        let download_job = jobs.running[&download].clone();
        jobs.set(&[detect], |key| { started.push(key); idle() });
        // A newer detection generation replaces the old one.
        let detect_job = jobs.running[&detect].clone();
        jobs.set(&[Key::Prereqs(Work::Detect(2))], |key| { started.push(key); idle() });
        assert_eq!(started.last(), Some(&Key::Prereqs(Work::Detect(2))));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !(download_job.is_finished() && detect_job.is_finished()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(download_job.is_finished() && detect_job.is_finished(), "jobs no longer wanted are stopped");
        jobs.stop();
        assert!(jobs.running.is_empty());
    }

    #[test]
    fn the_wanted_jobs_are_what_the_core_asks_for() {
        let app = crate::tests::app();
        let mut expected: Vec<Key> = crate::prereqs::work(&app.prereqs).into_iter().map(Key::Prereqs).collect();
        expected.extend(crate::dgvoodoo::work(&app.dgvoodoo).map(Key::Dgvoodoo));
        assert_eq!(wanted(&app), expected);
        // Detection runs from the start (prereqs::State's default).
        let fresh = crate::prereqs::State::default();
        if crate::preview::flags().is_none() {
            assert!(crate::prereqs::work(&fresh).iter().any(|work| matches!(work, Work::Detect(_))));
        }
    }
}
