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
//!
//! A hotkey is set by pressing it. The core says which field records
//! (`features_view.recording`); while one does, this side holds its keyboard hook
//! (`features::capture`), shows the modifiers as they are held, and sends the chord pressed to
//! the core, which checks and saves it. The recording stops when the field loses the focus,
//! the pointer goes down anywhere else, the window loses the foreground, or the page or its
//! section goes. The field's own key events are read as well: a key that reaches the field is
//! one the hook did not take (it is not installed, or Windows did not call it), so a recording
//! never depends on the hook alone. Each recording leaves how it read the keyboard and how it
//! ended in the launch history (`hotkeys: record: ...`).
//!
//! The monitor map is the window layout editor. The page says where the pointer is on it
//! (`map-pointer`) and which keys the focused tile gets (`tile-key`); this side knows the
//! monitors and the tiles in screen pixels, takes hold of a tile, snaps it (`features::snap`)
//! and shows the result in the one row that changed. Only when a tile is let go does the core
//! hear of it (`Msg::Place`), which remembers it or holds it for Apply. Nothing runs between
//! pointer and key events: no timer, no animation.
use super::{send, BackupLine, ClientLine, FeatureStrings, Features, HotkeyLine, LayoutLine, MainWindow, MapGuide, MapMonitor, MapTile, PlayLine, RelaunchLine};
use crate::features::capture::{self, Capture, Outcome, Recorder};
use crate::features::hotkeys::{self, Action, Hotkey, Problem};
use crate::features::game_window::{Monitor, Rect};
use crate::features::layouts::{self, Preset};
use crate::features::snap::{self, Grab, Map};
use crate::features::perf::Priority;
use crate::features::profiles::{self, GamepadSource, Tool};
use crate::features::recovery::{Decision, EndKind, Held};
use crate::features::ui::{self as fui, Msg, Section, Target};
use crate::features::{discord, ClientKey, FeatureSettings};
use crate::i18n::{t, tf, Lang};
use crate::{App, Message};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Instant, SystemTime};

macro_rules! put {
    ($view:expr, $get:ident, $set:ident, $value:expr) => {{
        let value = $value;
        if $view.$get() != value {
            $view.$set(value);
        }
    }};
}

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
    /// What the monitor map draws from, as of the last sync (and of the drag or nudge going on).
    map: MapInputs,
    /// The selected tile's account.
    selected: Option<String>,
    /// The tile being dragged.
    drag: Option<Dragging>,
    /// The tile the arrow keys are moving (its key is still down), and where it was.
    nudge: Option<(usize, Rect)>,
    /// The relaunch banner's rows: (account, scheduled).
    relaunches: Vec<(String, bool)>,
    sections: Rc<VecModel<SharedString>>,
    section_notes: Rc<VecModel<SharedString>>,
    monitors: Rc<VecModel<SharedString>>,
    map_monitors: Rc<VecModel<MapMonitor>>,
    map_tiles: Rc<VecModel<MapTile>>,
    map_guides: Rc<VecModel<MapGuide>>,
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

/// The monitors and tiles the map is drawn from, in screen pixels.
#[derive(Default)]
struct MapInputs {
    /// Every monitor, as Windows has them.
    monitors: Vec<Monitor>,
    screens: Vec<snap::Screen>,
    /// Each monitor's short name ("Display 2").
    names: Vec<String>,
    /// A tile per account, in Home order.
    tiles: Vec<Tile>,
}

/// An account's window on the map.
#[derive(Clone, Debug, PartialEq)]
struct Tile {
    /// The account's key.
    key: String,
    /// Its label (masked in privacy mode).
    name: String,
    /// In game.
    live: bool,
    /// Its game's window is open: the tile waits for Apply.
    open: bool,
    /// It has a place: an open window, or a remembered one.
    placed: bool,
    /// Moved on the map since its open window was.
    pending: bool,
    rect: Rect,
}

/// A tile taken hold of on the map.
struct Dragging {
    index: usize,
    drag: snap::Drag,
    /// Where the pointer went down, in screen pixels.
    press: (i32, i32),
    /// The pointer has gone far enough for this to be a drag (a click moves nothing).
    moved: bool,
    /// The tile was the selected one before this press.
    was_selected: bool,
}

/// How near a line takes a dragged tile, how near an edge a press takes hold of it, and how
/// far the pointer goes before a press is a drag: in map pixels, so they feel the same however
/// many screen pixels the map shows in one.
const SNAP_PX: f64 = 7.0;
const GRIP_PX: f64 = 5.0;
const SLOP_PX: f64 = 3.0;
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
            selected: None,
            drag: None,
            nudge: None,
            relaunches: Vec::new(),
            sections: Rc::default(),
            section_notes: Rc::default(),
            monitors: Rc::default(),
            map_monitors: Rc::default(),
            map_tiles: Rc::default(),
            map_guides: Rc::default(),
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
    /// The hotkey field that is recording.
    static RECORDING: RefCell<Option<Recording>> = const { RefCell::new(None) };
    /// The last recording's token.
    static TOKEN: Cell<u64> = const { Cell::new(0) };
    /// The window, for a recording's events (they arrive from the hook's thread).
    static WINDOW: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
}

/// The window closed: the page's models go with it (nothing stays in the background), and so
/// do a recording's keyboard hook and a tile being dragged.
pub(super) fn forget() {
    RECORDING.with(|cell| cell.borrow_mut().take());
    WINDOW.with(|cell| cell.borrow_mut().take());
    STATE.with(|cell| cell.borrow_mut().take());
}

/// A hotkey field's recording, on this side.
struct Recording {
    /// The field: 0 next window, 1 previous window, 2 + n the n-th account.
    row: i32,
    target: Target,
    /// Tells this recording's events from an earlier one's still on their way.
    token: u64,
    /// The keyboard hook, while it is in (it could not be installed, or the window has no
    /// handle: tests). It takes the keys it records, so they never reach the field.
    hook: Option<Capture>,
    /// The hook has reported: it shows the modifiers (it sees the Windows key; the field does
    /// not).
    hook_live: bool,
    /// The hook took the key that ended the recording (the field never saw its press).
    hook_took: bool,
    /// The decision for the field's own key events, and the modifiers those events have held
    /// down (a key event's own modifiers are not always the whole of it). A key that reaches
    /// the field is one the hook did not take, so it is read here, hook or no hook.
    recorder: Recorder,
    held: u8,
    /// The pointer press being handled went down on this field.
    claimed: bool,
    /// Where the recording's lines in the launch history go.
    log: Option<crate::features::Runtime>,
}

impl Recording {
    fn note(&self, text: &str) { if let Some(log) = &self.log { log.record_note(text); } }

    /// The recording is over: its hook is removed, and what the hook saw is noted.
    fn finish(mut self) {
        match self.hook.take().map(Capture::finish) {
            Some(seen) => self.note(&format!("ended: the hook saw {} key events, {} of them with another window in front", seen.keys, seen.away)),
            None => self.note("ended"),
        }
    }
}

/// A line in the launch history about the recording there is, if there is one.
fn note(text: &str) {
    RECORDING.with(|cell| { if let Ok(slot) = cell.try_borrow() { if let Some(recording) = slot.as_ref() { recording.note(text); } } });
}

/// Whether a hotkey field is recording.
pub(super) fn recording() -> bool { RECORDING.with(|cell| cell.try_borrow().is_ok_and(|slot| slot.is_some())) }

/// The window lost the foreground: a recording stops.
pub(super) fn window_left() {
    if !recording() { return; }
    note("stopped: the window is no longer in front");
    feature(Msg::HotkeyCancel);
}

/// The pointer went down somewhere in the window, before the page hears of it. Once the page
/// has (`pointer_settled`), a recording whose own field was not the one pressed stops.
pub(super) fn pointer_pressed() {
    let recording = RECORDING.with(|cell| cell.try_borrow_mut().is_ok_and(|mut slot| slot.as_mut().map(|r| r.claimed = false).is_some()));
    if recording { let _ = slint::invoke_from_event_loop(pointer_settled); }
}

fn pointer_settled() {
    if RECORDING.with(|cell| cell.borrow().as_ref().is_some_and(|r| !r.claimed)) {
        note("stopped: a press outside the field");
        feature(Msg::HotkeyCancel);
    }
}

/// The field a target's hotkey is in.
fn row_of(target: &Target, accounts: &[String]) -> Option<i32> {
    match target {
        Target::Next => Some(0),
        Target::Previous => Some(1),
        Target::Account(key) => accounts.iter().position(|k| k == key).map(|i| i as i32 + 2),
    }
}

fn target_at(row: i32) -> Option<Target> {
    match row {
        0 => Some(Target::Next),
        1 => Some(Target::Previous),
        _ => account_at(row - 2).map(Target::Account),
    }
}

/// What the recording field shows: the prompt, or the modifiers held.
fn show_live(g: &Features, modifiers: u8) {
    let text = if modifiers == 0 { t("Press a key…").to_owned() } else { format!("{}…", hotkeys::modifier_names(modifiers)) };
    put!(g, get_recording_text, set_recording_text, text.into());
    put!(g, get_recording_prompt, set_recording_prompt, modifiers == 0);
}

/// Starts or stops the recording the core asks for: its field must be on screen.
fn record_sync(ui: &MainWindow, g: &Features, app: &App, accounts: &[String]) {
    let view = &app.features_view;
    let wanted = view.recording.as_ref().filter(|_| view.open && view.section == Section::Hotkeys)
        .and_then(|target| row_of(target, accounts).map(|row| (row, target)));
    RECORDING.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|r| &r.target) == wanted.map(|(_, target)| target) { return; }
        // The one before ends here: its hook is removed with it.
        if let Some(ended) = slot.take() {
            put!(g, get_recording_taken, set_recording_taken, ended.hook_took);
            ended.finish();
        }
        let Some((row, target)) = wanted else { return };
        let token = TOKEN.get().wrapping_add(1);
        TOKEN.set(token);
        let window = super::native_handle(ui.window());
        let sink: capture::Sink = std::sync::Arc::new(move |event| { let _ = slint::invoke_from_event_loop(move || recorded(token, event)); });
        // 0: the window has no handle (tests).
        let hook = if window == 0 { Err(0) } else { Capture::start(window, sink) };
        let field = match target { Target::Next => "next window", Target::Previous => "previous window", Target::Account(_) => "an account" };
        let how = match &hook {
            Ok(hook) if hook.raw_keyboard_off() => "keyboard hook installed, raw keyboard input off for it".to_owned(),
            Ok(_) => "keyboard hook installed".to_owned(),
            Err(0) => "no window handle, reading the page's key events".to_owned(),
            Err(code) => format!("keyboard hook not installed (error {code}), reading the page's key events"),
        };
        let recording = Recording {
            row, target: target.clone(), token, hook: hook.ok(), hook_live: false, hook_took: false, recorder: Recorder::default(), held: 0, claimed: false,
            log: app.features.clone(),
        };
        recording.note(&format!("started for {field}: {how}"));
        *slot = Some(recording);
        show_live(g, 0);
    });
    put!(g, get_recording, set_recording, wanted.map_or(-1, |(row, _)| row));
}

/// One event of the recording `token`'s hook: the modifiers held are shown; its end goes to
/// the core.
fn recorded(token: u64, event: capture::Event) {
    let Some(target) = RECORDING.with(|cell| cell.borrow_mut().as_mut().filter(|r| r.token == token).map(|r| {
        r.hook_live = true;
        // (Tab ends a recording and is passed on: the field sees that one.)
        if let capture::Event::Key { taken, .. } = event { r.hook_took = taken; }
        r.target.clone()
    })) else { return };
    match event {
        capture::Event::Modifiers(modifiers) => show_modifiers(modifiers),
        capture::Event::Key { key, .. } => note(&format!("key from the hook: vk 0x{:02X}, scan 0x{:02X}{}", key.vk, key.scan, if key.extended { ", extended" } else { "" })),
        capture::Event::Done(outcome) => finished(target, outcome),
    }
}

fn show_modifiers(modifiers: u8) {
    if let Some(ui) = WINDOW.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade())) { show_live(&ui.global::<Features>(), modifiers); }
}

