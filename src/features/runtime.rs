//! The features' runtime in Crystal's window process: the one owner of the saved feature settings
//! and of the threads the features need, fed by the interface (game started, game ended, the
//! signed-in sessions, privacy, account order) and reporting back through `Event`s.
//!
//! What runs when, and nothing else:
//! - the desktop thread (`desktop`) while an account hotkey is set, or while a game runs and a
//!   window feature (remember, restore, background priority, window ordering, the cycle keys) is
//!   on;
//! - the notification watcher (`notify`) while notifications are on and a session is signed in;
//! - the Discord presence thread while presence is on and a game runs;
//! - a one-shot sleeper while an automatic relaunch is scheduled.
//!
//! The interface binds to `Runtime`'s methods and `Event`; a relaunch reaches it as
//! `Event::RelaunchDue`, which it starts as an ordinary Play.

use super::game_window::{self, Monitor};
use super::hotkeys::{self, Action, Hotkey, Problem};
use super::layouts::{self, Preset};
use super::recovery::{self, Decision, EndKind};
use super::{backups, desktop, discord, notify, order, perf, playtime, store, ClientKey, FeatureSettings};
use crate::backend::Config;
use crate::launch_log::ExitReport;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// What the runtime tells the interface.
#[derive(Clone, Debug)]
pub(crate) enum Event {
    /// Every configured hotkey and whether it works, after each (re)registration.
    Hotkeys(Vec<hotkeys::Status>),
    /// What recovery decided about an ended game (also returned by `game_ended`), or a scheduled
    /// relaunch that maintenance held back (`Offer` with `Held::Maintenance`).
    Recovery { account: String, decision: Decision },
    /// Start this account's ordinary Play now (a scheduled automatic relaunch is due).
    RelaunchDue(String),
    /// A desktop notification was shown, suppressed, or failed.
    Notification(notify::Report),
    /// Something a feature could not do (saving its settings, applying priority).
    Error(String),
}

pub(crate) type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// A game the interface saw start.
#[derive(Clone, Debug)]
pub(crate) struct GameStart {
    pub account: String,
    /// The automatic character login's slot, when on.
    pub slot: Option<u8>,
    pub label: String,
    /// The game's host process.
    pub pid: u32,
}

/// A game the interface saw end.
#[derive(Clone, Debug)]
pub(crate) struct GameEnd {
    pub account: String,
    /// How the host ended, when its supervisor reported it.
    pub report: Option<ExitReport>,
    /// The user closed it through Crystal (Shutdown, Sign out).
    pub user_closed: bool,
    /// FFXI is in maintenance or an outage, or an Update is running.
    pub maintenance: bool,
}

struct Game {
    key: ClientKey,
    label: String,
    pids: Vec<u32>,
    hwnd: Option<isize>,
    started: Instant,
    started_unix: u64,
    started_wall: SystemTime,
    /// The window was put where it was remembered (once per launch).
    restored: bool,
    /// The title the window first had, and a later different one (a launch tool's character name).
    first_title: Option<String>,
    character: Option<String>,
    /// The priority last applied, so a foreground change costs no call when nothing changes.
    priority: Option<perf::Priority>,
}

struct Inner {
    settings: FeatureSettings,
    /// features.json existed but couldn't be read: shown, and never saved over unasked.
    load_error: Option<String>,
    games: BTreeMap<String, Game>,
    order: Vec<String>,
    maintenance: bool,
    desktop: Option<desktop::Desktop>,
    /// What each registered hotkey id does.
    hotkeys: Vec<(i32, Action, Hotkey)>,
    hotkey_status: Vec<hotkeys::Status>,
    /// A Play is still bringing up games (the interface says): windows are ordered after it.
    launching: bool,
    /// The windows need ordering (a game started, or the Home order changed with games running).
    order_pending: bool,
    /// The windows as last ordered, in Home order: an automatic pass with nothing new is skipped.
    last_ordered: Vec<isize>,
    recovery: recovery::Policy,
    wake_generation: u64,
    playtime: playtime::Tracker,
    presence: discord::Presence,
    notify: notify::Watcher,
}

struct Shared {
    config: Config,
    sink: Sink,
    inner: Mutex<Inner>,
    /// The running games' processes, read by the notification thread without the main lock.
    pids: Mutex<Vec<u32>>,
    privacy: Arc<AtomicBool>,
    /// Moves whenever what the interface reads changes (`Runtime::revision`).
    revision: AtomicU64,
    /// A window-ordering pass is running (one at a time, whichever thread asked).
    ordering: AtomicBool,
}

/// The features' runtime. Cheap to clone (one shared state).
#[derive(Clone)]
pub(crate) struct Runtime {
    shared: Arc<Shared>,
}

fn now_unix() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) }

