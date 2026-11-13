//! FINAL FANTASY XI's gamepad settings and where each client keeps them.
//!
//! The game reads four string values from
//! `HKLM\SOFTWARE\[WOW6432Node\]PlayOnline{US,EU,}\SquareEnix\FinalFantasyXI`:
//!
//! * `padmode000`: six 0/1 flags: gamepad on, force feedback, sliders, hat
//!   switches, keep working while the window is inactive, XInput.
//! * `padsin000`: one button/axis id per game function (27 slots, see
//!   [`SLOT_COUNT`]); `-1` is unbound. Ids 0.. are buttons, 32.. are axes and a
//!   minus sign reverses an axis. What an id means depends on the input API:
//!   with XInput on the game numbers the XInput buttons its own way; with it
//!   off ids are DirectInput button numbers and DIJOYSTATE axis offsets.
//! * `padguid000`: the DirectInput instance the game binds to (all zero: any).
//! * `padexsin000` exists on newer installs; it is preserved, never edited.
//!
//! Retail FFXI (Windower, Ashita) reads the Windows registry, through the same
//! key accessor and region rule as the Graphics page (`game_config`). The
//! original values are backed up as a `.reg` file before the first change.
use super::detect::Btn;
use crate::i18n::{t, tf};
use std::path::{Path, PathBuf};

pub(crate) const SLOT_COUNT: usize = 27;
pub(crate) const MODE_COUNT: usize = 6;
/// The XInput defaults: what the FFXI config tool writes for an Xbox controller.
pub(crate) const DEFAULT_XINPUT_SIN: [i32; SLOT_COUNT] = [
    8, 9, 13, 12, 10, 0, 1, 3, 2, 15, -1, -1, 14, -33, -33, 32, 32, -36, -36, 35, 35, 6, 7, 5, 4,
    11, -1,
];
pub(crate) const DEFAULT_MODE: [i32; MODE_COUNT] = [1, 1, 0, 0, 0, 1];

/// `padmode000` flag positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flag {
    Enabled = 0,
    ForceFeedback = 1,
    Background = 4,
    XInput = 5,
}

/// How a `padsin000` id maps to a physical control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// XInput on: FFXI's own XInput numbering.
    XInput,
    /// DirectInput, Xbox-style device (the native client's DirectInput view, or
    /// an Xbox pad through Windows' HID driver).
    DInputXbox,
    /// DirectInput, Sony HID numbering (DualShock 4 / DualSense).
    DInputSony,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stick {
    Left,
    Right,
    Dpad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AxisDir {
    X,
    Y,
}

/// One decoded `padsin000` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    None,
    Button(Btn),
    Axis(Stick, AxisDir, bool),
    /// An id this layout does not name; shown as a number, kept as is.
    Raw(i32),
}

const XINPUT_BUTTONS: [Btn; 16] = [
    Btn::East,
    Btn::West,
    Btn::North,
    Btn::South,
    Btn::DRight,
    Btn::DLeft,
    Btn::DUp,
    Btn::DDown,
    Btn::LB,
    Btn::LT,
    Btn::LS,
    Btn::RB,
    Btn::RT,
    Btn::RS,
    Btn::Start,
    Btn::Back,
];
const DI_XBOX_BUTTONS: [Btn; 11] = [
    Btn::South,
    Btn::East,
    Btn::West,
    Btn::North,
    Btn::LB,
    Btn::RB,
    Btn::Back,
    Btn::Start,
    Btn::LS,
    Btn::RS,
    Btn::Guide,
];
// Sony's USB HID descriptor exposes touchpad click as button 14 and the
// DualSense mic button as button 15 (zero-based ids 13/14 in padsin000).
// Bluetooth omits the mic button; saving id 14 cannot make it appear there.
const DI_SONY_BUTTONS: [Btn; 15] = [
    Btn::West,
    Btn::South,
    Btn::East,
    Btn::North,
    Btn::LB,
    Btn::RB,
    Btn::LT,
    Btn::RT,
    Btn::Back,
    Btn::Start,
    Btn::LS,
    Btn::RS,
    Btn::Guide,
    Btn::Touchpad,
    Btn::Mute,
];