/// A recording's end goes to the core, which checks and saves the chord.
fn finished(target: Target, outcome: Outcome) {
    feature(match outcome {
        Outcome::Set(hotkey) => Msg::Hotkey(target, Some(hotkey)),
        Outcome::Clear => Msg::Hotkey(target, None),
        Outcome::Cancel => {
            note("stopped: Esc, or Tab left the field");
            Msg::HotkeyCancel
        }
    });
}

/// What a key event of the page's own is.
enum Typed {
    /// A modifier key (`MOD_*`).
    Modifier(u8),
    /// A key, as its virtual-key code.
    Key(u16),
}

/// The key Slint's text for a key event names. Slint gives text, not keys: the number pad's
/// keys are the digits, arrows and Enter they type, and a shifted or national character is no
/// key at all (None). The keyboard hook reads the keys themselves; this is for when it is out.
fn typed(text: &str) -> Option<Typed> {
    use slint::platform::Key;
    let mut chars = text.chars();
    let (c, None) = (chars.next()?, chars.next()) else { return None };
    let is = |key: Key| c == char::from(key);
    let modifier = if is(Key::Control) || is(Key::ControlR) { hotkeys::MOD_CONTROL }
        else if is(Key::Alt) || is(Key::AltGr) { hotkeys::MOD_ALT }
        else if is(Key::Shift) || is(Key::ShiftR) { hotkeys::MOD_SHIFT }
        else if is(Key::Meta) || is(Key::MetaR) { hotkeys::MOD_WIN }
        else { 0 };
    if modifier != 0 { return Some(Typed::Modifier(modifier)); }
    const NAMED: [(Key, u16); 19] = [
        (Key::Backspace, 0x08), (Key::Tab, 0x09), (Key::Backtab, 0x09), (Key::Return, 0x0D), (Key::Pause, 0x13), (Key::Escape, 0x1B),
        (Key::Space, 0x20), (Key::PageUp, 0x21), (Key::PageDown, 0x22), (Key::End, 0x23), (Key::Home, 0x24), (Key::LeftArrow, 0x25),
        (Key::UpArrow, 0x26), (Key::RightArrow, 0x27), (Key::DownArrow, 0x28), (Key::Insert, 0x2D), (Key::Delete, 0x2E), (Key::Menu, 0x5D),
        (Key::ScrollLock, 0x91),
    ];
    if let Some((_, code)) = NAMED.iter().find(|(key, _)| is(*key)) { return Some(Typed::Key(*code)); }
    let function = (c as u32).wrapping_sub(char::from(Key::F1) as u32);
    if function < 24 { return Some(Typed::Key(0x70 + function as u16)); }
    if c.is_ascii_alphanumeric() { return Some(Typed::Key(c.to_ascii_uppercase() as u16)); }
    // The unshifted punctuation keys have their own names in a saved hotkey.
    if c.is_ascii_punctuation() { return Hotkey::parse(text).map(|hotkey| Typed::Key(hotkey.key)); }
    None
}

/// A key event of the recording field's own (the page's `hotkey-key`): `modifiers` are the
/// ones held (`MOD_*`). Whether the key was the recording's.
fn key_event(text: &str, modifiers: u8, down: bool) -> bool {
    const TAKEN: capture::Step = capture::Step { swallow: true, event: None };
    let step = RECORDING.with(|cell| {
        let mut slot = cell.borrow_mut();
        let recording = slot.as_mut()?;
        let step = match typed(text) {
            Some(Typed::Modifier(bit)) => {
                recording.held = if down { recording.held | bit } else { recording.held & !bit };
                let step = recording.recorder.hold(if down { modifiers | recording.held } else { (modifiers | recording.held) & !bit });
                if recording.hook_live { TAKEN } else { step }
            }
            Some(Typed::Key(key)) if down => {
                let held = modifiers | recording.held;
                let step = recording.recorder.press(held, key);
                if step.event.is_some() {
                    let hook = if recording.hook.is_some() { " (the hook did not take it)" } else { "" };
                    recording.note(&format!("key from the page{hook}: vk 0x{key:02X}, modifiers 0x{held:X}"));
                }
                step
            }
            _ => TAKEN,
        };
        Some((recording.target.clone(), step))
    });
    let Some((target, step)) = step else { return false };
    match step.event {
        Some(capture::Event::Modifiers(modifiers)) => show_modifiers(modifiers),
        Some(capture::Event::Done(outcome)) => finished(target, outcome),
        Some(capture::Event::Key { .. }) | None => {}
    }
    step.swallow
}

fn read<R>(f: impl FnOnce(&State) -> Option<R>) -> Option<R> {
    STATE.with(|cell| cell.borrow().as_ref().and_then(f))
}

fn feature(msg: Msg) { send(Message::Feature(msg)); }

fn index(i: i32) -> Option<usize> { usize::try_from(i).ok() }

fn account_at(i: i32) -> Option<String> { read(|s| s.accounts.get(index(i)?).cloned()) }