impl Runtime {
    /// Load the saved feature settings and play time. Starts nothing that isn't configured: with
    /// no hotkey set, no thread runs until a game starts or a session signs in.
    pub(crate) fn new(config: Config, sink: Sink) -> Self {
        let (settings, load_error) = match store::load(&config) {
            Ok(settings) => (settings, None),
            Err(error) => (FeatureSettings::default(), Some(error)),
        };
        let pids = Mutex::new(Vec::new());
        let privacy = Arc::new(AtomicBool::new(false));
        let shared = Arc::new_cyclic(|weak: &std::sync::Weak<Shared>| {
            let foreground_weak = weak.clone();
            let foreground: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
                foreground_weak.upgrade().is_some_and(|shared| foreground_is_game(&lock(&shared.pids)))
            });
            let notify_sink = sink.clone();
            let notify = notify::Watcher::start(Arc::new(move |report| notify_sink(Event::Notification(report))),
                notify::Hooks::system(foreground, privacy.clone()), settings.notify.clone());
            Shared {
                inner: Mutex::new(Inner {
                    presence: discord::Presence::new(settings.playtime.discord),
                    playtime: playtime::Tracker::load(&playtime::path(&config)),
                    settings,
                    load_error,
                    games: BTreeMap::new(),
                    order: Vec::new(),
                    maintenance: false,
                    desktop: None,
                    hotkeys: Vec::new(),
                    hotkey_status: Vec::new(),
                    launching: false,
                    order_pending: false,
                    last_ordered: Vec::new(),
                    recovery: recovery::Policy::default(),
                    wake_generation: 0,
                    notify,
                }),
                config,
                sink,
                pids,
                privacy,
                revision: AtomicU64::new(0),
                ordering: AtomicBool::new(false),
            }
        });
        let runtime = Self { shared };
        runtime.refresh_desktop();
        runtime
    }

    fn inner(&self) -> MutexGuard<'_, Inner> { lock(&self.shared.inner) }

    fn emit(&self, event: Event) { (self.shared.sink)(event) }

    fn changed(&self) { self.shared.revision.fetch_add(1, Ordering::AcqRel); }

    /// Moves whenever the settings, the play time or a scheduled relaunch changes, so a view
    /// rebuilds only when something it shows did.
    pub(crate) fn revision(&self) -> u64 { self.shared.revision.load(Ordering::Acquire) }

    // ---- settings ------------------------------------------------------------------------

    pub(crate) fn settings(&self) -> FeatureSettings { self.inner().settings.clone() }

    /// Why the saved feature settings couldn't be read at start, if they couldn't.
    pub(crate) fn load_error(&self) -> Option<String> { self.inner().load_error.clone() }

    /// Change the settings, save them, and apply the change (hotkeys, threads, priorities).
    pub(crate) fn update(&self, change: impl FnOnce(&mut FeatureSettings)) -> Result<(), String> {
        let saved = {
            let mut inner = self.inner();
            change(&mut inner.settings);
            let saved = store::save(&self.shared.config, &inner.settings);
            if saved.is_ok() { inner.load_error = None; }
            self.changed();
            inner.notify.apply(&inner.settings.notify);
            let discord = inner.settings.playtime.discord;
            inner.presence.set_enabled(discord);
            saved
        };
        self.refresh_desktop();
        self.apply_priorities(None);
        self.update_presence();
        saved
    }

    pub(crate) fn set_privacy(&self, on: bool) {
        self.shared.privacy.store(on, Ordering::Relaxed);
        self.update_presence();
    }

    /// The launcher's account order: hotkey cycling, arrange-all and window ordering follow it. A
    /// change while games run puts their windows in the new order (`order_windows` on).
    pub(crate) fn set_order(&self, order: Vec<String>) {
        {
            let mut inner = self.inner();
            if inner.order == order { return; }
            inner.order = order;
            if !inner.games.is_empty() { inner.order_pending = true; }
        }
        self.order_if_due();
    }

    /// Whether `order` differs from the account order the runtime has (no allocation: the
    /// interface may ask on every observation).
    pub(crate) fn order_differs<'a>(&self, order: impl IntoIterator<Item = &'a str>) -> bool {
        let inner = self.inner();
        let mut have = inner.order.iter();
        for key in order {
            if have.next().map(String::as_str) != Some(key) { return true; }
        }
        have.next().is_some()
    }

    /// A Play is still bringing up games (sign-in, queue, or a game not yet started). When the
    /// last one is up, their windows are put in Home order once each has its window.
    pub(crate) fn set_launching(&self, on: bool) {
        if std::mem::replace(&mut self.inner().launching, on) == on || on { return; }
        self.order_if_due();
    }

    /// FFXI is down for maintenance or an outage, or an Update runs: no automatic relaunch.
    pub(crate) fn set_maintenance(&self, on: bool) { self.inner().maintenance = on; }

    // ---- games ---------------------------------------------------------------------------

    /// A game started (the interface saw its host's game running).
    pub(crate) fn game_started(&self, start: GameStart) {
        let key = ClientKey::new(start.account.clone(), start.slot);
        let pids = game_window::process_family(start.pid);
        let error = {
            let mut inner = self.inner();
            inner.recovery.cancel(&start.account);
            self.changed();
            if inner.settings.playtime.record { inner.playtime.start(&key, now_unix()); }
            let client = inner.settings.perf.client(&start.account);
            let priority = inner.settings.perf.priority_for(&start.account, true);
            let error = perf::apply(start.pid, priority, client.affinity).err();
            inner.games.insert(start.account.clone(), Game {
                key, label: start.label, pids, hwnd: None, started: Instant::now(), started_unix: now_unix(),
                started_wall: SystemTime::now(), restored: false, first_title: None, character: None, priority,
            });
            inner.order_pending = true;
            error
        };
        if let Some(error) = error { self.emit(Event::Error(error)); }
        self.sync_pids();
        self.refresh_desktop();
        // The window may already be up: the hooks only see later changes.
        if let Some(hwnd) = self.with_game(&start.account, |g| game_window::find_main_window(&g.pids)).flatten() {
            self.on_window(&start.account, hwnd);
        }
        self.update_presence();
        self.order_if_due();
    }

    /// A game ended. Records its play time and decides about recovery: the decision is returned
    /// and also sent as `Event::Recovery` (nothing for a clean quit).
    pub(crate) fn game_ended(&self, end: GameEnd) -> Decision {
        let decision = {
            let mut inner = self.inner();
            let Some(game) = inner.games.remove(&end.account) else { return Decision::None };
            inner.playtime.stop(&game.key, now_unix());
            self.changed();
            let clean = end.user_closed || end.report.as_ref().is_some_and(|r| r.kind == crate::launch_log::ExitKind::Normal);
            let wer = !clean && recovery::wer_report_since("pol.exe", game.started_wall);
            let kind = recovery::classify(end.report.as_ref(), end.user_closed, wer);
            let during_startup = end.report.as_ref().is_some_and(|r| r.during_startup);
            let maintenance = end.maintenance || inner.maintenance;
            let settings = inner.settings.recovery.clone();
            inner.recovery.on_end(&end.account, kind, during_startup, game.started.elapsed(), Instant::now(), maintenance, &settings)
        };
        self.sync_pids();
        self.refresh_desktop();
        self.update_presence();
        if let Decision::Relaunch { at, .. } = decision { self.schedule_wake(at); }
        if decision != Decision::None {
            self.emit(Event::Recovery { account: end.account.clone(), decision: decision.clone() });
        }
        decision
    }

    /// The user played, cancelled, or the relaunch started: drop a scheduled relaunch.
    pub(crate) fn cancel_relaunch(&self, account: &str) -> bool {
        self.changed();
        self.inner().recovery.cancel(account)
    }

    /// A scheduled relaunch for `account`: when, and which attempt.
    pub(crate) fn pending_relaunch(&self, account: &str) -> Option<(Instant, u8)> { self.inner().recovery.pending(account) }

    fn schedule_wake(&self, at: Instant) {
        let generation = {
            let mut inner = self.inner();
            inner.wake_generation += 1;
            inner.wake_generation
        };
        let weak = Arc::downgrade(&self.shared);
        let _ = std::thread::Builder::new().name("features-relaunch".into()).stack_size(64 * 1024).spawn(move || {
            std::thread::sleep(at.saturating_duration_since(Instant::now()));
            let Some(shared) = weak.upgrade() else { return };
            let runtime = Runtime { shared };
            let (due, next) = {
                let mut inner = runtime.inner();
                if inner.wake_generation != generation { return; }
                let maintenance = inner.maintenance;
                runtime.changed();
                (inner.recovery.due(Instant::now(), maintenance), inner.recovery.next_due())
            };
            for (account, outcome) in due {
                runtime.emit(match outcome {
                    Ok(_) => Event::RelaunchDue(account),
                    Err(held) => Event::Recovery { account, decision: Decision::Offer { kind: EndKind::Crash, held } },
                });
            }
            if let Some(next) = next { runtime.schedule_wake(next); }
        });
    }

    /// The signed-in sessions whose published friends and mail notifications follow.
    pub(crate) fn sessions(&self, watched: Vec<notify::Watched>) { self.inner().notify.set_accounts(watched); }

    fn with_game<T>(&self, account: &str, f: impl FnOnce(&Game) -> T) -> Option<T> { self.inner().games.get(account).map(f) }

    fn sync_pids(&self) {
        let pids: Vec<u32> = self.inner().games.values().flat_map(|g| g.pids.iter().copied()).collect();
        *lock(&self.shared.pids) = pids;
    }

    /// Whether a running game's window is in the foreground and not minimised.
    pub(crate) fn foreground_is_game(&self) -> bool { foreground_is_game(&lock(&self.shared.pids)) }

    // ---- windows -------------------------------------------------------------------------

    /// Start, adjust or end the desktop thread for what is configured now.
    /// The cycle keys are registered only while a game window is in front (`hotkeys::Gate`), so
    /// they need the thread only while games run; account hotkeys need it while they are set.
    fn refresh_desktop(&self) {
        let (plan, refused) = hotkeys::plan(&self.inner().settings.hotkeys);
        let mut report: Option<Vec<hotkeys::Status>> = None;
        let (track, dropped) = {
            let mut inner = self.inner();
            let s = &inner.settings;
            let in_game_keys = plan.iter().any(|(_, a, _)| a.in_game_only());
            let windows = s.layouts.remember || s.layouts.restore || s.perf.background || s.layouts.order_windows || in_game_keys;
            let track: Vec<u32> = if windows { inner.games.values().flat_map(|g| g.pids.iter().copied()).collect() } else { Vec::new() };
            let needed = plan.iter().any(|(_, a, _)| !a.in_game_only()) || !track.is_empty();
            let refused_before = inner.hotkey_status.iter().filter(|s| matches!(s.problem, Some(Problem::Duplicate | Problem::NeedsModifier | Problem::Reserved)));
            let changed = inner.hotkeys.iter().map(|(_, a, h)| (a.clone(), *h)).ne(plan.iter().map(|(_, a, h)| (a.clone(), *h)))
                || refused_before.ne(refused.iter());
            inner.hotkeys = plan.clone();
            // Until the desktop thread reports, each usable hotkey reads as fine (an in-game key
            // with no game running waits for one). Unchanged keys keep what was reported.
            if changed || inner.desktop.is_none() || !needed {
                inner.hotkey_status = plan.iter().map(|(_, action, hotkey)| hotkeys::Status { action: action.clone(), hotkey: *hotkey, problem: None })
                    .chain(refused.iter().cloned()).collect();
                report = changed.then(|| inner.hotkey_status.clone());
            }
            if !needed {
                (None, inner.desktop.take())
            } else {
                if inner.desktop.is_none() {
                    let weak = Arc::downgrade(&self.shared);
                    let sink: desktop::Sink = Arc::new(move |event| {
                        if let Some(shared) = weak.upgrade() { Runtime { shared }.on_desktop(event); }
                    });
                    match desktop::Desktop::start(sink) {
                        Ok(d) => {
                            send_hotkeys(&d, &plan);
                            inner.desktop = Some(d);
                        }
                        Err(error) => { drop(inner); self.emit(Event::Error(error)); return; }
                    }
                } else if changed {
                    if let Some(d) = &inner.desktop { send_hotkeys(d, &plan); }
                }
                // The thread reports what registering returned.
                report = None;
                (Some(track), None)
            }
        };
        // Joined outside the lock: the desktop thread's handlers take it.
        drop(dropped);
        if let Some(track) = track {
            if let Some(d) = &self.inner().desktop { d.send(desktop::Command::Track(track)); }
        }
        if let Some(statuses) = report { self.emit(Event::Hotkeys(statuses)); }
    }

    /// Every configured hotkey and its state (after the last registration).
    pub(crate) fn hotkey_status(&self) -> Vec<hotkeys::Status> { self.inner().hotkey_status.clone() }

    /// Handled on the desktop thread.
    fn on_desktop(&self, event: desktop::Event) {
        match event {
            desktop::Event::Hotkey(id) => {
                let action = self.inner().hotkeys.iter().find(|(i, _, _)| *i == id).map(|(_, a, _)| a.clone());
                if let Some(action) = action { self.run_action(&action); }
            }
            desktop::Event::Registered(results) => {
                let statuses: Vec<hotkeys::Status> = {
                    let mut inner = self.inner();
                    let mut all: Vec<hotkeys::Status> = inner.hotkeys.iter().map(|(id, action, hotkey)| {
                        let problem = match results.iter().find(|(i, _)| i == id).map(|(_, r)| *r) {
                            Some(Ok(())) | None => None,
                            Some(Err(1409)) => Some(Problem::InUse), // ERROR_HOTKEY_ALREADY_REGISTERED
                            Some(Err(code)) => Some(Problem::Failed(code)),
                        };
                        hotkeys::Status { action: action.clone(), hotkey: *hotkey, problem }
                    }).collect();
                    all.extend(inner.hotkey_status.iter().filter(|s| s.problem.is_some_and(|p| matches!(p, Problem::Duplicate | Problem::NeedsModifier | Problem::Reserved))).cloned());
                    inner.hotkey_status = all.clone();
                    all
                };
                self.emit(Event::Hotkeys(statuses));
            }
            desktop::Event::Foreground { hwnd, pid } => {
                // A game left behind: remember where it is now.
                let left: Vec<(String, isize)> = {
                    let inner = self.inner();
                    inner.games.iter().filter(|(_, g)| !g.pids.contains(&pid)).filter_map(|(k, g)| g.hwnd.map(|h| (k.clone(), h))).collect()
                };
                for (account, window) in left { self.remember(&account, window); }
                if let Some(account) = self.account_of(pid) {
                    let restore = self.with_game(&account, |g| !g.restored).unwrap_or(false);
                    if restore { self.on_window(&account, hwnd); }
                }
                self.apply_priorities(Some(pid));
            }
            desktop::Event::MoveSizeEnd { hwnd, pid } | desktop::Event::Minimized { hwnd, pid, .. } => {
                if let Some(account) = self.account_of(pid) {
                    if self.with_game(&account, |g| g.hwnd == Some(hwnd)).unwrap_or(false) { self.remember(&account, hwnd); }
                }
            }
            desktop::Event::Shown { hwnd, pid } => {
                if let Some(account) = self.account_of(pid) {
                    let known = self.with_game(&account, |g| g.hwnd.is_some_and(game_window::exists)).unwrap_or(true);
                    if !known { self.on_window(&account, hwnd); }
                }
            }
            desktop::Event::TitleChanged { hwnd, pid } => {
                if let Some(account) = self.account_of(pid) { self.title_changed(&account, hwnd); }
            }
        }
    }

    fn account_of(&self, pid: u32) -> Option<String> {
        self.inner().games.iter().find(|(_, g)| g.pids.contains(&pid)).map(|(k, _)| k.clone())
    }

    /// `hwnd` may be `account`'s game window: adopt it if it is the game's main window, and put
    /// it where it was remembered (once per launch).
    fn on_window(&self, account: &str, hwnd: isize) {
        let Some(pids) = self.with_game(account, |g| g.pids.clone()) else { return };
        if game_window::find_main_window(&pids) != Some(hwnd) { return; }
        let (restore, saved, adopted) = {
            let mut inner = self.inner();
            let restore_on = inner.settings.layouts.restore;
            let Some(game) = inner.games.get_mut(account) else { return };
            let adopted = game.hwnd.replace(hwnd) != Some(hwnd);
            if game.first_title.is_none() { game.first_title = Some(game_window::title(hwnd)); }
            let restore = restore_on && !game.restored;
            game.restored = true;
            let id = game.key.id();
            let fallback = ClientKey::new(game.key.account.clone(), None).id();
            (restore, inner.settings.layouts.saved.get(&id).or_else(|| inner.settings.layouts.saved.get(&fallback)).cloned(), adopted)
        };
        if let (true, Some(saved)) = (restore, saved) {
            if let Some((rect, shown)) = layouts::restore_rect(&saved, &game_window::monitors()) {
                game_window::apply(hwnd, rect, shown);
            }
        }
        if adopted { self.order_if_due(); }
    }

    fn remember(&self, account: &str, hwnd: isize) {
        let Some(placement) = game_window::capture(hwnd) else { return };
        let saved = {
            let mut inner = self.inner();
            if !inner.settings.layouts.remember || inner.load_error.is_some() { return; }
            let Some(key) = inner.games.get(account).map(|g| g.key.clone()) else { return };
            if !layouts::remember(&mut inner.settings.layouts, &key, &placement) { return; }
            self.changed();
            store::save(&self.shared.config, &inner.settings)
        };
        if let Err(error) = saved { self.emit(Event::Error(error)); }
    }

    fn title_changed(&self, account: &str, hwnd: isize) {
        let changed = {
            let mut inner = self.inner();
            let Some(game) = inner.games.get_mut(account).filter(|g| g.hwnd == Some(hwnd)) else { return };
            let title = game_window::title(hwnd).trim().chars().take(32).collect::<String>();
            let hint = (!title.is_empty() && game.first_title.as_deref().is_some_and(|first| first.trim() != title)).then_some(title);
            if hint == game.character { return; }
            game.character = hint.clone();
            let key = game.key.clone();
            inner.playtime.set_character(&key, hint);
            self.changed();
            true
        };
        if changed { self.update_presence(); }
    }

    /// Bring forward the game a hotkey names (on the desktop thread, which may set the
    /// foreground: the hotkey's input is its own). Cycling goes through the games that have a
    /// window, in Home order: one still loading (no window yet) or closed is skipped, and a
    /// minimised one is restored, as Alt+Tab does.
    fn run_action(&self, action: &Action) {
        let (order, current) = {
            let inner = self.inner();
            let order: Vec<String> = match action {
                Action::Account(_) => home_order(&inner),
                Action::Next | Action::Previous => home_windows(&inner).into_iter().map(|(k, _)| k).collect(),
            };
            let current = game_window::foreground().and_then(|(_, pid)| inner.games.iter().find(|(_, g)| g.pids.contains(&pid)).map(|(k, _)| k.clone()));
            (order, current)
        };
        if let Some(account) = hotkeys::target(action, &order, current.as_deref()) { self.focus(&account); }
    }

    /// Put the game windows' taskbar buttons and Alt+Tab order in Home order now (the "Order
    /// windows" button). Returns how many game windows there are; `Err` when there is none, or
    /// when Windows refused the Alt+Tab part (the taskbar part is still done).
    pub(crate) fn order_windows(&self) -> Result<usize, String> { self.order(true).unwrap_or(Ok(0)) }

    /// The automatic pass: when on, something changed, no Play is still bringing up games, and
    /// the windows differ from the last pass. It stays due while a game has no window yet, so
    /// that window is ordered too when it appears.
    fn order_if_due(&self) {
        {
            let mut inner = self.inner();
            if !inner.settings.layouts.order_windows || !inner.order_pending || inner.launching || inner.games.is_empty() { return; }
            if inner.games.values().all(|g| g.hwnd.is_some_and(game_window::exists)) { inner.order_pending = false; }
        }
        match self.order(false) {
            // Another pass is running: due again at the next change.
            None => self.inner().order_pending = true,
            Some(Err(error)) => self.emit(Event::Error(error)),
            Some(Ok(_)) => {}
        }
    }

    /// One ordering pass; `None` when another is running.
    fn order(&self, asked: bool) -> Option<Result<usize, String>> {
        struct Running<'a>(&'a AtomicBool);
        impl Drop for Running<'_> { fn drop(&mut self) { self.0.store(false, Ordering::Release); } }
        // One pass at a time: two interleaved activation sequences would undo each other.
        if self.shared.ordering.swap(true, Ordering::AcqRel) { return None; }
        let _running = Running(&self.shared.ordering);
        let hwnds: Vec<isize> = home_windows(&self.inner()).into_iter().map(|(_, h)| h).collect();
        if hwnds.is_empty() {
            return Some(if asked { Err(crate::i18n::t("No game window to order.").into()) } else { Ok(0) });
        }
        if !asked && self.inner().last_ordered == hwnds { return Some(Ok(hwnds.len())); }
        let windows: Vec<order::Window> = hwnds.iter().map(|h| order::Window { hwnd: *h, minimized: game_window::is_minimized(*h) }).collect();
        let plan = order::plan(&windows, game_window::foreground().map(|(h, _)| h));
        let outcome = order::apply(&plan);
        self.inner().last_ordered = hwnds.clone();
        if outcome.refused || !outcome.restored {
            return Some(Err(crate::i18n::t("Windows did not allow changing the Alt+Tab order.").into()));
        }
        Some(Ok(hwnds.len()))
    }

    /// Bring `account`'s game window forward. Whether it is now in the foreground.
    pub(crate) fn focus(&self, account: &str) -> bool {
        let Some((hwnd, pids)) = self.with_game(account, |g| (g.hwnd.filter(|h| game_window::exists(*h)), g.pids.clone())) else { return false };
        let Some(hwnd) = hwnd.or_else(|| game_window::find_main_window(&pids)) else { return false };
        game_window::bring_forward(hwnd)
    }

    /// The monitors now (for choosing where to arrange).
    pub(crate) fn monitors(&self) -> Vec<Monitor> { game_window::monitors() }

    /// Arrange every running game window with `preset` on the monitors named in `devices` (all,
    /// in Windows' order with the primary first, when empty), in the launcher's account order,
    /// and remember the result. Fullscreen games are left alone. Returns how many were placed.
    pub(crate) fn arrange(&self, preset: Preset, devices: &[String]) -> Result<usize, String> {
        let monitors = game_window::monitors();
        let chosen: Vec<&Monitor> = if devices.is_empty() { monitors.iter().collect() }
            else { devices.iter().filter_map(|d| monitors.iter().find(|m| &m.device == d)).collect() };
        let areas: Vec<_> = if chosen.is_empty() { monitors.iter().filter(|m| m.primary).map(|m| m.work).collect() } else { chosen.iter().map(|m| m.work).collect() };
        let (windows, aspect) = {
            let inner = self.inner();
            let windows: Vec<(String, isize)> = home_windows(&inner).into_iter()
                .filter(|(_, hwnd)| !game_window::capture(*hwnd).is_some_and(|p| p.mode == game_window::Mode::Fullscreen))
                .collect();
            (windows, inner.settings.layouts.aspect)
        };
        if windows.is_empty() { return Err(crate::i18n::t("No game window to arrange.").into()); }
        let rects = layouts::arrange(preset, windows.len(), &areas, aspect);
        let mut placed = 0;
        for ((account, hwnd), rect) in windows.iter().zip(rects) {
            if game_window::apply(*hwnd, rect, game_window::Shown::Normal) {
                placed += 1;
                self.remember(account, *hwnd);
            }
        }
        Ok(placed)
    }

    /// Forget a client's remembered window.
    pub(crate) fn forget_layout(&self, key: &ClientKey) -> Result<(), String> {
        self.update(|s| { s.layouts.saved.remove(&key.id()); })
    }

    /// Apply each running game's priority for the current foreground process.
    fn apply_priorities(&self, foreground_pid: Option<u32>) {
        let foreground_pid = foreground_pid.or_else(|| game_window::foreground().map(|(_, pid)| pid));
        let changes: Vec<(u32, perf::Priority)> = {
            let mut inner = self.inner();
            let perf = inner.settings.perf.clone();
            inner.games.iter_mut().filter_map(|(account, game)| {
                let front = foreground_pid.is_some_and(|pid| game.pids.contains(&pid));
                let want = perf.priority_for(account, front);
                // Background priority turned off: back to the account's own choice, or Normal.
                let want = want.or(game.priority.map(|_| perf::Priority::Normal));
                if want == game.priority { return None; }
                game.priority = want;
                Some((*game.pids.first()?, want?))
            }).collect()
        };
        for (pid, priority) in changes {
            if let Err(error) = perf::apply(pid, Some(priority), None) { self.emit(Event::Error(error)); }
        }
    }

    // ---- play time, presence, backups -----------------------------------------------------

    pub(crate) fn playtime(&self) -> Vec<playtime::Entry> { self.inner().playtime.entries() }

    pub(crate) fn discord_status(&self) -> discord::Status { self.inner().presence.status() }

    fn update_presence(&self) {
        let privacy = self.shared.privacy.load(Ordering::Relaxed);
        let mut inner = self.inner();
        if !inner.settings.playtime.discord { return; }
        let first = inner.order.iter().find_map(|k| inner.games.get(k)).or_else(|| inner.games.values().next());
        let activity = first.map(|game| discord::Activity {
            clients: inner.games.len(),
            character: game.character.clone().or_else(|| (!game.label.is_empty()).then(|| game.label.clone())),
            started_unix: inner.games.values().map(|g| g.started_unix).min().unwrap_or(game.started_unix),
        });
        match activity {
            Some(activity) => inner.presence.update(&activity, privacy),
            None => inner.presence.stop(),
        }
    }

    /// Back up USER data now.
    pub(crate) fn backup_now(&self, sources: &backups::Sources) -> Result<backups::BackupInfo, String> {
        let dir = backups::backups_dir(&self.shared.config);
        let info = backups::create(&dir, sources, backups::Reason::Manual)?;
        backups::prune(&dir, self.inner().settings.backups.keep);
        Ok(info)
    }

    pub(crate) fn backups(&self) -> Vec<backups::BackupInfo> { backups::list(&backups::backups_dir(&self.shared.config)) }

    /// Restore a backup; refused while any game Crystal started runs (and while the game or a
    /// launch tool runs at all, which `backups::restore` checks itself).
    pub(crate) fn restore(&self, path: &std::path::Path, sources: &backups::Sources, parts: backups::Parts) -> Result<backups::RestoreReport, String> {
        let running = !self.inner().games.is_empty();
        backups::restore(path, sources, parts, running)
    }
}

