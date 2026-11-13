//! A launcher or profile changed for an account whose session is signed in and idle (owner
//! decision 2026-11-13): Crystal signs that session out and back in with the new launcher, so
//! the next Play is not refused ("Sign out of PlayOnline before switching launch methods").
//!
//! It reuses the normal paths: the row's sign-out (`sign_out_account`), then the startup
//! sign-in's wait for the previous session to end (`startup_waiting`). Changes are debounced
//! per account, so several quick saves sign it out and in once. A game running or starting is
//! left alone: its next sign-in uses the new launcher, and a Play before then is refused on its
//! row as before.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::effect::Effect;
use crate::{session_worker, App, AccountRow, Message};

/// How long after the last change to an account's launcher its session is signed out.
pub(crate) const DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct LauncherChanges {
    /// Accounts whose session no longer matches their launcher: when to sign it out.
    due: HashMap<String, Instant>,
    /// Accounts signed out for a changed launcher: they sign back in once signed out.
    signing_out: HashSet<String>,
}

impl LauncherChanges {
    pub(crate) fn is_empty(&self) -> bool { self.due.is_empty() && self.signing_out.is_empty() }
}

impl App {
    /// A signed-in session that is idle: no game running or starting, nothing in flight.
    fn session_idle(row: &AccountRow) -> bool {
        row.pid.is_some() && !row.busy && !row.is_playing() && !row.game_start_pending()
            && !row.shutting_down && !row.signing_out
    }

    /// Whether `row`'s live session can no longer take its current launch plan.
    fn launcher_mismatch(&self, row: &AccountRow) -> bool {
        if self.preview { return false; }
        let Some(plan) = self.settings.launch_plan(&row.account) else { return false };
        let config = self.settings.config_for(&self.config, &row.account);
        session_worker::find(&config, &row.account).is_some_and(|session| session.ensure_launch_plan(&plan).is_err())
    }

    /// After accounts were saved: each idle signed-in session whose launcher or profile no
    /// longer matches, and that Crystal may sign in by itself, is due for a sign-out and
    /// sign-in DEBOUNCE from now (a later save moves it).
    pub(crate) fn note_launcher_changes(&mut self, now: Instant) {
        let due: Vec<String> = self.accounts.iter()
            .filter(|row| Self::session_idle(row) && self.presigned_row_ignoring_sign_out(row) && self.launcher_mismatch(row))
            .map(|row| row.key.clone()).collect();
        for key in due { self.launcher_changes.due.insert(key, now + DEBOUNCE); }
    }

    /// `presigned_row` for a row an edit may just have marked (the edit's own decline).
    fn presigned_row_ignoring_sign_out(&self, row: &AccountRow) -> bool {
        let known = if row.account.totp_key.is_empty() {
            self.settings.presigns(&row.key) && !row.otp_spent
        } else {
            self.settings.auto_sign_in && !row.authenticator_refused
        };
        row.account.mode == crate::backend::AccountMode::Retail && known && row.otp.is_empty()
    }

    /// Each Tick: sign out the sessions whose debounce is over. A row that is no longer idle
    /// (a game started meanwhile) or that matches again is dropped.
    pub(crate) fn tend_launcher_changes(&mut self, now: Instant) -> Effect<Message> {
        if self.launch_blocked() || self.vault_saving { return Effect::none(); }
        let ready: Vec<String> = self.launcher_changes.due.iter().filter(|(_, at)| now >= **at).map(|(key, _)| key.clone()).collect();
        let mut tasks = Vec::new();
        for key in ready {
            self.launcher_changes.due.remove(&key);
            let Some(row) = self.accounts.iter().find(|row| row.key == key) else { continue };
            if !Self::session_idle(row) || !self.launcher_mismatch(row) { continue; }
            if !self.preview {
                crate::launch_log::append_history(&self.config.history_path(), "Crystal",
                    &format!("launcher changed: #{} signs out and in again", self.accounts.iter().position(|r| r.key == key).map_or(0, |at| at + 1)), None);
            }
            self.launcher_changes.signing_out.insert(key.clone());
            tasks.push(self.sign_out_account(key));
        }
        Effect::batch(tasks)
    }