/// The preset a layout button fills the tiles with (`preset(int)`).
fn preset_at(i: i32) -> Option<Preset> {
    match i {
        0 => Some(Preset::Grid),
        1 => Some(Preset::MainLarge),
        2 => Some(Preset::FullScreen),
        _ => None,
    }
}

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
    g.set_map_monitors(ModelRc::from(state.map_monitors.clone()));
    g.set_map_tiles(ModelRc::from(state.map_tiles.clone()));
    g.set_map_guides(ModelRc::from(state.map_guides.clone()));
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
    g.on_preset(move |preset| {
        let Some(preset) = preset_at(preset) else { return };
        let picked = weak.upgrade().map_or(0, |ui| ui.global::<Features>().get_monitor());
        let device = read(|s| s.monitor_devices.get(index(picked)?).cloned()).flatten();
        feature(Msg::Preset(preset, device));
    });
    g.on_apply_layout(|| feature(Msg::ApplyLayout));
    g.on_restore_windows(|| feature(Msg::RestoreWindows));
    let weak = ui.as_weak();
    g.on_map_pointer(move |kind, fx, fy, fit, alt| { if let Some(ui) = weak.upgrade() { map_pointer(&ui.global::<Features>(), kind, fx, fy, fit, alt); } });
    let weak = ui.as_weak();
    g.on_tile_focused(move |tile| { if let Some(ui) = weak.upgrade() { tile_focused(&ui.global::<Features>(), tile); } });
    g.on_tile_blurred(tile_blurred);
    let weak = ui.as_weak();
    g.on_tile_key(move |tile, text, shift, down| weak.upgrade().is_some_and(|ui| tile_key(&ui.global::<Features>(), tile, &text, shift, down)));
    g.on_forget(|i| { if let Some(id) = read(|s| s.layout_ids.get(index(i)?).cloned()) { feature(Msg::Forget(id)); } });
    WINDOW.with(|cell| *cell.borrow_mut() = Some(ui.as_weak()));
    g.on_hotkey_record(|row| { if let Some(target) = target_at(row) { feature(Msg::HotkeyRecord(target)); } });
    g.on_hotkey_pointer(|row| RECORDING.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() { if let Some(recording) = slot.as_mut() { recording.claimed = recording.row == row; } }
    }));
    g.on_hotkey_blur(|row| {
        if RECORDING.with(|cell| cell.try_borrow().is_ok_and(|slot| slot.as_ref().is_some_and(|r| r.row == row))) {
            note("stopped: the field lost the focus");
            feature(Msg::HotkeyCancel);
        }
    });
    g.on_hotkey_key(|text, control, alt, shift, meta, down| {
        let held = [(control, hotkeys::MOD_CONTROL), (alt, hotkeys::MOD_ALT), (shift, hotkeys::MOD_SHIFT), (meta, hotkeys::MOD_WIN)];
        key_event(&text, held.iter().filter(|(on, _)| *on).fold(0, |all, (_, bit)| all | bit), down)
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
        record_sync(ui, &g, app, &state.accounts);
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
        // A tile held on the map is let go with its section.
        if view.section != Section::Windows { (state.drag, state.nudge) = (None, None); }
        match view.section {
            Section::Windows => windows(&g, app, &settings, state),
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
        launch_saved_note: t("The game opens at the size of a tile placed while it was closed.").into(),

        apply: t("Apply").into(),
        restore_windows: t("Restore windows").into(),
        monitor: t("Monitor").into(),
        grid: t("Grid").into(),
        main_large: t("Main large").into(),
        full_screen: t("Full screen").into(),
        map_note: t("Drag a window to move it, an edge to resize it. Alt turns snapping off. The arrow keys move it one pixel.").into(),
        in_game: t("In game").into(),
        remembered: t("Remembered windows").into(),
        no_layouts: t("No windows remembered yet.").into(),
        forget: t("Forget").into(),
        switch_windows: t("Switch windows").into(),
        per_account: t("Per account").into(),
        hotkey_none: t("Not set").into(),
        hotkey_note: t("Click a hotkey, then press the keys. Esc cancels; Backspace clears it.").into(),
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
    let remembered = &settings.layouts;
    put!(g, get_remember, set_remember, remembered.remember);
    put!(g, get_restore, set_restore, remembered.restore);
    put!(g, get_launch_saved, set_launch_saved, remembered.launch_at_saved_size);

    put!(g, get_can_restore, set_can_restore, app.features.as_ref().is_some_and(|r| r.can_restore_windows()));
    let view = &app.features_view;
    let monitors = &view.monitors;
    let mut labels = vec![t("All monitors").to_owned()];
    let mut names = Vec::new();
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
    let mut ids = Vec::new();
    let mut lines = Vec::new();
    for (id, saved) in &remembered.saved {
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
    // The tiles: an open game window where it is (or where the map moved it, until Apply), a
    // remembered one where it will open, and an account with neither where Grid would put it.
    let open = app.features.as_ref().map(|r| r.layout_windows()).unwrap_or_default();
    let spare = monitors.iter().find(|m| m.primary).or(monitors.first())
        .map(|m| layouts::arrange(Preset::Grid, app.accounts.len(), &[m.work], remembered.aspect)).unwrap_or_default();
    let mut tiles: Vec<Tile> = app.accounts.iter().enumerate().map(|(i, row)| {
        let window = open.iter().find(|(account, ..)| *account == row.key).map(|(_, rect, _)| *rect);
        let saved = remembered.saved.get(&app.feature_client_key(&row.key).id()).or_else(|| remembered.saved.get(&row.key))
            .and_then(|saved| layouts::restore_rect(saved, monitors)).map(|(rect, _)| rect);
        let pending = window.and(view.pending.get(&row.key).copied());
        let place = pending.or(window).or(saved);
        Tile {
            key: row.key.clone(),
            name: crate::privacy::mask(&row.account.label),
            live: crate::account_row::row_state(app, row) == crate::account_row::RowState::Playing,
            open: window.is_some(),
            placed: place.is_some(),
            pending: pending.is_some(),
            rect: place.or(spare.get(i).copied()).unwrap_or_default(),
        }
    }).collect();
    // A tile being dragged or nudged stays where the pointer or the keys have it; with other
    // accounts than before there is no such tile any more.
    let same = tiles.iter().map(|tile| &tile.key).eq(state.map.tiles.iter().map(|tile| &tile.key));
    let held = state.drag.as_ref().filter(|drag| drag.moved).map(|drag| drag.index).or(state.nudge.map(|(index, _)| index));
    match held.filter(|_| same) {
        Some(index) => tiles[index].rect = state.map.tiles[index].rect,
        None if !same => (state.drag, state.nudge) = (None, None),
        None => {}
    }
    state.map = MapInputs { screens: monitors.iter().map(snap::Screen::from).collect(), monitors: monitors.clone(), names, tiles };
    draw_map(g, state);
}

/// A tile's row on the map.
fn tile_row(map: &Map, index: usize, tile: &Tile) -> MapTile {
    let [x, y, w, h] = map.fractions(&tile.rect);
    MapTile {
        index: (index + 1).to_string().into(),
        name: tile.name.as_str().into(),
        size: format!("{} × {}", tile.rect.w, tile.rect.h).into(),
        x, y, w, h,
        live: tile.live,
        placed: tile.placed,
        pending: tile.pending,
    }
}

impl State {
    /// The map's geometry: only fractions are asked of it, so its width does not matter.
    fn map(&self) -> Option<Map> { snap::bounds(&self.map.screens).map(|all| Map::new(all, 1.0)) }

    fn selected_index(&self) -> Option<usize> {
        let key = self.selected.as_ref()?;
        self.map.tiles.iter().position(|tile| &tile.key == key)
    }
}

/// The whole map: every monitor to scale, every tile, the selection and what is said of it.
fn draw_map(g: &Features, state: &State) {
    let Some(map) = state.map() else {
        sync_model(&state.map_monitors, Vec::new());
        sync_model(&state.map_tiles, Vec::new());
        sync_model(&state.map_guides, Vec::new());
        put!(g, get_can_apply, set_can_apply, false);
        put!(g, get_apply_pending, set_apply_pending, false);
        selection(g, state);
        return;
    };
    put!(g, get_map_aspect, set_map_aspect, map.bounds.w as f32 / map.bounds.h.max(1) as f32);
    let several = state.map.monitors.len() > 1;
    sync_model(&state.map_monitors, state.map.monitors.iter().enumerate().map(|(i, monitor)| {
        let ([x, y, w, h], [work_x, work_y, work_w, work_h]) = (map.fractions(&monitor.rect), map.fractions(&monitor.work));
        let name = if several { state.map.names.get(i).and_then(|name| name.rsplit(' ').next()).unwrap_or_default() } else { "" };
        MapMonitor { x, y, w, h, work_x, work_y, work_w, work_h, name: name.into() }
    }).collect());
    sync_model(&state.map_tiles, state.map.tiles.iter().enumerate().map(|(i, tile)| tile_row(&map, i, tile)).collect());
    if state.drag.is_none() { sync_model(&state.map_guides, Vec::new()); }
    put!(g, get_can_apply, set_can_apply, state.map.tiles.iter().any(|tile| tile.open));
    put!(g, get_apply_pending, set_apply_pending, state.map.tiles.iter().any(|tile| tile.pending));
    selection(g, state);
}

/// One tile's row again (it moved), and what is said of the selected tile.
fn draw_tile(g: &Features, state: &State, index: usize) {
    let (Some(map), Some(tile)) = (state.map(), state.map.tiles.get(index)) else { return };
    let row = tile_row(&map, index, tile);
    if state.map_tiles.row_data(index).as_ref() != Some(&row) { state.map_tiles.set_row_data(index, row); }
    put!(g, get_map_readout, set_map_readout, readout(state).into());
}

/// The selected tile, and what is said of it.
fn selection(g: &Features, state: &State) {
    put!(g, get_map_selected, set_map_selected, state.selected_index().map_or(-1, |index| index as i32));
    put!(g, get_map_readout, set_map_readout, readout(state).into());
}

/// The selected tile in words: its account, its size, and where it is on its monitor.
fn readout(state: &State) -> String {
    let Some(tile) = state.selected_index().and_then(|index| state.map.tiles.get(index)) else { return String::new() };
    let Some(at) = snap::screen_of(&tile.rect, &state.map.screens) else { return String::new() };
    let (monitor, display) = (&state.map.monitors[at], state.map.names.get(at).cloned().unwrap_or_default());
    tf("{name} · {width} × {height} at {x}, {y} on {display}", &[
        ("name", &tile.name), ("width", &tile.rect.w), ("height", &tile.rect.h),
        ("x", &(tile.rect.x - monitor.rect.x)), ("y", &(tile.rect.y - monitor.rect.y)), ("display", &display),
    ])
}

/// The pointer's shape over what a press there would take hold of (`map-cursor`).
fn cursor(grab: Grab) -> i32 {
    match (grab.left || grab.right, grab.top || grab.bottom) {
        (false, false) => 1,
        (true, false) => 2,
        (false, true) => 3,
        (true, true) if grab.left == grab.top => 4,
        (true, true) => 5,
    }
}

/// Runs `f` on the page's state, unless a sync has it.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok().and_then(|mut slot| slot.as_mut().map(f)))
}

/// The pointer on the map (`map-pointer`): `kind` 0 pressed, 1 moved, 2 released, 3 the press
/// was taken away; (`fx`, `fy`) where it is as fractions of the map, `fit` how many pixels wide
/// the map is drawn, `alt` whether Alt is held (no snapping).
fn map_pointer(g: &Features, kind: i32, fx: f32, fy: f32, fit: f32, alt: bool) {
    let placed = with_state(|state| {
        let map = Map::new(snap::bounds(&state.map.screens)?, f64::from(fit.max(1.0)));
        let at = map.at(fx, fy);
        match kind {
            0 => pressed(g, state, &map, at),
            1 => moved(g, state, &map, at, alt),
            2 => return released(g, state, at),
            _ => cancel(g, state),
        }
        None
    }).flatten();
    if let Some((key, rect)) = placed { feature(Msg::Place(key, rect)); }
}

/// A press: the tile under it is selected and taken hold of, by its middle or by an edge; a
/// press on no tile selects none.
fn pressed(g: &Features, state: &mut State, map: &Map, at: (i32, i32)) {
    let rects: Vec<Rect> = state.map.tiles.iter().map(|tile| tile.rect).collect();
    let selected = state.selected_index();
    state.drag = snap::hit(&rects, selected, at, map.reach(GRIP_PX)).map(|(index, grab)| {
        put!(g, get_map_cursor, set_map_cursor, cursor(grab));
        Dragging { index, drag: snap::Drag::new(grab, rects[index]), press: at, moved: false, was_selected: selected == Some(index) }
    });
    state.selected = state.drag.as_ref().map(|drag| state.map.tiles[drag.index].key.clone());
    selection(g, state);
    // The pressed tile takes the keyboard focus (the arrow keys are its own).
    if state.drag.is_some() { g.set_map_focus(g.get_map_focus().wrapping_add(1)); }
}

/// The pointer moved: the tile held goes with it, snapped, and the lines holding it show; with
/// none held, the pointer takes the shape of what a press here would do.
fn moved(g: &Features, state: &mut State, map: &Map, at: (i32, i32), alt: bool) {
    let Some(mut drag) = state.drag.take() else {
        let rects: Vec<Rect> = state.map.tiles.iter().map(|tile| tile.rect).collect();
        let over = snap::hit(&rects, state.selected_index(), at, map.reach(GRIP_PX));
        put!(g, get_map_cursor, set_map_cursor, over.map_or(0, |(_, grab)| cursor(grab)));
        return;
    };
    let (dx, dy) = (at.0 - drag.press.0, at.1 - drag.press.1);
    if drag.moved || dx.abs().max(dy.abs()) >= map.reach(SLOP_PX) {
        drag.moved = true;
        let index = drag.index;
        let others: Vec<Rect> = state.map.tiles.iter().enumerate().filter(|(i, tile)| *i != index && tile.placed).map(|(_, tile)| tile.rect).collect();
        let step = drag.drag.step(dx, dy, at, &state.map.screens, &others, map.reach(SNAP_PX), !alt);
        state.map.tiles[index].rect = step.rect;
        sync_model(&state.map_guides, step.guides.iter().map(|guide| {
            // A line's place along its own axis, and its ends across it, as fractions of the map.
            let (w, h) = (map.bounds.w.max(1) as f32, map.bounds.h.max(1) as f32);
            let (along, across, origin) = if guide.vertical { (w, h, (map.bounds.x, map.bounds.y)) } else { (h, w, (map.bounds.y, map.bounds.x)) };
            MapGuide { vertical: guide.vertical, at: (guide.at - origin.0) as f32 / along, from: (guide.from - origin.1) as f32 / across, to: (guide.to - origin.1) as f32 / across }
        }).collect());
        draw_tile(g, state, index);
    }
    state.drag = Some(drag);
}

/// The press ended: a tile that was dragged is placed (the core remembers it, or holds it for
/// Apply). A click on the tile that was selected already goes to the next tile under the
/// pointer, so each of several that fill the same monitor can be reached.
fn released(g: &Features, state: &mut State, at: (i32, i32)) -> Option<(String, Rect)> {
    let drag = state.drag.take()?;
    sync_model(&state.map_guides, Vec::new());
    let tile = state.map.tiles.get(drag.index)?;
    if drag.moved {
        return (tile.rect != drag.drag.start() || !tile.placed).then(|| (tile.key.clone(), tile.rect));
    }
    if drag.was_selected { next_under(g, state, drag.index, at); }
    None
}

/// Selects the tile under `from` at `at` (and gives it the focus), when there is one.
fn next_under(g: &Features, state: &mut State, from: usize, at: (i32, i32)) {
    let rects: Vec<Rect> = state.map.tiles.iter().map(|tile| tile.rect).collect();
    let stack = snap::under(&rects, at);
    let Some(place) = stack.iter().position(|index| *index == from).filter(|_| stack.len() > 1) else { return };
    state.selected = Some(state.map.tiles[stack[(place + 1) % stack.len()]].key.clone());
    selection(g, state);
    g.set_map_focus(g.get_map_focus().wrapping_add(1));
}

/// The drag is off (Escape, or the press was taken away): the tile goes back where it was.
fn cancel(g: &Features, state: &mut State) {
    let Some(drag) = state.drag.take() else { return };
    sync_model(&state.map_guides, Vec::new());
    if let Some(tile) = state.map.tiles.get_mut(drag.index) { tile.rect = drag.drag.start(); }
    draw_tile(g, state, drag.index);
}

/// A tile took the keyboard focus (Tab, the controller's D-pad, or a press on it): it is the
/// selected one.
fn tile_focused(g: &Features, tile: i32) {
    with_state(|state| {
        let Some(key) = index(tile).and_then(|i| state.map.tiles.get(i)).map(|tile| tile.key.clone()) else { return };
        if state.selected.as_ref() != Some(&key) {
            state.selected = Some(key);
            selection(g, state);
        }
    });
}

/// The tile the arrow keys were moving is let go: it is placed where they left it.
fn end_nudge(state: &mut State) -> Option<(String, Rect)> {
    let (index, from) = state.nudge.take()?;
    let tile = state.map.tiles.get(index)?;
    (tile.rect != from || !tile.placed).then(|| (tile.key.clone(), tile.rect))
}

/// A tile lost the keyboard focus: what the arrow keys did to it is placed.
fn tile_blurred(tile: i32) {
    let tile = index(tile);
    let placed = with_state(|state| if state.nudge.is_some_and(|(at, _)| Some(at) == tile) { end_nudge(state) } else { None }).flatten();
    if let Some((key, rect)) = placed { feature(Msg::Place(key, rect)); }
}

/// A key on the focused tile (`tile-key`). The arrow keys move it one screen pixel (ten with
/// Shift) for as long as they repeat, and it is placed when the key is let go. Return or Space
/// (the controller's A) goes to the next tile under it. Escape lets go of a drag. Whether the
/// key was the tile's.
fn tile_key(g: &Features, tile: i32, text: &str, shift: bool, down: bool) -> bool {
    use slint::platform::Key;
    let is = |key: Key| text.chars().eq(std::iter::once(char::from(key)));
    let arrow = [(Key::LeftArrow, (-1, 0)), (Key::RightArrow, (1, 0)), (Key::UpArrow, (0, -1)), (Key::DownArrow, (0, 1))]
        .into_iter().find(|(key, _)| is(*key)).map(|(_, way)| way);
    let (taken, placed) = with_state(|state| {
        let Some(at) = index(tile).filter(|at| *at < state.map.tiles.len()) else { return (false, None) };
        if let Some((dx, dy)) = arrow {
            if state.drag.is_some() { return (true, None); }
            if !down { return (true, end_nudge(state)); }
            let step = if shift { 10 } else { 1 };
            let from = state.map.tiles[at].rect;
            state.map.tiles[at].rect = snap::nudge(from, dx * step, dy * step, &state.map.screens);
            if state.nudge.is_none_or(|(index, _)| index != at) { state.nudge = Some((at, from)); }
            draw_tile(g, state, at);
            return (true, None);
        }
        if !down { return (false, None); }
        if is(Key::Escape) && state.drag.is_some() {
            cancel(g, state);
            return (true, None);
        }
        if is(Key::Return) || text == " " {
            let r = state.map.tiles[at].rect;
            next_under(g, state, at, (r.x + r.w / 2, r.y + r.h / 2));
            return (true, None);
        }
        (false, None)
    }).unwrap_or((false, None));
    if let Some((key, rect)) = placed { feature(Msg::Place(key, rect)); }
    taken
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
        Section::Windows => match settings.layouts.saved.len() {
            0 => t("None remembered").to_owned(),
            count => tf("{count} remembered", &[("count", &count)]),
        },
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

/// One hotkey's row. Under the field: where an account's hotkey works when that is only over a
/// game (its chord has no Ctrl, Alt or Win), said quietly; then the first of: why the chord
/// last pressed was not taken (`error`); why the hotkey does not work; and, in amber, what it
/// clashes with while it does: another of Crystal's hotkeys (`set`: every hotkey set, by name),
/// a program that registered the same chord, and what `Hotkey::warnings` says of the chord.
fn hotkey_line(name: String, hotkey: Option<Hotkey>, action: &Action, set: &[(String, Action, Hotkey)], view: &fui::State, error: Option<&String>) -> HotkeyLine {
    let status = hotkey.and_then(|h| view.hotkeys.iter().find(|s| &s.action == action && s.hotkey == h));
    let problem = status.and_then(|s| s.problem);
    let in_game = hotkey.is_some_and(|h| hotkeys::in_game_only(action, h));
    let twin = hotkey.and_then(|h| set.iter().find(|(_, other, chord)| other != action && *chord == h)).map(|(name, _, _)| name);
    let elsewhere = || t("Another program also uses this hotkey.").to_owned();
    let (problem_text, warn) = match (error, problem, hotkey) {
        (Some(error), _, _) => (error.clone(), false),
        (None, Some(Problem::Duplicate), _) => (twin.map_or_else(|| Problem::Duplicate.text(), |name| tf("{name} uses this hotkey.", &[("name", name)])), true),
        // Over a game the keyboard hook takes the chord first: it works, and the other program's does not.
        (None, Some(Problem::InUse), _) if in_game => (elsewhere(), true),
        (None, Some(problem), _) => (problem.text(), false),
        (None, None, Some(hotkey)) => {
            let mut said: Vec<String> = Vec::new();
            if let Some(name) = twin { said.push(tf("Also set for {name}.", &[("name", name)])); }
            else if in_game && view.hotkeys_taken.contains(&hotkey) { said.push(elsewhere()); }
            said.extend(hotkey.warnings(in_game).iter().map(|warning| warning.text().to_owned()));
            (said.join(" "), !said.is_empty())
        }
        (None, None, None) => (String::new(), false),
    };
    // The cycle keys' rows are named "(in game only)" already.
    let note = if in_game && !action.in_game_only() { t("Active while a game is in front.") } else { "" };
    HotkeyLine {
        name: name.into(),
        text: hotkey.map(|h| h.label()).unwrap_or_default().into(),
        note: note.into(),
        active: status.is_some_and(|s| s.problem.is_none() || (in_game && s.problem == Some(Problem::InUse))) && error.is_none(),
        problem: problem_text.into(),
        warn,
    }
}

fn hotkeys(app: &App, settings: &FeatureSettings, state: &mut State) {
    let view = &app.features_view;
    let errors = &view.field_errors;
    let in_game = |name: &str| tf("{name} (in game only)", &[("name", &name)]);
    let label = |row: &crate::AccountRow| crate::privacy::mask(&row.account.label);
    let mut set: Vec<(String, Action, Hotkey)> = Vec::new();
    if let Some(hotkey) = settings.hotkeys.cycle { set.push((t("Next window").to_owned(), Action::Next, hotkey)); }
    if let Some(hotkey) = settings.hotkeys.cycle_back { set.push((t("Previous window").to_owned(), Action::Previous, hotkey)); }
    set.extend(app.accounts.iter().filter_map(|row| Some((label(row), Action::Account(row.key.clone()), *settings.hotkeys.accounts.get(&row.key)?))));
    let cycle = vec![
        hotkey_line(in_game(t("Next window")), settings.hotkeys.cycle, &Action::Next, &set, view, errors.get("hotkey:next")),
        hotkey_line(in_game(t("Previous window")), settings.hotkeys.cycle_back, &Action::Previous, &set, view, errors.get("hotkey:previous")),
    ];
    sync_model(&state.cycle_hotkeys, cycle);
    let accounts = app.accounts.iter().map(|row| hotkey_line(
        label(row),
        settings.hotkeys.accounts.get(&row.key).copied(),
        &Action::Account(row.key.clone()),
        &set,
        view,
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

    fn key(ui: &MainWindow, key: Key) {
        let text: SharedString = key.into();
        ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        ui.window().dispatch_event(WindowEvent::KeyReleased { text });
        render();
    }

    /// The top bar's Discord button: named by its tooltip, reached by the controller walker
    /// like the tools beside it, and it opens Crystal's invite.
    #[test]
    fn the_top_bar_opens_the_discord_invite() {
        let app = crate::tests::app();
        let ui = window(&app);
        assert_eq!(ui.global::<super::super::View>().get_strings().discord, "Join our Discord");
        let at = |name: &str| {
            assert!(pad::focus_named(ui.window(), name), "{name}");
            pad::focus_rect(ui.window()).unwrap()
        };
        let [privacy_x, privacy_y, privacy_w, privacy_h] = at("Privacy mode");
        let [x, y, w, h] = at("Join our Discord");
        assert_eq!((y, w, h), (privacy_y, privacy_w, privacy_h), "the tools' size, on their line");
        assert_eq!(x + w, privacy_x, "next to Privacy mode");
        // A (Return) presses it.
        let _ = take_sent();
        key(&ui, Key::Return);
        let sent = take_sent();
        assert!(matches!(sent.as_slice(), [Message::OpenUrl(url)] if url == crate::DISCORD_INVITE), "{sent:?}");
        ui.global::<super::super::View>().invoke_open_discord();
        assert!(matches!(take_sent().as_slice(), [Message::OpenUrl(url)] if url == crate::DISCORD_INVITE));
    }

    #[test]
    fn every_switch_has_its_own_message() {
        let all: Vec<String> = (0..13).map(|i| format!("{:?}", toggle(i, true).expect("mapped"))).collect();
        let mut unique = all.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 13, "{all:?}");
        assert!(toggle(13, true).is_none() && toggle(-1, true).is_none());
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
        // The six section tiles, plus each section's own controls (the three presets and the
        // monitor they fill, a tile per account on the map, the three switches, two Forget
        // buttons); the page lies right of the rail and under the top bar.
        let own = [
            (Section::Windows, 3 + 1 + app.accounts.len() + 3 + 2),
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
                    assert_eq!(refused.text, "Win+L");
                    assert!(!refused.problem.is_empty() && !refused.warn && !refused.active, "a chord Windows keeps is refused with its reason");
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

    /// Going to another page leaves the Features page.
    #[test]
    fn navigating_leaves_the_page() {
        let mut app = crate::tests::app();
        app.load_features_sample(Section::Windows, false);
        assert!(app.features_view.open);
        drop(app.update(Message::Navigate(crate::Page::Settings)));
        assert!(!app.features_view.open, "Features is a page: going elsewhere leaves it");
    }

    // ---- the window layout editor (the monitor map) ----

    use crate::features::runtime::StandInDesktop;
    use slint::platform::PointerEventButton;
    use slint::LogicalPosition;

    /// A mixed-DPI desktop: a 4K monitor at 150 % with its taskbar, a 1080p one at 100 % right of it.
    fn desktop() -> Vec<Monitor> {
        vec![
            Monitor { device: r"\\.\DISPLAY1".into(), rect: Rect::new(0, 0, 3840, 2160), work: Rect::new(0, 0, 3840, 2088), dpi: 144, primary: true },
            Monitor { device: r"\\.\DISPLAY2".into(), rect: Rect::new(3840, 0, 1920, 1080), work: Rect::new(3840, 0, 1920, 1040), dpi: 96, primary: false },
        ]
    }

    const SECOND: Rect = Rect::new(600, 600, 1280, 720);

    /// The Windows section over that desktop, with `open` game windows (account, rectangle,
    /// movable). The first account's window is remembered over the whole main monitor, the
    /// second's at `SECOND`; the third has none. The account keys.
    fn layout_page(app: &mut App, open: &[(usize, Rect, bool)]) -> (MainWindow, Vec<String>) {
        let ui = window(app);
        app.load_features_sample(Section::Windows, false);
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let runtime = app.features.clone().unwrap();
        runtime.stand_in(StandInDesktop { monitors: desktop(), windows: open.iter().map(|(i, rect, movable)| (keys[*i].clone(), *rect, *movable)).collect() });
        runtime.update(|s| s.layouts.saved.clear()).unwrap();
        runtime.place_windows(&[(ClientKey::new(keys[0].clone(), None), desktop()[0].rect), (ClientKey::new(keys[1].clone(), None), SECOND)]).unwrap();
        drop(app.feature_update(Msg::Section(Section::Windows)));
        sync(&ui, app);
        render();
        (ui, keys)
    }

    /// Where screen points are in the window: read from the first account's tile, which fills
    /// the main monitor (drawn a pixel inside its rectangle).
    struct OnMap { x: f32, y: f32, scale: f32 }
    impl OnMap {
        fn read(ui: &MainWindow, app: &App) -> Self {
            let name = app.accounts[0].account.label.clone();
            assert!(pad::focus_named(ui.window(), &name), "{:?}", pad::names(ui.window()));
            let [x, y, w, _] = pad::focus_rect(ui.window()).unwrap();
            Self { x: x - 1.0, y: y - 1.0, scale: (w + 2.0) / 3840.0 }
        }
        fn at(&self, x: i32, y: i32) -> LogicalPosition { LogicalPosition::new(self.x + x as f32 * self.scale, self.y + y as f32 * self.scale) }
    }

    fn pointer(ui: &MainWindow, event: WindowEvent) {
        ui.window().dispatch_event(event);
        render();
    }

    /// Press at `from` and carry the pointer to `to` in steps, the button still down.
    fn carry(ui: &MainWindow, from: LogicalPosition, to: LogicalPosition) {
        pointer(ui, WindowEvent::PointerMoved { position: from });
        pointer(ui, WindowEvent::PointerPressed { position: from, button: PointerEventButton::Left });
        for step in 1..=8 {
            let part = step as f32 / 8.0;
            pointer(ui, WindowEvent::PointerMoved { position: LogicalPosition::new(from.x + (to.x - from.x) * part, from.y + (to.y - from.y) * part) });
        }
    }

    fn let_go(app: &mut App, ui: &MainWindow, at: LogicalPosition) -> Vec<Message> {
        pointer(ui, WindowEvent::PointerReleased { position: at, button: PointerEventButton::Left });
        pump(app, ui)
    }

    fn drag(app: &mut App, ui: &MainWindow, from: LogicalPosition, to: LogicalPosition) -> Vec<Message> {
        carry(ui, from, to);
        let_go(app, ui, to)
    }

    fn click(app: &mut App, ui: &MainWindow, at: LogicalPosition) -> Vec<Message> { drag(app, ui, at, at) }

    fn remembered(app: &App, key: &str) -> layouts::Saved { app.features.as_ref().unwrap().settings().layouts.saved[key].clone() }

    /// The map shows every monitor to scale where Windows has it, with its work area, and a
    /// tile per account: where its window is remembered, or faintly where Grid would put it.
    #[test]
    fn the_map_shows_every_monitor_to_scale_and_a_tile_per_account() {
        let mut app = crate::tests::app();
        let (ui, _) = layout_page(&mut app, &[]);
        let g = ui.global::<Features>();
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(near(g.get_map_aspect(), 5760.0 / 2160.0));
        let monitors: Vec<MapMonitor> = g.get_map_monitors().iter().collect();
        assert_eq!(monitors.len(), 2);
        let (main, side) = (&monitors[0], &monitors[1]);
        assert!(near(main.x, 0.0) && near(main.w, 3840.0 / 5760.0) && near(main.h, 1.0) && near(main.work_h, 2088.0 / 2160.0), "{main:?}");
        assert!(near(side.x, 3840.0 / 5760.0) && near(side.w, 1920.0 / 5760.0) && near(side.h, 0.5) && near(side.work_h, 1040.0 / 2160.0), "{side:?}");
        assert_eq!((main.name.as_str(), side.name.as_str()), ("1", "2"));
        let tiles: Vec<MapTile> = g.get_map_tiles().iter().collect();
        assert_eq!(tiles.len(), app.accounts.len(), "a tile per account");
        assert_eq!((tiles[0].index.as_str(), tiles[0].name.as_str(), tiles[0].size.as_str(), tiles[0].placed), ("1", "Main account", "3840 × 2160", true));
        assert!(near(tiles[0].w, main.w) && near(tiles[0].h, 1.0), "over the whole main monitor");
        assert_eq!((tiles[1].size.as_str(), tiles[1].placed, tiles[1].pending), ("1280 × 720", true, false));
        assert!(!tiles[2].placed, "nothing remembered: drawn where Grid would put it");
        assert!(!g.get_can_apply() && !g.get_apply_pending(), "no game window is open: nothing to apply");
        assert_eq!((g.get_map_selected(), g.get_map_readout().as_str()), (-1, ""));
        // On screen the monitors keep that scale to the pixel: the 1080p one is half as wide and
        // half as tall as the 4K one, and starts where it ends.
        let on = OnMap::read(&ui, &app);
        let [x, y, w, h] = pad::focus_rect(ui.window()).unwrap();
        assert_eq!((x.fract(), y.fract(), w.fract(), h.fract()), (0.0, 0.0, 0.0, 0.0), "whole pixels");
        assert!(((h + 2.0) / (w + 2.0) - 2160.0 / 3840.0).abs() < 0.005, "{w} x {h}");
        assert_eq!((g.get_map_selected(), g.get_map_readout().as_str()), (0, "Main account · 3840 × 2160 at 0, 0 on Display 1"), "the focused tile is the selected one");
        assert!(on.at(5760, 2160).x < 1440.0 && on.at(5760, 2160).y < 900.0, "the map is on screen");
        // Privacy mode masks the tiles' names and what is said of the selected one.
        crate::privacy::set(true);
        sync(&ui, &app);
        assert!(g.get_map_tiles().iter().all(|tile| !tile.name.contains("account")) && !g.get_map_readout().contains("Main"));
        crate::privacy::set(false);
    }

    /// A real press, move and release on the map: a tile carried to a monitor's top edge fills
    /// that monitor exactly, one carried to a side edge fills that half, and each is saved as
    /// the account's remembered window, on the monitor it is now on.
    #[test]
    fn a_dragged_tile_snaps_and_is_saved_as_the_remembered_window() {
        let mut app = crate::tests::app();
        let (ui, keys) = layout_page(&mut app, &[]);
        let g = ui.global::<Features>();
        let on = OnMap::read(&ui, &app);
        let side = desktop()[1].clone();
        // A press on no tile selects none.
        assert!(click(&mut app, &ui, on.at(5000, 1600)).is_empty());
        assert_eq!(g.get_map_selected(), -1);
        // The second account's tile, by its middle, to the top edge of the 1080p monitor.
        carry(&ui, on.at(1240, 960), on.at(4800, 12));
        assert_eq!(g.get_map_selected(), 1, "the pressed tile is selected");
        assert_eq!(g.get_map_guides().row_count(), 4, "the lines of the monitor it fills show while it is held");
        assert_eq!(g.get_map_tiles().row_data(1).unwrap().size, "1920 × 1080");
        assert!(take_sent().is_empty(), "nothing is saved while it is held");
        let sent = let_go(&mut app, &ui, on.at(4800, 12));
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Place(key, rect))] if *key == keys[1] && *rect == side.rect), "{sent:?}");
        assert_eq!(g.get_map_guides().row_count(), 0);
        let saved = remembered(&app, &keys[1]);
        assert_eq!((saved.rect, saved.mode), (side.rect, crate::features::game_window::Mode::Fullscreen), "exactly the monitor: borderless full screen");
        assert_eq!(saved.monitor, layouts::MonitorRef::from(&side), "the monitor it is on now, with its own scaling");
        assert_eq!(g.get_map_readout(), "Second account · 1920 × 1080 at 0, 0 on Display 2");
        // The third account's tile (nothing remembered) to the main monitor's left edge: the
        // left half of its work area, above the taskbar.
        let spare = layouts::arrange(Preset::Grid, 3, &[desktop()[0].work], 16.0 / 9.0)[2];
        assert!(!g.get_map_tiles().row_data(2).unwrap().placed);
        let sent = drag(&mut app, &ui, on.at(spare.x + spare.w / 2, spare.y + spare.h / 2), on.at(8, 1000));
        let half = Rect::new(0, 0, 1920, 2088);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Place(key, rect))] if *key == keys[2] && *rect == half), "{sent:?}");
        assert_eq!(remembered(&app, &keys[2]).rect, half);
        assert!(g.get_map_tiles().row_data(2).unwrap().placed);
        // The same file is the one source: the remembered list has it too.
        assert_eq!(g.get_layouts().row_count(), 3);
        // By its right edge to the main monitor's two-thirds line: resized, the rest unmoved.
        let sent = drag(&mut app, &ui, on.at(1920 - 12, 1000), on.at(2560 + 20, 1000));
        let wide = Rect::new(0, 0, 2560, 2088);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Place(key, rect))] if *key == keys[2] && *rect == wide), "{sent:?}");
        // A click moves nothing and saves nothing.
        assert!(click(&mut app, &ui, on.at(1000, 1000)).is_empty());
        assert_eq!(remembered(&app, &keys[2]).rect, wide);
    }

    /// Alt places a tile freely; Escape lets go of a drag and the tile goes back.
    #[test]
    fn alt_turns_snapping_off_and_escape_drops_the_drag() {
        let mut app = crate::tests::app();
        let (ui, keys) = layout_page(&mut app, &[]);
        let g = ui.global::<Features>();
        let on = OnMap::read(&ui, &app);
        click(&mut app, &ui, on.at(5000, 1600));
        // 560 px left: 40 px from the monitor's edge, well within reach of it.
        let (from, to) = (on.at(1240, 960), on.at(680, 960));
        press(&ui, Key::Alt);
        let sent = drag(&mut app, &ui, from, to);
        release(&ui, Key::Alt);
        let [Message::Feature(Msg::Place(account, free))] = sent.as_slice() else { panic!("one tile placed") };
        let free = *free;
        assert_eq!(*account, keys[1]);
        assert!(free.x != 0 && (free.x - 40).abs() <= 12 && (free.y, free.w, free.h) == (600, 1280, 720), "placed freely: {free:?}");
        assert_eq!(remembered(&app, &keys[1]).rect, free);
        // Without Alt the same drop, upwards, takes the monitor's top edge (and, carried
        // straight up, not the left one beside it).
        let sent = drag(&mut app, &ui, on.at(free.x + 640, 960), on.at(free.x + 640, 960 - 560));
        let [Message::Feature(Msg::Place(_, top))] = sent.as_slice() else { panic!("one tile placed") };
        let top = *top;
        assert_eq!(top, Rect::new(free.x, 0, 1280, 720));
        // Escape while a tile is held: back where it was, nothing saved.
        let before = g.get_map_tiles().row_data(1).unwrap();
        carry(&ui, on.at(640, 360), on.at(2400, 1300));
        assert_ne!(g.get_map_tiles().row_data(1).unwrap(), before);
        key(&ui, Key::Escape);
        assert_eq!(g.get_map_tiles().row_data(1).unwrap(), before);
        assert_eq!(g.get_map_guides().row_count(), 0);
        assert!(let_go(&mut app, &ui, on.at(2400, 1300)).is_empty());
        assert!(g.get_open(), "Escape was the drag's, not Back");
        assert_eq!(remembered(&app, &keys[1]).rect, top);
    }

    /// The arrow keys move the selected tile a pixel (ten with Shift) and it is saved when the
    /// key is let go; the controller's walker goes from tile to tile and selects each.
    #[test]
    fn arrow_keys_nudge_the_selected_tile_and_the_walker_selects_tiles() {
        let mut app = crate::tests::app();
        let (ui, keys) = layout_page(&mut app, &[]);
        let g = ui.global::<Features>();
        assert!(pad::focus_named(ui.window(), "Second account"));
        render();
        assert_eq!(g.get_map_selected(), 1);
        for _ in 0..3 { press(&ui, Key::RightArrow); }
        assert_eq!(g.get_map_readout(), "Second account · 1280 × 720 at 603, 600 on Display 1");
        assert!(pump(&mut app, &ui).is_empty(), "held: not saved yet");
        release(&ui, Key::RightArrow);
        let sent = pump(&mut app, &ui);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Place(key, rect))] if *key == keys[1] && *rect == Rect::new(603, 600, 1280, 720)), "{sent:?}");
        assert_eq!(remembered(&app, &keys[1]).rect, Rect::new(603, 600, 1280, 720));
        press(&ui, Key::Shift);
        key(&ui, Key::UpArrow);
        release(&ui, Key::Shift);
        pump(&mut app, &ui);
        assert_eq!(remembered(&app, &keys[1]).rect, Rect::new(603, 590, 1280, 720));
        // At the monitors' edge a tile stops; a key that moved nothing saves nothing.
        assert!(pad::focus_named(ui.window(), "Main account"));
        key(&ui, Key::LeftArrow);
        assert!(pump(&mut app, &ui).is_empty());
        // The focus leaving while a key is still down places the tile all the same.
        assert!(pad::focus_named(ui.window(), "Second account"));
        press(&ui, Key::DownArrow);
        assert!(pad::focus_named(ui.window(), "Main account"));
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::Place(_, rect))] if *rect == Rect::new(603, 591, 1280, 720)));
        release(&ui, Key::DownArrow);
        // The D-pad: from the first tile to the one on the other monitor's side, and on.
        assert_eq!(g.get_map_selected(), 0);
        let before = pad::focus_rect(ui.window()).unwrap();
        assert!(pad::step(ui.window(), pad::Way::Down) || pad::step(ui.window(), pad::Way::Up));
        render();
        assert_ne!(pad::focus_rect(ui.window()).unwrap(), before, "the walker moved the focus");
    }

    /// The presets fill the tiles in one press. Tiles that fill the same monitor lie on top of
    /// each other: a click on the selected one, or Return on it, goes to the next under it.
    #[test]
    fn presets_fill_the_tiles_and_a_click_goes_through_a_stack() {
        let mut app = crate::tests::app();
        let (ui, keys) = layout_page(&mut app, &[]);
        let g = ui.global::<Features>();
        let on = OnMap::read(&ui, &app);
        click(&mut app, &ui, on.at(5000, 1600));
        let (main, side) = (desktop()[0].clone(), desktop()[1].clone());
        // Grid on every monitor: no two tiles overlap, each inside a work area.
        g.set_monitor(0);
        g.invoke_preset(0);
        pump(&mut app, &ui);
        let rects: Vec<Rect> = keys.iter().map(|key| remembered(&app, key).rect).collect();
        for (i, a) in rects.iter().enumerate() {
            assert!([main.work, side.work].iter().any(|area| area.intersection(a) == Some(*a)), "{a:?}");
            for b in &rects[i + 1..] { assert!(a.intersection(b).is_none(), "{a:?} overlaps {b:?}"); }
        }
        assert!(g.get_map_tiles().iter().all(|tile| tile.placed));
        // Main large on the 4K monitor.
        g.set_monitor(1);
        g.invoke_preset(1);
        pump(&mut app, &ui);
        let rects: Vec<Rect> = keys.iter().map(|key| remembered(&app, key).rect).collect();
        assert!(rects[1..].iter().all(|r| r.area() < rects[0].area() && r.x >= rects[0].right()), "{rects:?}");
        // Full screen there: every tile is the monitor, to the pixel.
        g.invoke_preset(2);
        pump(&mut app, &ui);
        for key in &keys {
            let saved = remembered(&app, key);
            assert_eq!((saved.rect, saved.mode), (main.rect, crate::features::game_window::Mode::Fullscreen));
        }
        // The last drawn is on top; each click on the selected one goes one further down.
        let middle = on.at(1900, 1000);
        let mut seen = Vec::new();
        for _ in 0..4 {
            assert!(click(&mut app, &ui, middle).is_empty());
            seen.push(g.get_map_selected());
        }
        assert_eq!(seen, [2, 1, 0, 2]);
        // Return (the controller's A) on the focused tile does the same.
        assert_eq!(pad::focused(ui.window()).as_deref(), Some("Crafting account"));
        key(&ui, Key::Return);
        assert_eq!(g.get_map_selected(), 1);
        assert_eq!(pad::focused(ui.window()).as_deref(), Some("Second account"));
        // Dragged off the stack, the selected one is alone again.
        let sent = drag(&mut app, &ui, middle, on.at(4800, 500));
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Place(key, _))] if *key == keys[1]), "{sent:?}");
        assert_eq!(remembered(&app, &keys[0]).rect, main.rect);
    }

    /// CRYSTAL_LAYOUT_SHOTS=<dir>: the layout editor as PNGs, for review: at rest with a tile
    /// selected, a tile held with the lines that hold it, a tile filling a monitor, a game
    /// window open with a tile waiting for Apply; dark, light, Japanese and the small window.
    #[test]
    fn layout_editor_screenshots() {
        let Some(dir) = std::env::var_os("CRYSTAL_LAYOUT_SHOTS").map(PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        crate::privacy::set(false);
        let shot = |name: &str, width: u32, height: u32| {
            SCREEN.with(|screen| {
                let screen = screen.borrow();
                let window = screen.as_ref().unwrap();
                slint::platform::update_timers_and_animations();
                let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                window.request_redraw();
                window.draw_if_needed(|renderer| { renderer.render(&mut pixels, width as usize); });
                let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                imagecrate::save_buffer_with_format(dir.join(format!("{name}.png")), &bytes, width, height, imagecrate::ExtendedColorType::Rgb8, imagecrate::ImageFormat::Png).unwrap();
            });
        };
        let mut app = crate::tests::app();
        app.accounts[0].pid = Some(7);
        app.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let (ui, keys) = layout_page(&mut app, &[]);
        let g = ui.global::<Features>();
        let theme = ui.global::<super::super::Theme>();
        let on = OnMap::read(&ui, &app);
        click(&mut app, &ui, on.at(5000, 1600));
        // Main large on the 4K monitor, then the third account's tile onto the 1080p one.
        g.set_monitor(1);
        g.invoke_preset(1);
        pump(&mut app, &ui);
        let third = remembered(&app, &keys[2]).rect;
        drag(&mut app, &ui, on.at(third.x + third.w / 2, third.y + third.h / 2), on.at(4800, 300));
        for (name, light) in [("editor-dark", false), ("editor-light", true)] {
            theme.set_light(light);
            render();
            shot(name, 1440, 900);
        }
        theme.set_light(false);
        // The second account's tile held under the first's right-hand neighbour: lines show.
        let second = remembered(&app, &keys[1]).rect;
        carry(&ui, on.at(second.x + second.w / 2, second.y + second.h / 2), on.at(second.x + second.w / 2 + 12, second.y + second.h / 2 + second.h + 30));
        shot("editor-drag-dark", 1440, 900);
        theme.set_light(true);
        render();
        shot("editor-drag-light", 1440, 900);
        theme.set_light(false);
        key(&ui, Key::Escape);
        let_go(&mut app, &ui, on.at(0, 0));
        // Carried to the main monitor's left edge: it fills that half.
        carry(&ui, on.at(second.x + second.w / 2, second.y + second.h / 2), on.at(6, 1100));
        shot("editor-fill-dark", 1440, 900);
        key(&ui, Key::Escape);
        let_go(&mut app, &ui, on.at(0, 0));
        // A game window open: its tile, moved, waits for Apply.
        app.features.as_ref().unwrap().stand_in(StandInDesktop { monitors: desktop(), windows: vec![(keys[0].clone(), remembered(&app, &keys[0]).rect, true)] });
        app.features.as_ref().unwrap().update(|s| s.layouts.undo = Default::default()).unwrap();
        drop(app.feature_update(Msg::Section(Section::Windows)));
        sync(&ui, &app);
        render();
        let first = remembered(&app, &keys[0]).rect;
        drag(&mut app, &ui, on.at(first.x + first.w / 2, first.y + first.h / 2), on.at(first.x + first.w / 2 + 30, first.y + first.h / 2 - first.y + 20 - 1));
        shot("editor-apply-dark", 1440, 900);
        crate::i18n::set(Lang::Ja);
        sync(&ui, &app);
        render();
        shot("editor-apply-ja", 1440, 900);
        crate::i18n::set(Lang::En);
    }

    /// With a game window open its tile is not saved when it is moved: it waits for Apply,
    /// which the page offers; the presets fill only the open windows' tiles.
    #[test]
    fn a_tile_of_an_open_window_waits_for_apply() {
        let mut app = crate::tests::app();
        let (ui, keys) = layout_page(&mut app, &[(1, SECOND, true), (2, Rect::new(3840, 0, 1920, 1080), false)]);
        let g = ui.global::<Features>();
        let on = OnMap::read(&ui, &app);
        click(&mut app, &ui, on.at(5000, 1600));
        assert!(g.get_can_apply() && !g.get_apply_pending());
        let before = remembered(&app, &keys[1]);
        // The open window's tile to the main monitor's top-left corner.
        let sent = drag(&mut app, &ui, on.at(1240, 960), on.at(680, 400));
        let corner = Rect::new(0, 0, 1280, 720);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Place(key, rect))] if *key == keys[1] && *rect == corner), "{sent:?}");
        assert_eq!(app.features_view.pending.get(&keys[1]), Some(&corner));
        assert_eq!(remembered(&app, &keys[1]), before, "not remembered until its window is there");
        let tile = g.get_map_tiles().row_data(1).unwrap();
        assert!(tile.pending && g.get_apply_pending());
        // Apply sends its message (no real window stands behind this one: it is said).
        let stops = pad::stop_origins(ui.window()).len();
        g.invoke_apply_layout();
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::ApplyLayout)]));
        assert_eq!(app.features_view.notice, Some((true, "No game window to move.".to_owned())));
        assert_eq!(app.features_view.pending.len(), 1, "the tile still waits");
        // A preset fills the open windows Apply may move (not the one that may hold its
        // display), and no remembered window.
        g.set_monitor(0);
        g.invoke_preset(2);
        pump(&mut app, &ui);
        assert_eq!(app.features_view.pending.iter().map(|(key, rect)| (key.clone(), *rect)).collect::<Vec<_>>(), vec![(keys[1].clone(), desktop()[0].rect)]);
        assert_eq!(remembered(&app, &keys[0]).rect, desktop()[0].rect);
        assert!(!app.features.as_ref().unwrap().settings().layouts.saved.contains_key(&keys[2]));
        // The windows closed: Apply goes, and so do the tiles that waited for it.
        app.features.as_ref().unwrap().stand_in(StandInDesktop { monitors: desktop(), windows: Vec::new() });
        drop(app.feature_update(Msg::Section(Section::Windows)));
        sync(&ui, &app);
        render();
        assert!(app.features_view.pending.is_empty() && !g.get_can_apply() && !g.get_apply_pending());
        assert_eq!(pad::stop_origins(ui.window()).len(), stops - 1, "the Apply button is gone");
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
        assert!(matches!(one(&|| g.invoke_hotkey_record(0)), Message::Feature(Msg::HotkeyRecord(Target::Next))));
        assert!(matches!(one(&|| g.invoke_hotkey_record(1)), Message::Feature(Msg::HotkeyRecord(Target::Previous))));
        assert!(matches!(one(&|| g.invoke_hotkey_record(3)), Message::Feature(Msg::HotkeyRecord(Target::Account(ref k))) if *k == keys[1]));
        assert!(sent(&|| g.invoke_hotkey_record(2 + keys.len() as i32)).is_empty());
        // Nothing is recording: a field losing the focus, a press and a key are nobody's.
        assert!(sent(&|| g.invoke_hotkey_blur(0)).is_empty());
        assert!(sent(&|| g.invoke_hotkey_pointer(0)).is_empty());
        assert!(sent(&|| { assert!(!g.invoke_hotkey_key("a".into(), true, false, false, false, true)); }).is_empty());
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
        assert!(matches!(one(&|| g.invoke_preset(1)), Message::Feature(Msg::Preset(Preset::MainLarge, None))));
        assert!(matches!(one(&|| g.invoke_preset(2)), Message::Feature(Msg::Preset(Preset::FullScreen, None))));
        assert!(sent(&|| g.invoke_preset(3)).is_empty());
        assert!(matches!(one(&|| g.invoke_apply_layout()), Message::Feature(Msg::ApplyLayout)));
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
        drop(app.feature_update(Msg::Hotkey(Target::Next, Hotkey::parse("Alt+Tab"))));
        sync(&ui, &app);
        assert!(!g.get_cycle_hotkeys().row_data(0).unwrap().problem.is_empty());
        assert_ne!(read(|s| Some(s.section_built)), built);
    }

    fn press(ui: &MainWindow, key: impl Into<SharedString>) {
        ui.window().dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        render();
    }

    fn release(ui: &MainWindow, key: impl Into<SharedString>) {
        ui.window().dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        render();
    }

    /// Carries what the page sent to the core and syncs, as the host does after each message,
    /// until nothing more is sent. What was sent.
    fn pump(app: &mut App, ui: &MainWindow) -> Vec<Message> {
        let mut all = Vec::new();
        loop {
            let sent = take_sent();
            if sent.is_empty() { break; }
            for message in sent {
                if let Message::Feature(msg) = &message { drop(app.feature_update(msg.clone())); }
                all.push(message);
            }
            sync(ui, app);
            render();
        }
        all
    }

    /// The Hotkeys section on screen, and its rows' names: next window, previous window, the
    /// first account.
    fn hotkeys_page(app: &mut App) -> (MainWindow, [String; 3]) {
        let ui = window(app);
        app.load_features_sample(Section::Hotkeys, false);
        sync(&ui, app);
        render();
        let g = ui.global::<Features>();
        let names = [
            g.get_cycle_hotkeys().row_data(0).unwrap().name.to_string(),
            g.get_cycle_hotkeys().row_data(1).unwrap().name.to_string(),
            g.get_account_hotkeys().row_data(0).unwrap().name.to_string(),
        ];
        (ui, names)
    }

    /// Return on the field named `name` (the controller's A sends the same): it records.
    fn record(app: &mut App, ui: &MainWindow, name: &str) {
        assert!(pad::focus_named(ui.window(), name), "{name}");
        key(ui, Key::Return);
        pump(app, ui);
    }

    fn cycle(app: &App) -> (Option<Hotkey>, Option<Hotkey>) {
        let hotkeys = app.features.as_ref().unwrap().settings().hotkeys;
        (hotkeys.cycle, hotkeys.cycle_back)
    }

    /// The page's own keys (no keyboard hook: this window has no handle): Return starts
    /// recording, the modifiers show as they are held and record nothing, and the next key
    /// pressed with them is saved at once.
    #[test]
    fn a_hotkey_is_recorded_by_pressing_it() {
        let mut app = crate::tests::app();
        let (ui, names) = hotkeys_page(&mut app);
        let g = ui.global::<Features>();
        assert_eq!((g.get_cycle_hotkeys().row_data(0).unwrap().text.as_str(), g.get_recording()), ("Ctrl+Tab", -1));
        assert!(pad::focus_named(ui.window(), &names[0]));
        key(&ui, Key::Return);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyRecord(Target::Next))]));
        assert_eq!((g.get_recording(), g.get_recording_text().as_str(), g.get_recording_prompt()), (0, "Press a key\u{2026}", true));
        assert!(recording());
        press(&ui, Key::Control);
        assert_eq!((g.get_recording_text().as_str(), g.get_recording_prompt()), ("Ctrl+\u{2026}", false));
        press(&ui, Key::Alt);
        press(&ui, Key::Shift);
        assert_eq!(g.get_recording_text(), "Ctrl+Alt+Shift+\u{2026}");
        release(&ui, Key::Shift);
        assert_eq!(g.get_recording_text(), "Ctrl+Alt+\u{2026}");
        release(&ui, Key::Alt);
        release(&ui, Key::Control);
        assert_eq!((g.get_recording_text().as_str(), g.get_recording_prompt()), ("Press a key\u{2026}", true), "all let go: the prompt again");
        assert!(pump(&mut app, &ui).is_empty() && g.get_recording() == 0, "modifiers alone end nothing");
        assert_eq!(cycle(&app).0, Hotkey::parse("Ctrl+Tab"));
        for modifier in [Key::Control, Key::Alt, Key::Shift] { press(&ui, modifier); }
        press(&ui, Key::F22);
        let sent = pump(&mut app, &ui);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Hotkey(Target::Next, Some(hotkey)))] if hotkey.to_string() == "Ctrl+Alt+Shift+F22"), "{sent:?}");
        // Saved and shown at once: no Return.
        assert_eq!(cycle(&app).0, Hotkey::parse("Ctrl+Alt+Shift+F22"));
        assert_eq!(g.get_cycle_hotkeys().row_data(0).unwrap().text, "Ctrl+Alt+Shift+F22");
        assert_eq!(g.get_recording(), -1);
        assert!(!recording());
        // The keys still held, their repeats and their releases are nobody's.
        for held in [Key::F22, Key::Shift, Key::Alt, Key::Control] { release(&ui, held); }
        assert!(pump(&mut app, &ui).is_empty());
        assert_eq!(pad::focused(ui.window()).as_deref(), Some(names[0].as_str()), "the field keeps the focus");
        // A number pad chord is shown spaced out, and saved as it always was.
        drop(app.feature_update(Msg::Hotkey(Target::Next, Hotkey::parse("Ctrl+Num0"))));
        sync(&ui, &app);
        assert_eq!(g.get_cycle_hotkeys().row_data(0).unwrap().text, "Ctrl+Num 0");
        assert_eq!(serde_json::to_value(app.features.as_ref().unwrap().settings().hotkeys).unwrap()["cycle"], "Ctrl+Num0");
    }

    /// Escape keeps the hotkey, Backspace and Delete clear it, Tab moves on; so do the focus
    /// leaving, a press anywhere else, another section, and the controller's Back.
    #[test]
    fn a_recording_ends_without_a_change_or_clears_the_hotkey() {
        let mut app = crate::tests::app();
        let (ui, names) = hotkeys_page(&mut app);
        let g = ui.global::<Features>();
        let before = cycle(&app);
        // Escape: cancelled, and the page stays (Escape is not Back while a field records).
        record(&mut app, &ui, &names[0]);
        assert_eq!(g.get_recording(), 0);
        key(&ui, Key::Escape);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyCancel)]));
        assert_eq!((g.get_recording(), cycle(&app), g.get_open()), (-1, before, true));
        // Tab: cancelled, and the focus moves on to the next field.
        record(&mut app, &ui, &names[0]);
        key(&ui, Key::Tab);
        assert!(pump(&mut app, &ui).iter().all(|m| matches!(m, Message::Feature(Msg::HotkeyCancel))));
        assert_eq!((g.get_recording(), cycle(&app)), (-1, before));
        assert_eq!(pad::focused(ui.window()).as_deref(), Some(names[1].as_str()));
        // The focus leaving (the controller's D-pad, a click on a field elsewhere).
        record(&mut app, &ui, &names[0]);
        assert!(pad::focus_named(ui.window(), &names[1]));
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyCancel)]));
        assert_eq!((g.get_recording(), cycle(&app)), (-1, before));
        // The controller's Back: the recording stops and the page stays; Back again leaves it.
        record(&mut app, &ui, &names[1]);
        assert_eq!(g.get_recording(), 1);
        drop(app.update(Message::ControllerBack));
        sync(&ui, &app);
        assert_eq!((g.get_recording(), recording(), app.features_view.open), (-1, false, true));
        // Another section: its field is gone, and so is the recording.
        record(&mut app, &ui, &names[1]);
        assert!(recording());
        drop(app.feature_update(Msg::Section(Section::Recovery)));
        sync(&ui, &app);
        assert!(!recording() && app.features_view.recording.is_none());
        open(&mut app, &ui, Section::Hotkeys);
        // Backspace: no hotkey. Delete the same.
        record(&mut app, &ui, &names[1]);
        key(&ui, Key::Backspace);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::Hotkey(Target::Previous, None))]));
        assert_eq!((g.get_recording(), cycle(&app)), (-1, (before.0, None)));
        assert_eq!(g.get_cycle_hotkeys().row_data(1).unwrap().text, "");
        record(&mut app, &ui, &names[0]);
        key(&ui, Key::Delete);
        pump(&mut app, &ui);
        assert_eq!(cycle(&app), (None, None));
        // The window closing takes a recording (and its keyboard hook) with it.
        record(&mut app, &ui, &names[0]);
        assert!(recording());
        forget();
        assert!(!recording());
    }

    /// A press on the recording field keeps it recording; a press anywhere else ends it (the
    /// host tells of every press before the page hears of it).
    #[test]
    fn a_click_records_and_a_click_elsewhere_stops() {
        use slint::platform::PointerEventButton;
        let mut app = crate::tests::app();
        let (ui, names) = hotkeys_page(&mut app);
        let g = ui.global::<Features>();
        let before = cycle(&app);
        let middle = |name: &str| {
            assert!(pad::focus_named(ui.window(), name));
            let [x, y, w, h] = pad::focus_rect(ui.window()).unwrap();
            slint::LogicalPosition::new(x + w / 2.0, y + h / 2.0)
        };
        let (first, second) = (middle(&names[0]), middle(&names[1]));
        let click = |position: slint::LogicalPosition| {
            pointer_pressed();
            ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
            pointer_settled();
            ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
            render();
        };
        click(first);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyRecord(Target::Next))]));
        assert_eq!(g.get_recording(), 0);
        assert_eq!(pad::focused(ui.window()).as_deref(), Some(names[0].as_str()), "the click gives it the focus");
        let token = RECORDING.with(|cell| cell.borrow().as_ref().map(|r| r.token));
        // On itself: it goes on recording (the same recording).
        click(first);
        assert!(pump(&mut app, &ui).iter().all(|m| matches!(m, Message::Feature(Msg::HotkeyRecord(Target::Next)))));
        assert_eq!((g.get_recording(), RECORDING.with(|cell| cell.borrow().as_ref().map(|r| r.token))), (0, token));
        // On another hotkey: this one stops, that one records.
        click(second);
        let sent = pump(&mut app, &ui);
        let (last, stopped) = sent.split_last().unwrap();
        assert!(matches!(last, Message::Feature(Msg::HotkeyRecord(Target::Previous))));
        assert!(!stopped.is_empty() && stopped.iter().all(|m| matches!(m, Message::Feature(Msg::HotkeyCancel))), "the press and the focus leaving both stop it");
        assert_eq!(g.get_recording(), 1);
        // On nothing at all (the page's margin): it stops, and nothing was changed.
        pointer_pressed();
        ui.window().dispatch_event(WindowEvent::PointerPressed { position: slint::LogicalPosition::new(700.0, 890.0), button: PointerEventButton::Left });
        pointer_settled();
        ui.window().dispatch_event(WindowEvent::PointerReleased { position: slint::LogicalPosition::new(700.0, 890.0), button: PointerEventButton::Left });
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyCancel)]));
        assert_eq!((g.get_recording(), recording(), cycle(&app)), (-1, false, before));
        // With nothing recording a press is only a press.
        pointer_pressed();
        pointer_settled();
        assert!(take_sent().is_empty());
    }

    /// Any key or combination is a hotkey (owner decision, 2026-11-11). Only a chord Windows
    /// makes impossible is refused, with what was pressed and why, and the hotkey stays. One
    /// that works but clashes is saved, with what it clashes with in amber. An account's hotkey
    /// with no Ctrl, Alt or Win works only over a game, and its row says so.
    #[test]
    fn any_chord_is_a_hotkey_and_only_what_windows_keeps_is_refused() {
        let mut app = crate::tests::app();
        let (ui, names) = hotkeys_page(&mut app);
        let g = ui.global::<Features>();
        let next = || g.get_cycle_hotkeys().row_data(0).unwrap();
        let account = || g.get_account_hotkeys().row_data(0).unwrap();
        let key0 = app.accounts[0].key.clone();
        // A bare letter, pressed in the field: it is the hotkey, saved at once, and the row
        // says what it costs in the game.
        record(&mut app, &ui, &names[0]);
        key(&ui, Key::A);
        pump(&mut app, &ui);
        assert_eq!((next().text.as_str(), next().problem.as_str(), next().warn, next().active, next().note.as_str()),
            ("A", "This key also types in FFXI chat.", true, true, ""));
        assert_eq!(cycle(&app).0, Hotkey::parse("A"));
        assert_eq!(g.get_recording(), -1);
        // A bare function or number pad key, and Shift with a key: nothing to say.
        for (text, shown) in [("F9", "F9"), ("Num0", "Num 0"), ("Shift+F5", "Shift+F5"), ("`", "`")] {
            drop(app.feature_update(Msg::Hotkey(Target::Next, Hotkey::parse(text))));
            sync(&ui, &app);
            let chat = if text == "`" { "This key also types in FFXI chat." } else { "" };
            assert_eq!((next().text.as_str(), next().problem.as_str(), next().active), (shown, chat, true), "{text}");
        }
        // The same keys on an account's hotkey: saved, in-game only (nothing is registered
        // until a game is in front), and the row says where it works.
        let mut saved = |text: &str| {
            drop(app.feature_update(Msg::Hotkey(Target::Account(key0.clone()), Hotkey::parse(text))));
            sync(&ui, &app);
            assert_eq!(app.features.as_ref().unwrap().settings().hotkeys.accounts.get(&key0).copied(), Hotkey::parse(text), "{text} is saved");
            (account().text.to_string(), account().note.to_string(), account().problem.to_string(), account().warn, account().active)
        };
        let in_game = "Active while a game is in front.".to_owned();
        assert_eq!(saved("F10"), ("F10".to_owned(), in_game.clone(), String::new(), false, true));
        assert_eq!(saved("Num1"), ("Num 1".to_owned(), in_game.clone(), String::new(), false, true));
        assert_eq!(saved("Shift+F6"), ("Shift+F6".to_owned(), in_game.clone(), String::new(), false, true));
        assert_eq!(saved("B"), ("B".to_owned(), in_game.clone(), "This key also types in FFXI chat.".to_owned(), true, true));
        assert_eq!(saved("Ctrl+NumEnter"), ("Ctrl+Num Enter".to_owned(), in_game.clone(), String::new(), false, true));
        // With Ctrl, Alt or Win it works everywhere: no note. Recorded here, Windows registers
        // it, so these use chords nothing else has.
        assert_eq!(saved("Ctrl+Alt+Shift+F18"), ("Ctrl+Alt+Shift+F18".to_owned(), String::new(), String::new(), false, true));
        // What Windows keeps for itself, and Shift with a number pad digit: refused, with the
        // chord pressed and why, and the hotkey stays.
        let mut refused = |text: &str| {
            drop(app.feature_update(Msg::Hotkey(Target::Account(key0.clone()), Hotkey::parse(text))));
            sync(&ui, &app);
            assert_eq!(app.features.as_ref().unwrap().settings().hotkeys.accounts.get(&key0).copied(), Hotkey::parse("Ctrl+Alt+Shift+F18"), "{text} is not saved");
            (account().text.to_string(), account().problem.to_string(), account().warn, account().active)
        };
        let kept = "Ctrl+Alt+Shift+F18".to_owned();
        assert_eq!(refused("Win+L"), (kept.clone(), "Win+L \u{b7} Windows reserves this hotkey.".to_owned(), false, false));
        assert_eq!(refused("Ctrl+Alt+Delete"), (kept.clone(), "Ctrl+Alt+Delete \u{b7} Windows reserves this hotkey.".to_owned(), false, false));
        assert_eq!(refused("Alt+Tab"), (kept.clone(), "Alt+Tab \u{b7} Windows reserves this hotkey.".to_owned(), false, false));
        assert_eq!(refused("Ctrl+Shift+Num1"), (kept.clone(), "Ctrl+Shift+Num 1 \u{b7} Shift does not work with number pad digits.".to_owned(), false, false));
        // Recording again takes the old reason away.
        record(&mut app, &ui, &names[2]);
        key(&ui, Key::Escape);
        pump(&mut app, &ui);
        assert_eq!((account().text.to_string(), account().problem.as_str().to_owned()), (kept.clone(), String::new()));
        drop(app.feature_update(Msg::Hotkey(Target::Account(key0.clone()), None)));
        // The game's macro keys: saved, and said.
        drop(app.feature_update(Msg::Hotkey(Target::Next, Hotkey::parse("Ctrl+1"))));
        sync(&ui, &app);
        assert_eq!((next().text.as_str(), next().problem.as_str(), next().warn, next().active), ("Ctrl+1", "FFXI uses this for macros.", true, true));
        // The same chord on two of Crystal's hotkeys: both rows name the other. (The cycle key
        // has it; the account's is never registered.)
        let chord = Hotkey::parse("Ctrl+Alt+Shift+F20");
        drop(app.feature_update(Msg::Hotkey(Target::Next, chord)));
        drop(app.feature_update(Msg::Hotkey(Target::Account(key0.clone()), chord)));
        sync(&ui, &app);
        assert_eq!(app.features.as_ref().unwrap().settings().hotkeys.accounts.get(&key0).copied(), chord, "saved all the same");
        assert_eq!((next().problem.to_string(), next().warn, next().active), (format!("Also set for {}.", names[2]), true, true));
        assert_eq!((account().problem.to_string(), account().warn, account().active), ("Next window uses this hotkey.".to_owned(), true, false));
        // A chord another program has registered: the cycle key still works over a game (the
        // keyboard hook takes it first), and says so.
        drop(app.feature_update(Msg::Hotkey(Target::Account(key0.clone()), None)));
        drop(app.feature_update(Msg::Hotkey(Target::Next, Hotkey::parse("Ctrl+Alt+Shift+F19"))));
        app.features_view.hotkeys_taken = vec![Hotkey::parse("Ctrl+Alt+Shift+F19").unwrap()];
        app.features_view.touch();
        sync(&ui, &app);
        assert_eq!((next().problem.as_str(), next().warn, next().active), ("Another program also uses this hotkey.", true, true));
    }

    /// The page's key events are read while the keyboard hook is in too: a key that reaches
    /// the field is one the hook did not take. (2026-11-11: with the hook installed and never
    /// called, every key reached the field and was dropped, so a recording never ended.)
    #[test]
    fn a_key_the_hook_does_not_take_is_recorded_from_the_page() {
        let mut app = crate::tests::app();
        let (ui, names) = hotkeys_page(&mut app);
        let g = ui.global::<Features>();
        let history = app.features.as_ref().unwrap().history_path();
        record(&mut app, &ui, &names[0]);
        // A hook that is installed and takes nothing: no window is its own (0 is never in front).
        let hook = Capture::start(0, std::sync::Arc::new(|_| {})).unwrap();
        RECORDING.with(|cell| cell.borrow_mut().as_mut().unwrap().hook = Some(hook));
        press(&ui, Key::Control);
        assert_eq!(g.get_recording_text(), "Ctrl+\u{2026}", "the modifiers show from the page until the hook reports");
        press(&ui, Key::F9);
        let sent = pump(&mut app, &ui);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Hotkey(Target::Next, Some(hotkey)))] if hotkey.to_string() == "Ctrl+F9"), "{sent:?}");
        assert_eq!((g.get_recording(), recording(), cycle(&app).0), (-1, false, Hotkey::parse("Ctrl+F9")));
        release(&ui, Key::F9);
        release(&ui, Key::Control);
        // Escape cancels the same way.
        record(&mut app, &ui, &names[0]);
        let hook = Capture::start(0, std::sync::Arc::new(|_| {})).unwrap();
        RECORDING.with(|cell| cell.borrow_mut().as_mut().unwrap().hook = Some(hook));
        key(&ui, Key::Escape);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyCancel)]));
        assert_eq!((g.get_recording(), recording(), cycle(&app).0), (-1, false, Hotkey::parse("Ctrl+F9")));
        // Once the hook has reported, it shows the modifiers (it sees the Windows key, which
        // never reaches the page); a key that still reaches the page is recorded all the same.
        record(&mut app, &ui, &names[0]);
        let token = RECORDING.with(|cell| cell.borrow().as_ref().unwrap().token);
        recorded(token, capture::Event::Modifiers(hotkeys::MOD_WIN | hotkeys::MOD_CONTROL));
        assert_eq!(g.get_recording_text(), "Ctrl+Win+\u{2026}");
        press(&ui, Key::Control);
        assert_eq!(g.get_recording_text(), "Ctrl+Win+\u{2026}", "the page's view of the modifiers does not replace the hook's");
        // The hook's own report ends it: the key it took, then the chord.
        recorded(token, capture::Event::Key { key: capture::Raw { vk: 0x60, scan: 0x52, extended: false }, taken: true });
        recorded(token, capture::Event::Done(Outcome::Set(Hotkey::parse("Ctrl+Win+Num0").unwrap())));
        let sent = pump(&mut app, &ui);
        assert!(matches!(sent.as_slice(), [Message::Feature(Msg::Hotkey(Target::Next, Some(hotkey)))] if hotkey.to_string() == "Ctrl+Win+Num0"), "{sent:?}");
        release(&ui, Key::Control);
        // The hook took the press of the key that ended a recording, so the field never saw
        // it: while that key is held, its repeats arrive as presses not marked as repeats. On
        // Return or Space (a chord may be on either) they start no new recording.
        record(&mut app, &ui, &names[0]);
        let token = RECORDING.with(|cell| cell.borrow().as_ref().unwrap().token);
        recorded(token, capture::Event::Key { key: capture::Raw { vk: 0x0D, scan: 0x1C, extended: false }, taken: true });
        recorded(token, capture::Event::Done(Outcome::Set(Hotkey::parse("Ctrl+Alt+Enter").unwrap())));
        pump(&mut app, &ui);
        assert_eq!((g.get_recording(), g.get_recording_taken(), cycle(&app).0), (-1, true, Hotkey::parse("Ctrl+Alt+Enter")));
        for _ in 0..3 { press(&ui, Key::Return); }
        assert!(pump(&mut app, &ui).is_empty() && g.get_recording() == -1, "the held key's repeats record nothing");
        // Let go, Return records again.
        release(&ui, Key::Return);
        key(&ui, Key::Return);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyRecord(Target::Next))]));
        // The controller's Back ends a recording with no key at all: its A (Return) records at
        // once after it.
        drop(app.update(Message::ControllerBack));
        sync(&ui, &app);
        render();
        assert_eq!((g.get_recording(), g.get_recording_taken()), (-1, false));
        key(&ui, Key::Return);
        assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::HotkeyRecord(Target::Next))]));
        key(&ui, Key::Escape);
        pump(&mut app, &ui);
        assert_eq!((g.get_recording(), cycle(&app).0), (-1, Hotkey::parse("Ctrl+Alt+Enter")));
        // A report of a recording that is over is nobody's.
        recorded(token, capture::Event::Done(Outcome::Clear));
        assert!(take_sent().is_empty());
        assert_eq!(cycle(&app).0, Hotkey::parse("Ctrl+Alt+Enter"));
        // What happened is in the launch history: how each recording read the keyboard, the
        // codes of the key that ended it and nothing else of what was typed, and how it ended.
        // (How many keys a hook saw is the keyboard's business while the test ran.)
        let lines: Vec<String> = std::fs::read_to_string(&history).unwrap_or_default().lines()
            .filter_map(|line| line.split("hotkeys: record: ").nth(1).map(|rest| rest.split(" | ").next().unwrap_or(rest).to_owned()))
            .map(|line| if line.starts_with("ended: the hook saw ") && line.ends_with(" of them with another window in front") { "ended: the hook saw".to_owned() } else { line })
            .collect();
        assert_eq!(lines, [
            "started for next window: no window handle, reading the page's key events",
            "key from the page (the hook did not take it): vk 0x78, modifiers 0x2",
            "saved",
            "ended: the hook saw",
            "started for next window: no window handle, reading the page's key events",
            "key from the page (the hook did not take it): vk 0x1B, modifiers 0x0",
            "stopped: Esc, or Tab left the field",
            "ended: the hook saw",
            "started for next window: no window handle, reading the page's key events",
            "key from the hook: vk 0x60, scan 0x52",
            "saved",
            "ended",
            "started for next window: no window handle, reading the page's key events",
            "key from the hook: vk 0x0D, scan 0x1C",
            "saved",
            "ended",
            "started for next window: no window handle, reading the page's key events",
            "ended",
            "started for next window: no window handle, reading the page's key events",
            "key from the page: vk 0x1B, modifiers 0x0",
            "stopped: Esc, or Tab left the field",
            "ended",
        ], "{lines:#?}");
    }

    /// The conflict check: as a cycle key is set (and as the page opens), Windows is asked
    /// whether another program has the chord registered, and the row says so.
    #[test]
    fn a_chord_another_program_registered_is_said_as_the_hotkey_is_set() {
        use crate::features::desktop;
        let chord = Hotkey::parse("Ctrl+Alt+Shift+Win+F14").unwrap();
        // Another desktop thread stands in for the other program.
        let other = desktop::Desktop::start(std::sync::Arc::new(|_| {})).unwrap();
        other.send(desktop::Command::Hotkeys(vec![(1, chord)]));
        let (done, registered) = std::sync::mpsc::channel();
        other.send(desktop::Command::Run(Box::new(move || { let _ = done.send(()); })));
        registered.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let mut app = crate::tests::app();
        let (ui, _) = hotkeys_page(&mut app);
        let g = ui.global::<Features>();
        let next = || g.get_cycle_hotkeys().row_data(0).unwrap();
        drop(app.feature_update(Msg::Hotkey(Target::Next, Some(chord))));
        sync(&ui, &app);
        assert_eq!(app.features_view.hotkeys_taken, vec![chord]);
        assert_eq!((next().text.as_str(), next().problem.as_str(), next().warn, next().active),
            ("Ctrl+Alt+Shift+Win+F14", "Another program also uses this hotkey.", true, true));
        // The program lets it go: asked again as the page opens, the row is clear.
        drop(other);
        drop(app.feature_update(Msg::Open(true)));
        sync(&ui, &app);
        assert!(app.features_view.hotkeys_taken.is_empty());
        assert_eq!((next().problem.as_str(), next().warn, next().active), ("", false, true));
    }

    #[test]
    fn the_pages_own_key_events_name_keys_as_far_as_text_can() {
        let key = |text: SharedString| match typed(&text) { Some(Typed::Key(code)) => Some(code), _ => None };
        let modifier = |key: Key| match typed(&SharedString::from(key)) { Some(Typed::Modifier(bit)) => Some(bit), _ => None };
        assert_eq!(modifier(Key::Control), Some(hotkeys::MOD_CONTROL));
        assert_eq!(modifier(Key::ControlR), Some(hotkeys::MOD_CONTROL));
        assert_eq!(modifier(Key::Alt), Some(hotkeys::MOD_ALT));
        assert_eq!(modifier(Key::AltGr), Some(hotkeys::MOD_ALT));
        assert_eq!(modifier(Key::ShiftR), Some(hotkeys::MOD_SHIFT));
        assert_eq!(modifier(Key::Meta), Some(hotkeys::MOD_WIN));
        assert_eq!(modifier(Key::Return), None);
        for (slint, code) in [(Key::F1, 0x70), (Key::F12, 0x7B), (Key::F24, 0x87), (Key::Return, 0x0D), (Key::Escape, 0x1B), (Key::Backtab, 0x09),
            (Key::LeftArrow, 0x25), (Key::PageDown, 0x22), (Key::Insert, 0x2D), (Key::Delete, 0x2E), (Key::Space, 0x20), (Key::Pause, 0x13)] {
            assert_eq!(key(slint.into()), Some(code), "{slint:?}");
        }
        assert_eq!(key("a".into()), Some(0x41));
        assert_eq!(key("Z".into()), Some(0x5A));
        // A digit is the top row's: text can't tell the number pad (the keyboard hook does).
        assert_eq!(key("0".into()), Some(0x30));
        assert_eq!(key(";".into()), Some(0xBA));
        assert_eq!(key("-".into()), Some(0xBD));
        // A shifted or national character names no key.
        for text in ["!", "+", "\u{e9}", "ab", ""] { assert_eq!(key(text.into()), None, "{text}"); }
    }
}
