//! The launcher features' view model (ui/features.slint): `wire` turns the `Features` global's
//! callbacks into `Message::Feature` (or the core message an account's own control sends), and
//! `sync` pushes `App::features_view`, the runtime's saved settings and its read-outs into it
//! after each batch of messages.
//!
//! Work is skipped when its inputs did not move: the relaunch banner and the open section are
//! rebuilt only when the language, privacy mode, the accounts, the runtime's revision or the
//! features state's revision changed, and only the section on screen is built. Within a
//! rebuild only changed rows are set. Nothing here reads files: the launch profiles come read
//! from the core (`features_view.profiles`).
//!
//! Privacy mode masks every account label and character name, as Home does. Launch profile
//! names, the display device and the gamepad line are not masked: the account editor's own
//! profile picker shows profile names unmasked in both builds, and the others name no one.
//! Nothing here animates or polls: a relaunch's time is shown as a clock time, so no countdown
//! ticks.
use super::{send, BackupLine, ClientLine, FeatureStrings, Features, HotkeyLine, LayoutLine, MainWindow, MapTile, PlayLine, RelaunchLine};
use crate::features::hotkeys::{Action, Status};
use crate::features::game_window::Rect;
use crate::features::layouts::{self, Preset};
use crate::features::perf::Priority;
use crate::features::profiles::{self, GamepadSource, Tool};
use crate::features::recovery::{Decision, EndKind, Held};
use crate::features::ui::{self as fui, Msg, Section, Target};
use crate::features::{discord, ClientKey, FeatureSettings};
use crate::i18n::{t, tf, Lang};
use crate::{App, Message};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Instant, SystemTime};

/// What a part of the sheet was last built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Inputs {
    section: Option<Section>,
    lang: Lang,
    privacy: bool,
    accounts: u64,
    runtime: u64,
    view: u64,
}

/// What the callbacks act on, as of the last sync, and the models the window draws from.
struct State {
    lang: Option<Lang>,
    /// Account keys in Home's order (hotkeys and clients rows).
    accounts: Vec<String>,
    accounts_print: Option<u64>,
    banner_built: Option<Inputs>,
    section_built: Option<Inputs>,
    layout_ids: Vec<String>,
    backup_paths: Vec<PathBuf>,
    keep_values: Vec<u32>,
    monitor_devices: Vec<Option<String>>,
    /// What the monitor map draws from, as of the last sync.
    map: MapInputs,
    /// The relaunch banner's rows: (account, scheduled).
    relaunches: Vec<(String, bool)>,
    sections: Rc<VecModel<SharedString>>,
    section_notes: Rc<VecModel<SharedString>>,
    monitors: Rc<VecModel<SharedString>>,
    monitor_names: Rc<VecModel<SharedString>>,
    map_tiles: Rc<VecModel<MapTile>>,
    keep_items: Rc<VecModel<SharedString>>,
    priorities: Rc<VecModel<SharedString>>,
    layouts: Rc<VecModel<LayoutLine>>,
    cycle_hotkeys: Rc<VecModel<HotkeyLine>>,
    account_hotkeys: Rc<VecModel<HotkeyLine>>,
    backups: Rc<VecModel<BackupLine>>,
    clients: Rc<VecModel<ClientLine>>,
    playtime: Rc<VecModel<PlayLine>>,
    relaunch_lines: Rc<VecModel<RelaunchLine>>,
}

/// The game windows and monitor work areas the map is drawn from.
#[derive(Default)]
struct MapInputs {
    /// Every monitor: (device, work area), in the order Arrange all uses.
    areas: Vec<(String, Rect)>,
    /// The running games in account order: (account label, in game).
    windows: Vec<(String, bool)>,
    aspect: f32,
}

