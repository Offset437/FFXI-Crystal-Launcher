//! Connected controllers: gilrs (Windows.Gaming.Input) on a worker thread,
//! classified by USB vendor/product id, with live button/axis state.
use crate::i18n::{t, tf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[path = "hardware.rs"]
mod hardware;

/// Logical controls in SDL's positional naming (South is the bottom face
/// button: A on Xbox, Cross on PlayStation, B on Nintendo).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Btn {
    South,
    East,
    West,
    North,
    LB,
    RB,
    LT,
    RT,
    Back,
    Start,
    LS,
    RS,
    DUp,
    DDown,
    DLeft,
    DRight,
    Guide,
    Touchpad,
    Mute,
}

impl Btn {
    pub(crate) const ALL: [Btn; 19] = [
        Btn::South,
        Btn::East,
        Btn::West,
        Btn::North,
        Btn::LB,
        Btn::RB,
        Btn::LT,
        Btn::RT,
        Btn::Back,
        Btn::Start,
        Btn::LS,
        Btn::RS,
        Btn::DUp,
        Btn::DDown,
        Btn::DLeft,
        Btn::DRight,
        Btn::Guide,
        Btn::Touchpad,
        Btn::Mute,
    ];
    pub(crate) fn bit(self) -> u32 {
        1 << (self as u32)
    }
    pub(crate) fn is_dpad(self) -> bool {
        matches!(self, Btn::DUp | Btn::DDown | Btn::DLeft | Btn::DRight)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Xbox,
    RogXboxAlly,
    RogXboxAllyX,
    DualSense,
    DualSenseEdge,
    DualShock4,
    SwitchPro,
    Generic,
}

impl Family {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Family::Xbox => "Xbox",
            Family::RogXboxAlly => "ROG Xbox Ally",
            Family::RogXboxAllyX => "ROG Xbox Ally X",
            Family::DualSense => "PlayStation DualSense",
            Family::DualSenseEdge => "PlayStation DualSense Edge",
            Family::DualShock4 => "PlayStation DualShock 4",
            Family::SwitchPro => "Nintendo Switch Pro",
            Family::Generic => t("Generic controller"),
        }
    }
    pub(crate) fn is_playstation(self) -> bool {
        matches!(self, Family::DualSense | Family::DualSenseEdge | Family::DualShock4)
    }
    pub(crate) fn is_xbox(self) -> bool {
        matches!(self, Family::Xbox | Family::RogXboxAlly | Family::RogXboxAllyX)
    }
    pub(crate) fn is_handheld(self) -> bool {
        matches!(self, Family::RogXboxAlly | Family::RogXboxAllyX)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatterySource {
    Controller,
    Handheld,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BatteryState {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Idle,
}

/// WGI's low-cost controller reports are discrete levels, not measured %.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatteryLevel {
    Critical,
    Low,
    Medium,
    Full,
}

/// Classifies by USB ids, falling back to the device name.
pub(crate) fn classify(vid: Option<u16>, pid: Option<u16>, name: &str) -> Family {
    match (vid, pid) {
        (Some(0x045E), _) => return Family::Xbox,
        (Some(0x054C), Some(0x0CE6)) => return Family::DualSense,
        (Some(0x054C), Some(0x0DF2)) => return Family::DualSenseEdge,
        (Some(0x054C), _) => return Family::DualShock4,
        (Some(0x057E), Some(0x2009)) => return Family::SwitchPro,
        _ => (),
    }
    let n = name.to_ascii_lowercase();
    if n.contains("xbox") || n.contains("xinput") {
        Family::Xbox
    } else if n.contains("dualsense edge") {
        Family::DualSenseEdge
    } else if n.contains("dualsense") {
        Family::DualSense
    } else if n.contains("dualshock") || n.contains("playstation") {
        Family::DualShock4
    } else if n.contains("pro controller") {
        Family::SwitchPro
    } else {
        Family::Generic
    }
}

/// Live input, quantised so tiny stick noise does not cause redraws.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Input {
    pub buttons: u32,
    pub lt: f32,
    pub rt: f32,
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
}