impl Layout {
    fn buttons(self) -> &'static [Btn] {
        match self {
            Layout::XInput => &XINPUT_BUTTONS,
            Layout::DInputXbox => &DI_XBOX_BUTTONS,
            Layout::DInputSony => &DI_SONY_BUTTONS,
        }
    }
    /// Axis ids: 32 + the DIJOYSTATE axis index (X, Y, Z, Rx, Ry, Rz), the
    /// POV hat as 40 (x) / 41 (y).
    fn axis_id(self, stick: Stick, dir: AxisDir) -> Option<i32> {
        Some(match (self, stick, dir) {
            (_, Stick::Left, AxisDir::X) => 32,
            (_, Stick::Left, AxisDir::Y) => 33,
            (Layout::DInputSony, Stick::Right, AxisDir::X) => 34,
            (Layout::DInputSony, Stick::Right, AxisDir::Y) => 37,
            (_, Stick::Right, AxisDir::X) => 35,
            (_, Stick::Right, AxisDir::Y) => 36,
            (Layout::XInput, Stick::Dpad, _) => return None,
            (_, Stick::Dpad, AxisDir::X) => 40,
            (_, Stick::Dpad, AxisDir::Y) => 41,
        })
    }
    pub(crate) fn decode(self, id: i32) -> Control {
        if id == -1 || (self == Layout::XInput && id == 16) {
            return Control::None;
        }
        if (0..32).contains(&id) {
            return self
                .buttons()
                .get(id as usize)
                .map_or(Control::Raw(id), |b| Control::Button(*b));
        }
        let (neg, abs) = (id < 0, id.unsigned_abs() as i32);
        for stick in [Stick::Left, Stick::Right, Stick::Dpad] {
            for dir in [AxisDir::X, AxisDir::Y] {
                if self.axis_id(stick, dir) == Some(abs) {
                    return Control::Axis(stick, dir, neg);
                }
            }
        }
        Control::Raw(id)
    }
    pub(crate) fn encode_button(self, b: Btn) -> Option<i32> {
        self.buttons().iter().position(|x| *x == b).map(|i| i as i32)
    }
    fn encode(self, c: Control) -> i32 {
        match c {
            Control::None => -1,
            Control::Button(b) => self.encode_button(b).unwrap_or(-1),
            Control::Axis(stick, dir, neg) => self
                .axis_id(stick, dir)
                .map_or(-1, |id| if neg { -id } else { id }),
            Control::Raw(id) => id,
        }
    }
    /// The four `padsin000` ids (up, down, left, right) for a direction group.
    pub(crate) fn group(self, stick: Stick) -> [i32; 4] {
        match (self, stick) {
            (Layout::XInput, Stick::Dpad) => [6, 7, 5, 4],
            _ => {
                let y = self.axis_id(stick, AxisDir::Y).unwrap_or(-1);
                let x = self.axis_id(stick, AxisDir::X).unwrap_or(-1);
                // Y reversed, as the shipped defaults have it.
                [-y, -y, x, x]
            }
        }
    }
    /// Which stick a direction group's four ids describe, if they match one.
    pub(crate) fn group_stick(self, ids: &[i32]) -> Option<Stick> {
        [Stick::Left, Stick::Right, Stick::Dpad].into_iter().find(|s| {
            let want = self.group(*s);
            ids.len() == 4
                && (0..4).all(|i| want[i].unsigned_abs() == ids[i].unsigned_abs())
        })
    }
}

/// The rows the bindings list shows, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Single(usize),
    /// Four slots from here: up, down, left, right.
    Group(usize),
}