/// The values a launch of `account` (with its automatic login `slot`) serves its game, from the
/// saved feature settings: the remembered window's size and mode, and background mute. Read from
/// disk because launches are prepared in the session worker process.
pub(crate) fn launch_values(config: &Config, account: &str, slot: Option<u8>) -> Vec<(&'static str, u32)> {
    let Ok(settings) = store::load(config) else { return Vec::new() };
    let mut values = layouts::launch_values(&settings.layouts, &ClientKey::new(account, slot));
    values.extend(settings.perf.launch_values(account));
    values
}

/// The automatic backup before an Update or Repair of the game in `install`: its USER folder and
/// the configured Windower and Ashita settings, when `before_update` is on. Runs before any file
/// changes; a failure is the caller's to log and never stops the update.
pub(crate) fn backup_before_maintenance(config: &Config, install: &std::path::Path, kind: backups::Maintenance) -> Result<Option<backups::BackupInfo>, String> {
    use crate::pol_less::{tools, Tool};
    let settings = store::load(config).unwrap_or_default();
    let launcher = crate::load_settings(config).unwrap_or_default();
    let sources = backups::Sources {
        ffxi: vec![install.to_path_buf()],
        windower: tools::detect(Tool::Windower, &launcher.windower_path),
        ashita: tools::detect(Tool::Ashita, &launcher.ashita_path),
    };
    backups::before_maintenance(config, &sources, kind, &settings.backups)
}

