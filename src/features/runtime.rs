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
use super::{backups, desktop, discord, keyhook, notify, order, perf, playtime, store, ClientKey, FeatureSettings};
use crate::backend::Config;
use crate::launch_log::ExitReport;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a switching hotkey waits for an ordering pass on another thread to stop.
const ORDER_YIELD: Duration = Duration::from_millis(700);
/// How long after a hotkey's switch the foreground and the stacking are read again for the
/// history: whatever undoes a switch (a game or a tool taking the foreground back) shows there.
const AFTER_SWITCH_MS: u32 = 300;
/// How long the window a hotkey gave the foreground gets to come to the top of the game windows
/// by itself (its thread handles the activation between frames) before it is moved there.
const RAISE_SETTLE: Duration = Duration::from_millis(150);

/// The window-ordering flag (`Shared::ordering`), held: released when dropped.
struct OrderingHeld<'a>(&'a AtomicBool);
impl<'a> OrderingHeld<'a> {
    /// None while another holds it. (Built only once taken: dropping one releases the flag.)
    fn take(flag: &'a AtomicBool) -> Option<Self> { (!flag.swap(true, Ordering::AcqRel)).then(|| Self(flag)) }
}
impl Drop for OrderingHeld<'_> { fn drop(&mut self) { self.0.store(false, Ordering::Release); } }

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
    /// Windows refused an activation of the ordering pass while these games ran: automatic passes
    /// only stack the windows (`order::Plan::stack_only`) until the last game ends.
    alt_tab_refused: bool,
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
    /// A window-ordering pass is running (one at a time, whichever thread asked), or a hotkey is
    /// switching windows: neither starts while the other is at it.
    ordering: AtomicBool,
    /// Counts the switching hotkeys pressed. An ordering pass that sees it move stops: its own
    /// activations, and the foreground it gives back at the end, would undo the hotkey's switch.
    switches: AtomicU64,
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
                    alt_tab_refused: false,
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
                switches: AtomicU64::new(0),
            }
        });
        let runtime = Self { shared };
        runtime.refresh_desktop();
        runtime
    }

    fn inner(&self) -> MutexGuard<'_, Inner> { lock(&self.shared.inner) }

    fn emit(&self, event: Event) { (self.shared.sink)(event) }

    /// A diagnostic line in the launch history (`hotkeys: ...`): no account names, no keys typed.
    fn diag(&self, line: &str) {
        crate::launch_log::append_history(&self.shared.config.history_path(), "Crystal", line, None);
    }

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
        self.order_if_due("Home order changed");
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
        self.order_if_due("Play finished");
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
        self.order_if_due("game started");
    }

    /// A game ended. Records its play time and decides about recovery: the decision is returned
    /// and also sent as `Event::Recovery` (nothing for a clean quit).
    pub(crate) fn game_ended(&self, end: GameEnd) -> Decision {
        let decision = {
            let mut inner = self.inner();
            // Its Arrange all undo stays: Restore windows still puts its remembered window back.
            let Some(game) = inner.games.remove(&end.account) else { return Decision::None };
            if inner.games.is_empty() { inner.alt_tab_refused = false; }
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
            desktop::Event::Hotkey { id, via, key } => {
                let action = self.inner().hotkeys.iter().find(|(i, _, _)| *i == id).map(|(_, a, _)| a.clone());
                match action {
                    Some(action) => self.run_action(&action, via, key),
                    None => self.diag(&format!("hotkeys: {} id {id}: no hotkey has this id", via.name())),
                }
            }
            desktop::Event::Log(line) => self.diag(&line),
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
        if adopted { self.order_if_due("game window found"); }
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
    /// foreground at once: a WM_HOTKEY's input is its own). Cycling goes through the games that
    /// have a window, in Home order: one still loading (no window yet) or closed is skipped, and a
    /// minimised one is restored, as Alt+Tab does (`hotkeys::cycle`). One history line: the press,
    /// where it went (places in Home order, never names) and each foreground step's result.
    ///
    /// The window given the foreground is also put in front of the other game windows when it is
    /// not (`game_window::raise`): the games cover each other exactly, so the one seen is the one
    /// on top of the stack, whichever has the keyboard. An ordering pass on another thread stops
    /// for the switch and none starts during it (`Shared::switches`, `Shared::ordering`). The
    /// history line ends with the game windows' stacking, and a second line reads the foreground
    /// and the stacking again a moment later (`after_switch`).
    fn run_action(&self, action: &Action, via: desktop::Via, key: Option<keyhook::Fired>) {
        let switches = self.shared.switches.fetch_add(1, Ordering::AcqRel) + 1;
        let asked = Instant::now();
        let held = loop {
            if let Some(held) = OrderingHeld::take(&self.shared.ordering) { break Some(held); }
            if asked.elapsed() >= ORDER_YIELD { break None; }
            std::thread::sleep(Duration::from_millis(5));
        };
        let waited = asked.elapsed();
        let front = game_window::foreground();
        let (games, in_front) = {
            let inner = self.inner();
            let in_front = front.and_then(|(_, pid)| inner.games.iter().find(|(_, g)| g.pids.contains(&pid)).map(|(k, _)| k.clone()));
            (home_candidates(&inner), in_front)
        };
        // Each game window with its place in Home order.
        let windows: Vec<(usize, isize)> = games.iter().enumerate().filter_map(|(i, g)| Some((i + 1, g.window?))).collect();
        // No window in front at all (a game between two windows of its own): the game the user
        // is looking at is the one on top of the stack, and cycling goes on from it.
        let current = if front.is_some() { in_front } else {
            let on_top = game_window::z_order(&windows.iter().map(|(_, h)| *h).collect::<Vec<_>>()).into_iter().find(|h| game_window::is_shown(*h));
            on_top.and_then(|hwnd| games.iter().find(|g| g.window == Some(hwnd)).map(|g| g.account.clone()))
        };
        let switch = hotkeys::cycle(action, &games, current.as_deref());
        let result = switch.as_ref().map(|s| game_window::activate(s.hwnd, s.restore, true));
        let raised = match (&switch, &result) {
            (Some(s), Some(r)) if r.ok() => Some(game_window::raise(s.hwnd, &windows.iter().map(|(_, h)| *h).collect::<Vec<_>>(), RAISE_SETTLE)),
            _ => None,
        };
        drop(held);
        let name = match action {
            Action::Next => "next".to_owned(),
            Action::Previous => "previous".to_owned(),
            Action::Account(key) => format!("account #{}", games.iter().position(|g| &g.account == key).map_or(0, |i| i + 1)),
        };
        let how = key.map_or(via.name().to_owned(), |k| format!("{} ({})", via.name(), k.describe()));
        let from = current.as_ref().and_then(|c| games.iter().position(|g| &g.account == c)).map_or("-".to_owned(), |i| format!("#{}", i + 1));
        let from = if front.is_none() { format!("{from} (no window in front)") } else { from };
        let open = games.iter().filter(|g| g.window.is_some()).count();
        let mut line = match (&switch, &result) {
            (Some(s), Some(r)) => format!("hotkeys: {name} via {how}: {from} -> #{} of {} ({open} with a window): {}; {}{}; z {}",
                s.index, games.len(), r.describe(), if r.ok() { "in front" } else { "NOT in front" }, describe_raise(raised), stack_line(&windows)),
            _ => format!("hotkeys: {name} via {how}: from {from}, no window to switch to ({} running, {open} with a window)", games.len()),
        };
        if waited >= Duration::from_millis(20) {
            line.push_str(&format!("; waited {} ms for an ordering pass{}", waited.as_millis(), if waited >= ORDER_YIELD { ", which did not stop" } else { "" }));
        }
        self.diag(&line);
        if let Some(s) = &switch {
            let weak = Arc::downgrade(&self.shared);
            let (index, target) = (s.index, s.hwnd);
            desktop::later(AFTER_SWITCH_MS, Box::new(move || {
                if let Some(shared) = weak.upgrade() { Runtime { shared }.after_switch(index, target, switches, &windows); }
            }));
        }
    }

    /// The history's second look at a hotkey's switch to `target` (place `index` in Home order),
    /// `AFTER_SWITCH_MS` later: which window is in front now, and the game windows' stacking.
    fn after_switch(&self, index: usize, target: isize, switches: u64, windows: &[(usize, isize)]) {
        let front = match game_window::foreground() {
            None => "no window".to_owned(),
            Some((hwnd, _)) if hwnd == target => format!("#{index}"),
            Some((hwnd, pid)) => {
                let who = match windows.iter().find(|(_, h)| *h == hwnd) {
                    Some((other, _)) => format!("#{other}"),
                    None if lock(&self.shared.pids).contains(&pid) => "another window of a game".to_owned(),
                    None => "not a game".to_owned(),
                };
                // Another hotkey since explains it; anything else took the foreground back.
                let why = if self.shared.switches.load(Ordering::Acquire) != switches { "a later hotkey" } else { "NOT the target" };
                format!("{who} ({why})")
            }
        };
        self.diag(&format!("hotkeys: {AFTER_SWITCH_MS} ms after -> #{index}: in front {front}; z {}", stack_line(windows)));
    }

    /// Put the game windows' taskbar buttons and Alt+Tab order in Home order now (the "Order
    /// windows" button). Returns how many game windows there are; `Err` when there is none, or
    /// when Windows refused the Alt+Tab part (the taskbar part is still done).
    pub(crate) fn order_windows(&self) -> Result<usize, String> { self.order(None).unwrap_or(Ok(0)) }

    /// The automatic pass: when on, something changed, no Play is still bringing up games, every
    /// running game has its window (games coming up one by one get one pass, not one per window),
    /// and the windows differ from the last pass. It stays due while a game has no window yet.
    /// `why` names what made it due, for the history.
    fn order_if_due(&self, why: &'static str) {
        {
            let mut inner = self.inner();
            if !inner.settings.layouts.order_windows || !inner.order_pending || inner.launching || inner.games.is_empty() { return; }
            if !inner.games.values().all(|g| g.hwnd.is_some_and(game_window::exists)) { return; }
            inner.order_pending = false;
        }
        match self.order(Some(why)) {
            // Another pass is running, or a hotkey is switching: due again at the next change.
            None => self.inner().order_pending = true,
            Some(Err(error)) => self.emit(Event::Error(error)),
            Some(Ok(_)) => {}
        }
    }

    /// One ordering pass, asked for (`why` is None) or automatic (`why` names its cause); `None`
    /// when another is running or a hotkey is switching windows. Every pass leaves one history
    /// line with its cause. An automatic pass whose activations Windows refuses still sets the
    /// order by stacking (`order::apply`), and later automatic passes only stack. A switching
    /// hotkey pressed meanwhile stops the pass (it would undo the switch): it is then due again.
    fn order(&self, why: Option<&'static str>) -> Option<Result<usize, String>> {
        let asked = why.is_none();
        // One pass at a time: two interleaved activation sequences would undo each other.
        let _running = OrderingHeld::take(&self.shared.ordering)?;
        let switches = self.shared.switches.load(Ordering::Acquire);
        let (hwnds, refused_before) = {
            let inner = self.inner();
            (home_windows(&inner).into_iter().map(|(_, h)| h).collect::<Vec<isize>>(), inner.alt_tab_refused)
        };
        if hwnds.is_empty() {
            return Some(if asked { Err(crate::i18n::t("No game window to order.").into()) } else { Ok(0) });
        }
        if !asked && self.inner().last_ordered == hwnds { return Some(Ok(hwnds.len())); }
        let windows: Vec<order::Window> = hwnds.iter().map(|h| order::Window { hwnd: *h, minimized: game_window::is_minimized(*h) }).collect();
        let mut plan = order::plan(&windows, game_window::foreground().map(|(h, _)| h));
        plan.stack_only = !asked && refused_before;
        let outcome = order::apply(&plan, &|| self.shared.switches.load(Ordering::Acquire) != switches);
        {
            let mut inner = self.inner();
            if outcome.stopped { inner.order_pending = true; } else { inner.last_ordered = hwnds.clone(); }
            if outcome.refused { inner.alt_tab_refused = true; }
        }
        if outcome.taskbar > 0 || !plan.activate.is_empty() {
            let kind = match (why, plan.stack_only) {
                (None, _) => "asked".to_owned(),
                (Some(why), false) => why.to_owned(),
                (Some(why), true) => format!("{why}, stack only"),
            };
            self.diag(&format!("hotkeys: order ({kind}): {}", outcome.describe(hwnds.len())));
        }
        if asked && !outcome.stopped && !outcome.alt_tab_set() {
            return Some(Err(crate::i18n::t("Windows did not allow changing the Alt+Tab order.").into()));
        }
        Some(Ok(hwnds.len()))
    }

    /// The monitors now (for choosing where to arrange).
    pub(crate) fn monitors(&self) -> Vec<Monitor> { game_window::monitors() }

    /// Arrange every running game window with `preset` on the monitors named in `devices` (all,
    /// in Windows' order with the primary first, when empty), in the launcher's account order,
    /// and remember the result. Games that may hold their display (fullscreen) are left alone; a
    /// borderless game at its monitor's size is arranged (`game_window::holds_display`). Returns
    /// how many were placed.
    pub(crate) fn arrange(&self, preset: Preset, devices: &[String]) -> Result<usize, String> {
        let monitors = game_window::monitors();
        let chosen: Vec<&Monitor> = if devices.is_empty() { monitors.iter().collect() }
            else { devices.iter().filter_map(|d| monitors.iter().find(|m| &m.device == d)).collect() };
        let areas: Vec<_> = if chosen.is_empty() { monitors.iter().filter(|m| m.primary).map(|m| m.work).collect() } else { chosen.iter().map(|m| m.work).collect() };
        let (found, aspect) = {
            let inner = self.inner();
            (home_windows(&inner), inner.settings.layouts.aspect)
        };
        let front = game_window::foreground().map(|(hwnd, _)| hwnd);
        let placements: Vec<Option<game_window::Placement>> = found.iter().map(|(_, hwnd)| game_window::capture(*hwnd)).collect();
        let windows = arrangeable(&found, &placements, front);
        if windows.is_empty() {
            let text = if found.is_empty() { crate::i18n::t("No game window to arrange.") } else { crate::i18n::t("Fullscreen games are left alone.") };
            return Err(text.into());
        }
        let rects = layouts::arrange(preset, windows.len(), &areas, aspect);
        let mut placed = 0;
        let mut moved = Vec::new();
        for ((account, hwnd), rect) in windows.iter().zip(rects) {
            let before = found.iter().position(|(_, h)| h == hwnd).and_then(|i| placements[i].clone());
            if game_window::apply(*hwnd, rect, game_window::Shown::Normal) {
                placed += 1;
                self.remember(account, *hwnd);
                // Where it landed (a game may adjust the size it is given).
                let arranged = game_window::capture(*hwnd).map_or(rect, |p| p.rect);
                let id = self.with_game(account, |g| g.key.id());
                if let (Some(before), Some(id)) = (before, id) {
                    moved.push((id, layouts::Arranged { before: layouts::Saved::from_placement(&before), arranged }));
                }
            }
        }
        let undone = moved.len();
        if !moved.is_empty() {
            let saved = {
                let mut inner = self.inner();
                inner.settings.layouts.undo = next_undo(&inner.settings.layouts.undo, moved);
                self.changed();
                inner.load_error.is_none().then(|| store::save(&self.shared.config, &inner.settings))
            };
            if let Some(Err(error)) = saved { self.emit(Event::Error(error)); }
        }
        self.diag(&format!("features: arrange: {placed} of {} windows moved, {undone} can be restored", windows.len()));
        Ok(placed)
    }

    /// Whether Restore windows has something to do: an Arrange all moved a window of a game that
    /// runs now and has its window.
    pub(crate) fn can_restore_windows(&self) -> bool {
        let inner = self.inner();
        let undo = &inner.settings.layouts.undo;
        !undo.is_empty() && inner.games.values().any(|g| g.hwnd.is_some_and(game_window::exists) && undo.contains_key(&g.key.id()))
    }

    /// Undo the last Arrange all (the first of several in a row, `next_undo`), also after Crystal
    /// or the games restarted: each running game's window goes back exactly as it was (rectangle,
    /// show state and frame; `restore_step`), by the move the launch-time restore uses, and each
    /// client's remembered window goes back to that placement too, so its next start is the
    /// original one (Arrange all remembered the arranged one). A window that may hold its display
    /// now is left alone and stays restorable. One history line per window. Returns how many
    /// windows were put back.
    pub(crate) fn restore_windows(&self) -> Result<usize, String> {
        struct Target { place: usize, hwnd: isize }
        let (undo, targets) = {
            let mut inner = self.inner();
            let undo = std::mem::take(&mut inner.settings.layouts.undo);
            let targets: BTreeMap<String, Target> = home_candidates(&inner).into_iter().enumerate()
                .filter_map(|(i, c)| Some((inner.games[&c.account].key.id(), Target { place: i + 1, hwnd: c.window? })))
                .collect();
            (undo, targets)
        };
        if undo.is_empty() { return Err(crate::i18n::t("No arranged window is open.").into()); }
        let front = game_window::foreground().map(|(hwnd, _)| hwnd);
        let monitors = game_window::monitors();
        let (mut restored, mut open, mut held) = (0usize, 0usize, 0usize);
        let mut keep: BTreeMap<String, layouts::Arranged> = BTreeMap::new();
        let mut put_back: Vec<(String, layouts::Saved)> = Vec::new();
        for (id, entry) in undo {
            let Some(target) = targets.get(&id) else {
                // Not running: only its remembered window goes back.
                self.diag(&format!("features: restore -: {} -> {}: not running, remembered window put back", show(entry.arranged), show(entry.before.rect)));
                put_back.push((id, entry.before));
                continue;
            };
            let now = game_window::capture(target.hwnd);
            let from = now.as_ref().map_or_else(|| "?".to_owned(), |p| show(p.rect));
            match restore_step(&entry, now.as_ref(), front == Some(target.hwnd), &monitors) {
                None => {
                    self.diag(&format!("features: restore #{}: window unreadable, kept", target.place));
                    keep.insert(id, entry);
                }
                Some(Step::Held) => {
                    held += 1;
                    self.diag(&format!("features: restore #{}: {from}: may hold its display, left alone", target.place));
                    keep.insert(id, entry);
                }
                Some(Step::Move { rect, shown, frame }) => {
                    open += 1;
                    let ok = game_window::apply_frame(target.hwnd, rect, shown, frame);
                    let landed = game_window::capture(target.hwnd);
                    let result = match (&landed, ok) {
                        (_, false) => "failed".to_owned(),
                        (Some(p), true) if shown == game_window::Shown::Normal && p.rect != rect => format!("landed at {}", show(p.rect)),
                        (Some(p), true) if frame.is_some_and(|f| f != p.frame) => "ok, frame not taken".to_owned(),
                        _ => "ok".to_owned(),
                    };
                    self.diag(&format!("features: restore #{}: {from} -> {}: {result}", target.place, show(rect)));
                    if ok {
                        restored += 1;
                        put_back.push((id, entry.before));
                    } else {
                        keep.insert(id, entry);
                    }
                }
            }
        }
        let saved = {
            let mut inner = self.inner();
            // An Arrange all meanwhile (another thread) recorded its own undo: it wins.
            for (id, entry) in keep { inner.settings.layouts.undo.entry(id).or_insert(entry); }
            if inner.settings.layouts.remember {
                for (id, before) in put_back { inner.settings.layouts.saved.insert(id, before); }
            }
            self.changed();
            inner.load_error.is_none().then(|| store::save(&self.shared.config, &inner.settings))
        };
        if let Some(Err(error)) = saved { self.emit(Event::Error(error)); }
        if open == 0 {
            let text = if held > 0 { crate::i18n::t("Fullscreen games are left alone.") } else { crate::i18n::t("No arranged window is open.") };
            return Err(text.into());
        }
        Ok(restored)
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

/// The running games in Home order, each with its window now (none while it is still loading or
/// once closed) and whether that window is minimised.
fn home_candidates(inner: &Inner) -> Vec<hotkeys::Candidate> {
    home_order(inner).into_iter().map(|account| {
        let game = &inner.games[&account];
        let window = game.hwnd.filter(|h| game_window::exists(*h)).or_else(|| game_window::find_main_window(&game.pids));
        hotkeys::Candidate { minimized: window.is_some_and(game_window::is_minimized), window, account }
    }).collect()
}

/// The game windows' stacking for the history (`hotkeys::describe_z`): `windows` is each one with
/// its place in Home order.
fn stack_line(windows: &[(usize, isize)]) -> String {
    let hwnds: Vec<isize> = windows.iter().map(|(_, hwnd)| *hwnd).collect();
    let z: Vec<hotkeys::Stacked> = game_window::z_order(&hwnds).into_iter().filter_map(|hwnd| {
        let index = windows.iter().find(|(_, h)| *h == hwnd)?.0;
        let minimized = game_window::is_minimized(hwnd);
        Some(hotkeys::Stacked { index, minimized, hidden: !minimized && !game_window::is_shown(hwnd), topmost: game_window::is_topmost(hwnd) })
    }).collect();
    hotkeys::describe_z(&z)
}

/// What putting a hotkey's window in front of the other game windows took, for the history:
/// nothing to say when it was in front at once (or was not given the foreground); how long it
/// took to get there by itself; or how many were still in front of it when it was moved.
fn describe_raise(raised: Option<game_window::Raise>) -> String {
    match raised {
        Some(raise) if raise.over > 0 => {
            let lowered = if raise.lowered > 0 { format!(", {} always on top put behind it", raise.lowered) } else { String::new() };
            format!("; {} in front of it after {} ms: {}{lowered}", raise.over, raise.waited, if raise.top { "raised" } else { "NOT raised" })
        }
        Some(raise) if raise.waited > 0 => format!("; on top after {} ms", raise.waited),
        _ => String::new(),
    }
}

/// The running games that have a window now, in Home order, with that window.
fn home_windows(inner: &Inner) -> Vec<(String, isize)> {
    home_candidates(inner).into_iter().filter_map(|c| Some((c.account, c.window?))).collect()
}

/// The game windows Arrange all may move: `found` (Home order) with each one's placement now
/// (`None` when it could not be read) and the window in front. A game that may hold its display
/// is left alone; every other one is kept, in order.
fn arrangeable(found: &[(String, isize)], placements: &[Option<game_window::Placement>], front: Option<isize>) -> Vec<(String, isize)> {
    found.iter().zip(placements)
        .filter(|((_, hwnd), placement)| !placement.as_ref().is_some_and(|p| game_window::holds_display(p, front == Some(*hwnd))))
        .map(|(window, _)| window.clone())
        .collect()
}

/// The undo after an Arrange all that moved `moved` (by `ClientKey::id`). A window still where
/// the previous Arrange all left it keeps its earlier `before`, so a restore after several in a
/// row returns to the placement before the first; so does a game restarted since, which its
/// remembered window put back in the arranged place. One the user moved since starts again from
/// where it is now. A client this pass left alone (in front, or not running) keeps its entry.
fn next_undo(old: &BTreeMap<String, layouts::Arranged>, moved: Vec<(String, layouts::Arranged)>) -> BTreeMap<String, layouts::Arranged> {
    let mut next = old.clone();
    for (id, mut new) in moved {
        if let Some(prev) = old.get(&id) {
            if prev.arranged == new.before.rect { new.before = prev.before.clone(); }
        }
        next.insert(id, new);
    }
    next
}

/// What Restore windows does with one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// It may hold its display now (`game_window::holds_display`), as Arrange all also leaves alone.
    Held,
    /// Put it here, shown so, with this frame (`None`: leave the frame as it is).
    Move { rect: game_window::Rect, shown: game_window::Shown, frame: Option<game_window::Frame> },
}

/// Where Restore windows puts `entry`'s window back, given its placement `now` (`None` when it
/// can't be read). On its unchanged monitor it goes back exactly, shown as it was (a borderless
/// game covering its monitor gets the whole monitor again); when that monitor changed, it is
/// placed as a remembered window is at launch (`layouts::restore_rect`). The frame goes back with it.
fn restore_step(entry: &layouts::Arranged, now: Option<&game_window::Placement>, in_front: bool, monitors: &[Monitor]) -> Option<Step> {
    let now = now?;
    // Only the window as it is now decides this: the place it goes back to may well cover its
    // monitor (FFXI's borderless full screen), which is what it is being restored to.
    if game_window::holds_display(now, in_front) { return Some(Step::Held); }
    let before = &entry.before;
    let monitor = &before.monitor;
    let frame = before.frame;
    if monitors.iter().any(|m| m.device == monitor.device && m.rect == monitor.rect && m.work == monitor.work && m.dpi == monitor.dpi) {
        return Some(Step::Move { rect: before.rect, shown: before.shown, frame });
    }
    let (rect, shown) = layouts::restore_rect(before, monitors)?;
    Some(Step::Move { rect, shown, frame })
}

/// `(x,y wxh)` for the history.
fn show(r: game_window::Rect) -> String { format!("({},{} {}x{})", r.x, r.y, r.w, r.h) }

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
    fn the_history_says_what_raising_a_window_took_and_how_the_games_are_stacked() {
        let raise = |waited, over, lowered, top| Some(game_window::Raise { waited, over, lowered, top });
        assert_eq!(describe_raise(None), "");
        assert_eq!(describe_raise(raise(0, 0, 0, true)), "", "in front at once: nothing to say");
        assert_eq!(describe_raise(raise(35, 0, 0, true)), "; on top after 35 ms", "the game's own activation got it there");
        assert_eq!(describe_raise(raise(150, 2, 0, true)), "; 2 in front of it after 150 ms: raised");
        assert_eq!(describe_raise(raise(150, 1, 1, true)), "; 1 in front of it after 150 ms: raised, 1 always on top put behind it");
        assert_eq!(describe_raise(raise(150, 1, 0, false)), "; 1 in front of it after 150 ms: NOT raised");
        // Windows that are gone are in no stacking.
        assert_eq!(stack_line(&[(1, 0x7FFF_FFF1), (2, 0x7FFF_FFF3)]), "no game window");
        assert_eq!(stack_line(&[]), "no game window");
    }

    #[test]
    fn a_hotkey_with_no_game_window_switches_nothing_and_holds_ordering_only_meanwhile() {
        let (runtime, dir, _) = runtime();
        runtime.game_started(GameStart { account: "a".into(), slot: None, label: String::new(), pid: std::process::id() });
        // No window to switch to: nothing is activated, and the press is still counted (an
        // ordering pass running on another thread stops for it).
        runtime.run_action(&Action::Next, desktop::Via::Hook, None);
        assert_eq!(runtime.shared.switches.load(Ordering::Acquire), 1);
        assert!(!runtime.shared.ordering.load(Ordering::Acquire), "released after the switch");
        // While a pass (or another switch) holds the flag, an ordering pass does not start.
        let held = OrderingHeld::take(&runtime.shared.ordering).unwrap();
        assert!(OrderingHeld::take(&runtime.shared.ordering).is_none());
        assert_eq!(runtime.order(Some("test")), None);
        drop(held);
        assert_eq!(runtime.order(Some("test")), Some(Ok(0)), "no window: nothing to order");
        runtime.game_ended(GameEnd { account: "a".into(), report: None, user_closed: true, maintenance: false });
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The owner's five clients as Windows reported them (2026-11-10): FFXI borderless (`0034` =
    /// 3) at the monitor's 3840x2160, each an unowned WS_POPUP window covering DISPLAY1 exactly,
    /// and Crystal's own window in front while Arrange all is clicked.
    fn covering(shown: game_window::Shown) -> game_window::Placement {
        let monitor = game_window::Monitor {
            device: r"\\.\DISPLAY1".into(), rect: game_window::Rect::new(0, 0, 3840, 2160),
            work: game_window::Rect::new(0, 0, 3840, 2088), dpi: 144, primary: true,
        };
        game_window::Placement {
            rect: monitor.rect, client: (3840, 2160), shown, mode: game_window::Mode::Fullscreen,
            frame: game_window::Frame::from_styles(0x9400_0000, 0), monitor,
        }
    }

    #[test]
    fn arrange_all_moves_borderless_games_at_their_monitors_size() {
        let found: Vec<(String, isize)> = (1..=5).map(|i| (format!("acct{i}"), 0x1000 + i as isize)).collect();
        let placements: Vec<_> = found.iter().map(|_| Some(covering(game_window::Shown::Normal))).collect();
        // Crystal in front: none of them holds the display, all five are arranged in Home order.
        assert_eq!(arrangeable(&found, &placements, Some(0x9999)), found);
        assert_eq!(arrangeable(&found, &placements, None), found);
        // The game in front may be fullscreen: only it is left alone.
        let rest: Vec<_> = found.iter().filter(|(_, h)| *h != 0x1003).cloned().collect();
        assert_eq!(arrangeable(&found, &placements, Some(0x1003)), rest);
        // A minimised one covering its monitor may be a fullscreen game that lost the display.
        let mut minimised = placements.clone();
        minimised[0] = Some(covering(game_window::Shown::Minimized));
        assert_eq!(arrangeable(&found, &minimised, Some(0x9999)), found[1..].to_vec());
        // Windowed and smaller borderless games always move; an unreadable window is kept (its
        // move fails on its own).
        let mut mixed = placements.clone();
        mixed[1] = Some(game_window::Placement { mode: game_window::Mode::Windowed, ..covering(game_window::Shown::Minimized) });
        mixed[2] = None;
        assert_eq!(arrangeable(&found[..3], &mixed[..3], Some(0x1001)), found[1..3].to_vec());
    }

    /// A windowed game at `rect` on the owner's DISPLAY1.
    fn windowed(rect: game_window::Rect, shown: game_window::Shown) -> game_window::Placement {
        game_window::Placement {
            rect, client: (rect.w - 16, rect.h - 39), shown, mode: game_window::Mode::Windowed,
            frame: game_window::Frame::from_styles(0x14CF_0000, 0x300), ..covering(shown)
        }
    }

    /// A borderless window at `rect` (where Arrange all leaves a borderless game).
    fn tiled(rect: game_window::Rect) -> game_window::Placement {
        game_window::Placement { rect, client: (rect.w, rect.h), mode: game_window::Mode::Borderless, ..covering(game_window::Shown::Normal) }
    }

    /// Arrange all moving clients `ids` (each now at `now`) to `to`: what it records.
    fn arranged(ids: &[&str], now: &[game_window::Placement], to: &[game_window::Rect]) -> Vec<(String, layouts::Arranged)> {
        ids.iter().zip(now).zip(to)
            .map(|((id, before), arranged)| (id.to_string(), layouts::Arranged { before: layouts::Saved::from_placement(before), arranged: *arranged }))
            .collect()
    }

    fn monitors() -> Vec<Monitor> { vec![covering(game_window::Shown::Normal).monitor] }

    fn moved_to(step: Option<Step>) -> Option<(game_window::Rect, game_window::Shown, Option<game_window::Frame>)> {
        match step { Some(Step::Move { rect, shown, frame }) => Some((rect, shown, frame)), _ => None }
    }

    #[test]
    fn restore_windows_puts_the_owners_borderless_games_back_over_the_whole_monitor() {
        use game_window::Shown;
        let ids = ["acct1", "acct2", "acct3", "acct4", "acct5"];
        let original = vec![covering(Shown::Normal); 5];
        let rects = layouts::arrange(Preset::MainLarge, 5, &[original[0].monitor.work], 16.0 / 9.0);
        let undo = next_undo(&BTreeMap::new(), arranged(&ids, &original, &rects));
        // The tiles as the live windows showed them after Arrange all, Crystal in front.
        assert_eq!(rects[0], game_window::Rect::new(0, 234, 2880, 1620));
        assert_eq!(rects[1], game_window::Rect::new(2896, 0, 928, 522));
        for (id, rect) in ids.iter().zip(&rects) {
            let now = tiled(*rect);
            assert!(!game_window::holds_display(&now, false));
            // The target covers the monitor; only the window's state now decides "fullscreen".
            let step = restore_step(&undo[*id], Some(&now), false, &monitors());
            assert_eq!(moved_to(step), Some((original[0].monitor.rect, Shown::Normal, Some(original[0].frame))), "{id}");
        }
        // Even in front, a tiled window is not holding its display: it goes back too.
        assert!(moved_to(restore_step(&undo["acct1"], Some(&tiled(rects[0])), true, &monitors())).is_some());
    }

    #[test]
    fn restore_windows_returns_each_window_to_its_place_before_arrange_all() {
        use game_window::{Rect, Shown};
        let ids = ["acct1", "acct2", "acct3"];
        let original = vec![
            windowed(Rect::new(100, 80, 1296, 759), Shown::Normal),
            // Covering the monitor behind Crystal: borderless at monitor size, arranged.
            covering(Shown::Normal),
            windowed(Rect::new(-8, 1200, 1936, 1119), Shown::Maximized),
        ];
        let grid = layouts::arrange(Preset::Grid, 3, &[original[0].monitor.work], 16.0 / 9.0);
        let undo = next_undo(&BTreeMap::new(), arranged(&ids, &original, &grid));
        for ((id, p), r) in ids.iter().zip(&original).zip(&grid) {
            let step = restore_step(&undo[*id], Some(&windowed(*r, Shown::Normal)), false, &monitors());
            assert_eq!(moved_to(step), Some((p.rect, p.shown, Some(p.frame))), "{id}: the exact rect, show state and frame");
        }
        // A changed monitor: placed as a remembered window is at launch (a covering one fills it).
        let mut smaller = monitors();
        smaller[0].rect = Rect::new(0, 0, 2560, 1440);
        smaller[0].work = Rect::new(0, 0, 2560, 1392);
        let step = restore_step(&undo["acct2"], Some(&windowed(grid[1], Shown::Normal)), false, &smaller);
        assert_eq!(moved_to(step).map(|(r, ..)| r), Some(Rect::new(0, 0, 2560, 1440)));
    }

    #[test]
    fn a_second_arrange_all_or_a_restart_keeps_the_placement_before_the_first() {
        use game_window::{Rect, Shown};
        let ids = ["acct1", "acct2"];
        let original = vec![covering(Shown::Normal), windowed(Rect::new(900, 600, 1296, 759), Shown::Normal)];
        let area = [original[0].monitor.work];
        let large = layouts::arrange(Preset::MainLarge, 2, &area, 16.0 / 9.0);
        let grid = layouts::arrange(Preset::Grid, 2, &area, 16.0 / 9.0);
        let first = next_undo(&BTreeMap::new(), arranged(&ids, &original, &large));
        // Arranged again, untouched since (or the games restarted and their remembered windows put
        // them back in the arranged place): the undo still holds the original placement.
        let at_large: Vec<_> = large.iter().map(|r| tiled(*r)).collect();
        let second = next_undo(&first, arranged(&ids, &at_large, &grid));
        assert_eq!(second["acct1"].before.rect, original[0].rect);
        assert_eq!(second["acct2"].before.rect, original[1].rect);
        assert_eq!(second["acct1"].arranged, grid[0]);
        // The user moved the second one after the first Arrange all: it starts again from there.
        let dragged = windowed(Rect::new(40, 40, 1296, 759), Shown::Normal);
        let third = next_undo(&first, arranged(&ids, &[at_large[0].clone(), dragged.clone()], &grid));
        assert_eq!((third["acct1"].before.rect, third["acct2"].before.rect), (original[0].rect, dragged.rect));
        // A client the second pass left alone (not running, say) keeps its entry.
        let partial = next_undo(&first, arranged(&ids[..1], &at_large[..1], &grid[..1]));
        assert_eq!(partial.len(), 2);
        assert_eq!(partial["acct2"], first["acct2"]);
    }

    #[test]
    fn restore_windows_skips_unreadable_windows_and_games_holding_their_display() {
        use game_window::{Rect, Shown};
        let ids = ["acct1", "acct2"];
        let original = vec![windowed(Rect::new(0, 0, 1296, 759), Shown::Normal); 2];
        let grid = layouts::arrange(Preset::Grid, 2, &[original[0].monitor.work], 16.0 / 9.0);
        let undo = next_undo(&BTreeMap::new(), arranged(&ids, &original, &grid));
        assert_eq!(restore_step(&undo["acct1"], None, false, &monitors()), None);
        // It went fullscreen and is in front, or covers its monitor minimised: left alone.
        assert_eq!(restore_step(&undo["acct2"], Some(&covering(Shown::Normal)), true, &monitors()), Some(Step::Held));
        assert_eq!(restore_step(&undo["acct2"], Some(&covering(Shown::Minimized)), false, &monitors()), Some(Step::Held));
        // Covering its monitor behind Crystal is FFXI's borderless full screen: it moves.
        assert!(moved_to(restore_step(&undo["acct2"], Some(&covering(Shown::Normal)), false, &monitors())).is_some());
    }

    #[test]
    fn the_undo_survives_a_restart_and_restore_puts_the_remembered_window_back() {
        let (runtime, dir, _) = runtime();
        let original = covering(game_window::Shown::Normal);
        let tile = game_window::Rect::new(2896, 0, 928, 522);
        // What Arrange all leaves behind: the tile remembered (so the game would start there, at
        // 928x522), and the undo, saved.
        runtime.update(|s| {
            layouts::remember(&mut s.layouts, &ClientKey::new("a", None), &tiled(tile));
            s.layouts.undo = next_undo(&BTreeMap::new(), arranged(&["a"], &[original.clone()], &[tile]));
        }).unwrap();
        assert_eq!(layouts::launch_values(&runtime.settings().layouts, &ClientKey::new("a", None)), vec![("0001", 928), ("0002", 522), ("0034", 3)]);
        // Crystal restarts: the undo is still there.
        let config = Config { experiment: dir.clone(), ..Config::default() };
        let again = Runtime::new(config.clone(), Arc::new(|_| {}));
        assert_eq!(again.settings().layouts.undo.len(), 1);
        // No game runs: nothing is offered, and a restore puts back only the remembered window.
        assert!(!again.can_restore_windows());
        assert!(again.restore_windows().is_err(), "no window was put back");
        let after = store::load(&config).unwrap();
        assert!(after.layouts.undo.is_empty());
        assert_eq!(after.layouts.saved["a"], layouts::Saved::from_placement(&original));
        assert!(layouts::launch_values(&after.layouts, &ClientKey::new("a", None)).is_empty(), "it starts at its own resolution again");
        // Nothing left to undo: nothing is saved or said beyond that.
        assert!(again.restore_windows().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restore_windows_is_offered_only_while_an_arranged_window_is_open() {
        let (runtime, dir, _) = runtime();
        assert!(!runtime.can_restore_windows(), "no Arrange all yet");
        assert!(runtime.restore_windows().is_err());
        // A game running without a window (a windowless stand-in) offers nothing either.
        runtime.game_started(GameStart { account: "a".into(), slot: None, label: String::new(), pid: std::process::id() });
        runtime.inner().settings.layouts.undo = next_undo(&BTreeMap::new(),
            arranged(&["a"], &[covering(game_window::Shown::Normal)], &[game_window::Rect::new(0, 0, 10, 10)]));
        assert!(!runtime.can_restore_windows());
        runtime.game_ended(GameEnd { account: "a".into(), report: None, user_closed: true, maintenance: false });
        assert_eq!(runtime.settings().layouts.undo.len(), 1, "a game ending keeps its undo");
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
                mode: game_window::Mode::Windowed, frame: None,
                monitor: layouts::MonitorRef { device: "x".into(), rect: game_window::Rect::new(0, 0, 1920, 1080), work: game_window::Rect::new(0, 0, 1920, 1040), dpi: 96 },
            });
        }).unwrap();
        assert_eq!(launch_values(&config, "a", Some(4)), vec![("0001", 1280), ("0002", 720), ("0034", 1), ("0035", 0)]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