pub(crate) const ROWS: [Row; 18] = [
    Row::Single(7),
    Row::Single(5),
    Row::Single(6),
    Row::Single(8),
    Row::Group(13),
    Row::Group(17),
    Row::Group(21),
    Row::Single(4),
    Row::Single(0),
    Row::Single(1),
    Row::Single(3),
    Row::Single(2),
    Row::Single(9),
    Row::Single(10),
    Row::Single(11),
    Row::Single(26),
    Row::Single(25),
    Row::Single(12),
];

impl Row {
    /// What the game function bound to this row is called in the list.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Row::Single(7) => t("Confirm"),
            Row::Single(5) => t("Cancel"),
            Row::Single(6) => t("Main menu"),
            Row::Single(8) => t("Active window"),
            Row::Group(13) => t("Move character"),
            Row::Group(17) => t("Camera"),
            Row::Group(21) => t("Menu cursor / target"),
            Row::Single(4) => t("Heal / lock target"),
            Row::Single(0) => t("Auto-run"),
            Row::Single(1) => t("Macro palette (Ctrl)"),
            Row::Single(3) => t("Macro palette (Alt)"),
            Row::Single(2) => t("First / third person"),
            Row::Single(9) => t("Hide windows"),
            Row::Single(10) => t("Hold: menus with stick"),
            Row::Single(11) => t("Hold: camera with stick"),
            Row::Single(26) => t("Swap stick roles"),
            Row::Single(25) => t("Screenshot"),
            Row::Single(12) => t("Log out"),
            _ => "",
        }
    }
}

/// The values the game reads, as stored (lists keep their length).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PadConfig {
    pub mode: Vec<i32>,
    pub sin: Vec<i32>,
}

impl Default for PadConfig {
    fn default() -> Self {
        Self {
            mode: DEFAULT_MODE.to_vec(),
            sin: DEFAULT_XINPUT_SIN.to_vec(),
        }
    }
}

pub(crate) fn parse_list(value: &str) -> Option<Vec<i32>> {
    value
        .split(',')
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.parse::<i32>().ok())
        .collect()
}

pub(crate) fn format_list(values: &[i32]) -> String {
    values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
}

/// Bindings a layout change unbound because the target layout has no such
/// control (Xbox LT/RT under DirectInput, Sony touchpad/mic under XInput),
/// with the layout they were made in, so the reverse change can put them back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Unbound {
    pub layout: Option<Layout>,
    pub slots: Vec<(usize, i32)>,
}

fn is_axis(id: i32) -> bool {
    id.unsigned_abs() >= 32
}