impl Input {
    pub(crate) fn pressed(&self, b: Btn) -> bool {
        self.buttons & b.bit() != 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pad {
    pub id: usize,
    pub name: String,
    pub family: Family,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    /// `Some(true)` wireless, `Some(false)` cable, `None` unknown.
    pub wireless: Option<bool>,
    pub battery: Option<u8>,
    pub battery_source: Option<BatterySource>,
    pub battery_state: BatteryState,
    pub battery_level: Option<BatteryLevel>,
    pub rumble: bool,
    pub input: Input,
}

impl Pad {
    pub(crate) fn connection(&self) -> String {
        let transport = if self.family.is_handheld() {
            t("Built-in")
        } else {
            match self.wireless {
                Some(false) => t("Wired"),
                Some(true) => t("Wireless"),
                None => t("Connected"),
            }
        };
        let handheld = self.battery_source == Some(BatterySource::Handheld);
        let battery = match (self.battery.filter(|p| *p <= 100), self.battery_level) {
            (Some(b), _) if handheld => tf("Handheld battery {percent}%", &[("percent", &b)]),
            (Some(b), _) => tf("Controller battery {percent}%", &[("percent", &b)]),
            (_, Some(level)) => {
                let label = match level {
                    BatteryLevel::Critical => t("Critical"),
                    BatteryLevel::Low => t("Low"),
                    BatteryLevel::Medium => t("Medium"),
                    BatteryLevel::Full => t("Full"),
                };
                tf("Controller battery: {level}", &[("level", &label)])
            }
            _ if handheld => t("Handheld battery unavailable").into(),
            _ => t("Controller battery unavailable").into(),
        };
        let mut connection = format!("{transport} · {battery}");
        if self.battery_state == BatteryState::Charging {
            connection.push_str(" · ");
            connection.push_str(t("Charging"));
        }
        connection
    }
}

enum Command {
    Rumble(usize),
}

/// The running worker's command channel, tagged with the worker's number.
fn commands() -> &'static Mutex<Option<(u64, mpsc::Sender<Command>)>> {
    static C: OnceLock<Mutex<Option<(u64, mpsc::Sender<Command>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// Asks the worker to buzz a controller briefly.
pub(crate) fn rumble(id: usize) {
    if let Ok(guard) = commands().lock() {
        if let Some((_, tx)) = guard.as_ref() {
            let _ = tx.send(Command::Rumble(id));
        }
    }
}

fn q(v: f32) -> f32 {
    (v * 50.0).round() / 50.0
}

/// WGI raw button indices follow the Sony HID descriptor. gilrs' standard
/// Button enum has no touchpad or mic control, but its cache retains these
/// native codes even when the SDL mapping leaves them unnamed.
/// DualSense USB declares 15 buttons; Bluetooth declares only 14, so the
/// mic button requires USB for both capture and ordinary FFXI DirectInput.
fn sony_extra_buttons(family: Family, buttons: impl IntoIterator<Item = (u32, bool)>) -> u32 {
    buttons.into_iter().fold(0, |mask, (code, pressed)| {
        let button = match (family, code) {
            (Family::DualSense | Family::DualSenseEdge | Family::DualShock4, 13) => Some(Btn::Touchpad),
            (Family::DualSense | Family::DualSenseEdge, 14) => Some(Btn::Mute),
            _ => None,
        };
        mask | button.filter(|_| pressed).map_or(0, Btn::bit)
    })
}

fn read_pad(id: gilrs::GamepadId, g: &gilrs::Gamepad, hardware: &hardware::Snapshot) -> Pad {
    use gilrs::{Axis, Button};
    const MAP: [(Button, Btn); 17] = [
        (Button::South, Btn::South),
        (Button::East, Btn::East),
        (Button::West, Btn::West),
        (Button::North, Btn::North),
        (Button::LeftTrigger, Btn::LB),
        (Button::RightTrigger, Btn::RB),
        (Button::LeftTrigger2, Btn::LT),
        (Button::RightTrigger2, Btn::RT),
        (Button::Select, Btn::Back),
        (Button::Start, Btn::Start),
        (Button::LeftThumb, Btn::LS),
        (Button::RightThumb, Btn::RS),
        (Button::DPadUp, Btn::DUp),
        (Button::DPadDown, Btn::DDown),
        (Button::DPadLeft, Btn::DLeft),
        (Button::DPadRight, Btn::DRight),
        (Button::Mode, Btn::Guide),
    ];
    let (vid, pid) = (g.vendor_id(), g.product_id());
    let (family, wireless, power) = hardware.metadata(vid, pid, g.name());
    let mut input = Input::default();
    for (gb, b) in MAP {
        if g.is_pressed(gb) {
            input.buttons |= b.bit();
        }
    }
    {
        input.buttons |= sony_extra_buttons(
            family,
            g.state().buttons().map(|(code, data)| (code.into_u32(), data.is_pressed())),
        );
    }
    let trigger = |b: Button| g.button_data(b).map_or(0.0, |d| d.value());
    input.lt = q(trigger(Button::LeftTrigger2));
    input.rt = q(trigger(Button::RightTrigger2));
    if input.lt > 0.5 {
        input.buttons |= Btn::LT.bit();
    }
    if input.rt > 0.5 {
        input.buttons |= Btn::RT.bit();
    }
    input.lx = q(g.value(Axis::LeftStickX));
    input.ly = q(g.value(Axis::LeftStickY));
    input.rx = q(g.value(Axis::RightStickX));
    input.ry = q(g.value(Axis::RightStickY));
    Pad {
        id: usize::from(id),
        name: g.name().to_string(),
        family,
        vid,
        pid,
        wireless,
        battery: power.percent,
        battery_source: power.source,
        battery_state: power.state,
        battery_level: power.level,
        rumble: g.is_ff_supported(),
        input,
    }
}

fn snapshot(gilrs: &gilrs::Gilrs, hardware: &hardware::Snapshot) -> Vec<Pad> {
    gilrs
        .gamepads()
        .filter(|(_, g)| g.is_connected())
        .map(|(id, g)| read_pad(id, &g, hardware))
        .collect()
}

fn publish_change(last: &mut Option<Vec<Pad>>, now: Vec<Pad>, emit: &mut impl FnMut(Vec<Pad>) -> bool) -> bool {
    if last.as_ref() == Some(&now) {
        return true;
    }
    if !emit(now.clone()) {
        return false;
    }
    *last = Some(now);
    true
}

fn play_rumble(gilrs: &mut gilrs::Gilrs, id: usize) -> Option<gilrs::ff::Effect> {
    use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Repeat, Replay, Ticks};
    let gid = gilrs.gamepads().find(|(g, _)| usize::from(*g) == id)?.0;
    let replay = Replay {
        play_for: Ticks::from_ms(260),
        ..Default::default()
    };
    let effect = EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong { magnitude: 42_000 },
            scheduling: replay,
            ..Default::default()
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak { magnitude: 52_000 },
            scheduling: replay,
            ..Default::default()
        })
        .repeat(Repeat::For(Ticks::from_ms(260)))
        .gamepads(&[gid])
        .finish(gilrs)
        .ok()?;
    effect.play().ok()?;
    Some(effect)
}

