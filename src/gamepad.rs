//! Controller detection and FFXI gamepad setup (Settings > Controller).
//!
//! * `detect`: connected controllers through gilrs (Windows.Gaming.Input),
//!   classified by USB vendor/product id, with live input on a worker thread.
//! * `art`: the Xbox / DualSense / generic illustrations and button glyphs.
//! * `ffxi`: FFXI's `padmode000` / `padsin000` values, read from and written
//!   to the Windows registry.
//!
//! Edits save automatically; running clients reload through their scoped host.
//! The design preview (`--preview-settings-controller`) fakes a controller
//! from `XI_PREVIEW_PAD=xbox|dualsense|ds4|ally|ally-x|switch|generic|none` and never
//! writes anything.
mod art;
pub(crate) mod detect;
mod ffxi;

use crate::i18n::{t, tf};
use crate::palette::{colors, Color};
use crate::App;
use detect::{Btn, Family, Input, Pad};
use ffxi::{Control, Flag, Layout, PadConfig, Row, Stick, ROWS};
use crate::effect::Effect;

struct Target {
    source: String,
    saved: PadConfig,
    edit: PadConfig,
    /// Bindings the last XInput toggle could not map, for the reverse toggle.
    unbound: ffxi::Unbound,
}

impl Target {
    fn from(loaded: ffxi::Loaded) -> Self {
        Self {
            source: loaded.source,
            saved: loaded.config.clone(),
            edit: loaded.config,
            unbound: ffxi::Unbound::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeKind {
    Info,
    /// The pressed control cannot be bound; capture continues.
    Capture,
    /// Writing the registry failed.
    Save,
    /// Running games did not take the settings.
    Live,
}

struct Notice {
    kind: NoticeKind,
    text: String,
}

impl Notice {
    fn error(&self) -> bool {
        self.kind != NoticeKind::Info
    }
    /// Re-applying can fix it.
    fn retry(&self) -> bool {
        matches!(self.kind, NoticeKind::Save | NoticeKind::Live)
    }
}

#[derive(Default)]
struct Saving {
    revision: u64,
    in_flight: Option<u64>,
    live: Option<(u64, std::collections::BTreeMap<String, String>)>,
    applying: Option<u64>,
    persisted: bool,
}

pub(crate) struct State {
    preview: bool,
    pads: Vec<Pad>,
    selected: Option<usize>,
    prev: Input,
    retail: Target,
    capture: Option<Row>,
    notice: Option<Notice>,
    saving: Saving,
}

impl Default for State {
    fn default() -> Self {
        if crate::preview::flags().is_some() {
            return Self::preview();
        }
        Self {
            preview: false,
            pads: Vec::new(),
            selected: None,
            prev: Input::default(),
            retail: Target::from(ffxi::load_retail()),
            capture: None,
            notice: None,
            saving: Saving::default(),
        }
    }
}

impl State {
    pub(crate) fn controller_pad(&self) -> Option<&Pad> { self.pad() }
    pub(crate) fn capturing(&self) -> bool { self.capture.is_some() }
    /// Synthetic input for the controller highlight and binding chips.
    fn preview() -> Self {
        let kind = std::env::var("XI_PREVIEW_PAD").unwrap_or_else(|_| "xbox".into());
        let ps5 = matches!(kind.to_ascii_lowercase().as_str(), "dualsense" | "ps5");
        let input = Input {
            buttons: Btn::South.bit() | if ps5 { Btn::Touchpad.bit() | Btn::Mute.bit() } else { 0 },
            rt: 0.6,
            lx: 0.55,
            ly: 0.35,
            ..Default::default()
        };
        let pad = |name: &str, vid: u16, pid: u16, wireless: bool| Pad {
            id: 0,
            name: name.into(),
            family: detect::classify(Some(vid), Some(pid), name),
            vid: Some(vid),
            pid: Some(pid),
            wireless: Some(wireless),
            battery: wireless.then_some(80),
            battery_source: wireless.then_some(detect::BatterySource::Controller),
            battery_state: detect::BatteryState::Discharging,
            battery_level: None,
            rumble: true,
            input,
        };
        let pads = match kind.to_ascii_lowercase().as_str() {
            "none" => vec![],
            "dualsense" | "ps5" => vec![pad("DualSense Wireless Controller", 0x054C, 0x0CE6, false)],
            "ds4" | "ps4" => vec![pad("Wireless Controller", 0x054C, 0x09CC, true)],
            "switch" => vec![pad("Pro Controller", 0x057E, 0x2009, true)],
            "generic" => vec![pad("USB Gamepad", 0x0079, 0x0006, false)],
            "ally" | "rog-xbox-ally" | "ally-x" | "rog-xbox-ally-x" => {
                let mut handheld = pad("ROG Xbox Ally", 0x045E, 0x028E, false);
                handheld.family = if kind.to_ascii_lowercase().ends_with("-x") { Family::RogXboxAllyX } else { Family::RogXboxAlly };
                handheld.name = handheld.family.label().into();
                handheld.battery = Some(64);
                handheld.battery_source = Some(detect::BatterySource::Handheld);
                vec![handheld]
            }
            _ => vec![pad("Xbox Wireless Controller", 0x045E, 0x0B13, true)],
        };
        let target = |source: &str| {
            let mut t = Target {
                source: source.into(),
                saved: PadConfig::default(),
                edit: PadConfig::default(),
                unbound: ffxi::Unbound::default(),
            };
            if ps5 {
                t.saved.set_flag(Flag::XInput, false, Layout::DInputSony, &mut ffxi::Unbound::default());
                t.edit = t.saved.clone();
            }
            t
        };
        Self {
            preview: true,
            selected: pads.first().map(|p| p.id),
            prev: input,
            pads,
            retail: target(r"HKLM\SOFTWARE\WOW6432Node\PlayOnlineUS\SquareEnix\FinalFantasyXI"),
            capture: std::env::var_os("XI_PREVIEW_CAPTURE").map(|_| Row::Single(4)),
            notice: None,
            saving: Saving::default(),
        }
    }

    pub(crate) fn take_live_update(&mut self) -> Option<(u64, std::collections::BTreeMap<String, String>)> {
        if self.saving.applying.is_some() { return None; }
        let update = self.saving.live.take()?;
        self.saving.applying = Some(update.0);
        Some(update)
    }
    pub(crate) fn take_persisted_change(&mut self) -> bool { std::mem::take(&mut self.saving.persisted) }
    fn pad(&self) -> Option<&Pad> {
        self.selected
            .and_then(|id| self.pads.iter().find(|p| p.id == id))
            .or(self.pads.first())
    }
    /// The controller's DirectInput numbering that FFXI sees when XInput is off.
    fn di_layout(&self) -> Layout {
        self.known_di_layout().unwrap_or(Layout::DInputXbox)
    }
    /// The DirectInput numbering the bindings are in, when it is known: Sony
    /// ids in stored DirectInput bindings, else the connected controller.
    /// Without either, renumbering would guess, so the XInput toggle waits.
    fn known_di_layout(&self) -> Option<Layout> {
        let edit = &self.retail.edit;
        let sony_ids = !edit.flag(Flag::XInput)
            && ROWS.iter().any(|row| match *row {
                Row::Single(i) => matches!(edit.sin[i], 13 | 14),
                Row::Group(i) => edit.sin[i..i + 4].iter().any(|id| matches!(id.unsigned_abs(), 34 | 37)),
            });
        if sony_ids {
            return Some(Layout::DInputSony);
        }
        self.pad().map(|p| if p.family.is_playstation() { Layout::DInputSony } else { Layout::DInputXbox })
    }
    fn layout(&self) -> Layout {
        if self.retail.edit.flag(Flag::XInput) {
            Layout::XInput
        } else {
            self.di_layout()
        }
    }
    fn try_capture(&mut self, input: Input, newly: u32) {
        let Some(row) = self.capture else {
            return;
        };
        let layout = self.layout();
        match row {
            Row::Single(slot) => {
                let Some(b) = Btn::ALL.into_iter().find(|b| newly & b.bit() != 0) else {
                    return;
                };
                match layout.encode_button(b) {
                    Some(id) => {
                        self.retail.edit.bind_button(slot, id);
                        self.capture = None;
                        self.notice = None;
                    }
                    None => {
                        self.notice = Some(Notice {
                            kind: NoticeKind::Capture,
                            text: if matches!(b, Btn::Touchpad | Btn::Mute) && layout == Layout::XInput {
                                t("Touchpad click and Mic button need DirectInput. Turn XInput mode off, then press the button again.").into()
                            } else {
                                t("FFXI cannot use that control for a button in this input mode. Try another.").into()
                            },
                        })
                    }
                }
            }
            Row::Group(first) => {
                let dpad = [Btn::DUp, Btn::DDown, Btn::DLeft, Btn::DRight];
                let stick = if dpad.iter().any(|b| newly & b.bit() != 0) {
                    Some(Stick::Dpad)
                } else if input.lx.abs().max(input.ly.abs()) > 0.6
                    && self.prev.lx.abs().max(self.prev.ly.abs()) <= 0.6 {
                    Some(Stick::Left)
                } else if input.rx.abs().max(input.ry.abs()) > 0.6
                    && self.prev.rx.abs().max(self.prev.ry.abs()) <= 0.6 {
                    Some(Stick::Right)
                } else {
                    None
                };
                if let Some(stick) = stick {
                    self.retail.edit.bind_group(first, layout.group(stick), layout);
                    self.capture = None;
                    self.notice = None;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Pads(Vec<Pad>),
    Select(usize),
    SetFlag(Flag, bool),
    Capture(Row),
    CancelCapture,
    Clear(Row),
    Reset,
    Apply,
    SaveLater(u64),
    /// The revision, the edit it saved, and what was written (or the error).
    Saved(u64, PadConfig, Result<(PadConfig, String), String>),
    LiveResult(u64, Result<String, String>),
    Rumble,
}

pub(crate) fn update(state: &mut State, msg: Msg, config: &crate::backend::Config) -> Effect<Msg> {
    let before = state.retail.edit.clone();
    let toggles_xinput = matches!(msg, Msg::SetFlag(Flag::XInput, _));
    match msg {
        Msg::Pads(pads) => {
            state.pads = pads;
            if state.pad().map(|p| p.id) != state.selected {
                state.selected = state.pad().map(|p| p.id);
                state.prev = state.pad().map(|p| p.input).unwrap_or_default();
            }
            let input = state.pad().map(|p| p.input).unwrap_or_default();
            let newly = input.buttons & !state.prev.buttons;
            state.try_capture(input, newly);
            state.prev = input;
        }
        Msg::Select(id) => {
            state.selected = Some(id);
            state.prev = state.pad().map(|p| p.input).unwrap_or_default();
        }
        Msg::SetFlag(flag, on) => {
            let di = if flag == Flag::XInput { state.known_di_layout() } else { Some(state.di_layout()) };
            if let Some(di) = di {
                let t = &mut state.retail;
                t.edit.set_flag(flag, on, di, &mut t.unbound);
            }
        }
        Msg::Capture(row) => {
            state.capture = Some(row);
            state.prev = state.pad().map(|p| p.input).unwrap_or_default();
            state.notice = None;
        }
        Msg::CancelCapture => {
            state.capture = None;
            if state.notice.as_ref().is_some_and(|n| n.kind == NoticeKind::Capture) {
                state.notice = None;
            }
        }
        Msg::Clear(row) => {
            let t = &mut state.retail;
            match row {
                Row::Single(i) => t.edit.sin[i] = -1,
                Row::Group(i) => t.edit.sin[i..i + 4].copy_from_slice(&[-1; 4]),
            }
            state.capture = None;
        }
        Msg::Reset => {
            let di = state.di_layout();
            state.retail.edit.reset(di);
            state.capture = None;
        }

        Msg::Apply => {
            state.capture = None;
            if !state.preview {
                if state.retail.edit == state.retail.saved {
                    state.saving.revision = state.saving.revision.wrapping_add(1);
                    state.saving.live = Some((state.saving.revision, controller_values(&state.retail.edit)));
                } else { return save_later(state.saving.revision); }
            }
        }
        Msg::SaveLater(revision) => {
            if state.preview || revision != state.saving.revision || state.saving.in_flight.is_some()
                || state.retail.edit == state.retail.saved { return Effect::none(); }
            state.saving.in_flight = Some(revision);
            let pad = state.retail.edit.clone();
            let base = state.retail.saved.clone();
            let config = config.clone();
            return Effect::perform(async move {
                let snapshot = pad.clone();
                let result = tokio::task::spawn_blocking(move || ffxi::save_retail(&config, &base, &snapshot)).await
                    .unwrap_or_else(|_| Err(t("Could not save controller settings.").into()));
                Msg::Saved(revision, pad, result)
            }, |result| result);
        }
        Msg::Saved(revision, pad, result) => {
            if state.saving.in_flight != Some(revision) { return Effect::none(); }
            state.saving.in_flight = None;
            let failed = result.is_err();
            match result {
                Ok((written, _)) => {
                    // Values another tool stored meanwhile were kept in what was
                    // written; follow them wherever the user has not edited since.
                    if written != pad {
                        let t = &mut state.retail;
                        t.edit = PadConfig::rebased(&written, &pad, &t.edit);
                    }
                    state.retail.saved = written;
                    state.saving.persisted = true;
                    if revision == state.saving.revision {
                        state.saving.live = Some((revision, controller_values(&state.retail.edit)));
                        state.notice = Some(Notice { kind: NoticeKind::Info, text: t("Saved. Applying to running games…").into() });
                    }
                }
                Err(text) => state.notice = Some(Notice { kind: NoticeKind::Save, text }),
            }
            if state.retail.edit != before {
                // Rebased onto another tool's values: not the user's toggle.
                state.retail.unbound = ffxi::Unbound::default();
            }
            // A rebase is not a user edit: skip the shared block, which would
            // drop the live update and notice set above.
            if state.retail.edit != state.retail.saved && (!failed || revision != state.saving.revision) {
                return save_later(state.saving.revision);
            }
            return Effect::none();
        }
        Msg::LiveResult(revision, result) => {
            if state.saving.applying != Some(revision) { return Effect::none(); }
            state.saving.applying = None;
            if revision == state.saving.revision && state.retail.edit == state.retail.saved {
                state.notice = Some(match result {
                    Ok(text) => Notice { kind: NoticeKind::Info, text },
                    Err(text) => Notice { kind: NoticeKind::Live, text },
                });
            }
        }
        Msg::Rumble => {
            if let Some(p) = state.pad() {
                detect::rumble(p.id);
            }
        }
    }
    if state.retail.edit != before {
        if !toggles_xinput {
            // Bound, cleared or reset since: a reverse toggle restores nothing.
            state.retail.unbound = ffxi::Unbound::default();
        }
        state.saving.revision = state.saving.revision.wrapping_add(1);
        state.saving.live = None;
        state.notice = None;
        if !state.preview && state.retail.edit != state.retail.saved { return save_later(state.saving.revision); }
    }
    Effect::none()
}

fn controller_values(pad: &PadConfig) -> std::collections::BTreeMap<String, String> {
    pad.values().into_iter().map(|(name, value)| (name.to_owned(), value)).collect()
}

fn save_later(revision: u64) -> Effect<Msg> {
    Effect::perform(async move { tokio::time::sleep(std::time::Duration::from_millis(200)).await; revision }, Msg::SaveLater)
}

/// Whether the controller thread should run (not in a design preview's capture).
pub(crate) fn watching(state: &State) -> bool {
    !(state.preview && !crate::preview_idle())
}

fn btn_label(family: Family, b: Btn) -> &'static str {
    let ps = family.is_playstation();
    let nin = family == Family::SwitchPro;
    match b {
        Btn::LB if ps => "L1",
        Btn::RB if ps => "R1",
        Btn::LT if ps => "L2",
        Btn::RT if ps => "R2",
        Btn::LS if ps => "L3",
        Btn::RS if ps => "R3",
        Btn::LB if nin => "L",
        Btn::RB if nin => "R",
        Btn::LT if nin => "ZL",
        Btn::RT if nin => "ZR",
        Btn::LB => "LB",
        Btn::RB => "RB",
        Btn::LT => "LT",
        Btn::RT => "RT",
        Btn::LS => "LS",
        Btn::RS => "RS",
        Btn::Back if family == Family::DualShock4 => "Share",
        Btn::Back if ps => "Create",
        Btn::Back if nin => "Minus",
        Btn::Back => "View",
        Btn::Start if ps => "Options",
        Btn::Start if nin => "Plus",
        Btn::Start => "Menu",
        Btn::Guide if ps => "PS",
        Btn::Guide if nin => "Home",
        Btn::Guide => "Guide",
        Btn::Touchpad => t("Touchpad click"),
        Btn::Mute => t("Mic button"),
        Btn::DUp => t("Up"),
        Btn::DDown => t("Down"),
        Btn::DLeft => t("Left"),
        Btn::DRight => t("Right"),
        Btn::South | Btn::East | Btn::West | Btn::North => "",
    }
}


/// Settings > Controller as plain data: the lines, chips and pictures (`art`'s SVG documents)
/// the Slint page shows, and the messages its controls send.

pub(crate) mod front {
    use super::*;

    /// How one binding reads (`binding_chip`).
    #[derive(Clone, Debug, PartialEq)]
    pub(crate) enum Chip {
        /// Muted words ("Not set").
        Words(&'static str),
        /// A small bordered label, lit while its control is held.
        Keycap { label: String, lit: bool },
        /// A drawn glyph, with words beside it when it has any.
        Picture { svg: String, label: String },
    }

    #[derive(Clone, Debug, PartialEq)]
    pub(crate) struct Binding {
        pub row: Row,
        pub label: &'static str,
        /// Edited since the last save.
        pub changed: bool,
        /// Waiting for a press: `prompt` shows with Unbind instead of the chip.
        pub capturing: bool,
        pub prompt: &'static str,
        pub chip: Chip,
        /// Its control is held right now.
        pub live: bool,
    }

    /// A note's colour: the muted ink, or the theme palette's warning, success or danger.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Tone {
        Sub,
        Warning,
        Success,
        Danger,
    }

    #[derive(Clone, Debug, PartialEq)]
    pub(crate) struct Switch {
        pub label: &'static str,
        pub flag: Flag,
        pub on: bool,
        pub enabled: bool,
    }

    #[derive(Clone, Debug, PartialEq)]
    pub(crate) struct Front {
        // The device bar.
        pub badge: String,
        pub title: String,
        pub sub: String,
        /// Every controller, when there are several: (id, name, selected).
        pub pads: Vec<(usize, String, bool)>,
        pub status: &'static str,
        pub retry: bool,
        // Live test.
        pub picture: String,
        pub connected: bool,
        pub caption: &'static str,
        /// The triggers' names and pulls, then the sticks' read-outs.
        pub triggers: [(&'static str, f32); 2],
        pub sticks: [String; 2],
        pub rumble: bool,
        /// The PlayStation extras' keycaps: (label, held).
        pub extras: Vec<(&'static str, bool)>,
        // Button bindings.
        pub bindings_sub: String,
        pub bindings: Vec<Binding>,
        // Game options.
        pub switches: [Switch; 4],
        pub notes: Vec<(String, Tone)>,
    }

    /// The muted ink, accent and ink of `light`'s palette (the chips are drawn in them).
    fn inks(light: bool) -> (Color, Color, Color) {
        let c = colors(light);
        (c.sub, c.accent, c.ink)
    }

    fn chip(family: Family, layout: Layout, row: Row, pad: &PadConfig, input: &Input, light: bool) -> (Chip, bool) {
        let (sub, accent, ink) = inks(light);
        match row {
            Row::Group(first) => {
                let ids = &pad.sin[first..first + 4];
                match layout.group_stick(ids) {
                    Some(Stick::Dpad) => {
                        let all = [Btn::DUp, Btn::DDown, Btn::DLeft, Btn::DRight];
                        let lit: Vec<Btn> = all.into_iter().filter(|b| input.pressed(*b)).collect();
                        let svg = art::dpad_chip(if lit.is_empty() { &all } else { &lit }, sub, if lit.is_empty() { sub } else { accent });
                        (Chip::Picture { svg, label: t("D-pad").into() }, !lit.is_empty())
                    }
                    Some(stick) => {
                        let (x, y, name) = match stick {
                            Stick::Left => (input.lx, input.ly, t("Left stick")),
                            _ => (input.rx, input.ry, t("Right stick")),
                        };
                        let live = x.abs().max(y.abs()) > 0.3;
                        (Chip::Picture { svg: art::stick_chip(sub, if live { accent } else { sub }), label: name.into() }, live)
                    }
                    None if ids.iter().all(|v| *v == -1) => (Chip::Words(t("Not set")), false),
                    None => (Chip::Keycap { label: t("Custom").into(), lit: false }, false),
                }
            }
            Row::Single(slot) => match layout.decode(pad.sin[slot]) {
                Control::None => (Chip::Words(t("Not set")), false),
                Control::Button(b) if matches!(b, Btn::South | Btn::East | Btn::West | Btn::North) => {
                    let lit = input.pressed(b);
                    if family == Family::Generic {
                        let name = match b {
                            Btn::South => t("Bottom button"),
                            Btn::East => t("Right button"),
                            Btn::West => t("Left button"),
                            _ => t("Top button"),
                        };
                        return (Chip::Keycap { label: name.into(), lit }, lit);
                    }
                    (Chip::Picture { svg: art::face_chip(family, b, light, lit), label: String::new() }, lit)
                }
                Control::Button(b) if b.is_dpad() => {
                    let lit = input.pressed(b);
                    let label = tf("D-pad {direction}", &[("direction", &btn_label(family, b).to_lowercase())]);
                    (Chip::Picture { svg: art::dpad_chip(&[b], sub, if lit { accent } else { ink }), label }, lit)
                }
                Control::Button(b) => {
                    let lit = input.pressed(b);
                    (Chip::Keycap { label: btn_label(family, b).into(), lit }, lit)
                }
                Control::Axis(stick, dir, neg) => {
                    let name = match stick {
                        Stick::Left => t("Left stick"),
                        Stick::Right => t("Right stick"),
                        Stick::Dpad => t("D-pad"),
                    };
                    let d = match (dir, neg) {
                        (ffxi::AxisDir::X, false) => "→",
                        (ffxi::AxisDir::X, true) => "←",
                        (ffxi::AxisDir::Y, false) => "↓",
                        (ffxi::AxisDir::Y, true) => "↑",
                    };
                    (Chip::Keycap { label: format!("{name} {d}"), lit: false }, false)
                }
                Control::Raw(id) if !(-1..32).contains(&id) => (Chip::Keycap { label: tf("Axis {id}", &[("id", &id)]), lit: false }, false),
                Control::Raw(id) => (Chip::Keycap { label: tf("Button {number}", &[("number", &(id + 1))]), lit: false }, false),
            },
        }
    }

    pub(crate) fn front(app: &App) -> Front {
        let st = &app.gamepad;
        let light = app.settings.light_theme;
        let (sub, accent, _) = inks(light);
        let target = &st.retail;
        let pad = st.pad();
        let input = pad.map(|p| p.input).unwrap_or_default();
        let layout = st.layout();

        // The device bar (`device_bar`).
        let (badge, title, sub_line) = match pad {
            Some(p) => {
                let ids = match (p.vid, p.pid) {
                    (Some(v), Some(d)) => format!(" · USB {v:04X}:{d:04X}"),
                    _ => String::new(),
                };
                let badge = if p.family == Family::Generic { art::stick_chip(sub, accent) } else { art::face_chip(p.family, Btn::South, light, false) };
                (badge, p.name.clone(), format!("{} · {}{}", p.family.label(), p.connection(), ids))
            }
            None => (art::stick_chip(sub, sub), t("No controller connected").into(), t("Plug in or pair a controller.").into()),
        };
        let selected = pad.map(|p| p.id);
        let pads = if st.pads.len() > 1 { st.pads.iter().map(|p| (p.id, p.name.clone(), Some(p.id) == selected)).collect() } else { Vec::new() };
        let changes = target.edit.changes(&target.saved);
        let failed = st.notice.as_ref().is_some_and(|n| n.kind == NoticeKind::Save);
        let status = if changes > 0 && failed {
            t("Controller settings could not be saved")
        } else if st.saving.in_flight.is_some() || changes > 0 {
            t("Saving controller settings…")
        } else {
            ""
        };

        // Live test (`live_card`).
        let family = pad.map_or(Family::Generic, |p| p.family);
        let caption = match pad {
            Some(p) if p.family.is_playstation() => t("Press any button, move the sticks or pull L2/R2 to test it."),
            Some(p) if p.family.is_xbox() => t("Press any button, move the sticks or pull LT/RT to test it."),
            Some(_) => t("Press any button or move the sticks to test it."),
            None => t("Waiting for a controller…"),
        };
        let (lt, rt) = match family {
            f if f.is_playstation() => ("L2", "R2"),
            Family::SwitchPro => ("ZL", "ZR"),
            _ => ("LT", "RT"),
        };
        let mut extras = Vec::new();
        if family.is_playstation() {
            extras.push((btn_label(family, Btn::Touchpad), input.pressed(Btn::Touchpad)));
            if matches!(family, Family::DualSense | Family::DualSenseEdge) {
                extras.push((btn_label(family, Btn::Mute), input.pressed(Btn::Mute)));
            }
        }

        // Button bindings (`bindings_card`).
        let chip_family = pad.map_or(if layout == Layout::DInputSony { Family::DualSense } else { Family::Xbox }, |p| p.family);
        let bindings = ROWS
            .iter()
            .map(|&row| {
                let changed = match row {
                    Row::Single(i) => target.edit.sin[i] != target.saved.sin[i],
                    Row::Group(i) => target.edit.sin[i..i + 4] != target.saved.sin[i..i + 4],
                };
                let (chip, live) = chip(chip_family, layout, row, &target.edit, &input, light);
                Binding {
                    row,
                    label: row.label(),
                    changed,
                    capturing: st.capture == Some(row),
                    prompt: match row {
                        Row::Group(_) => t("Move a stick or press the D-pad…"),
                        Row::Single(_) => t("Press a button…"),
                    },
                    chip,
                    live,
                }
            })
            .collect();
        let mode = if layout == Layout::XInput { "XInput" } else { "DirectInput" };
        let bindings_sub = tf(
            "FINAL FANTASY XI · {client} · {mode} · click an action, then press the button for it",
            &[("client", &t("Windower / Ashita")), ("mode", &mode)],
        );

        // Game options (`options_card`).
        let switch = |label: &'static str, flag: Flag| Switch {
            label,
            flag,
            on: target.edit.flag(flag),
            // Renumbering for XInput needs to know which controller the bindings are for.
            enabled: flag != Flag::XInput || st.known_di_layout().is_some(),
        };
        let switches = [
            switch(t("Use a controller in FFXI"), Flag::Enabled),
            switch(t("Vibration (force feedback)"), Flag::ForceFeedback),
            switch(t("XInput mode"), Flag::XInput),
            switch(t("Keep working in the background"), Flag::Background),
        ];
        let mut notes = Vec::new();
        let family_known = pad.map(|p| p.family);
        if st.preview {
            notes.push((t("Design preview: a synthetic controller is shown and nothing is ever written.").to_owned(), Tone::Sub));
        }
        if matches!(family_known, Some(Family::DualSense | Family::DualSenseEdge)) {
            notes.push((t("Touchpad click and Mic button can be bound with XInput mode off. Connect by USB to use the Mic button; Bluetooth does not expose it to FFXI.").to_owned(), Tone::Sub));
        }
        if family_known.is_some_and(|f| f.is_playstation()) && target.edit.flag(Flag::XInput) {
            notes.push((t("FFXI's XInput mode only sees Xbox-compatible controllers. Turn it off to use this PlayStation controller through DirectInput, or map it with Steam Input.").to_owned(), Tone::Warning));
        }
        if app.settings.launch_with == crate::backend::LaunchWith::Ashita {
            notes.push((t("Ashita's boot controller settings take priority at launch. Changes here also apply to running games.").to_owned(), Tone::Sub));
        }
        notes.push((tf("Stored in {source}", &[("source", &target.source)]), Tone::Sub));
        if let Some(n) = st.notice.as_ref() {
            notes.push((n.text.clone(), if n.error() { Tone::Danger } else { Tone::Success }));
        }

        Front {
            badge,
            title,
            sub: sub_line,
            pads,
            status,
            retry: st.notice.as_ref().is_some_and(Notice::retry),
            picture: art::controller(family, &input, light, accent),
            connected: pad.is_some(),
            caption,
            triggers: [(lt, input.lt), (rt, input.rt)],
            sticks: [format!("{:+.2}  {:+.2}", input.lx, input.ly), format!("{:+.2}  {:+.2}", input.rx, input.ry)],
            rumble: pad.is_some_and(|p| p.rumble),
            extras,
            bindings_sub,
            bindings,
            switches,
            notes,
        }
    }

    /// The messages the page's controls send, as `view` sends them.
    pub(crate) fn select(id: usize) -> Msg { Msg::Select(id) }
    pub(crate) fn retry() -> Msg { Msg::Apply }
    pub(crate) fn rumble() -> Msg { Msg::Rumble }
    pub(crate) fn reset() -> Msg { Msg::Reset }
    pub(crate) fn set_flag(flag: Flag, on: bool) -> Msg { Msg::SetFlag(flag, on) }
    /// A binding row's press: capture its control, or stop capturing.
    pub(crate) fn press(binding: &Binding) -> Msg { if binding.capturing { Msg::CancelCapture } else { Msg::Capture(binding.row) } }
    pub(crate) fn unbind(binding: &Binding) -> Msg { Msg::Clear(binding.row) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(state: &mut State, message: Msg) -> Effect<Msg> {
        super::update(state, message, &crate::backend::Config::default())
    }

    fn ps5_state(xinput: bool) -> State {
        let target = || {
            let mut config = PadConfig::default();
            config.set_flag(Flag::XInput, xinput, Layout::DInputSony, &mut ffxi::Unbound::default());
            Target { source: "test fixture".into(), saved: config.clone(), edit: config, unbound: ffxi::Unbound::default() }
        };
        State {
            preview: true,
            pads: vec![Pad {
                id: 0,
                name: "DualSense".into(),
                family: Family::DualSense,
                vid: Some(0x054C),
                pid: Some(0x0CE6),
                wireless: Some(false),
                battery: None,
                battery_source: None,
                battery_state: detect::BatteryState::Unknown,
                battery_level: None,
                rumble: false,
                input: Input::default(),
            }],
            selected: Some(0),
            prev: Input::default(),
            retail: target(),
            capture: None,
            notice: None,
            saving: Saving::default(),
        }
    }

    #[test]
    fn rapid_edits_save_the_latest_snapshot_without_overlapping_writes() {
        let mut state = ps5_state(false);
        state.preview = false;
        let background = !state.retail.edit.flag(Flag::Background);
        let _ = update(&mut state, Msg::SetFlag(Flag::Background, background));
        let first = state.saving.revision;
        let first_pad = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(first));
        assert_eq!(state.saving.in_flight, Some(first));
        let _ = update(&mut state, Msg::Clear(Row::Single(6)));
        let latest = state.saving.revision;
        assert_ne!(latest, first);
        let _ = update(&mut state, Msg::SaveLater(latest));
        assert_eq!(state.saving.in_flight, Some(first));
        let _ = update(&mut state, Msg::Saved(first, first_pad.clone(), Ok((first_pad, "Saved".into()))));
        assert!(state.take_persisted_change());
        assert!(state.take_live_update().is_none(), "an older disk snapshot must not be applied live");
        let _ = update(&mut state, Msg::SaveLater(first));
        assert!(state.saving.in_flight.is_none(), "a stale debounce must not start another write");
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(latest));
        let _ = update(&mut state, Msg::Saved(latest, snapshot.clone(), Ok((snapshot.clone(), "Saved".into()))));
        let (revision, values) = state.take_live_update().unwrap();
        assert_eq!(revision, latest);
        assert_eq!(values, controller_values(&snapshot));
        assert_eq!(state.retail.saved, snapshot);
    }

    #[test]
    fn live_updates_coalesce_and_a_stale_reply_does_not_replace_current_feedback() {
        let mut state = ps5_state(false);
        state.preview = false;
        let _ = update(&mut state, Msg::Apply);
        let (first, _) = state.take_live_update().unwrap();
        let _ = update(&mut state, Msg::Clear(Row::Single(6)));
        let latest = state.saving.revision;
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(latest));
        let _ = update(&mut state, Msg::Saved(latest, snapshot.clone(), Ok((snapshot, "Saved".into()))));
        assert!(state.take_live_update().is_none(), "one confirmation wait per launcher");
        let _ = update(&mut state, Msg::LiveResult(first, Err("old failure".into())));
        assert!(!state.notice.as_ref().unwrap().error());
        assert_eq!(state.take_live_update().unwrap().0, latest);
        let _ = update(&mut state, Msg::LiveResult(first, Err("old failure".into())));
        assert_eq!(state.saving.applying, Some(latest));
        let _ = update(&mut state, Msg::LiveResult(latest, Ok("Applied".into())));
        assert_eq!(state.notice.as_ref().unwrap().text, "Applied");
        assert!(state.saving.applying.is_none());
    }

    #[test]
    fn failed_saves_keep_edits_for_retry_and_battery_refreshes_do_not_save() {
        let mut state = ps5_state(false);
        state.preview = false;
        let mut pads = state.pads.clone();
        pads[0].battery = Some(55);
        let _ = update(&mut state, Msg::Pads(pads));
        assert_eq!(state.saving.revision, 0);
        assert!(state.take_live_update().is_none());
        let before = state.retail.saved.clone();
        let _ = update(&mut state, Msg::Clear(Row::Single(6)));
        let revision = state.saving.revision;
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(revision));
        let _ = update(&mut state, Msg::Saved(revision, snapshot.clone(), Err("Access denied".into())));
        assert_eq!(state.retail.saved, before);
        assert_eq!(state.retail.edit, snapshot);
        assert!(state.saving.in_flight.is_none());
        assert!(state.notice.as_ref().unwrap().retry());
        assert!(!state.take_persisted_change());
        assert!(state.take_live_update().is_none());
        let _ = update(&mut state, Msg::Apply);
        let _ = update(&mut state, Msg::SaveLater(revision));
        assert_eq!(state.saving.in_flight, Some(revision));
    }

    fn press(state: &mut State, buttons: u32) {
        let mut pads = state.pads.clone();
        pads[0].input.buttons = buttons;
        let _ = update(state, Msg::Pads(pads));
    }

    #[test]
    fn gamepad_ps5_extra_controls_capture_only_after_a_new_press() {
        let mut state = ps5_state(false);
        for (button, slot, expected) in [(Btn::Touchpad, 6, 13), (Btn::Mute, 8, 14)] {
            press(&mut state, button.bit());
            let _ = update(&mut state, Msg::Capture(Row::Single(slot)));
            let original = state.retail.edit.sin[slot];
            press(&mut state, button.bit());
            assert_eq!(state.capture, Some(Row::Single(slot)));
            assert_eq!(state.retail.edit.sin[slot], original);
            press(&mut state, 0);
            press(&mut state, button.bit());
            assert!(state.capture.is_none());
            assert!(state.notice.is_none());
            assert_eq!(state.retail.edit.sin[slot], expected);
        }
        assert_ne!(state.retail.edit, state.retail.saved);
    }

    #[test]
    fn gamepad_group_capture_waits_for_a_new_stick_movement() {
        let mut state = ps5_state(false);
        let mut pads = state.pads.clone();
        pads[0].input.lx = 0.8;
        let _ = update(&mut state, Msg::Pads(pads.clone()));
        let _ = update(&mut state, Msg::Capture(Row::Group(17)));
        let before = state.retail.edit.clone();
        // Another button or battery update must not capture a held stick.
        pads[0].input.buttons = Btn::South.bit();
        let _ = update(&mut state, Msg::Pads(pads.clone()));
        assert_eq!(state.capture, Some(Row::Group(17)));
        assert_eq!(state.retail.edit, before);
        pads[0].input.lx = 0.0;
        let _ = update(&mut state, Msg::Pads(pads.clone()));
        pads[0].input.ly = 0.8;
        let _ = update(&mut state, Msg::Pads(pads));
        assert!(state.capture.is_none());
        assert_eq!(state.retail.edit.sin[17..21], Layout::DInputSony.group(Stick::Left));
    }

    #[test]
    fn back_cancels_gamepad_capture_before_closing_the_active_page() {
        let mut app = crate::tests::app();
        app.page = crate::Page::Settings;
        let _ = app.update(crate::Message::Gamepad(Msg::Capture(Row::Group(17))));
        assert!(app.gamepad.capturing());
        let _ = app.update(crate::Message::ControllerBack);
        assert!(!app.gamepad.capturing());
        assert!(app.page == crate::Page::Settings);
    }

    #[test]
    fn gamepad_ps5_extra_controls_in_xinput_keep_capture_and_explain_directinput() {
        let mut state = ps5_state(true);
        for button in [Btn::Touchpad, Btn::Mute] {
            press(&mut state, 0);
            let _ = update(&mut state, Msg::Capture(Row::Single(6)));
            press(&mut state, button.bit());
            assert_eq!(state.capture, Some(Row::Single(6)));
            assert_eq!(state.retail.edit, state.retail.saved);
            let notice = state.notice.as_ref().unwrap();
            assert_eq!(notice.kind, NoticeKind::Capture);
            assert!(!notice.retry(), "a capture error offers no Retry save");
            assert!(notice.text.contains("Turn XInput mode off"));
        }
        let _ = update(&mut state, Msg::SetFlag(Flag::XInput, false));
        press(&mut state, 0);
        press(&mut state, Btn::Mute.bit());
        assert!(state.capture.is_none());
        assert!(state.notice.is_none());
        assert_eq!(state.retail.edit.sin[6], 14);
    }

    #[test]
    fn cancelling_a_capture_clears_its_error() {
        let mut state = ps5_state(true);
        press(&mut state, 0);
        let _ = update(&mut state, Msg::Capture(Row::Single(6)));
        press(&mut state, Btn::Touchpad.bit());
        assert!(state.notice.is_some());
        let _ = update(&mut state, Msg::CancelCapture);
        assert!(state.notice.is_none());
    }

    /// A save that kept values another tool stored meanwhile carries them
    /// into the page wherever the user has not edited since.
    #[test]
    fn a_merged_save_rebases_the_page_onto_what_was_written() {
        let mut state = ps5_state(false);
        state.preview = false;
        let _ = update(&mut state, Msg::SetFlag(Flag::Background, true));
        let revision = state.saving.revision;
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(revision));
        let mut written = snapshot.clone();
        written.sin[7] = 2; // rebound outside the launcher
        let _ = update(&mut state, Msg::Saved(revision, snapshot, Ok((written.clone(), "Saved".into()))));
        assert_eq!(state.retail.saved, written);
        assert_eq!(state.retail.edit, written);
        assert!(state.saving.in_flight.is_none());
        // Running games get the merged values too.
        let (_, live) = state.take_live_update().expect("merged save is applied live");
        assert_eq!(live, controller_values(&written));
    }

    /// Without a controller or Sony ids, renumbering for XInput would guess
    /// the bindings' layout, so the toggle does nothing.
    #[test]
    fn xinput_toggle_waits_for_a_known_directinput_layout() {
        let mut state = ps5_state(false);
        state.pads.clear();
        // Sony ids in the stored bindings name the layout without a pad.
        assert_eq!(state.known_di_layout(), Some(Layout::DInputSony));
        let _ = update(&mut state, Msg::SetFlag(Flag::XInput, true));
        assert!(state.retail.edit.flag(Flag::XInput));
        // XInput bindings say nothing about the DirectInput layout.
        assert_eq!(state.known_di_layout(), None);
        let before = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SetFlag(Flag::XInput, false));
        assert_eq!(state.retail.edit, before);
    }
}