impl PadConfig {
    /// Builds a config from the stored strings; missing or malformed values
    /// fall back to the defaults (and lists are padded to their known length).
    pub(crate) fn from_values(mode: Option<&str>, sin: Option<&str>) -> Self {
        let mut mode = mode.and_then(parse_list).unwrap_or_else(|| DEFAULT_MODE.to_vec());
        let mut sin = sin.and_then(parse_list).unwrap_or_else(|| DEFAULT_XINPUT_SIN.to_vec());
        mode.resize(mode.len().max(MODE_COUNT), 0);
        sin.resize(sin.len().max(SLOT_COUNT), -1);
        Self { mode, sin }
    }
    pub(crate) fn flag(&self, f: Flag) -> bool {
        self.mode.get(f as usize).is_some_and(|v| *v != 0)
    }
    /// Sets a flag. Turning XInput on or off renumbers the bindings so each
    /// function keeps its physical control; `unbound` carries what a previous
    /// change could not map and receives what this one cannot.
    pub(crate) fn set_flag(&mut self, f: Flag, on: bool, di: Layout, unbound: &mut Unbound) {
        if f == Flag::XInput && self.flag(f) != on {
            let (from, to) = if on { (di, Layout::XInput) } else { (Layout::XInput, di) };
            let lost = self.translate(from, to);
            if unbound.layout == Some(to) {
                self.restore(&unbound.slots);
            }
            *unbound = Unbound { layout: Some(from), slots: lost };
        }
        self.mode[f as usize] = on as i32;
    }
    /// Renumbers every binding from one layout to another; returns the
    /// single-slot bindings (with their old ids) the target cannot express.
    pub(crate) fn translate(&mut self, from: Layout, to: Layout) -> Vec<(usize, i32)> {
        let mut lost = Vec::new();
        for row in ROWS {
            match row {
                Row::Single(i) => {
                    let old = self.sin[i];
                    self.sin[i] = to.encode(from.decode(old));
                    if old != -1 && self.sin[i] == -1 {
                        lost.push((i, old));
                    }
                }
                Row::Group(i) => {
                    if let Some(stick) = from.group_stick(&self.sin[i..i + 4]) {
                        // Keep each reversed axis reversed (relative to the
                        // layout's canonical group, whose Y is reversed).
                        let (canon_from, canon_to) = (from.group(stick), to.group(stick));
                        for j in 0..4 {
                            let reversed = is_axis(canon_from[j]) && (self.sin[i + j] < 0) != (canon_from[j] < 0);
                            self.sin[i + j] = if reversed && is_axis(canon_to[j]) { -canon_to[j] } else { canon_to[j] };
                        }
                    } else {
                        for j in i..i + 4 {
                            self.sin[j] = to.encode(from.decode(self.sin[j]));
                        }
                    }
                }
            }
        }
        lost
    }
    /// Puts back bindings a layout change unbound, where the slot is still
    /// unbound and no other function has taken that id since.
    fn restore(&mut self, slots: &[(usize, i32)]) {
        for &(slot, id) in slots {
            let taken = ROWS.iter().any(|row| matches!(*row, Row::Single(other) if other != slot && self.sin[other] == id));
            if self.sin[slot] == -1 && !taken {
                self.sin[slot] = id;
            }
        }
    }
    /// The shipped bindings, in the current input mode. Game options (the
    /// `padmode000` flags) are left as they are.
    pub(crate) fn reset(&mut self, di: Layout) {
        let mut fresh = PadConfig::default();
        if !self.flag(Flag::XInput) {
            fresh.translate(Layout::XInput, di);
        }
        self.sin[..SLOT_COUNT].copy_from_slice(&fresh.sin);
    }
    /// Binds a button to a single-slot function; a function that already had
    /// that button takes over this slot's previous one (a swap), so no
    /// button ends up doing two things by accident.
    pub(crate) fn bind_button(&mut self, slot: usize, id: i32) {
        let old = self.sin[slot];
        for row in ROWS {
            if let Row::Single(other) = row {
                if other != slot && self.sin[other] == id {
                    self.sin[other] = old;
                }
            }
        }
        self.sin[slot] = id;
    }
    /// Binds a direction group; another group on the same stick (whatever
    /// its axis reversal) takes this group's previous stick.
    pub(crate) fn bind_group(&mut self, first: usize, ids: [i32; 4], layout: Layout) {
        let old: [i32; 4] = self.sin[first..first + 4].try_into().unwrap_or([-1; 4]);
        let stick = layout.group_stick(&ids);
        for row in ROWS {
            if let Row::Group(other) = row {
                if other != first && stick.is_some() && layout.group_stick(&self.sin[other..other + 4]) == stick {
                    self.sin[other..other + 4].copy_from_slice(&old);
                }
            }
        }
        self.sin[first..first + 4].copy_from_slice(&ids);
    }
    /// How many visible settings differ (flags plus binding rows).
    pub(crate) fn changes(&self, other: &PadConfig) -> usize {
        let flags = [Flag::Enabled, Flag::ForceFeedback, Flag::Background, Flag::XInput]
            .into_iter()
            .filter(|f| self.flag(*f) != other.flag(*f))
            .count();
        let rows = ROWS
            .iter()
            .filter(|row| match **row {
                Row::Single(i) => self.sin[i] != other.sin[i],
                Row::Group(i) => self.sin[i..i + 4] != other.sin[i..i + 4],
            })
            .count();
        let hidden = (self.mode != other.mode || self.sin != other.sin) as usize;
        (flags + rows).max(hidden)
    }
    pub(crate) fn values(&self) -> [(&'static str, String); 2] {
        [
            ("padmode000", format_list(&self.mode)),
            ("padsin000", format_list(&self.sin)),
        ]
    }
    /// `base` changed to `edit` laid over `current`: every entry the user
    /// changed from `base` takes the edit, everything else keeps `current`
    /// (what another tool may have stored since `base` was read).
    pub(crate) fn rebased(current: &PadConfig, base: &PadConfig, edit: &PadConfig) -> PadConfig {
        let merge = |cur: &[i32], base: &[i32], edit: &[i32]| -> Vec<i32> {
            let mut out = cur.to_vec();
            out.resize(out.len().max(edit.len()), -1);
            for (i, e) in edit.iter().enumerate() {
                if base.get(i) != Some(e) {
                    out[i] = *e;
                }
            }
            out
        };
        PadConfig {
            mode: merge(&current.mode, &base.mode, &edit.mode),
            sin: merge(&current.sin, &base.sin, &edit.sin),
        }
    }
}

// ---- registry backups (.reg) --------------------------------------------------

/// UTF-16LE with a BOM: what `reg export` writes and `reg import` expects.
fn utf16le_bom(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in text.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

fn reg_line(name: &str, value: &str) -> String {
    format!("\"{name}\"=\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---- where the game keeps the values --------------------------------------------

pub(crate) struct Loaded {
    pub config: PadConfig,
    /// Where the values were read from, for the UI.
    pub source: String,
}

fn backup_dir(config: &crate::backend::Config) -> PathBuf {
    config.experiment.join("iced-launcher/gamepad-backup")
}

/// Writes `bytes` to `path` only if it does not exist yet: the first backup is
/// the original and is never replaced. The file appears complete or not at
/// all, so a failed write never leaves a truncated "original" behind.
fn backup_once(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| tf("Could not create the backup folder: {error}", &[("error", &e)]))?;
    }
    let id = crate::backend::unique_id().map_err(|e| tf("Could not write the backup: {error}", &[("error", &e)]))?;
    let temp = path.with_extension(format!("{id}.tmp"));
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()))
        // A hard link never replaces an existing file (another save may have
        // made the backup meanwhile: that one is the original).
        .and_then(|_| match std::fs::hard_link(&temp, path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            // No hard links here (FAT32, exFAT, some shares): write the
            // backup in place, still never replacing an existing one.
            Err(_) => match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut f) => {
                    let written = f.write_all(bytes).and_then(|_| f.sync_all());
                    drop(f);
                    if written.is_err() {
                        // Never leave a truncated "original" behind.
                        let _ = std::fs::remove_file(path);
                    }
                    written
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e),
            },
            Ok(()) => Ok(()),
        });
    let _ = std::fs::remove_file(&temp);
    result.map_err(|e| tf("Could not write the backup: {error}", &[("error", &e)]))
}