/// `backup_before_maintenance`, with its outcome written to the launch history.
pub(crate) fn backup_before_maintenance_logged(config: &Config, install: &std::path::Path, kind: backups::Maintenance) {
    let line = match backup_before_maintenance(config, install, kind) {
        Ok(Some(info)) => format!("backup before update: {} ({} files)", info.file_name, info.files),
        Ok(None) => return,
        Err(error) => format!("backup before update failed: {error}"),
    };
    crate::launch_log::append_history(&config.history_path(), "Crystal", &line, None);
}

/// The running games' accounts in Home order (games of accounts not in it last, by key).
fn home_order(inner: &Inner) -> Vec<String> {
    let mut keys: Vec<String> = inner.order.iter().filter(|k| inner.games.contains_key(*k)).cloned().collect();
    keys.extend(inner.games.keys().filter(|k| !inner.order.contains(k)).cloned());
    keys
}

/// The running games that have a window now, in Home order, with that window.
fn home_windows(inner: &Inner) -> Vec<(String, isize)> {
    home_order(inner).into_iter().filter_map(|k| {
        let game = &inner.games[&k];
        let hwnd = game.hwnd.filter(|h| game_window::exists(*h)).or_else(|| game_window::find_main_window(&game.pids))?;
        Some((k, hwnd))
    }).collect()
}