/// Runs the controller thread, calling `emit` with the full list whenever a
/// controller connects, disconnects, changes input, or changes reported power
/// or transport. Returns when `emit` reports the receiver is gone, or as soon
/// as `keep_going` says so (checked every pass, so a thread whose listener
/// went away stops even while nothing changes).
pub(crate) fn run(mut emit: impl FnMut(Vec<Pad>) -> bool, keep_going: impl Fn() -> bool) {
    let mut gilrs = match gilrs::Gilrs::new() {
        Ok(g) => g,
        Err(gilrs::Error::NotImplemented(g)) => g,
        Err(_) => {
            emit(Vec::new());
            return;
        }
    };
    static WORKERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let worker = WORKERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    if let Ok(mut guard) = commands().lock() {
        *guard = Some((worker, tx));
    }
    let mut last: Option<Vec<Pad>> = None;
    let mut effects: Vec<(Instant, gilrs::ff::Effect)> = Vec::new();
    let mut hardware = hardware::Snapshot::refresh();
    let mut last_power = Instant::now();
    while keep_going() {
        // gilrs updates pad state only while events are drained, so with no
        // event and no hardware refresh the pads are unchanged.
        let mut changed = last.is_none();
        let mut topology_changed = false;
        while let Some(event) = gilrs.next_event() {
            changed = true;
            topology_changed |= matches!(event.event, gilrs::EventType::Connected | gilrs::EventType::Disconnected);
        }
        while let Ok(Command::Rumble(id)) = rx.try_recv() {
            if let Some(e) = play_rumble(&mut gilrs, id) {
                effects.push((Instant::now(), e));
            }
        }
        effects.retain(|(t, _)| t.elapsed() < Duration::from_secs(1));
        // Poll battery/transport independently of input; emit changes even if
        // the controller has stayed untouched throughout the interval.
        if topology_changed || last_power.elapsed() >= Duration::from_secs(5) {
            hardware = hardware::Snapshot::refresh();
            last_power = Instant::now();
            changed = true;
        }
        if changed && !publish_change(&mut last, snapshot(&gilrs, &hardware), &mut emit) {
            break;
        }
        // Input drives the launcher only while its window is in front (controller navigation,
        // the Controller page's live view): behind a game, a slow look for (dis)connects.
        std::thread::sleep(Duration::from_millis(if crate::window_chrome::is_foreground() || !effects.is_empty() { 16 } else { 250 }));
    }
    // A newer thread may have installed its own sender meanwhile; keep that one.
    if let Ok(mut guard) = commands().lock() {
        if guard.as_ref().is_some_and(|(owner, _)| *owner == worker) {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad() -> Pad {
        Pad {
            id: 0, name: "Xbox Controller".into(), family: Family::Xbox,
            vid: Some(0x045e), pid: Some(0x0b12), wireless: None, battery: None,
            battery_source: Some(BatterySource::Controller), battery_state: BatteryState::Unknown,
            battery_level: None, rumble: false, input: Input::default(),
        }
    }

    #[test]
    fn gamepad_connection_does_not_infer_transport_from_charging() {
        let mut controller = pad();
        controller.battery = Some(42);
        controller.battery_state = BatteryState::Charging;
        for (wireless, prefix) in [(Some(false), "Wired"), (Some(true), "Wireless"), (None, "Connected")] {
            controller.wireless = wireless;
            let label = controller.connection();
            assert!(label.starts_with(prefix));
            assert!(label.contains("Controller battery 42%"));
            assert!(label.ends_with("Charging"));
        }
    }

    #[test]
    fn gamepad_connection_names_handheld_source_and_missing_battery() {
        let mut controller = pad();
        controller.family = Family::RogXboxAlly;
        controller.battery_source = Some(BatterySource::Handheld);
        controller.battery_state = BatteryState::Idle;
        assert_eq!(controller.connection(), "Built-in · Handheld battery unavailable");
        controller.battery = Some(42);
        assert_eq!(controller.connection(), "Built-in · Handheld battery 42%");
        controller.battery = Some(255);
        assert!(!controller.connection().contains("255%"));
    }

    #[test]
    fn gamepad_connection_labels_coarse_full_without_a_percentage() {
        let mut controller = pad();
        controller.wireless = Some(true);
        controller.battery_level = Some(BatteryLevel::Full);
        assert_eq!(controller.connection(), "Wireless · Controller battery: Full");
        assert!(!controller.connection().contains('%'));
        controller.battery_level = None;
        assert_eq!(controller.connection(), "Wireless · Controller battery unavailable");
    }

    #[test]
    fn gamepad_worker_publishes_battery_changes_without_input_changes() {
        let first = pad();
        let mut previous = None;
        let mut published = Vec::new();
        let mut emit = |pads| { published.push(pads); true };
        assert!(publish_change(&mut previous, vec![first.clone()], &mut emit));
        assert!(publish_change(&mut previous, vec![first.clone()], &mut emit));
        let mut next = first.clone();
        next.battery = Some(42);
        assert_eq!(first.input, next.input);
        assert!(publish_change(&mut previous, vec![next], &mut emit));
        let mut next = first.clone();
        next.battery_state = BatteryState::Charging;
        assert!(publish_change(&mut previous, vec![next], &mut emit));
        assert_eq!(published.len(), 3);
    }

    #[test]
    fn gamepad_classifies_by_usb_ids() {
        assert_eq!(classify(Some(0x045E), Some(0x0B12), "Controller"), Family::Xbox);
        assert_eq!(classify(Some(0x054C), Some(0x0CE6), "Wireless Controller"), Family::DualSense);
        assert_eq!(classify(Some(0x054C), Some(0x0DF2), ""), Family::DualSenseEdge);
        assert_eq!(classify(Some(0x054C), Some(0x09CC), ""), Family::DualShock4);
        assert_eq!(classify(Some(0x057E), Some(0x2009), ""), Family::SwitchPro);
        assert_eq!(classify(None, None, "Xbox One For Windows"), Family::Xbox);
        assert_eq!(classify(Some(0x0F0D), Some(0x0001), "Arcade Stick"), Family::Generic);
    }

    #[test]
    fn gamepad_sony_extra_buttons_capture_press_and_release() {
        for family in [Family::DualSense, Family::DualSenseEdge] {
            assert_eq!(sony_extra_buttons(family, [(13, true), (14, true)]), Btn::Touchpad.bit() | Btn::Mute.bit());
            assert_eq!(sony_extra_buttons(family, [(13, false), (14, true)]), Btn::Mute.bit());
            assert_eq!(sony_extra_buttons(family, [(13, false), (14, false)]), 0);
        }
        assert_eq!(sony_extra_buttons(Family::DualShock4, [(13, true), (14, true)]), Btn::Touchpad.bit());
    }

    #[test]
    fn gamepad_sony_extra_buttons_ignore_other_devices_axes_and_hats() {
        for family in [Family::Xbox, Family::SwitchPro, Family::Generic] {
            assert_eq!(sony_extra_buttons(family, [(13, true), (14, true)]), 0);
        }
        // WGI encodes the element kind in the high 16 bits. Axis 13/14
        // and synthesized D-pad events must never become extra buttons.
        assert_eq!(sony_extra_buttons(Family::DualSense, [(0x1000d, true), (0x2000e, true), (u32::MAX, true)]), 0);
        assert_eq!(sony_extra_buttons(Family::DualSense, []), 0);
    }
}