fn text_value(values: &crate::game_config::RegValues, name: &str) -> Option<String> {
    match values.get(name) {
        Some(crate::game_config::RegData::Text(v)) => Some(v.clone()),
        _ => None,
    }
}

fn key_location(vendor: &str) -> String {
    format!("HKLM\\SOFTWARE\\WOW6432Node\\{vendor}\\{}\\FinalFantasyXI", crate::win::reg::publisher(vendor))
}

pub(crate) fn load_retail() -> Loaded {
    if let Some(vendor) = crate::game_config::controller_region() {
        let values = crate::game_config::read_retail(vendor).ok().flatten().unwrap_or_default();
        let (mode, sin) = (text_value(&values, "padmode000"), text_value(&values, "padsin000"));
        return Loaded {
            config: PadConfig::from_values(mode.as_deref(), sin.as_deref()),
            source: key_location(vendor),
        };
    }
    Loaded {
        config: PadConfig::default(),
        source: t("FINAL FANTASY XI is not installed (no registry key)").into(),
    }
}

/// Writes `edit` to the registry, backing up the originals first. Values
/// another tool stored since `base` was read are kept where the user did not
/// change them; returns what was written.
pub(crate) fn save_retail(config: &crate::backend::Config, base: &PadConfig, edit: &PadConfig) -> Result<(PadConfig, String), String> {
    use crate::game_config::RegData as Data;
    let vendor = crate::game_config::controller_region().ok_or(t("FINAL FANTASY XI's registry key was not found."))?;
    let stored = crate::game_config::read_retail(vendor)?.unwrap_or_default();
    let mut backup = format!(
        "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\{vendor}\\{}\\FinalFantasyXI]\r\n",
        crate::win::reg::publisher(vendor)
    );
    for name in ["padmode000", "padsin000", "padguid000", "padexsin000"] {
        if let Some(v) = text_value(&stored, name) {
            backup.push_str(&reg_line(name, &v));
            backup.push_str("\r\n");
        }
    }
    backup_once(&backup_dir(config).join(format!("registry-{vendor}.reg")), &utf16le_bom(&backup))?;
    let current = PadConfig::from_values(text_value(&stored, "padmode000").as_deref(), text_value(&stored, "padsin000").as_deref());
    let merged = if current == *base { edit.clone() } else { PadConfig::rebased(&current, base, edit) };
    let values: Vec<(&str, Data)> = merged.values().into_iter().map(|(name, v)| (name, Data::Text(v))).collect();
    let changes: Vec<(&str, &Data)> = values.iter().map(|(name, d)| (*name, d)).collect();
    crate::game_config::write_retail(vendor, &changes)?;
    Ok((
        merged,
        tf(
            "Saved to the registry. The original values are backed up in {path}.",
            &[("path", &backup_dir(config).display())],
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_unknown_axis_ids_remain_unbound_to_a_direction_group() {
        for layout in [Layout::XInput, Layout::DInputXbox, Layout::DInputSony] {
            assert_eq!(layout.group_stick(&[i32::MIN; 4]), None);
            assert_eq!(layout.group_stick(&[-1; 4]), None);
            let reversed: Vec<_> = layout.group(Stick::Left).into_iter().map(|axis| -axis).collect();
            assert_eq!(layout.group_stick(&reversed), Some(Stick::Left));
        }
    }

    #[test]
    fn gamepad_defaults_decode_to_the_xbox_layout() {
        let l = Layout::XInput;
        let d = DEFAULT_XINPUT_SIN;
        assert_eq!(l.decode(d[7]), Control::Button(Btn::South)); // confirm: A
        assert_eq!(l.decode(d[5]), Control::Button(Btn::East)); // cancel: B
        assert_eq!(l.decode(d[6]), Control::Button(Btn::West)); // main menu: X
        assert_eq!(l.decode(d[8]), Control::Button(Btn::North)); // active window: Y
        assert_eq!(l.group_stick(&d[13..17]), Some(Stick::Left));
        assert_eq!(l.group_stick(&d[17..21]), Some(Stick::Right));
        assert_eq!(l.group_stick(&d[21..25]), Some(Stick::Dpad));
        assert_eq!(l.decode(-33), Control::Axis(Stick::Left, AxisDir::Y, true));
        assert_eq!(l.decode(16), Control::None);
    }

    #[test]
    fn gamepad_ps5_extra_bindings_round_trip_in_saved_directinput_values() {
        let mut config = PadConfig::default();
        config.set_flag(Flag::XInput, false, Layout::DInputSony, &mut Unbound::default());
        config.bind_button(6, Layout::DInputSony.encode_button(Btn::Touchpad).unwrap());
        config.bind_button(8, Layout::DInputSony.encode_button(Btn::Mute).unwrap());
        let values = config.values();
        let loaded = PadConfig::from_values(Some(&values[0].1), Some(&values[1].1));
        assert!(!loaded.flag(Flag::XInput));
        assert_eq!(loaded.sin[6], 13);
        assert_eq!(loaded.sin[8], 14);
        assert_eq!(Layout::DInputSony.decode(loaded.sin[6]), Control::Button(Btn::Touchpad));
        assert_eq!(Layout::DInputSony.decode(loaded.sin[8]), Control::Button(Btn::Mute));
        assert_eq!(Layout::XInput.encode_button(Btn::Touchpad), None);
        assert_eq!(Layout::XInput.encode_button(Btn::Mute), None);
    }

    #[test]
    fn gamepad_lists_round_trip_and_keep_their_length() {
        let real = "-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1";
        let c = PadConfig::from_values(Some("0,0,0,0,0,0"), Some(real));
        assert_eq!(c.sin.len(), 32);
        assert_eq!(format_list(&c.sin), real);
        assert_eq!(c.values()[0].1, "0,0,0,0,0,0");
        let short = PadConfig::from_values(Some("1,1"), Some("8,9"));
        assert_eq!(short.mode.len(), MODE_COUNT);
        assert_eq!(short.sin.len(), SLOT_COUNT);
        assert_eq!(PadConfig::from_values(Some("x"), None).mode, DEFAULT_MODE.to_vec());
    }

    #[test]
    fn gamepad_xinput_toggle_keeps_each_function_on_its_button() {
        let mut c = PadConfig::default();
        let mut unbound = Unbound::default();
        c.set_flag(Flag::XInput, false, Layout::DInputSony, &mut unbound);
        let l = Layout::DInputSony;
        assert_eq!(l.decode(c.sin[7]), Control::Button(Btn::South));
        assert_eq!(c.sin[7], 1); // DirectInput cross
        assert_eq!(l.group_stick(&c.sin[17..21]), Some(Stick::Right));
        assert_eq!(&c.sin[17..21], &[-37, -37, 34, 34]);
        assert_eq!(&c.sin[21..25], &[-41, -41, 40, 40]);
        c.set_flag(Flag::XInput, true, Layout::DInputSony, &mut unbound);
        // Sony has L2/R2, so nothing was lost.
        assert_eq!(c.sin[..SLOT_COUNT], DEFAULT_XINPUT_SIN);
    }

    /// Xbox pads have no DirectInput trigger buttons: turning XInput off and
    /// on again must give the LT/RT macro palettes back.
    #[test]
    fn gamepad_xinput_round_trip_on_an_xbox_pad_keeps_the_trigger_bindings() {
        let mut c = PadConfig::default();
        let mut unbound = Unbound::default();
        c.set_flag(Flag::XInput, false, Layout::DInputXbox, &mut unbound);
        assert_eq!((c.sin[1], c.sin[3]), (-1, -1));
        c.set_flag(Flag::XInput, true, Layout::DInputXbox, &mut unbound);
        assert_eq!(c.sin[..SLOT_COUNT], DEFAULT_XINPUT_SIN);
        // A binding made in between keeps its button; the old one is not forced back.
        c.set_flag(Flag::XInput, false, Layout::DInputXbox, &mut unbound);
        c.bind_button(1, 0);
        c.set_flag(Flag::XInput, true, Layout::DInputXbox, &mut unbound);
        assert_ne!(c.sin[1], 9);
        // Sony's touchpad survives an XInput round trip too.
        let mut s = PadConfig::default();
        let mut unbound = Unbound::default();
        s.set_flag(Flag::XInput, false, Layout::DInputSony, &mut unbound);
        s.bind_button(6, 13);
        s.set_flag(Flag::XInput, true, Layout::DInputSony, &mut unbound);
        assert_eq!(s.sin[6], -1);
        s.set_flag(Flag::XInput, false, Layout::DInputSony, &mut unbound);
        assert_eq!(s.sin[6], 13);
    }

    #[test]
    fn gamepad_xinput_toggle_keeps_reversed_axes() {
        let mut c = PadConfig::default();
        c.sin[17..21].copy_from_slice(&[36, 36, 35, 35]); // camera Y not reversed
        let mut unbound = Unbound::default();
        c.set_flag(Flag::XInput, false, Layout::DInputXbox, &mut unbound);
        assert_eq!(&c.sin[17..21], &[36, 36, 35, 35]);
        c.set_flag(Flag::XInput, false, Layout::DInputSony, &mut unbound);
        c.set_flag(Flag::XInput, true, Layout::DInputXbox, &mut unbound);
        c.set_flag(Flag::XInput, false, Layout::DInputSony, &mut unbound);
        assert_eq!(&c.sin[17..21], &[37, 37, 34, 34]);
        c.set_flag(Flag::XInput, true, Layout::DInputSony, &mut unbound);
        assert_eq!(&c.sin[17..21], &[36, 36, 35, 35]);
    }

    #[test]
    fn gamepad_binding_a_used_button_swaps() {
        let mut c = PadConfig::default();
        c.bind_button(7, 0); // confirm := B (was cancel's)
        assert_eq!(c.sin[7], 0);
        assert_eq!(c.sin[5], 3); // cancel took A
        c.bind_group(13, Layout::XInput.group(Stick::Right), Layout::XInput);
        assert_eq!(Layout::XInput.group_stick(&c.sin[17..21]), Some(Stick::Left));
        let d = PadConfig::default();
        assert_eq!(c.changes(&d), 4);
        c.reset(Layout::DInputXbox);
        assert_eq!(c.changes(&d), 0);
    }

    /// A group on the same stick with other axis reversal still swaps, so two
    /// functions never end up on one stick.
    #[test]
    fn gamepad_group_swap_finds_a_reversed_group_on_the_same_stick() {
        let mut c = PadConfig::default();
        c.sin[17..21].copy_from_slice(&[36, 36, 35, 35]);
        c.bind_group(13, Layout::XInput.group(Stick::Right), Layout::XInput);
        assert_eq!(Layout::XInput.group_stick(&c.sin[13..17]), Some(Stick::Right));
        assert_eq!(Layout::XInput.group_stick(&c.sin[17..21]), Some(Stick::Left));
    }

    #[test]
    fn gamepad_reset_keeps_the_game_options() {
        let mut c = PadConfig::default();
        c.mode[Flag::Background as usize] = 1;
        c.mode[Flag::ForceFeedback as usize] = 0;
        c.mode[Flag::Enabled as usize] = 0;
        c.bind_button(7, 0);
        c.reset(Layout::DInputXbox);
        assert_eq!(c.sin[..SLOT_COUNT], DEFAULT_XINPUT_SIN);
        assert!(c.flag(Flag::Background));
        assert!(!c.flag(Flag::ForceFeedback));
        assert!(!c.flag(Flag::Enabled));
    }

    #[test]
    fn gamepad_saves_merge_over_values_changed_outside_the_launcher() {
        let base = PadConfig::default();
        let mut edit = base.clone();
        edit.mode[Flag::ForceFeedback as usize] = 0;
        let mut current = base.clone();
        current.bind_button(7, 0); // rebinding in the official Config tool
        let merged = PadConfig::rebased(&current, &base, &edit);
        assert_eq!(merged.sin, current.sin);
        assert!(!merged.flag(Flag::ForceFeedback));
    }

    #[test]
    fn gamepad_backup_is_utf16_and_never_replaced_or_truncated() {
        let bytes = utf16le_bom("Windows Registry Editor Version 5.00\r\n\"padsin000\"=\"8,9\"\r\n");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let units: Vec<u16> = bytes[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert!(String::from_utf16(&units).unwrap().contains("\"padsin000\"=\"8,9\""));
        let dir = std::env::temp_dir().join(format!("crystal-pad-backup-{}", crate::backend::unique_id().unwrap()));
        let path = dir.join("registry-PlayOnlineUS.reg");
        backup_once(&path, b"original").unwrap();
        backup_once(&path, b"later").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