impl State {
    fn new() -> Self {
        Self {
            lang: None,
            accounts: Vec::new(),
            accounts_print: None,
            banner_built: None,
            section_built: None,
            layout_ids: Vec::new(),
            backup_paths: Vec::new(),
            keep_values: Vec::new(),
            monitor_devices: Vec::new(),
            map: MapInputs::default(),
            relaunches: Vec::new(),
            sections: Rc::default(),
            section_notes: Rc::default(),
            monitors: Rc::default(),
            monitor_names: Rc::default(),
            map_tiles: Rc::default(),
            keep_items: Rc::default(),
            priorities: Rc::default(),
            layouts: Rc::default(),
            cycle_hotkeys: Rc::default(),
            account_hotkeys: Rc::default(),
            backups: Rc::default(),
            clients: Rc::default(),
            playtime: Rc::default(),
            relaunch_lines: Rc::default(),
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The window closed: the page's models go with it (nothing stays in the background).
pub(super) fn forget() {
    STATE.with(|cell| cell.borrow_mut().take());
}

fn read<R>(f: impl FnOnce(&State) -> Option<R>) -> Option<R> {
    STATE.with(|cell| cell.borrow().as_ref().and_then(f))
}

fn feature(msg: Msg) { send(Message::Feature(msg)); }

fn index(i: i32) -> Option<usize> { usize::try_from(i).ok() }

fn account_at(i: i32) -> Option<String> { read(|s| s.accounts.get(index(i)?).cloned()) }

/// The `toggle(int, bool)` callback's switches, in ui/features.slint's documented order.
fn toggle(i: i32, on: bool) -> Option<Msg> {
    Some(match i {
        0 => Msg::Remember(on),
        1 => Msg::RestoreLayouts(on),
        2 => Msg::LaunchAtSavedSize(on),
        3 => Msg::Offer(on),
        4 => Msg::AutoRelaunch(on),
        5 => Msg::BeforeUpdate(on),
        6 => Msg::Notify(on),
        7 => Msg::NotifyMail(on),
        8 => Msg::NotifyFriends(on),
        9 => Msg::BackgroundPriority(on),
        10 => Msg::MuteBackground(on),
        11 => Msg::RecordPlaytime(on),
        12 => Msg::Discord(on),
        13 => Msg::OrderWindows(on),
        _ => return None,
    })
}

/// The priority a client's dropdown index picks: 0 is the Windows default (`Some(None)`), then
/// `Priority::ALL` in order; anything else is no choice.
fn priority_choice(choice: i32) -> Option<Option<Priority>> {
    match index(choice)? {
        0 => Some(None),
        c => Priority::ALL.get(c - 1).copied().map(Some),
    }
}

/// Connect the `Features` global's callbacks.
pub(super) fn wire(ui: &MainWindow) {
    let state = State::new();
    let g = ui.global::<Features>();
    g.set_sections(ModelRc::from(state.sections.clone()));
    g.set_section_notes(ModelRc::from(state.section_notes.clone()));
    g.set_monitors(ModelRc::from(state.monitors.clone()));
    g.set_monitor_names(ModelRc::from(state.monitor_names.clone()));
    g.set_map_tiles(ModelRc::from(state.map_tiles.clone()));
    g.set_keep_items(ModelRc::from(state.keep_items.clone()));
    g.set_priorities(ModelRc::from(state.priorities.clone()));
    g.set_layouts(ModelRc::from(state.layouts.clone()));
    g.set_cycle_hotkeys(ModelRc::from(state.cycle_hotkeys.clone()));
    g.set_account_hotkeys(ModelRc::from(state.account_hotkeys.clone()));
    g.set_backups(ModelRc::from(state.backups.clone()));
    g.set_clients(ModelRc::from(state.clients.clone()));
    g.set_playtime(ModelRc::from(state.playtime.clone()));
    g.set_relaunches(ModelRc::from(state.relaunch_lines.clone()));
    STATE.with(|cell| *cell.borrow_mut() = Some(state));

    g.on_open_sheet(|open| feature(Msg::Open(open)));
    g.on_section_picked(|i| { if let Some(section) = index(i).and_then(|i| Section::ALL.get(i)) { feature(Msg::Section(*section)); } });
    g.on_toggle(|i, on| { if let Some(msg) = toggle(i, on) { feature(msg); } });
    let weak = ui.as_weak();
    g.on_arrange(move |preset| {
        let preset = if preset == 1 { Preset::MainLarge } else { Preset::Grid };
        let picked = weak.upgrade().map_or(0, |ui| ui.global::<Features>().get_monitor());
        let device = read(|s| s.monitor_devices.get(index(picked)?).cloned()).flatten();
        feature(Msg::Arrange(preset, device));
    });
    g.on_restore_windows(|| feature(Msg::RestoreWindows));
    let weak = ui.as_weak();
    g.on_map_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        STATE.with(|cell| { if let Some(state) = cell.borrow().as_ref() { map_tiles(&ui.global::<Features>(), state); } });
    });
    g.on_forget(|i| { if let Some(id) = read(|s| s.layout_ids.get(index(i)?).cloned()) { feature(Msg::Forget(id)); } });
    g.on_hotkey_entered(|i, text| {
        let target = match i {
            0 => Some(Target::Next),
            1 => Some(Target::Previous),
            _ => account_at(i - 2).map(Target::Account),
        };
        if let Some(target) = target { feature(Msg::Hotkey(target, text.into())); }
    });
    g.on_relaunch_cancel(|i| { if let Some((key, _)) = read(|s| s.relaunches.get(index(i)?).cloned()) { feature(Msg::CancelRelaunch(key)); } });
    // Relaunch is the row's own Play.
    g.on_relaunch_now(|i| { if let Some((key, _)) = read(|s| s.relaunches.get(index(i)?).cloned()) { send(Message::Launch(key)); } });
    g.on_relaunch_dismiss(|i| { if let Some((key, _)) = read(|s| s.relaunches.get(index(i)?).cloned()) { feature(Msg::DismissRecovery(key)); } });
    g.on_back_up_now(|| feature(Msg::BackupNow));
    g.on_restore_ask(|i| { if let Some(path) = read(|s| s.backup_paths.get(index(i)?).cloned()) { feature(Msg::AskRestore(path)); } });
    g.on_restore_confirm(|| feature(Msg::RestoreBackup));
    g.on_restore_cancel(|| feature(Msg::CancelRestore));
    g.on_keep_picked(|i| { if let Some(keep) = read(|s| s.keep_values.get(index(i)?).copied()) { feature(Msg::Keep(keep)); } });
    g.on_priority_picked(|i, choice| {
        if let (Some(key), Some(priority)) = (account_at(i), priority_choice(choice)) { feature(Msg::Priority(key, priority)); }
    });
    g.on_processors_entered(|i, text| { if let Some(key) = account_at(i) { feature(Msg::Affinity(key, text.into())); } });
    g.on_mute_client(|i, on| { if let Some(key) = account_at(i) { feature(Msg::MuteClient(key, on)); } });
    // An account's launch profile is the account sheet's: the features sheet steps aside for it.
    g.on_edit_account(|i| {
        if let Some(key) = account_at(i) {
            feature(Msg::Open(false));
            send(Message::EditAccount(key));
        }
    });
}

macro_rules! put {
    ($view:expr, $get:ident, $set:ident, $value:expr) => {{
        let value = $value;
        if $view.$get() != value {
            $view.$set(value);
        }
    }};
}

fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let old = model.row_count();
    for (i, row) in rows.iter().enumerate() {
        if i < old {
            if model.row_data(i).as_ref() != Some(row) { model.set_row_data(i, row.clone()); }
        } else {
            model.push(row.clone());
        }
    }
    for i in (rows.len()..old).rev() { model.remove(i); }
}

fn strings_model(model: &VecModel<SharedString>, items: impl IntoIterator<Item = String>) {
    sync_model(model, items.into_iter().map(SharedString::from).collect());
}

/// A fingerprint of what the sheet shows of the accounts: their order, labels, whether each
/// runs, and what picks its launch profile. No allocation.
fn accounts_print(app: &App) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for row in &app.accounts {
        let account = &row.account;
        (&row.key, &account.label, row.pid.is_some()).hash(&mut hash);
        std::mem::discriminant(&account.mode).hash(&mut hash);
        account.launch_with.as_ref().map(std::mem::discriminant).hash(&mut hash);
        (&account.windower_profile, &account.ashita_boot_ini).hash(&mut hash);
    }
    std::mem::discriminant(&app.settings.launch_with).hash(&mut hash);
    (&app.settings.windower_profile, &app.settings.ashita_boot_ini).hash(&mut hash);
    hash.finish()
}