    /// A row's sign-out finished: one that was for a changed launcher signs back in through the
    /// startup sign-in, once its previous session has ended.
    pub(crate) fn launcher_signed_out(&mut self, key: &str, signed_out: bool) {
        if !self.launcher_changes.signing_out.remove(key) || !signed_out { return; }
        if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) {
            // Not the user's sign-out: the account is to be signed in again.
            row.signed_out = false;
            row.resign_declined = false;
        }
        if self.presigned(key) && !self.startup_waiting.iter().any(|(waiting, _)| waiting == key) {
            self.startup_waiting.push((key.to_owned(), Instant::now()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicBool, Arc};

    /// A fixture app with real account folders, every row signed in and idle with an Ashita
    /// session, its launcher Ashita; Crystal may sign all of them in by itself.
    fn fixture() -> (App, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("crystal-launcher-change-{}", crate::backend::unique_id().unwrap()));
        let mut app = crate::tests::app();
        app.preview = false;
        app.launches = Some(Vec::new());
        app.config = crate::backend::Config { experiment: root.join("data"), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.settings.auto_sign_in = true;
        for index in 0..app.accounts.len() {
            let key = app.accounts[index].key.clone();
            app.settings.account_otp.remove(&key);
            app.settings.account_no_otp.insert(key);
            let row = &mut app.accounts[index];
            (row.pid, row.alive, row.game_running) = (Some(400 + index as u32), Some(Arc::new(AtomicBool::new(true))), Some(Arc::new(AtomicBool::new(false))));
            (row.busy, row.preparing_session, row.otp_spent) = (false, false, false);
            row.account.launch_with = Some(crate::backend::LaunchWith::Ashita);
            let config = app.settings.config_for(&app.config, &app.accounts[index].account);
            session_worker::save_live_record_with_tool_for_test(&config, &app.accounts[index].account, crate::pol_less::Tool::Ashita);
        }
        (app, root)
    }

    /// The tool changed on an idle signed-in session: after the debounce it signs out, then signs
    /// in again (the startup sign-in, with the plan then current) once the old session ended.
    #[test]
    fn a_changed_launcher_signs_an_idle_session_out_and_in_again() {
        let (mut app, root) = fixture();
        let key = app.accounts[0].key.clone();
        assert!(app.presigned(&key));
        app.accounts[0].account.launch_with = Some(crate::backend::LaunchWith::Windower);
        let now = Instant::now();
        app.note_launcher_changes(now);
        assert!(app.launcher_changes.due.contains_key(&key) && app.launcher_changes.due.len() == 1, "only the changed account");
        let _ = app.tend_launcher_changes(now + DEBOUNCE - Duration::from_millis(1));
        assert!(!app.accounts[0].signing_out, "not before the debounce");
        let _ = app.tend_launcher_changes(now + DEBOUNCE);
        assert!(app.accounts[0].signing_out && app.accounts[0].busy);
        // The sign-out finished (the old worker's record is gone with it).
        std::fs::remove_dir_all(&root).unwrap();
        let generation = app.accounts[0].launch_generation;
        let _ = app.update(Message::AccountSignedOut(key.clone(), generation, Ok(())));
        assert!(app.accounts[0].pid.is_none() && !app.accounts[0].signed_out);
        assert!(app.startup_waiting.iter().any(|(waiting, _)| *waiting == key));
        // The startup sign-in takes it once the row is free: a sign-in only, with Windower.
        let started = app.take_waiting_startups(Instant::now());
        assert_eq!(started, [key.clone()]);
        app.start_startup(started, crate::launch_timing::LaunchTrigger::StartupSignIn);
        let launches = app.launches.as_ref().unwrap();
        assert!(matches!(launches.last(), Some((keys, true, _, _)) if *keys == [key.clone()]), "a sign-in only");
        assert_eq!(app.settings.launch_plan(&app.accounts[0].account).unwrap().tool, crate::pol_less::Tool::Windower);
    }

    /// A game running (or starting) is never signed out for a launcher change.
    #[test]
    fn a_running_game_is_left_alone() {
        let (mut app, root) = fixture();
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(true)));
        app.accounts[1].queued_play = Some(Instant::now());
        for row in app.accounts.iter_mut().take(2) { row.account.launch_with = Some(crate::backend::LaunchWith::Windower); }
        let now = Instant::now();
        app.note_launcher_changes(now);
        assert!(app.launcher_changes.due.is_empty());
        // One that started a game after the change was noted is dropped at its turn.
        app.accounts[2].account.launch_with = Some(crate::backend::LaunchWith::Windower);
        app.note_launcher_changes(now);
        app.accounts[2].game_running = Some(Arc::new(AtomicBool::new(true)));
        let _ = app.tend_launcher_changes(now + DEBOUNCE);
        assert!(app.accounts.iter().all(|row| !row.signing_out) && app.launcher_changes.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Several quick changes sign the session out once.
    #[test]
    fn quick_changes_sign_out_once() {
        let (mut app, root) = fixture();
        let start = Instant::now();
        for (step, choice) in [crate::backend::LaunchWith::Windower, crate::backend::LaunchWith::Vanilla, crate::backend::LaunchWith::Windower].into_iter().enumerate() {
            app.accounts[0].account.launch_with = Some(choice);
            app.note_launcher_changes(start + Duration::from_millis(500) * step as u32);
        }
        let _ = app.tend_launcher_changes(start + DEBOUNCE);
        assert!(!app.accounts[0].signing_out, "the last change moved the debounce");
        let _ = app.tend_launcher_changes(start + Duration::from_millis(1000) + DEBOUNCE);
        assert!(app.accounts[0].signing_out);
        // A save during its sign-out schedules nothing more.
        app.note_launcher_changes(start + Duration::from_secs(4));
        assert!(app.launcher_changes.due.is_empty() && app.launcher_changes.signing_out.len() == 1);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