/// The cycle keys go to the gate (registered only while a game is in front), the rest are
/// registered for as long as they are set.
fn send_hotkeys(desktop: &desktop::Desktop, plan: &[(i32, Action, Hotkey)]) {
    let (in_game, always): (Vec<_>, Vec<_>) = plan.iter().partition(|(_, action, _)| action.in_game_only());
    desktop.send(desktop::Command::Hotkeys(always.iter().map(|(id, _, h)| (*id, *h)).collect()));
    desktop.send(desktop::Command::GameHotkeys(in_game.iter().map(|(id, _, h)| (*id, *h)).collect()));
}

fn foreground_is_game(pids: &[u32]) -> bool {
    game_window::foreground().is_some_and(|(hwnd, pid)| pids.contains(&pid) && !game_window::is_minimized(hwnd))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> { mutex.lock().unwrap_or_else(|e| e.into_inner()) }

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> (Runtime, std::path::PathBuf, Arc<Mutex<Vec<Event>>>) {
        let dir = std::env::temp_dir().join(format!("xi-features-runtime-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config { experiment: dir.clone(), ..Config::default() };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        (Runtime::new(config, Arc::new(move |e| sink_events.lock().unwrap().push(e))), dir, events)
    }

    #[test]
    fn nothing_runs_until_something_needs_it() {
        let (runtime, dir, _) = runtime();
        assert!(runtime.inner().desktop.is_none(), "the default cycle keys and no game: no desktop thread");
        assert!(runtime.inner().hotkeys.iter().any(|(_, a, h)| *a == Action::Next && *h == hotkeys::DEFAULT_CYCLE));
        assert!(runtime.hotkey_status().iter().all(|s| s.problem.is_none()), "the cycle keys wait for a game");
        assert!(!runtime.inner().notify.is_running());
        assert_eq!(runtime.discord_status(), discord::Status::Off);
        // A cycle key never starts the thread on its own: it is only registered over a game.
        runtime.update(|s| s.hotkeys.cycle = Hotkey::parse("Ctrl+Alt+Shift+F22")).unwrap();
        assert!(runtime.inner().desktop.is_none());
        // An account hotkey (registered for as long as it is set) starts it; clearing it ends it.
        runtime.update(|s| { s.hotkeys.accounts.insert("a".into(), Hotkey::parse("Ctrl+Alt+Shift+F21").unwrap()); }).unwrap();
        assert!(runtime.inner().desktop.is_some());
        runtime.update(|s| s.hotkeys.accounts.clear()).unwrap();
        assert!(runtime.inner().desktop.is_none());
        // The change was saved where the next start reads it.
        assert!(dir.join("iced-launcher/features.json").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hotkey_problems_are_reported_as_events() {
        let (runtime, dir, events) = runtime();
        runtime.update(|s| s.hotkeys.cycle = Hotkey::parse("F2")).unwrap();
        let reported = events.lock().unwrap().iter().any(|e| matches!(e, Event::Hotkeys(list) if list.iter().any(|s| s.problem == Some(Problem::NeedsModifier))));
        assert!(reported);
        assert!(runtime.inner().desktop.is_none(), "an unusable hotkey starts nothing");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_game_is_timed_and_its_crash_is_offered_for_relaunch() {
        let (runtime, dir, events) = runtime();
        // A process this test starts stands in for a game host (it has no window).
        let mut child = std::process::Command::new("cmd.exe").args(["/c", "set /p x="]).stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null()).spawn().unwrap();
        runtime.game_started(GameStart { account: "acct".into(), slot: Some(2), label: "Main".into(), pid: child.id() });
        assert!(runtime.inner().desktop.is_some(), "remember/restore are on by default: the game is watched");
        let report = ExitReport { kind: crate::launch_log::ExitKind::Code { code: 3 }, exit_status: Some(3), during_startup: false, summary: String::new() };
        let decision = runtime.game_ended(GameEnd { account: "acct".into(), report: Some(report), user_closed: false, maintenance: false });
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(decision, Decision::Offer { kind: EndKind::Disconnect, held: recovery::Held::Off });
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, Event::Recovery { account, .. } if account == "acct")));
        assert!(runtime.inner().desktop.is_none(), "no game and no hotkey: the thread ended");
        let played = runtime.playtime();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].key, ClientKey::new("acct", Some(2)));
        assert_eq!(played[0].sessions, 1);
        // An end the interface reports twice, or for an unknown game, decides nothing.
        assert_eq!(runtime.game_ended(GameEnd { account: "acct".into(), report: None, user_closed: false, maintenance: false }), Decision::None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn window_ordering_waits_for_the_play_and_for_windows() {
        let (runtime, dir, events) = runtime();
        assert!(runtime.settings().layouts.order_windows, "on by default");
        assert!(runtime.order_differs(["a", "b"]));
        runtime.set_order(vec!["a".into(), "b".into()]);
        assert!(!runtime.order_differs(["a", "b"]));
        assert!(runtime.order_differs(["b", "a"]) && runtime.order_differs(["a"]) && runtime.order_differs(["a", "b", "c"]));
        assert!(runtime.order_windows().is_err(), "no game window to order");
        // A game with no window (a windowless stand-in) during a Play: the pass stays due.
        runtime.set_launching(true);
        runtime.game_started(GameStart { account: "a".into(), slot: None, label: String::new(), pid: std::process::id() });
        assert!(runtime.inner().order_pending && runtime.inner().desktop.is_some(), "ordering watches for the window");
        runtime.set_launching(false);
        assert!(runtime.inner().order_pending, "still due until the game has its window");
        assert!(runtime.inner().last_ordered.is_empty(), "nothing to order yet");
        // A Home order change while it runs keeps it due; turning the setting off stops it.
        runtime.set_order(vec!["b".into(), "a".into()]);
        assert!(runtime.inner().order_pending);
        runtime.update(|s| s.layouts.order_windows = false).unwrap();
        runtime.game_ended(GameEnd { account: "a".into(), report: None, user_closed: true, maintenance: false });
        assert!(!events.lock().unwrap().iter().any(|e| matches!(e, Event::Error(_))));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_automatic_relaunch_is_scheduled_and_cancelled_by_the_user() {
        let (runtime, dir, _) = runtime();
        runtime.update(|s| { s.recovery.auto_relaunch = true; s.layouts.remember = false; s.layouts.restore = false; }).unwrap();
        runtime.game_started(GameStart { account: "a".into(), slot: None, label: String::new(), pid: std::process::id() });
        let crash = ExitReport { kind: crate::launch_log::ExitKind::Other, exit_status: Some(0xC000_0005u32 as i32), during_startup: false, summary: String::new() };
        let decision = runtime.game_ended(GameEnd { account: "a".into(), report: Some(crash), user_closed: false, maintenance: false });
        assert!(matches!(decision, Decision::Relaunch { kind: EndKind::Crash, attempt: 1, .. }), "{decision:?}");
        assert!(runtime.pending_relaunch("a").is_some());
        assert!(runtime.cancel_relaunch("a"));
        assert!(runtime.pending_relaunch("a").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn launch_values_combine_the_remembered_size_and_background_mute() {
        let (runtime, dir, _) = runtime();
        let config = Config { experiment: dir.clone(), ..Config::default() };
        assert!(launch_values(&config, "a", None).is_empty());
        runtime.update(|s| {
            s.perf.mute_background = true;
            s.layouts.saved.insert("a".into(), layouts::Saved {
                rect: game_window::Rect::new(0, 0, 1296, 759), client: (1280, 720), shown: game_window::Shown::Normal,
                mode: game_window::Mode::Windowed,
                monitor: layouts::MonitorRef { device: "x".into(), rect: game_window::Rect::new(0, 0, 1920, 1080), work: game_window::Rect::new(0, 0, 1920, 1040), dpi: 96 },
            });
        }).unwrap();
        assert_eq!(launch_values(&config, "a", Some(4)), vec![("0001", 1280), ("0002", 720), ("0034", 1), ("0035", 0)]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