pub(super) fn sync(ui: &MainWindow, app: &App) {
    let g = ui.global::<Features>();
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(State::new);
        let lang = crate::i18n::current();
        if state.lang != Some(lang) {
            state.lang = Some(lang);
            put!(g, get_strings, set_strings, strings());
            strings_model(&state.sections, Section::ALL.iter().map(|s| section_title(*s).to_owned()));
            strings_model(&state.priorities, std::iter::once(t("Windows default").to_owned()).chain(Priority::ALL.iter().map(|p| p.label().to_owned())));
        }
        let print = accounts_print(app);
        if state.accounts_print != Some(print) {
            state.accounts_print = Some(print);
            state.accounts = app.accounts.iter().map(|row| row.key.clone()).collect();
        }
        let view = &app.features_view;
        let inputs = Inputs {
            section: None,
            lang,
            privacy: crate::privacy::on(),
            accounts: print,
            runtime: app.features.as_ref().map_or(0, |r| r.revision()),
            view: view.revision,
        };
        if state.banner_built != Some(inputs) {
            state.banner_built = Some(inputs);
            relaunches(app, state);
        }
        sheet_gamepad(&g, app);
        // --preview-modal-launch: the account sheet scrolled to its Launch settings.
        put!(g, get_sheet_scroll, set_sheet_scroll, if crate::preview::has("--preview-modal-launch") { 2000.0 } else { 0.0 });
        put!(g, get_open, set_open, view.open);
        if !view.open {
            state.section_built = None;
            return;
        }
        // Discord's state changes on its own thread, with nothing else moving.
        if view.section == Section::Activity { discord_status(&g, app); }
        let inputs = Inputs { section: Some(view.section), ..inputs };
        if state.section_built == Some(inputs) { return; }
        state.section_built = Some(inputs);
        let settings = app.features.as_ref().map(|r| r.settings()).unwrap_or_default();
        put!(g, get_section, set_section, Section::ALL.iter().position(|s| *s == view.section).unwrap_or(0) as i32);
        let (error, notice) = view.notice.clone().unwrap_or_default();
        let notice = app.features.as_ref().and_then(|r| r.load_error()).filter(|_| notice.is_empty()).map_or((error, notice), |e| (true, e));
        put!(g, get_notice, set_notice, notice.1.into());
        put!(g, get_notice_error, set_notice_error, notice.0);
        strings_model(&state.section_notes, section_notes(app, &settings));
        // The map and the Windows tile name the layout and monitor whichever section is open.
        windows(&g, app, &settings, state);
        match view.section {
            Section::Windows => {}
            Section::Hotkeys => hotkeys(app, &settings, state),
            Section::Recovery => recovery(&g, &settings),
            Section::Backups => backups(&g, app, &settings, state),
            Section::Clients => clients(&g, app, &settings, state),
            Section::Activity => activity(&g, app, &settings, state),
        }
    });
}

fn strings() -> FeatureStrings {
    FeatureStrings {
        entry: t("Features").into(),
        caption: t("Launcher").to_uppercase().into(),
        title: t("Features").into(),
        close: t("Close").into(),
        layouts: t("Window layouts").to_uppercase().into(),
        remember: t("Remember each game window").into(),
        remember_note: t("Saves its place and size when it moves.").into(),
        restore: t("Put windows back at launch").into(),
        restore_note: t("Per account, and per character slot with automatic login.").into(),
        launch_saved: t("Start at the saved size").into(),
        launch_saved_note: t("The game opens at the remembered resolution.").into(),
        order_windows: t("Order game windows").into(),
        order_windows_note: t("Taskbar and Alt+Tab follow the Home order.").into(),
        arrange: t("Arrange all").into(),
        restore_windows: t("Restore windows").into(),
        monitor: t("Monitor").into(),
        layout: t("Layout").into(),
        grid: t("Grid").into(),
        main_large: t("Main large").into(),
        list_sep: t(", ").into(),
        arrange_note: t("Fullscreen games are left alone.").into(),
        in_game: t("In game").into(),
        no_windows: t("No game windows open").into(),
        remembered: t("Remembered windows").into(),
        no_layouts: t("No windows remembered yet.").into(),
        forget: t("Forget").into(),
        switch_windows: t("Switch windows").into(),
        per_account: t("Per account").into(),
        hotkey_placeholder: t("e.g. Ctrl+Alt+1").into(),
        hotkey_note: t("Press Return to apply. Leave blank for none.").into(),
        no_accounts: t("No accounts yet").into(),
        recovery: t("Crash recovery").into(),
        offer: t("Offer Relaunch after a crash or disconnect").into(),
        offer_note: t("Shown above the play bar.").into(),
        auto_relaunch: t("Relaunch automatically").into(),
        auto_note: SharedString::new(),
        pending: t("Waiting").into(),
        no_pending: t("Nothing waiting to relaunch.").into(),
        backups: t("USER data").into(),
        back_up_now: t("Back up now").into(),
        before_update: t("Back up before Update and Repair").into(),
        keep: t("Keep").into(),
        saved_backups: t("Saved backups").into(),
        no_backups: t("No backups yet.").into(),
        restore_action: t("Restore files").into(),
        restore_question: t("Replace the current files with this backup? They are backed up first.").into(),
        cancel: t("Cancel").into(),
        background: t("Background clients").into(),
        background_priority: t("Lower their priority").into(),
        background_priority_note: t("Below normal until brought forward.").into(),
        mute_background: t("Mute them").into(),
        mute_background_note: t("Applies at the next launch.").into(),
        priority: t("Priority").into(),
        processors: t("Processors").into(),
        processors_placeholder: t("All").into(),
        mute_client: t("Mute in background").into(),
        edit_account: t("Edit account").into(),
        running: t("Running").into(),
        notifications: t("Notifications").into(),
        notify: t("Show desktop notifications").into(),
        notify_mail: t("New mail").into(),
        notify_friends: t("Friends coming online").into(),
        discord: "Discord".into(),
        discord_switch: t("Show play status in Discord").into(),
        playtime: t("Play time").into(),
        record: t("Record play time").into(),
        no_playtime: t("No play time recorded yet.").into(),
        relaunch: t("Relaunch").into(),
        dismiss: t("Dismiss").into(),
    }
}

/// A wall-clock time for `at` ("14:32").
fn clock(at: Instant) -> String {
    let wall = SystemTime::now() + at.saturating_duration_since(Instant::now());
    chrono::DateTime::<chrono::Local>::from(wall).format("%H:%M").to_string()
}

fn date(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// The banner's rows: scheduled relaunches first in account order, then offers.
fn relaunches(app: &App, state: &mut State) {
    let mut keys = Vec::new();
    let mut lines = Vec::new();
    if let Some(runtime) = &app.features {
        for row in &app.accounts {
            if let Some((at, attempt)) = runtime.pending_relaunch(&row.key) {
                let name = crate::privacy::mask(&row.account.label);
                lines.push(RelaunchLine {
                    text: tf("Relaunching {name} at {time} (try {attempt})", &[("name", &name), ("time", &clock(at)), ("attempt", &attempt)]).into(),
                    scheduled: true,
                });
                keys.push((row.key.clone(), true));
            }
        }
    }
    for (key, decision) in &app.features_view.offers {
        if keys.iter().any(|(k, _)| k == key) { continue; }
        let Decision::Offer { kind, held } = decision else { continue };
        let name = app.feature_account_label(key);
        let mut text = match kind {
            EndKind::Crash => tf("{name} crashed.", &[("name", &name)]),
            EndKind::Disconnect => tf("{name} lost its connection.", &[("name", &name)]),
            EndKind::UserClosed | EndKind::Unknown => tf("{name} closed unexpectedly.", &[("name", &name)]),
        };
        match held {
            Held::Maintenance => { text.push(' '); text.push_str(t("No automatic relaunch during maintenance.")); }
            Held::Limit => { text.push(' '); text.push_str(t("Automatic relaunch limit reached.")); }
            Held::Startup => { text.push(' '); text.push_str(t("It failed while starting.")); }
            Held::Off | Held::NotAutomatic => {}
        }
        lines.push(RelaunchLine { text: text.into(), scheduled: false });
        keys.push((key.clone(), false));
    }
    state.relaunches = keys;
    sync_model(&state.relaunch_lines, lines);
}

/// The launch profiles the core read for the detected tool folders (none yet: empty lists).
fn lists(app: &App) -> std::borrow::Cow<'_, profiles::Lists> {
    match &app.features_view.profiles {
        Some((_, lists)) => std::borrow::Cow::Borrowed(lists.as_ref()),
        None => std::borrow::Cow::Owned(profiles::Lists::default()),
    }
}

