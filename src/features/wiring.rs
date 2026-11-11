//! The window's few hooks into the features runtime, kept here so the window itself only calls
//! four methods: `start_features` at start, `features_observed` from its session observation,
//! `features_order_changed` when the Home order changes, and `features_event` for the runtime's
//! events. The interface toolkit plays no part.

use super::runtime::{GameEnd, GameStart, Runtime};
use super::{notify, Event};
use crate::backend::AccountMode;
use crate::launch_log::ExitReport;
use crate::{App, WorkerEvent};
use std::sync::{Arc, Mutex};

impl App {
    /// Start the runtime (nothing runs until something is configured or a game starts).
    pub(crate) fn start_features(&mut self) {
        let sender = Mutex::new(self.sender.clone());
        let runtime = Runtime::new(self.config.clone(), Arc::new(move |event| {
            let _ = sender.lock().unwrap_or_else(|e| e.into_inner()).send(WorkerEvent::Features(event));
        }));
        runtime.set_privacy(self.settings.privacy);
        runtime.set_order(self.accounts.iter().map(|row| row.key.clone()).collect());
        self.features = Some(runtime);
    }

    /// After each session observation: games that started and ended, and the signed-in sessions.
    pub(crate) fn features_observed(&mut self, started: Vec<String>, ended: Vec<(String, Option<ExitReport>, bool)>) {
        let Some(runtime) = self.features.clone() else { return };
        // Preview rows carry made-up PIDs: nothing may act on those processes.
        if self.preview { return; }
        let maintenance = self.features_maintenance();
        runtime.set_maintenance(maintenance);
        // The Home order, whichever screen changed it (a no-allocation compare per observation).
        if runtime.order_differs(self.accounts.iter().map(|row| row.key.as_str())) {
            runtime.set_order(self.accounts.iter().map(|row| row.key.clone()).collect());
        }
        for (account, report, user_closed) in ended {
            runtime.game_ended(GameEnd { account, report, user_closed, maintenance });
        }
        for key in started {
            let Some(row) = self.accounts.iter().find(|row| row.key == key) else { continue };
            let Some(pid) = row.pid else { continue };
            let slot = self.settings.account_auto_login.get(&key).filter(|login| login.enabled).map(|login| login.character_slot);
            runtime.game_started(GameStart { account: key, slot, label: row.account.label.clone(), pid });
        }

        let sessions: Vec<String> = self.accounts.iter()
            .filter(|row| row.pid.is_some() && row.account.mode == AccountMode::Retail).map(|row| row.key.clone()).collect();
        if sessions != self.features_sessions {
            let watched = sessions.iter().filter_map(|key| self.accounts.iter().find(|row| &row.key == key)).map(|row| notify::Watched {
                account_key: row.key.clone(),
                label: row.account.label.clone(),
                directory: self.config.account_directory(&row.account),
            }).collect();
            runtime.sessions(watched);
            self.features_sessions = sessions;
        }
    }

    /// The Home account order changed (a drag finished): the cycle keys and the next launch
    /// follow it now rather than at the next observation. No running window is touched.
    pub(crate) fn features_order_changed(&mut self) {
        if self.preview { return; }
        if let Some(runtime) = &self.features { runtime.set_order(self.accounts.iter().map(|row| row.key.clone()).collect()); }
    }

    /// FFXI is down, or an Update, Repair or setup runs: no automatic relaunch.
    fn features_maintenance(&self) -> bool {
        self.service_down() || self.maintenance.is_some_and(|kind| kind.blocks_launch())
    }

    pub(crate) fn features_event(&mut self, event: Event) {
        self.features_view.touch();
        match event {
            // An ordinary Play, so a relaunch's traffic is exactly a Play's. Maintenance that
            // began meanwhile holds it: the row keeps its "closed unexpectedly" status.
            Event::RelaunchDue(key) => {
                self.features_view.offers.remove(&key);
                if !self.features_maintenance() { self.play(vec![key]); }
            }
            Event::Error(error) => {
                crate::launch_log::append_history(&self.config.history_path(), "Crystal", &format!("features: {error}"), None);
            }
            Event::Hotkeys(list) => self.features_view.hotkeys = list,
            // An offer waits on the account until it plays again or it is dismissed; a scheduled
            // relaunch shows from the runtime's pending list.
            Event::Recovery { account, decision } => match decision {
                super::recovery::Decision::Offer { .. } => { self.features_view.offers.insert(account, decision); }
                _ => { self.features_view.offers.remove(&account); }
            },
        }
    }
}