fn defaults(app: &App) -> profiles::Defaults {
    profiles::Defaults {
        launch_with: app.settings.launch_with,
        windower_profile: app.settings.windower_profile.clone(),
        ashita_boot_ini: app.settings.ashita_boot_ini.clone(),
    }
}

/// "Background gamepad: on (set by the profile)".
fn gamepad_text(gamepad: profiles::BackgroundGamepad) -> String {
    let state = if gamepad.on { t("on") } else { t("off") };
    let source = match gamepad.source {
        GamepadSource::Profile => t("set by the profile"),
        GamepadSource::NotSet => t("not set in the profile"),
        GamepadSource::Registry => t("from the game's settings"),
        GamepadSource::Unknown => t("unknown"),
    };
    tf("Background gamepad: {state} ({source})", &[("state", &state), ("source", &source)])
}

/// The account sheet's background-gamepad line, for the profile picked there (saved or not).
fn sheet_gamepad(g: &Features, app: &App) {
    let text = match &app.editor {
        Some(editor) if editor.account.mode == crate::backend::AccountMode::Retail => {
            let view = profiles::account_profile(&editor.account, &defaults(app), &lists(app));
            if view.tool == Tool::Native { String::new() } else { gamepad_text(view.background_gamepad) }
        }
        _ => String::new(),
    };
    put!(g, get_sheet_gamepad, set_sheet_gamepad, text.into());
}

fn windows(g: &Features, app: &App, settings: &FeatureSettings, state: &mut State) {
    let layouts = &settings.layouts;
    put!(g, get_remember, set_remember, layouts.remember);
    put!(g, get_restore, set_restore, layouts.restore);
    put!(g, get_launch_saved, set_launch_saved, layouts.launch_at_saved_size);
    put!(g, get_order_windows, set_order_windows, layouts.order_windows);
    put!(g, get_can_restore, set_can_restore, app.features.as_ref().is_some_and(|r| r.can_restore_windows()));
    let monitors = &app.features_view.monitors;
    let mut labels = vec![t("All monitors").to_owned()];
    let mut names = labels.clone();
    let mut devices = vec![None];
    for (i, monitor) in monitors.iter().enumerate() {
        let number = monitor.device.trim_start_matches(r"\\.\DISPLAY").parse::<u32>().unwrap_or(i as u32 + 1);
        let (w, h) = (monitor.rect.w, monitor.rect.h);
        let mut label = tf("Display {number} · {width}×{height}", &[("number", &number), ("width", &w), ("height", &h)]);
        if monitor.primary { label = tf("{display} · primary", &[("display", &label)]); }
        labels.push(label);
        names.push(format!("{} {number}", t("Display")));
        devices.push(Some(monitor.device.clone()));
    }
    if state.monitor_devices != devices { put!(g, get_monitor, set_monitor, 0); }
    state.monitor_devices = devices;
    strings_model(&state.monitors, labels);
    strings_model(&state.monitor_names, names);
    let mut ids = Vec::new();
    let mut lines = Vec::new();
    for (id, saved) in &layouts.saved {
        let Some(key) = ClientKey::parse(id) else { continue };
        let display = saved.monitor.device.trim_start_matches(r"\\.\");
        let size = format!("{} × {}", saved.client.0, saved.client.1);
        lines.push(LayoutLine {
            name: app.feature_client_label(&key).into(),
            detail: format!("{size} · {display}").into(),
            size: size.into(),
            display: display.to_uppercase().into(),
        });
        ids.push(id.clone());
    }
    state.layout_ids = ids;
    sync_model(&state.layouts, lines);
    state.map = MapInputs {
        areas: monitors.iter().map(|m| (m.device.clone(), m.work)).collect(),
        windows: app.accounts.iter()
            .filter(|row| row.pid.is_some())
            .map(|row| (crate::privacy::mask(&row.account.label), crate::account_row::row_state(app, row) == crate::account_row::RowState::Playing))
            .collect(),
        aspect: layouts.aspect,
    };
    map_tiles(g, state);
}

/// The monitor map: where Arrange all would put the running games with the layout and monitor
/// chosen, as fractions of the area they share.
fn map_tiles(g: &Features, state: &State) {
    let preset = if g.get_preset() == 1 { Preset::MainLarge } else { Preset::Grid };
    let picked = index(g.get_monitor()).and_then(|i| state.monitor_devices.get(i).cloned()).flatten();
    let areas: Vec<Rect> = state.map.areas.iter().filter(|(device, _)| picked.as_ref().is_none_or(|p| p == device)).map(|(_, work)| *work).collect();
    let areas = if areas.is_empty() { state.map.areas.iter().map(|(_, work)| *work).take(1).collect() } else { areas };
    let Some(first) = areas.first().copied() else {
        sync_model(&state.map_tiles, Vec::new());
        return;
    };
    let (left, top) = areas.iter().fold((first.x, first.y), |(x, y), a| (x.min(a.x), y.min(a.y)));
    let (right, bottom) = areas.iter().fold((first.right(), first.bottom()), |(x, y), a| (x.max(a.right()), y.max(a.bottom())));
    let (width, height) = (((right - left).max(1)) as f32, ((bottom - top).max(1)) as f32);
    put!(g, get_map_aspect, set_map_aspect, width / height);
    let rects = layouts::arrange(preset, state.map.windows.len(), &areas, state.map.aspect);
    let tiles = state.map.windows.iter().zip(rects).enumerate().map(|(i, ((name, live), r))| MapTile {
        index: (i + 1).to_string().into(),
        name: name.as_str().into(),
        size: format!("{} × {}", r.w, r.h).into(),
        x: (r.x - left) as f32 / width,
        y: (r.y - top) as f32 / height,
        w: r.w as f32 / width,
        h: r.h as f32 / height,
        live: *live,
    }).collect();
    sync_model(&state.map_tiles, tiles);
}

fn section_title(section: Section) -> &'static str {
    match section {
        Section::Windows => t("Windows"),
        Section::Hotkeys => t("Hotkeys"),
        Section::Recovery => t("Crash recovery"),
        Section::Backups => t("Backups"),
        Section::Clients => t("Clients"),
        Section::Activity => t("Activity"),
    }
}

/// Each section tile's state in a few words, in `Section::ALL` order (Windows' is worded by the
/// page from the layout and monitor chosen).
fn section_notes(app: &App, settings: &FeatureSettings) -> Vec<String> {
    let hotkeys = &settings.hotkeys;
    let set = usize::from(hotkeys.cycle.is_some()) + usize::from(hotkeys.cycle_back.is_some()) + hotkeys.accounts.len();
    let recovery = &settings.recovery;
    let on_off = |on: bool| if on { t("on") } else { t("off") };
    Section::ALL.iter().map(|section| match section {
        Section::Windows => String::new(),
        Section::Hotkeys if set == 0 => t("Not set").to_owned(),
        Section::Hotkeys => tf("{count} set", &[("count", &set)]),
        Section::Recovery => if recovery.auto_relaunch { t("Relaunches automatically") } else if recovery.offer { t("Offers a relaunch") } else { t("Off") }.to_owned(),
        Section::Backups => match app.features_view.backups.iter().map(|b| b.created).max() {
            Some(created) => tf("Last {date}", &[("date", &date(created))]),
            None => t("No backups yet").to_owned(),
        },
        Section::Clients => if settings.perf.background { t("Background at low priority") } else { t("Background at normal priority") }.to_owned(),
        Section::Activity => tf("Play time {playtime}, Discord {discord}", &[("playtime", &on_off(settings.playtime.record)), ("discord", &on_off(settings.playtime.discord))]),
    }).collect()
}

fn hotkey_line(name: String, hotkey: Option<crate::features::hotkeys::Hotkey>, action: &Action, statuses: &[Status], error: Option<&String>) -> HotkeyLine {
    let status = hotkey.and_then(|h| statuses.iter().find(|s| &s.action == action && s.hotkey == h));
    let problem = error.cloned().or_else(|| status.and_then(|s| s.problem).map(|p| p.text())).unwrap_or_default();
    HotkeyLine {
        name: name.into(),
        text: hotkey.map(|h| h.to_string()).unwrap_or_default().into(),
        active: status.is_some_and(|s| s.problem.is_none()) && error.is_none(),
        problem: problem.into(),
    }
}

fn hotkeys(app: &App, settings: &FeatureSettings, state: &mut State) {
    let view = &app.features_view;
    let statuses = &view.hotkeys;
    let errors = &view.field_errors;
    let in_game = |name: &str| tf("{name} (in game only)", &[("name", &name)]);
    let cycle = vec![
        hotkey_line(in_game(t("Next window")), settings.hotkeys.cycle, &Action::Next, statuses, errors.get("hotkey:next")),
        hotkey_line(in_game(t("Previous window")), settings.hotkeys.cycle_back, &Action::Previous, statuses, errors.get("hotkey:previous")),
    ];
    sync_model(&state.cycle_hotkeys, cycle);
    let accounts = app.accounts.iter().map(|row| hotkey_line(
        crate::privacy::mask(&row.account.label),
        settings.hotkeys.accounts.get(&row.key).copied(),
        &Action::Account(row.key.clone()),
        statuses,
        errors.get(&format!("hotkey:{}", row.key)),
    )).collect();
    sync_model(&state.account_hotkeys, accounts);
}

fn recovery(g: &Features, settings: &FeatureSettings) {
    let recovery = &settings.recovery;
    put!(g, get_offer, set_offer, recovery.offer);
    put!(g, get_auto_relaunch, set_auto_relaunch, recovery.auto_relaunch);
    let note = tf("Up to {count} tries in {minutes} minutes. Never during maintenance.", &[("count", &recovery.max_attempts), ("minutes", &recovery.window_minutes)]);
    put!(g, get_auto_note, set_auto_note, note.into());
}

fn size_text(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.0} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn backups(g: &Features, app: &App, settings: &FeatureSettings, state: &mut State) {
    let view = &app.features_view;
    put!(g, get_busy, set_busy, view.busy);
    put!(g, get_backing_up, set_backing_up, view.busy && view.restoring.is_none());
    put!(g, get_backups_loading, set_backups_loading, !view.backups_loaded);
    put!(g, get_before_update, set_before_update, settings.backups.before_update);
    let mut keep: Vec<u32> = fui::KEEP.to_vec();
    if !keep.contains(&settings.backups.keep) { keep.push(settings.backups.keep); keep.sort_unstable(); }
    strings_model(&state.keep_items, keep.iter().map(|count| tf("{count} backups", &[("count", count)])));
    put!(g, get_keep, set_keep, keep.iter().position(|k| *k == settings.backups.keep).map_or(-1, |i| i as i32));
    state.keep_values = keep;
    let mut paths = Vec::new();
    let lines = view.backups.iter().map(|info| {
        use crate::features::backups::Reason;
        let reason = match info.reason {
            Reason::Manual => t("Manual"),
            Reason::BeforeUpdate => t("Before update"),
            Reason::BeforeRepair => t("Before repair"),
            Reason::BeforeRestore => t("Before restore"),
        };
        let mut parts = Vec::new();
        if info.parts.ffxi { parts.push("FFXI"); }
        if info.parts.windower { parts.push("Windower"); }
        if info.parts.ashita { parts.push("Ashita"); }
        paths.push(info.path.clone());
        BackupLine {
            date: date(info.created).into(),
            detail: format!("{reason} · {} · {} · {}", parts.join(" + "), if info.files == 1 { t("1 file").to_owned() } else { tf("{count} files", &[("count", &info.files)]) }, size_text(info.size)).into(),
            confirming: view.confirm_restore.as_ref() == Some(&info.path),
            restoring: view.restoring.as_ref() == Some(&info.path),
        }
    }).collect();
    state.backup_paths = paths;
    sync_model(&state.backups, lines);
}

fn clients(g: &Features, app: &App, settings: &FeatureSettings, state: &mut State) {
    let perf = &settings.perf;
    put!(g, get_background_priority, set_background_priority, perf.background);
    put!(g, get_mute_background, set_mute_background, perf.mute_background);
    let defaults = defaults(app);
    let errors = &app.features_view.field_errors;
    let lists = lists(app);
    let lines = app.accounts.iter().map(|row| {
        let client = perf.client(&row.key);
        let view = profiles::account_profile(&row.account, &defaults, &lists);
        let profile = match view.tool {
            Tool::Native => t("Stock client: no launch profile").to_owned(),
            tool => {
                // A profile's own name is user-authored: masked in privacy mode.
                let name = if view.effective.is_empty() { t("Windower's default profile").to_owned() } else { crate::privacy::mask(&view.effective) };
                let name = if view.missing { tf("{name} (missing)", &[("name", &name)]) } else { name };
                let tool = if tool == Tool::Windower { "Windower" } else { "Ashita" };
                let line = tf("{tool} · {profile}", &[("tool", &tool), ("profile", &name)]);
                if view.uses_global_default { tf("{line} · default", &[("line", &line)]) } else { line }
            }
        };
        ClientLine {
            name: crate::privacy::mask(&row.account.label).into(),
            running: row.pid.is_some(),
            priority: client.priority.and_then(|p| Priority::ALL.iter().position(|q| *q == p)).map_or(0, |i| i as i32 + 1),
            processors: client.affinity.map(fui::affinity_text).unwrap_or_default().into(),
            processors_error: errors.get(&format!("affinity:{}", row.key)).cloned().unwrap_or_default().into(),
            mute: client.mute_in_background,
            profile: profile.into(),
            gamepad: gamepad_text(view.background_gamepad).into(),
            profile_missing: view.missing,
        }
    }).collect();
    sync_model(&state.clients, lines);
}

fn discord_status(g: &Features, app: &App) {
    let status = app.features.as_ref().map_or(discord::Status::Off, |r| r.discord_status());
    let (code, text) = match status {
        discord::Status::Off => (0, String::new()),
        discord::Status::Unavailable => (3, t("Not available in this build.").to_owned()),
        discord::Status::Connected => (1, t("Connected to Discord.").to_owned()),
        discord::Status::NotRunning => (2, t("Waiting for Discord.").to_owned()),
        discord::Status::Error(error) => (3, error),
    };
    put!(g, get_discord_state, set_discord_state, code);
    put!(g, get_discord_status, set_discord_status, text.into());
}

fn activity(g: &Features, app: &App, settings: &FeatureSettings, state: &mut State) {
    put!(g, get_notify, set_notify, settings.notify.enabled);
    put!(g, get_notify_mail, set_notify_mail, settings.notify.mail);
    put!(g, get_notify_friends, set_notify_friends, settings.notify.friends_online);
    put!(g, get_discord, set_discord, settings.playtime.discord);
    put!(g, get_record, set_record, settings.playtime.record);
    let entries = app.features.as_ref().map(|r| r.playtime()).unwrap_or_default();
    let lines = entries.iter().map(|entry| {
        let client = app.feature_client_label(&entry.key);
        let (name, detail_lead) = match &entry.character {
            Some(character) => (crate::privacy::mask(character), format!("{client} · ")),
            None => (client, String::new()),
        };
        let sessions = if entry.sessions == 1 { t("1 session").to_owned() } else { tf("{count} sessions", &[("count", &entry.sessions)]) };
        PlayLine {
            name: name.into(),
            total: duration_text(entry.total_secs).into(),
            detail: format!("{detail_lead}{sessions} · {}", tf("last {date}", &[("date", &date(entry.last_played))])).into(),
        }
    }).collect();
    sync_model(&state.playtime, lines);
}

/// Play time as "3 hr 20 min" (the remaining-time words Home uses).
fn duration_text(secs: u64) -> String {
    let minutes = secs / 60;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    match (hours, minutes) {
        (0, minutes) => tf("{minutes} min", &[("minutes", &minutes)]),
        (hours, 0) => tf("{hours} hr", &[("hours", &hours)]),
        (hours, minutes) => tf("{hours} hr {minutes} min", &[("hours", &hours), ("minutes", &minutes)]),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{pad, take_sent};
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Key, WindowEvent};

    struct Headless(Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> { Ok(self.0.clone()) }
    }

    thread_local! {
        static SCREEN: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    }

    /// Runs change handlers and draws a frame, so `if` and `for` parts are built and laid out
    /// as on screen.
    fn render() {
        slint::platform::update_timers_and_animations();
        SCREEN.with(|screen| {
            if let Some(window) = screen.borrow().as_ref() {
                window.request_redraw();
                window.draw_if_needed(|renderer| {
                    let mut pixels = vec![slint::platform::software_renderer::Rgb565Pixel::default(); 1440 * 900];
                    renderer.render(&mut pixels, 1440);
                });
            }
        });
        slint::platform::update_timers_and_animations();
    }

    /// The main window on the software renderer, no window on screen, with `app` synced in.
    fn window(app: &App) -> MainWindow {
        SCREEN.with(|screen| {
            if screen.borrow().is_none() {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                window.set_size(slint::PhysicalSize::new(1440, 900));
                slint::platform::set_platform(Box::new(Headless(window.clone()))).unwrap();
                *screen.borrow_mut() = Some(window);
            }
        });
        let ui = MainWindow::new().unwrap();
        super::super::view::wire(&ui);
        wire(&ui);
        super::super::view::sync(&ui, app);
        sync(&ui, app);
        ui.show().unwrap();
        render();
        let _ = take_sent();
        ui
    }

    fn open(app: &mut App, ui: &MainWindow, section: Section) {
        drop(app.feature_update(Msg::Section(section)));
        sync(ui, app);
        render();
    }

    fn type_text(ui: &MainWindow, text: &str) {
        for c in text.chars() {
            let text: SharedString = c.to_string().into();
            ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window().dispatch_event(WindowEvent::KeyReleased { text });
        }
    }

    fn key(ui: &MainWindow, key: Key) {
        let text: SharedString = key.into();
        ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window().dispatch_event(WindowEvent::KeyReleased { text });
        render();
    }

    #[test]
    fn every_switch_has_its_own_message() {
        let all: Vec<String> = (0..14).map(|i| format!("{:?}", toggle(i, true).expect("mapped"))).collect();
        let mut unique = all.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 14, "{all:?}");
        assert!(toggle(14, true).is_none() && toggle(-1, true).is_none());
        assert!(matches!(toggle(4, false), Some(Msg::AutoRelaunch(false))));
        assert_eq!(priority_choice(0), Some(None));
        assert_eq!(priority_choice(1), Some(Some(Priority::ALL[0])));
        assert_eq!(priority_choice(Priority::ALL.len() as i32 + 1), None);
        assert_eq!(priority_choice(-1), None);
    }

    /// The page shows the runtime's settings and read-outs, and the controller walker reaches
    /// each section's controls (it is a page, not a modal: the rail and top bar stay reachable).
    #[test]
    fn the_page_draws_the_runtime_and_the_walker_reaches_its_controls() {
        let mut app = crate::tests::app();
        // Closed: Home's stops, the Features entry among them; no modal.
        let ui = window(&app);
        assert!(!pad::stop_origins(ui.window()).is_empty());
        assert!(pad::modal_rect(ui.window()).is_none());
        app.load_features_sample(Section::Backups, false);
        sync(&ui, &app);
        render();
        let g = ui.global::<Features>();
        assert!(g.get_open());
        assert_eq!(g.get_section(), 3);
        assert_eq!(g.get_backups().row_count(), 2);
        assert!(g.get_backups().row_data(0).unwrap().confirming, "the sample asks to restore the newest");
        assert!(!g.get_backups_loading());
        assert!(pad::modal_rect(ui.window()).is_none(), "the page is not modal");
        // The six section tiles, plus each section's own controls (the layout's two segments and
        // the monitor, Arrange all, the four switches, two Forget buttons); the page lies right of
        // the rail and under the top bar.
        let own = [
            (Section::Windows, 2 + 1 + 1 + 4 + 2),
            (Section::Hotkeys, 2 + app.accounts.len()),
            (Section::Recovery, 2),
            (Section::Backups, 3 + 1 + 2),
            (Section::Clients, 2 + 4 * app.accounts.len()),
            (Section::Activity, 5),
        ];
        for (i, (section, controls)) in own.into_iter().enumerate() {
            open(&mut app, &ui, section);
            assert_eq!(g.get_section(), i as i32);
            let stops: Vec<_> = pad::stop_origins(ui.window()).into_iter().filter(|(sx, sy)| *sx >= 356.0 && *sy > 100.0).collect();
            assert_eq!(stops.len(), 6 + controls, "{section:?}: {stops:?}");
            match section {
                Section::Windows => assert_eq!(g.get_layouts().row_count(), 2),
                Section::Hotkeys => {
                    assert_eq!(g.get_cycle_hotkeys().row_count(), 2);
                    let refused = g.get_account_hotkeys().row_data(1).unwrap();
                    assert_eq!(refused.text, "F5");
                    assert!(!refused.problem.is_empty(), "a bare key is refused with its reason");
                }
                Section::Recovery => assert!(g.get_auto_relaunch()),
                Section::Clients => {
                    assert_eq!(g.get_clients().row_count(), app.accounts.len());
                    let first = g.get_clients().row_data(0).unwrap();
                    assert_eq!((first.priority, first.processors.as_str()), (4, "0-3"), "Above normal, CPUs 0-3");
                }
                Section::Activity => assert_eq!(g.get_playtime().row_count(), 2),
                Section::Backups => {}
            }
        }
        // Back (Escape, the controller's B) closes it in the core; going to another page does too.
        assert!(!app.controller_modal());
        // Closing hides the page; the banner stays for an offered relaunch.
        app.features_view.offers.insert(app.accounts[0].key.clone(), Decision::Offer { kind: EndKind::Disconnect, held: Held::Off });
        drop(app.update(Message::ControllerBack));
        sync(&ui, &app);
        render();
        assert!(!g.get_open());
        assert!(pad::modal_rect(ui.window()).is_none());
        assert_eq!(g.get_relaunches().row_count(), 1);
        assert!(!g.get_relaunches().row_data(0).unwrap().scheduled);
        // The account sheet names the background-gamepad value of the profile picked there.
        assert!(g.get_sheet_gamepad().is_empty());
        drop(app.update(Message::EditAccount(app.accounts[0].key.clone())));
        sync(&ui, &app);
        assert!(g.get_sheet_gamepad().contains(t("off")), "{}", g.get_sheet_gamepad());
        // The account sheet is modal too.
        super::super::view::sync(&ui, &app);
        // Nav.modal (the sheet's controls take the focus) is set with the Downloads sync, as the host runs it.
        super::super::downloads::sync(&ui, &app);
        render();
        assert!(pad::modal_rect(ui.window()).is_some_and(|(_, _, w, _)| w == 464.0));
        assert!(pad::stop_origins(ui.window()).iter().all(|(sx, _)| *sx >= 1440.0 - 464.0));
    }

    /// The monitor map draws one tile per running game, and follows the layout chosen; going to
    /// another page leaves the Features page.
    #[test]
    fn the_map_follows_the_layout_and_navigating_leaves_the_page() {
        let mut app = crate::tests::app();
        for row in app.accounts.iter_mut() { row.pid = Some(1); }
        let ui = window(&app);
        app.load_features_sample(Section::Windows, false);
        sync(&ui, &app);
        render();
        let g = ui.global::<Features>();
        if app.features_view.monitors.is_empty() { return; }
        assert_eq!(g.get_map_tiles().row_count(), app.accounts.len(), "a tile per running game");
        g.set_preset(1);
        g.invoke_map_changed();
        let main_large = g.get_map_tiles().row_data(0).unwrap();
        g.set_preset(0);
        g.invoke_map_changed();
        let grid = g.get_map_tiles().row_data(0).unwrap();
        assert!(main_large.w > grid.w, "the main window is the large one: {} against {}", main_large.w, grid.w);
        assert!(g.get_map_tiles().iter().all(|t| t.x >= 0.0 && t.y >= 0.0 && t.x + t.w <= 1.001 && t.y + t.h <= 1.001));
        assert!(app.features_view.open);
        drop(app.update(Message::Navigate(crate::Page::Settings)));
        assert!(!app.features_view.open, "Features is a page: going elsewhere leaves it");
    }
    /// Restore windows shows beside Arrange all only while the last Arrange all can be undone.
    #[test]
    fn restore_windows_shows_only_with_something_to_restore() {
        let mut app = crate::tests::app();
        let ui = window(&app);
        app.load_features_sample(Section::Windows, false);
        sync(&ui, &app);
        render();
        let g = ui.global::<Features>();
        let stops = || pad::stop_origins(ui.window()).len();
        assert!(!g.get_can_restore(), "no Arrange all yet: no Restore windows");
        let without = stops();
        g.set_can_restore(true);
        render();
        assert_eq!(stops(), without + 1, "the button is there and reachable");
        // The next rebuild follows the runtime, which has nothing to restore.
        drop(app.feature_update(Msg::RestoreWindows));
        sync(&ui, &app);
        render();
        assert!(!g.get_can_restore());
        assert_eq!(stops(), without);
        assert!(app.features_view.notice.as_ref().is_some_and(|(error, _)| *error), "nothing to restore is said");
    }

    /// Each callback sends the message its control is for.
    #[test]
    fn each_callback_sends_its_message() {
        let mut app = crate::tests::app();
        let ui = window(&app);
        app.load_features_sample(Section::Backups, false);
        for section in Section::ALL { open(&mut app, &ui, section); }
        open(&mut app, &ui, Section::Backups);
        app.features_view.offers.insert(app.accounts[1].key.clone(), Decision::Offer { kind: EndKind::Crash, held: Held::Off });
        app.features_view.touch();
        sync(&ui, &app);
        let _ = take_sent();
        let g = ui.global::<Features>();
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let sent = |call: &dyn Fn()| { call(); take_sent() };
        let one = |call: &dyn Fn()| { let mut all = sent(call); assert_eq!(all.len(), 1); all.remove(0) };
        assert!(matches!(one(&|| g.invoke_open_sheet(true)), Message::Feature(Msg::Open(true))));
        assert!(matches!(one(&|| g.invoke_section_picked(4)), Message::Feature(Msg::Section(Section::Clients))));
        assert!(sent(&|| g.invoke_section_picked(6)).is_empty());
        assert!(matches!(one(&|| g.invoke_toggle(12, true)), Message::Feature(Msg::Discord(true))));
        assert!(matches!(one(&|| g.invoke_hotkey_entered(0, "Ctrl+1".into())), Message::Feature(Msg::Hotkey(Target::Next, ref t)) if t == "Ctrl+1"));
        assert!(matches!(one(&|| g.invoke_hotkey_entered(1, "".into())), Message::Feature(Msg::Hotkey(Target::Previous, _))));
        assert!(matches!(one(&|| g.invoke_hotkey_entered(3, "F6".into())), Message::Feature(Msg::Hotkey(Target::Account(ref k), _)) if *k == keys[1]));
        assert!(sent(&|| g.invoke_hotkey_entered(2 + keys.len() as i32, "F6".into())).is_empty());
        assert!(matches!(one(&|| g.invoke_priority_picked(0, 0)), Message::Feature(Msg::Priority(ref k, None)) if *k == keys[0]));
        assert!(matches!(one(&|| g.invoke_priority_picked(1, 2)), Message::Feature(Msg::Priority(ref k, Some(p))) if *k == keys[1] && p == Priority::ALL[1]));
        assert!(sent(&|| g.invoke_priority_picked(0, 99)).is_empty());
        assert!(matches!(one(&|| g.invoke_processors_entered(1, "0-1".into())), Message::Feature(Msg::Affinity(ref k, ref t)) if *k == keys[1] && t == "0-1"));
        assert!(matches!(one(&|| g.invoke_mute_client(0, true)), Message::Feature(Msg::MuteClient(ref k, true)) if *k == keys[0]));
        let edit = sent(&|| g.invoke_edit_account(1));
        assert!(matches!(edit.as_slice(), [Message::Feature(Msg::Open(false)), Message::EditAccount(k)] if *k == keys[1]));
        assert!(matches!(one(&|| g.invoke_back_up_now()), Message::Feature(Msg::BackupNow)));
        assert!(matches!(one(&|| g.invoke_restore_ask(1)), Message::Feature(Msg::AskRestore(ref p)) if *p == app.features_view.backups[1].path));
        assert!(matches!(one(&|| g.invoke_restore_confirm()), Message::Feature(Msg::RestoreBackup)));
        assert!(matches!(one(&|| g.invoke_restore_cancel()), Message::Feature(Msg::CancelRestore)));
        assert!(matches!(one(&|| g.invoke_keep_picked(1)), Message::Feature(Msg::Keep(5))));
        let layout = app.features.as_ref().unwrap().settings().layouts.saved.keys().next().unwrap().clone();
        assert!(matches!(one(&|| g.invoke_forget(0)), Message::Feature(Msg::Forget(ref id)) if *id == layout));
        g.set_monitor(0);
        assert!(matches!(one(&|| g.invoke_arrange(1)), Message::Feature(Msg::Arrange(Preset::MainLarge, None))));
        assert!(matches!(one(&|| g.invoke_restore_windows()), Message::Feature(Msg::RestoreWindows)));
        assert!(matches!(one(&|| g.invoke_relaunch_now(0)), Message::Launch(ref k) if *k == keys[1]));
        assert!(matches!(one(&|| g.invoke_relaunch_dismiss(0)), Message::Feature(Msg::DismissRecovery(ref k)) if *k == keys[1]));
        assert!(matches!(one(&|| g.invoke_relaunch_cancel(0)), Message::Feature(Msg::CancelRelaunch(ref k)) if *k == keys[1]));
    }

    /// A sync with nothing changed sets nothing; a changed input reaches its row.
    #[test]
    fn a_second_sync_with_nothing_changed_changes_nothing() {
        fn rows<T: Clone + 'static>(model: ModelRc<T>) -> Vec<T> { model.iter().collect() }
        let mut app = crate::tests::app();
        let ui = window(&app);
        app.load_features_sample(Section::Hotkeys, false);
        sync(&ui, &app);
        let g = ui.global::<Features>();
        let before = (rows(g.get_cycle_hotkeys()), rows(g.get_account_hotkeys()), rows(g.get_relaunches()), g.get_strings());
        let built = read(|s| Some(s.section_built));
        sync(&ui, &app);
        sync(&ui, &app);
        assert_eq!((rows(g.get_cycle_hotkeys()), rows(g.get_account_hotkeys()), rows(g.get_relaunches()), g.get_strings()), before);
        assert_eq!(read(|s| Some(s.section_built)), built, "nothing moved: nothing rebuilt");
        // A refused chord moves the features state, and the row shows why.
        drop(app.feature_update(Msg::Hotkey(Target::Next, "nonsense".into())));
        sync(&ui, &app);
        assert!(!g.get_cycle_hotkeys().row_data(0).unwrap().problem.is_empty());
        assert_ne!(read(|s| Some(s.section_built)), built);
    }

    /// A chord field shows the saved chord: after Return, when its row changes under a draft,
    /// and when the focus leaves with a draft never applied.
    #[test]
    fn fields_follow_their_rows() {
        let mut app = crate::tests::app();
        let ui = window(&app);
        app.load_features_sample(Section::Hotkeys, false);
        sync(&ui, &app);
        render();
        let g = ui.global::<Features>();
        // The sample starts with the default cycle keys; clear the first row.
        let mut row = g.get_cycle_hotkeys().row_data(0).unwrap();
        row.text = "".into();
        g.get_cycle_hotkeys().set_row_data(0, row);
        render();
        assert_eq!(pad::field_texts(ui.window())[0], "");
        pad::focus_field(ui.window(), 0);
        type_text(&ui, "ctrl+alt+9");
        render();
        assert_eq!(pad::field_texts(ui.window())[0], "ctrl+alt+9");
        // The core saved it in its own form: the field shows that, not the draft.
        let mut row = g.get_cycle_hotkeys().row_data(0).unwrap();
        row.text = "Ctrl+Alt+9".into();
        g.get_cycle_hotkeys().set_row_data(0, row);
        render();
        assert_eq!(pad::field_texts(ui.window())[0], "Ctrl+Alt+9");
        // Return applies the draft and the field comes back to what is saved.
        type_text(&ui, "x");
        key(&ui, Key::Return);
        assert!(take_sent().iter().any(|m| matches!(m, Message::Feature(Msg::Hotkey(Target::Next, t)) if t == "Ctrl+Alt+9x")));
        assert_eq!(pad::field_texts(ui.window())[0], "Ctrl+Alt+9");
        // A draft left without Return is dropped.
        type_text(&ui, "y");
        render();
        assert_eq!(pad::field_texts(ui.window())[0], "Ctrl+Alt+9y");
        pad::focus_field(ui.window(), 1);
        render();
        assert_eq!(pad::field_texts(ui.window())[0], "Ctrl+Alt+9");
        assert!(take_sent().is_empty(), "nothing applied without Return");
    }
}
