//! Client switching hotkeys: one to cycle through the running game windows (and one to cycle
//! back), plus optional per-account hotkeys that bring that account's game forward. None is
//! set until the user sets it.
//!
//! They are registered with RegisterHotKey on the desktop thread's message-only window
//! (`desktop`): Windows delivers only these chords to Crystal, and a chord another program
//! already owns is reported back as a conflict instead of being stolen. The in-game hotkeys
//! (`in_game_only`) are registered only while a Crystal-launched game window is in the
//! foreground (`Gate`), so their keys stay with every other program. For that same span a
//! low-level keyboard hook (`keyhook`) also takes exactly those chords, because a game reading
//! the keyboard through DirectInput keeps registered hotkeys from arriving; it passes every
//! other key on and records none. One press is one switch, however long the key is held
//! (`keyhook::Matcher`); each press leaves its result in the launch history, and no window's
//! z-order is ever changed. While a game is in front the hook takes the hotkeys that work
//! everywhere too: FFXI's DirectInput keeps every registered hotkey from arriving.
//!
//! What a chord may be (owner decision, 2026-11-11): any key or combination, bare keys and
//! Shift+key included. Refused are only the chords Windows makes impossible (`Hotkey::problem`):
//! the ones it keeps for itself, and Shift with a number pad digit. Which hotkeys are in-game
//! only (`in_game_only`): the cycle keys, and any hotkey whose chord has no Ctrl, Alt or Win
//! (`Hotkey::game_only`), so a bare key is never taken from another program's typing. A chord
//! with Ctrl, Alt or Win on an account's hotkey works everywhere. What else to know about a
//! chord is said without refusing it (`Hotkey::warnings`).
//!
//! The number pad: its keys are chords of their own (`Num0`, `NumEnter`), told from the digits,
//! the arrows and the main Enter by scan code (`canonical`), so the hook matches them whatever
//! Num Lock says. RegisterHotKey knows only virtual keys: outside a game a number pad digit
//! needs Num Lock on, Shift with a number pad digit is refused (Windows releases Shift to type
//! it), and a chord on the number pad's Enter is an in-game hotkey (`registration`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const MOD_ALT: u8 = 1;
pub(crate) const MOD_CONTROL: u8 = 2;
pub(crate) const MOD_SHIFT: u8 = 4;
pub(crate) const MOD_WIN: u8 = 8;

/// Set on a key for the extended-key twin of a virtual key (`KF_EXTENDED`): only the number
/// pad's Enter, which Windows gives VK_RETURN like the main one.
pub(crate) const EXTENDED: u16 = 0x100;
/// The number pad's Enter.
pub(crate) const NUM_ENTER: u16 = 0x0D | EXTENDED;

/// A chord: modifiers (`MOD_*`, the RegisterHotKey bits) and a key: a virtual-key code, or
/// `NUM_ENTER`. Saved as text, e.g. `"Ctrl+Alt+F1"`, `"Ctrl+Num0"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Hotkey {
    pub modifiers: u8,
    pub key: u16,
}

impl Serialize for Hotkey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(&self.to_string()) }
}
impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Hotkey::parse(&text).ok_or_else(|| serde::de::Error::custom("not a hotkey"))
    }
}

/// Key names and their codes (letters and digits are their own codes; a key with no name is
/// written as its code, `0xAD`).
const NAMES: &[(&str, u16)] = &[
    ("Backspace", 0x08), ("Tab", 0x09), ("Enter", 0x0D), ("Pause", 0x13), ("Esc", 0x1B), ("Space", 0x20),
    ("PageUp", 0x21), ("PageDown", 0x22), ("End", 0x23), ("Home", 0x24), ("Left", 0x25), ("Up", 0x26),
    ("Right", 0x27), ("Down", 0x28), ("PrintScreen", 0x2C), ("Insert", 0x2D), ("Delete", 0x2E), ("Menu", 0x5D),
    ("Num0", 0x60), ("Num1", 0x61), ("Num2", 0x62), ("Num3", 0x63), ("Num4", 0x64), ("Num5", 0x65),
    ("Num6", 0x66), ("Num7", 0x67), ("Num8", 0x68), ("Num9", 0x69),
    ("Num*", 0x6A), ("Num+", 0x6B), ("Num-", 0x6D), ("Num.", 0x6E), ("Num/", 0x6F), ("NumEnter", NUM_ENTER),
    ("ScrollLock", 0x91),
    (";", 0xBA), ("=", 0xBB), (",", 0xBC), ("-", 0xBD), (".", 0xBE), ("/", 0xBF), ("`", 0xC0),
    ("[", 0xDB), ("\\", 0xDC), ("]", 0xDD), ("'", 0xDE),
];

/// The number pad's digits and decimal point: (scan code, the key's code with Num Lock off,
/// its code with Num Lock on).
const PAD: [(u16, u16, u16); 11] = [
    (0x52, 0x2D, 0x60), (0x4F, 0x23, 0x61), (0x50, 0x28, 0x62), (0x51, 0x22, 0x63), (0x4B, 0x25, 0x64), (0x4C, 0x0C, 0x65),
    (0x4D, 0x27, 0x66), (0x47, 0x24, 0x67), (0x48, 0x26, 0x68), (0x49, 0x21, 0x69), (0x53, 0x2E, 0x6E),
];

/// The key a keyboard event is, for a chord: its virtual key, except that the number pad's
/// keys are themselves whatever Num Lock (or Shift) makes Windows call them. A number pad digit
/// pressed with Num Lock off arrives as Insert, End, an arrow and so on with the number pad's
/// scan code and no extended-key flag (the keys those names belong to carry the flag); the
/// number pad's Enter arrives as Enter with the flag.
pub(crate) fn canonical(vk: u16, scan: u16, extended: bool) -> u16 {
    if extended { return if vk == 0x0D { NUM_ENTER } else { vk }; }
    PAD.iter().find(|(s, off, on)| *s == scan && (vk == *off || vk == *on)).map_or(vk, |(_, _, on)| *on)
}

/// The virtual keys Windows may report `key` as being down under: its own, and for a number
/// pad digit the key it doubles as with Num Lock off.
pub(crate) fn virtual_keys(key: u16) -> [u16; 2] {
    let vk = key & 0xFF;
    [vk, PAD.iter().find(|(_, _, on)| *on == vk).map_or(vk, |(_, off, _)| *off)]
}

fn pad_digit(key: u16) -> bool { matches!(key, 0x60..=0x69 | 0x6E) }
fn pad_key(key: u16) -> bool { pad_digit(key) || matches!(key, 0x6A | 0x6B | 0x6D | 0x6F) || key == NUM_ENTER }

impl Hotkey {
    pub(crate) const fn new(modifiers: u8, key: u16) -> Self { Self { modifiers, key } }

    /// `"Ctrl+Alt+F1"`-style text; modifiers in any order, names ignore case and spaces
    /// (`"Ctrl+Num 0"`, `"ctrl+numpad0"`).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut modifiers = 0u8;
        let mut key = None;
        for part in text.split('+') {
            let part: String = part.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
            let part = part.strip_prefix("numpad").map_or(part.clone(), |rest| format!("num{rest}"));
            // "Num+" splits into "Num" and "": put the plus back.
            let part = if part.is_empty() && key == Some(u16::MAX) { "num+" } else { part.as_str() };
            let bit = match part {
                "ctrl" | "control" => MOD_CONTROL,
                "alt" => MOD_ALT,
                "shift" => MOD_SHIFT,
                "win" | "windows" => MOD_WIN,
                _ => 0,
            };
            if bit != 0 {
                if key.is_some() || modifiers & bit != 0 { return None; }
                modifiers |= bit;
                continue;
            }
            if key.is_some_and(|k| k != u16::MAX) { return None; }
            if part == "num" { key = Some(u16::MAX); continue; }
            key = Some(key_code(part)?);
        }
        let key = key.filter(|k| *k != u16::MAX)?;
        Some(Self { modifiers, key })
    }

    /// The chord as the page shows it: the saved text with the number pad's keys spaced out
    /// (`"Ctrl+Num 0"`, `"Num Enter"`). `parse` reads it back.
    pub(crate) fn label(&self) -> String {
        let name = key_name(self.key);
        let name = match name.strip_prefix("Num") { Some(rest) => format!("Num {rest}"), None => name };
        format!("{}{name}", modifier_names(self.modifiers))
    }

    /// What RegisterHotKey is given for this chord: (modifiers, virtual key). None for the
    /// number pad's Enter: Windows would take the main Enter with it, so only the keyboard hook
    /// (which sees the extended-key flag) may take that chord.
    pub(crate) fn registration(&self) -> Option<(u32, u32)> {
        (self.key & EXTENDED == 0).then(|| (u32::from(self.modifiers), u32::from(self.key)))
    }

    /// Why this chord can't be a Crystal hotkey, if it can't: only what Windows makes
    /// impossible.
    /// - A chord Windows keeps for itself: the Windows key with no other modifier (Win+L, Win+D
    ///   and the rest are the shell's), Alt+Tab, Alt+Esc, Ctrl+Esc, Ctrl+Alt+Del and
    ///   Ctrl+Alt+Tab.
    /// - Shift with a number pad digit: Windows lets Shift go to type the digit's other key,
    ///   so the chord arrives without it.
    ///
    /// Every other key or combination is a hotkey, a bare key and Shift+key included.
    pub(crate) fn problem(&self) -> Option<Problem> {
        let (m, key) = (self.modifiers, self.key);
        let unshifted = m & !MOD_SHIFT;
        let reserved = (m & MOD_WIN != 0 && m & (MOD_CONTROL | MOD_ALT | MOD_SHIFT) == 0)
            || (unshifted == MOD_ALT && key == 0x09) // Alt+Tab, Alt+Shift+Tab
            || (m == MOD_ALT && key == 0x1B) // Alt+Esc
            || (m == MOD_CONTROL | MOD_ALT && matches!(key, 0x2E | 0x6E | 0x09)) // Ctrl+Alt+Del (either Del), Ctrl+Alt+Tab
            || (unshifted == MOD_CONTROL && key == 0x1B); // Ctrl+Esc, Ctrl+Shift+Esc
        if reserved { return Some(Problem::Reserved); }
        (m & MOD_SHIFT != 0 && pad_digit(key)).then_some(Problem::ShiftNumpad)
    }

    /// The chord makes its hotkey an in-game one, whatever the hotkey is for: it has no Ctrl,
    /// Alt or Win (a bare key or Shift+key, which every program types with), or it is on the
    /// number pad's Enter (RegisterHotKey can't tell that from the main one; the keyboard hook,
    /// which works over a game, can).
    pub(crate) fn game_only(&self) -> bool {
        self.modifiers & (MOD_CONTROL | MOD_ALT | MOD_WIN) == 0 || self.key == NUM_ENTER
    }

    /// What to know about a chord that works (`problem` is None); none of it stops the chord
    /// being the hotkey. `in_game_only`: the hotkey is taken only while a game is in front
    /// (`in_game_only`).
    /// - Ctrl or Alt with a top-row digit: the game's macro keys.
    /// - A letter, digit, punctuation key or Space with no Ctrl, Alt or Win: the game's chat
    ///   types with it, and the hotkey takes it first.
    /// - On a hotkey that works everywhere: a shortcut other programs commonly use (Ctrl or
    ///   Alt alone, with or without Shift, on a typing, editing or navigation key; Ctrl+F4,
    ///   Alt+F4), and a number pad digit (RegisterHotKey sees it only with Num Lock on).
    pub(crate) fn warnings(&self, in_game_only: bool) -> Vec<Warning> {
        let (m, key) = (self.modifiers, self.key);
        let unshifted = m & !MOD_SHIFT;
        let mut all = Vec::new();
        if (0x30..=0x39).contains(&key) && matches!(m, MOD_CONTROL | MOD_ALT) { all.push(Warning::GameMacro); }
        let typing = matches!(key, 0x20 | 0x30..=0x39 | 0x41..=0x5A | 0xBA..=0xC0 | 0xDB..=0xDE);
        if unshifted == 0 && typing { all.push(Warning::Chat); }
        if in_game_only { return all; }
        let single = unshifted == MOD_CONTROL || unshifted == MOD_ALT;
        let function = (0x70..=0x87).contains(&key);
        let special = function || pad_key(key) || matches!(key, 0x13 | 0x2C | 0x5D | 0x91) || !named(key);
        if single && (!special || key == 0x73) { all.push(Warning::Common); }
        if pad_digit(key) { all.push(Warning::NumLock); }
        all
    }
}

/// `"Ctrl+Alt+"` for the modifiers held.
pub(crate) fn modifier_names(modifiers: u8) -> String {
    [(MOD_CONTROL, "Ctrl+"), (MOD_ALT, "Alt+"), (MOD_SHIFT, "Shift+"), (MOD_WIN, "Win+")].iter()
        .filter(|(bit, _)| modifiers & bit != 0).map(|(_, name)| *name).collect()
}

/// `name` in lower case with no spaces.
fn key_code(name: &str) -> Option<u16> {
    let name = match name { "escape" => "esc", "return" => "enter", other => other };
    if let Some((_, code)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) { return Some(*code); }
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() { return Some(u16::from(bytes[0].to_ascii_uppercase())); }
    if let Some(hex) = name.strip_prefix("0x") {
        return u16::from_str_radix(hex, 16).ok().filter(|code| (1..=0xFE).contains(code) && !named(*code));
    }
    let number = name.strip_prefix('f')?.parse::<u16>().ok().filter(|n| (1..=24).contains(n))?;
    Some(0x6F + number) // VK_F1 = 0x70
}

/// Whether `code` has a name of its own (not written as `0xAD`).
fn named(code: u16) -> bool {
    NAMES.iter().any(|(_, c)| *c == code) || matches!(code, 0x30..=0x39 | 0x41..=0x5A | 0x70..=0x87)
}

fn key_name(code: u16) -> String {
    if let Some((name, _)) = NAMES.iter().find(|(_, c)| *c == code) { return (*name).to_owned(); }
    match code {
        0x30..=0x39 | 0x41..=0x5A => char::from(code as u8).to_string(),
        0x70..=0x87 => format!("F{}", code - 0x6F),
        _ => format!("0x{code:02X}"),
    }
}

impl std::fmt::Display for Hotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&modifier_names(self.modifiers))?;
        f.write_str(&key_name(self.key))
    }
}

/// The cycle chords Crystal set by itself up to 1.2.4: Ctrl+Tab forward, Ctrl+Shift+Tab back.
/// No hotkey is set by default now (owner decision, 2026-11-11: people choose their own); a
/// settings file from those versions has them cleared once (`Settings::clear_old_defaults`).
pub(crate) const OLD_CYCLE: Hotkey = Hotkey::new(MOD_CONTROL, 0x09);
pub(crate) const OLD_CYCLE_BACK: Hotkey = Hotkey::new(MOD_CONTROL | MOD_SHIFT, 0x09);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Bring the next game window forward, while a game window is in the foreground. None
    /// (always saved, as `null`: a Crystal up to 1.2.4 reads a missing field as Ctrl+Tab): off.
    #[serde(deserialize_with = "chord")]
    pub cycle: Option<Hotkey>,
    /// The previous one.
    #[serde(deserialize_with = "chord")]
    pub cycle_back: Option<Hotkey>,
    /// Bring this account's game window forward, by account key.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", deserialize_with = "chords")]
    pub accounts: BTreeMap<String, Hotkey>,
}

// A chord that does not read (a newer Crystal's, or a typo) is dropped on its own: the others,
// and every other feature's settings, load as they were.
fn chord<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Hotkey>, D::Error> {
    Ok(serde_json::Value::deserialize(d)?.as_str().and_then(Hotkey::parse))
}

fn chords<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Hotkey>, D::Error> {
    let all = BTreeMap::<String, serde_json::Value>::deserialize(d)?;
    Ok(all.into_iter().filter_map(|(key, v)| Some((key, Hotkey::parse(v.as_str()?)?))).collect())
}

impl Settings {
    /// A cycle key that is still the chord Crystal used to set by itself is cleared; one the
    /// user chose is kept. Whether anything was cleared.
    pub(crate) fn clear_old_defaults(&mut self) -> bool {
        let before = (self.cycle, self.cycle_back);
        if self.cycle == Some(OLD_CYCLE) { self.cycle = None; }
        if self.cycle_back == Some(OLD_CYCLE_BACK) { self.cycle_back = None; }
        before != (self.cycle, self.cycle_back)
    }
}

/// What a hotkey does.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Action {
    Next,
    Previous,
    Account(String),
}

impl Action {
    /// Whether the hotkey is an in-game one whatever its chord: the cycle keys (their chord
    /// stays with browsers and editors everywhere else).
    pub(crate) fn in_game_only(&self) -> bool { matches!(self, Self::Next | Self::Previous) }
}

/// Whether `action`'s hotkey is registered and taken only while a game window is in the
/// foreground: the cycle keys always, an account's hotkey when its chord has no Ctrl, Alt or
/// Win (`Hotkey::game_only`). Any other is registered for as long as it is set.
pub(crate) fn in_game_only(action: &Action, hotkey: Hotkey) -> bool { action.in_game_only() || hotkey.game_only() }

/// Why a hotkey is not active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// Windows keeps this chord for itself.
    Reserved,
    /// Shift with a number pad digit never arrives as pressed.
    ShiftNumpad,
    /// Another of Crystal's hotkeys has the same chord.
    Duplicate,
    /// Another program registered it first.
    InUse,
    /// RegisterHotKey failed for another reason.
    Failed(u32),
}

impl Problem {
    /// The chord itself can't be used (it is never registered); the others are what Windows
    /// answered when it was.
    pub(crate) fn refused(&self) -> bool { !matches!(self, Self::InUse | Self::Failed(_)) }

    pub(crate) fn text(&self) -> String {
        use crate::i18n::{t, tf};
        match self {
            Self::Reserved => t("Windows reserves this hotkey.").into(),
            Self::ShiftNumpad => t("Shift does not work with number pad digits.").into(),
            Self::Duplicate => t("Another Crystal hotkey uses this.").into(),
            Self::InUse => t("Another program already uses this hotkey.").into(),
            Self::Failed(code) => tf("Could not register this hotkey (error {code}).", &[("code", code)]),
        }
    }
}

/// What to know about a hotkey that works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Warning {
    /// Ctrl or Alt with a digit: the game's macro keys. The hotkey takes the key from them.
    GameMacro,
    /// A key the game's chat types with, with no Ctrl, Alt or Win: the hotkey takes it first,
    /// so chat can't type it.
    Chat,
    /// A shortcut other programs commonly use, on a hotkey that works outside the game: the
    /// hotkey takes it from them.
    Common,
    /// A number pad digit on a hotkey that works outside the game: RegisterHotKey sees it only
    /// with Num Lock on (the keyboard hook, over a game, sees it either way).
    NumLock,
}

impl Warning {
    pub(crate) fn text(&self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::GameMacro => t("FFXI uses this for macros."),
            Self::Chat => t("This key also types in FFXI chat."),
            Self::Common => t("Other programs use this shortcut. Try Ctrl+Alt, an F key or the number pad."),
            Self::NumLock => t("Outside the game this needs Num Lock on."),
        }
    }
}

/// One configured hotkey and whether it works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Status {
    pub action: Action,
    pub hotkey: Hotkey,
    pub problem: Option<Problem>,
}

/// The hotkeys to register, in a stable order, each with an id for WM_HOTKEY, and the ones that
/// can't be registered at all (invalid or duplicate) with their problem. Of two equal chords the
/// cycle keys win, then the account first in key order.
pub(crate) fn plan(settings: &Settings) -> (Vec<(i32, Action, Hotkey)>, Vec<Status>) {
    let mut wanted: Vec<(Action, Hotkey)> = Vec::new();
    if let Some(h) = settings.cycle { wanted.push((Action::Next, h)); }
    if let Some(h) = settings.cycle_back { wanted.push((Action::Previous, h)); }
    wanted.extend(settings.accounts.iter().map(|(k, h)| (Action::Account(k.clone()), *h)));
    let mut register = Vec::new();
    let mut refused = Vec::new();
    let mut taken: Vec<Hotkey> = Vec::new();
    for (action, hotkey) in wanted {
        let problem = hotkey.problem().or_else(|| taken.contains(&hotkey).then_some(Problem::Duplicate));
        match problem {
            Some(problem) => refused.push(Status { action, hotkey, problem: Some(problem) }),
            None => {
                taken.push(hotkey);
                register.push((register.len() as i32 + 1, action, hotkey));
            }
        }
    }
    (register, refused)
}

/// The account to bring forward for `action`: `order` lists the running clients' accounts in the
/// launcher's account order; `current` is the account whose game is in the foreground, if any.
/// Next/Previous from no game go to the first/last client.
pub(crate) fn target(action: &Action, order: &[String], current: Option<&str>) -> Option<String> {
    match action {
        Action::Account(key) => order.iter().any(|k| k == key).then(|| key.clone()),
        Action::Next | Action::Previous => {
            if order.is_empty() { return None; }
            let n = order.len();
            let at = current.and_then(|c| order.iter().position(|k| k == c));
            let index = match (action, at) {
                (Action::Next, Some(i)) => (i + 1) % n,
                (Action::Previous, Some(i)) => (i + n - 1) % n,
                (Action::Previous, None) => n - 1,
                _ => 0,
            };
            Some(order[index].clone())
        }
    }
}

/// A running game as the hotkeys see it, in Home order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub account: String,
    /// Its main window, when it has one now (none while it is still loading, or once closed).
    pub window: Option<isize>,
    pub minimized: bool,
}

/// The window a hotkey brings forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Switch {
    pub account: String,
    pub hwnd: isize,
    /// Its place among the running games in Home order, from 1 (for the history: never a name).
    pub index: usize,
    /// It is minimised: restore it first, as Alt+Tab does.
    pub restore: bool,
}

/// Where `action` goes: the cycle keys step through the games that have a window, in Home order
/// (`games`), from the one in front (`current`, an account); a game without a window is skipped.
/// An account hotkey goes to that account's window.
pub(crate) fn cycle(action: &Action, games: &[Candidate], current: Option<&str>) -> Option<Switch> {
    let open: Vec<String> = games.iter().filter(|g| g.window.is_some()).map(|g| g.account.clone()).collect();
    let account = target(action, &open, current)?;
    let index = games.iter().position(|g| g.account == account)?;
    let game = &games[index];
    Some(Switch { hwnd: game.window?, index: index + 1, restore: game.minimized, account })
}

/// Registers and releases hotkeys on Windows (the desktop thread), or a stand-in in tests.
pub(crate) trait Registrar {
    fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), u32>;
    fn unregister(&mut self, id: i32);
}

/// The in-game hotkeys' arm/disarm state: registered while a game window is in the foreground,
/// released the moment anything else is. A chord another program owns is reported (with the
/// Windows error) when arming finds it taken, and stays reported until an arm succeeds.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    keys: Vec<(i32, Hotkey)>,
    /// Ids registered now.
    armed: Vec<i32>,
    /// What was last reported for each key.
    results: Vec<(i32, Result<(), u32>)>,
}

impl Gate {
    pub(crate) fn armed(&self) -> bool { !self.armed.is_empty() }

    pub(crate) fn is_key(&self, id: i32) -> bool { self.keys.iter().any(|(i, _)| *i == id) }

    pub(crate) fn keys(&self) -> &[(i32, Hotkey)] { &self.keys }

    /// Each key's state: Ok while it works or waits for a game window, or the Windows error the
    /// last arm returned.
    pub(crate) fn results(&self) -> &[(i32, Result<(), u32>)] { &self.results }

    /// Replace the keys; `game` is whether a game window is in the foreground now. Always
    /// reports (the keys changed).
    pub(crate) fn set_keys(&mut self, keys: Vec<(i32, Hotkey)>, game: bool, registrar: &mut impl Registrar) {
        self.disarm(registrar);
        self.results = keys.iter().map(|(id, _)| (*id, Ok(()))).collect();
        self.keys = keys;
        if game { self.arm(registrar); }
    }

    /// The foreground changed: arm for a game window, disarm for anything else. Whether the
    /// reported results changed.
    pub(crate) fn focus(&mut self, game: bool, registrar: &mut impl Registrar) -> bool {
        match (game, self.armed()) {
            (true, false) => self.arm(registrar),
            (false, true) => { self.disarm(registrar); false }
            _ => false,
        }
    }

    fn arm(&mut self, registrar: &mut impl Registrar) -> bool {
        let results: Vec<_> = self.keys.iter().map(|(id, hotkey)| (*id, registrar.register(*id, *hotkey))).collect();
        self.armed = results.iter().filter(|(_, r)| r.is_ok()).map(|(id, _)| *id).collect();
        let changed = results != self.results;
        self.results = results;
        changed
    }

    pub(crate) fn disarm(&mut self, registrar: &mut impl Registrar) {
        for id in self.armed.drain(..) { registrar.unregister(id); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows as the gate sees it: chords another program owns, and what is registered now.
    #[derive(Default)]
    struct Fake { owned_elsewhere: Vec<Hotkey>, registered: Vec<(i32, Hotkey)>, calls: usize }
    impl Registrar for Fake {
        fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), u32> {
            self.calls += 1;
            if self.owned_elsewhere.contains(&hotkey) || self.registered.iter().any(|(_, h)| *h == hotkey) { return Err(1409); }
            self.registered.push((id, hotkey));
            Ok(())
        }
        fn unregister(&mut self, id: i32) { self.registered.retain(|(i, _)| *i != id); }
    }

    #[test]
    fn in_game_keys_are_registered_only_while_a_game_is_in_front() {
        let mut windows = Fake::default();
        let mut gate = Gate::default();
        let keys = vec![(1, OLD_CYCLE), (2, OLD_CYCLE_BACK)];
        // Set while a browser is in front: nothing registered, both reported as fine.
        gate.set_keys(keys.clone(), false, &mut windows);
        assert!(!gate.armed() && windows.registered.is_empty());
        assert_eq!(gate.results(), &[(1, Ok(())), (2, Ok(()))]);
        // A game comes to the front: both registered; nothing new to report.
        assert!(!gate.focus(true, &mut windows));
        assert_eq!(windows.registered, keys);
        // Game to game: no churn.
        let calls = windows.calls;
        assert!(!gate.focus(true, &mut windows));
        assert_eq!(windows.calls, calls);
        // Anything else in front: released at once, so Ctrl+Tab reaches it.
        assert!(!gate.focus(false, &mut windows));
        assert!(!gate.armed() && windows.registered.is_empty());
        // Changing the keys while a game is in front re-registers them.
        gate.focus(true, &mut windows);
        gate.set_keys(vec![(1, Hotkey::new(MOD_CONTROL | MOD_ALT, 0x27))], true, &mut windows);
        assert_eq!(windows.registered, vec![(1, Hotkey::new(MOD_CONTROL | MOD_ALT, 0x27))]);
        gate.set_keys(vec![], true, &mut windows);
        assert!(!gate.armed() && windows.registered.is_empty());
    }

    #[test]
    fn a_chord_another_program_owns_is_reported_until_it_is_free() {
        let mut windows = Fake { owned_elsewhere: vec![OLD_CYCLE], ..Fake::default() };
        let mut gate = Gate::default();
        gate.set_keys(vec![(1, OLD_CYCLE), (2, OLD_CYCLE_BACK)], false, &mut windows);
        // The first arm finds Ctrl+Tab taken: a change to report.
        assert!(gate.focus(true, &mut windows));
        assert_eq!(gate.results(), &[(1, Err(1409)), (2, Ok(()))]);
        assert!(gate.armed(), "the free chord still works");
        // Leaving and coming back finds the same: nothing new to report.
        gate.focus(false, &mut windows);
        assert!(windows.registered.is_empty());
        assert!(!gate.focus(true, &mut windows));
        // The other program let it go: the next arm takes it and reports the change.
        gate.focus(false, &mut windows);
        windows.owned_elsewhere.clear();
        assert!(gate.focus(true, &mut windows));
        assert_eq!(gate.results(), &[(1, Ok(())), (2, Ok(()))]);
        // Disarm releases everything (the thread ending).
        gate.disarm(&mut windows);
        assert!(windows.registered.is_empty());
    }

    #[test]
    fn hotkeys_parse_and_print_the_same_way() {
        let h = Hotkey::parse("ctrl+alt+f1").unwrap();
        assert_eq!(h, Hotkey::new(MOD_CONTROL | MOD_ALT, 0x70));
        assert_eq!(h.to_string(), "Ctrl+Alt+F1");
        for text in ["Ctrl+Shift+Tab", "Alt+Num+", "Win+Ctrl+Right", "Ctrl+`", "Ctrl+Alt+3", "F13", "Alt+PageDown"] {
            let parsed = Hotkey::parse(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(Hotkey::parse(&parsed.to_string()), Some(parsed), "{text}");
        }
        assert_eq!(Hotkey::parse("Shift+Ctrl+A").unwrap().to_string(), "Ctrl+Shift+A");
        for bad in ["", "Ctrl", "Ctrl+Ctrl+A", "Ctrl+A+B", "Ctrl+F25", "Ctrl+Nope", "A+Ctrl"] {
            assert_eq!(Hotkey::parse(bad), None, "{bad}");
        }
        // Saved as text.
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, "\"Ctrl+Alt+F1\"");
        assert_eq!(serde_json::from_str::<Hotkey>(&json).unwrap(), h);
    }

    fn hotkey(text: &str) -> Hotkey { Hotkey::parse(text).unwrap_or_else(|| panic!("{text}")) }

    fn problem(text: &str) -> Option<Problem> { hotkey(text).problem() }

    #[test]
    fn only_the_chords_windows_makes_impossible_are_refused() {
        for text in ["Alt+Tab", "Alt+Shift+Tab", "Alt+Esc", "Win+L", "Win+D", "Win+Num1", "Win+F5", "Ctrl+Alt+Delete", "Ctrl+Alt+Num.",
            "Ctrl+Alt+Tab", "Ctrl+Esc", "Ctrl+Shift+Esc"] {
            assert_eq!(problem(text), Some(Problem::Reserved), "{text}");
        }
        // The Windows key with another modifier is nobody's.
        assert_eq!(problem("Win+Alt+1"), None);
        assert_eq!(problem("Win+Ctrl+Num0"), None);
        assert_eq!(problem("Win+Shift+S"), None);
        // Windows lets Shift go to type a number pad digit's other key: the chord never arrives.
        for text in ["Shift+Num1", "Ctrl+Shift+Num0", "Alt+Shift+Num."] {
            assert_eq!(problem(text), Some(Problem::ShiftNumpad), "{text}");
        }
        assert_eq!(problem("Ctrl+Shift+Num+"), None, "the operators are the same key either way");
        assert!(Problem::Reserved.refused() && Problem::Duplicate.refused() && Problem::ShiftNumpad.refused());
        assert!(!Problem::InUse.refused() && !Problem::Failed(5).refused());
    }

    #[test]
    fn any_other_key_or_combination_is_a_hotkey() {
        // Bare keys and Shift+key: letters, digits, punctuation, editing and navigation keys,
        // function keys, the number pad.
        for text in ["A", "1", "`", ";", "Space", "Enter", "Tab", "Esc", "Left", "Home", "Insert", "Delete", "Backspace", "Shift+A", "Shift+Tab",
            "Shift+Right", "F1", "F5", "F12", "F13", "Shift+F5", "Shift+F24", "Num0", "Num9", "Num.", "Num+", "Num-", "Num*", "Num/", "NumEnter",
            "Pause", "ScrollLock", "0xAD"] {
            assert_eq!(problem(text), None, "{text}");
            assert!(hotkey(text).game_only(), "{text}: no Ctrl, Alt or Win");
        }
        // With Ctrl, Alt or Win: the shortcuts other programs use among them.
        for text in ["Ctrl+C", "Ctrl+V", "Ctrl+1", "Ctrl+Tab", "Ctrl+Shift+Tab", "Alt+D", "Alt+Left", "Alt+Enter", "Alt+Space", "Alt+F4", "Ctrl+F4",
            "Ctrl+Home", "Ctrl+Delete", "Ctrl+Alt+1", "Ctrl+Alt+Right", "Ctrl+Alt+Shift+A", "Ctrl+F1", "Alt+F5", "Ctrl+Num1", "Alt+Num+",
            "Ctrl+Alt+Num/", "Ctrl+Pause", "Alt+PrintScreen", "Ctrl+0xAD", "Ctrl+Win+A"] {
            assert_eq!(problem(text), None, "{text}");
            assert!(!hotkey(text).game_only(), "{text}");
        }
    }

    #[test]
    fn a_chord_with_no_ctrl_alt_or_win_makes_any_hotkey_an_in_game_one() {
        let account = Action::Account("k".into());
        // The cycle keys are in-game hotkeys whatever their chord.
        for text in ["Ctrl+Tab", "Ctrl+Alt+Right", "F5", "A", "Num0"] {
            assert!(in_game_only(&Action::Next, hotkey(text)) && in_game_only(&Action::Previous, hotkey(text)), "{text}");
        }
        // An account's hotkey works everywhere with Ctrl, Alt or Win...
        for text in ["Ctrl+Alt+1", "Ctrl+F1", "Alt+Num5", "Win+Shift+S", "Ctrl+Shift+A"] {
            assert!(!in_game_only(&account, hotkey(text)), "{text}");
        }
        // ...and only over a game without: a bare key or Shift+key is never taken from typing
        // in another program.
        for text in ["A", "Shift+A", "1", "`", "F5", "Shift+F5", "Num0", "Space", "Tab", "F13"] {
            assert!(in_game_only(&account, hotkey(text)), "{text}");
        }
        // The number pad's Enter: RegisterHotKey can't tell it from the main one, so a chord on
        // it is taken by the keyboard hook, over a game.
        assert!(in_game_only(&account, hotkey("Ctrl+NumEnter")));
        assert!(!in_game_only(&account, hotkey("Ctrl+Enter")));
        assert!(Action::Next.in_game_only() && Action::Previous.in_game_only() && !account.in_game_only());
    }

    #[test]
    fn the_number_pad_is_registered_under_its_own_keys() {
        assert_eq!(problem("Ctrl+NumEnter"), None);
        assert_eq!(hotkey("Ctrl+NumEnter").registration(), None);
        assert_eq!(hotkey("NumEnter").registration(), None);
        // Every other chord is registered under its own virtual key: a recorded Num 0 is
        // VK_NUMPAD0, never the top row's 0 or Insert.
        assert_eq!(hotkey("Ctrl+Num0").registration(), Some((u32::from(MOD_CONTROL), 0x60)));
        assert_eq!(hotkey("Ctrl+0").registration(), Some((u32::from(MOD_CONTROL), 0x30)));
        assert_eq!(hotkey("Ctrl+Insert").registration(), Some((u32::from(MOD_CONTROL), 0x2D)));
        assert_eq!(hotkey("Ctrl+Alt+Enter").registration(), Some((u32::from(MOD_CONTROL | MOD_ALT), 0x0D)));
        assert_eq!(hotkey("Num0").registration(), Some((0, 0x60)));
        assert_eq!(hotkey("A").registration(), Some((0, 0x41)));
        assert_eq!(hotkey("Shift+F5").registration(), Some((u32::from(MOD_SHIFT), 0x74)));
    }

    #[test]
    fn a_working_chord_says_what_it_clashes_with() {
        let warnings = |text: &str, in_game: bool| hotkey(text).warnings(in_game);
        // The game's macro keys: Ctrl or Alt with a top-row digit, on any hotkey (every hotkey
        // is taken over a game).
        for text in ["Ctrl+1", "Alt+0", "Ctrl+9"] {
            assert_eq!(warnings(text, true), vec![Warning::GameMacro], "{text}");
            assert_eq!(warnings(text, false), vec![Warning::GameMacro, Warning::Common], "{text}");
        }
        // A key chat types with, with no Ctrl, Alt or Win: the hotkey takes it from chat.
        for text in ["A", "Z", "1", "0", "`", ";", "/", "[", "'", "Space", "Shift+A", "Shift+1"] {
            assert_eq!(warnings(text, true), vec![Warning::Chat], "{text}");
        }
        for text in ["Ctrl+Alt+1", "Ctrl+Shift+1", "Ctrl+Num1", "Ctrl+Tab", "F5", "Shift+F5", "Num0", "Num.", "Tab", "Enter", "Left", "Insert", "NumEnter", "Ctrl+NumEnter"] {
            assert_eq!(warnings(text, true), vec![], "{text}");
        }
        // On a hotkey that works everywhere: a shortcut other programs use (Ctrl or Alt alone,
        // with or without Shift, on a typing, editing or navigation key; Ctrl+F4, Alt+F4).
        for text in ["Ctrl+C", "Ctrl+V", "Ctrl+Z", "Ctrl+S", "Ctrl+W", "Ctrl+Tab", "Ctrl+Shift+Tab", "Ctrl+Shift+T", "Alt+D", "Alt+Left", "Alt+Enter",
            "Alt+Space", "Ctrl+Home", "Ctrl+Backspace", "Ctrl+Delete", "Ctrl+Insert", "Ctrl+Enter", "Ctrl+=", "Ctrl+-", "Ctrl+PageDown", "Alt+PageDown",
            "Ctrl+F4", "Ctrl+Shift+F4", "Alt+F4"] {
            assert_eq!(warnings(text, false), vec![Warning::Common], "{text}");
            assert_eq!(warnings(text, true), vec![], "{text}: over a game there is no other program");
        }
        // Two of Ctrl, Alt and Win; a function key; a key no program types with.
        for text in ["Ctrl+Alt+1", "Ctrl+Alt+Right", "Ctrl+Alt+Shift+A", "Win+Shift+S", "Ctrl+F1", "Alt+F5", "Ctrl+Shift+F12", "Ctrl+Num+", "Alt+Num+",
            "Ctrl+Pause", "Ctrl+ScrollLock", "Alt+PrintScreen", "Ctrl+0xAD"] {
            assert_eq!(warnings(text, false), vec![], "{text}");
        }
        // Outside the game a number pad digit is registered as VK_NUMPADn: Num Lock must be on.
        for text in ["Ctrl+Num0", "Ctrl+Alt+Num5", "Alt+Num."] { assert_eq!(warnings(text, false), vec![Warning::NumLock], "{text}"); }
    }

    #[test]
    fn number_pad_keys_are_told_by_scan_code_whatever_num_lock_says() {
        // (virtual key, scan code, extended flag) as the keyboard hook reports them.
        // Num Lock on, then off: the same key, the same chord.
        for (digit, scan, off) in [(0u16, 0x52, 0x2D), (1, 0x4F, 0x23), (2, 0x50, 0x28), (3, 0x51, 0x22), (4, 0x4B, 0x25), (5, 0x4C, 0x0C),
            (6, 0x4D, 0x27), (7, 0x47, 0x24), (8, 0x48, 0x26), (9, 0x49, 0x21)] {
            assert_eq!(canonical(0x60 + digit, scan, false), 0x60 + digit, "Num{digit}, Num Lock on");
            assert_eq!(canonical(off, scan, false), 0x60 + digit, "Num{digit}, Num Lock off");
            // The key that name belongs to (the arrows, Insert, Home and the rest) carries the
            // extended flag and stays itself.
            assert_eq!(canonical(off, scan, true), off, "the key Num{digit} doubles as");
            assert_eq!(virtual_keys(0x60 + digit), [0x60 + digit, off]);
        }
        assert_eq!(canonical(0x6E, 0x53, false), 0x6E);
        assert_eq!(canonical(0x2E, 0x53, false), 0x6E, "the decimal point with Num Lock off is not Delete");
        assert_eq!(canonical(0x2E, 0x53, true), 0x2E);
        // The top row's digits and every other key are themselves.
        assert_eq!(canonical(0x30, 0x0B, false), 0x30);
        assert_eq!(canonical(0x41, 0x1E, false), 0x41);
        assert_eq!(canonical(0x09, 0x0F, false), 0x09);
        // Enter: the number pad's carries the extended flag.
        assert_eq!(canonical(0x0D, 0x1C, false), 0x0D);
        assert_eq!(canonical(0x0D, 0x1C, true), NUM_ENTER);
        assert_eq!(virtual_keys(NUM_ENTER), [0x0D, 0x0D]);
        assert_eq!(virtual_keys(0x09), [0x09, 0x09]);
        // A key sent by software with no scan code is taken at its word.
        assert_eq!(canonical(0x2D, 0, false), 0x2D);
    }

    #[test]
    fn number_pad_chords_read_well_and_are_saved_as_older_versions_wrote_them() {
        for (saved, shown, key) in [("Ctrl+Num0", "Ctrl+Num 0", 0x60), ("Num0", "Num 0", 0x60), ("Ctrl+Alt+Num9", "Ctrl+Alt+Num 9", 0x69),
            ("Alt+Num+", "Alt+Num +", 0x6B), ("Ctrl+Num-", "Ctrl+Num -", 0x6D), ("Ctrl+Num*", "Ctrl+Num *", 0x6A), ("Ctrl+Num/", "Ctrl+Num /", 0x6F),
            ("Ctrl+Num.", "Ctrl+Num .", 0x6E), ("Ctrl+NumEnter", "Ctrl+Num Enter", NUM_ENTER)] {
            let hotkey = Hotkey::parse(saved).unwrap_or_else(|| panic!("{saved}"));
            assert_eq!(hotkey.key, key, "{saved}");
            // Saved in the form every version reads; shown spaced out; both read back.
            assert_eq!(hotkey.to_string(), saved);
            assert_eq!(serde_json::to_string(&hotkey).unwrap(), format!("\"{saved}\""));
            assert_eq!(hotkey.label(), shown);
            assert_eq!(Hotkey::parse(shown), Some(hotkey), "{shown}");
        }
        // Other spellings of the same keys.
        assert_eq!(Hotkey::parse("ctrl + numpad 0"), Hotkey::parse("Ctrl+Num0"));
        assert_eq!(Hotkey::parse("Ctrl+Numpad+"), Hotkey::parse("Ctrl+Num+"));
        assert_eq!(Hotkey::parse("Ctrl+Numpad Enter"), Hotkey::parse("Ctrl+NumEnter"));
        assert_ne!(Hotkey::parse("Ctrl+Num0"), Hotkey::parse("Ctrl+0"));
        assert_ne!(Hotkey::parse("Ctrl+NumEnter"), Hotkey::parse("Ctrl+Enter"));
        // The other keys are shown as they are saved.
        for text in ["Ctrl+Tab", "Ctrl+Alt+F1", "Alt+PageDown", "Ctrl+Shift+A", "Ctrl+Alt+Esc", "Ctrl+Alt+Backspace", "Alt+Pause", "Ctrl+Win+0xAD"] {
            let hotkey = Hotkey::parse(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!((hotkey.to_string(), hotkey.label()), (text.to_owned(), text.to_owned()));
        }
        assert_eq!(Hotkey::parse("Ctrl+Escape"), Hotkey::parse("Ctrl+Esc"));
        assert_eq!(Hotkey::parse("Ctrl+Return"), Hotkey::parse("Ctrl+Enter"));
        // A key with a name is never written as its code.
        for bad in ["Ctrl+0x41", "Ctrl+0x00", "Ctrl+0xFF", "Ctrl+0x100", "Ctrl+0xZZ", "Num", "Ctrl+Num", "Ctrl+Num10"] {
            assert_eq!(Hotkey::parse(bad), None, "{bad}");
        }
        assert_eq!(modifier_names(MOD_CONTROL | MOD_ALT), "Ctrl+Alt+");
        assert_eq!(modifier_names(MOD_WIN | MOD_SHIFT), "Shift+Win+");
        assert_eq!(modifier_names(0), "");
    }

    #[test]
    fn the_plan_registers_each_chord_once_and_reports_the_rest() {
        let mut s = Settings::default();
        assert_eq!(plan(&s), (vec![], vec![]), "no hotkey is set until the user sets one");
        s.cycle = Hotkey::parse("Ctrl+Alt+Right");
        s.cycle_back = Hotkey::parse("Ctrl+Alt+Right");
        s.accounts.insert("b".into(), Hotkey::parse("Ctrl+Alt+2").unwrap());
        s.accounts.insert("a".into(), Hotkey::parse("Win+L").unwrap());
        let (register, refused) = plan(&s);
        assert_eq!(register.iter().map(|(id, a, _)| (*id, a.clone())).collect::<Vec<_>>(), vec![(1, Action::Next), (2, Action::Account("b".into()))]);
        assert_eq!(refused.len(), 2);
        assert_eq!(refused[0].problem, Some(Problem::Duplicate));
        assert_eq!(refused[1], Status { action: Action::Account("a".into()), hotkey: Hotkey::parse("Win+L").unwrap(), problem: Some(Problem::Reserved) });
        // Any key is a hotkey: bare keys and shortcuts other programs use are registered like
        // the rest (the bare ones only over a game: `in_game_only`).
        let mut s = Settings { cycle: Hotkey::parse("Num0"), cycle_back: Hotkey::parse("A"), ..Settings::default() };
        s.accounts.insert("a".into(), Hotkey::parse("Num1").unwrap());
        s.accounts.insert("b".into(), Hotkey::parse("Ctrl+1").unwrap());
        s.accounts.insert("c".into(), Hotkey::parse("Shift+F5").unwrap());
        let (register, refused) = plan(&s);
        assert_eq!(register.iter().map(|(_, a, h)| (a.clone(), h.to_string(), in_game_only(a, *h))).collect::<Vec<_>>(), vec![
            (Action::Next, "Num0".to_owned(), true), (Action::Previous, "A".to_owned(), true), (Action::Account("a".into()), "Num1".to_owned(), true),
            (Action::Account("b".into()), "Ctrl+1".to_owned(), false), (Action::Account("c".into()), "Shift+F5".to_owned(), true),
        ]);
        assert_eq!(refused, vec![]);
    }

    #[test]
    fn cycling_follows_the_account_order_and_wraps() {
        let order = vec!["a".to_owned(), "b".into(), "c".into()];
        assert_eq!(target(&Action::Next, &order, Some("a")).as_deref(), Some("b"));
        assert_eq!(target(&Action::Next, &order, Some("c")).as_deref(), Some("a"));
        assert_eq!(target(&Action::Previous, &order, Some("a")).as_deref(), Some("c"));
        assert_eq!(target(&Action::Next, &order, None).as_deref(), Some("a"));
        assert_eq!(target(&Action::Previous, &order, None).as_deref(), Some("c"));
        assert_eq!(target(&Action::Next, &order, Some("gone")).as_deref(), Some("a"));
        assert_eq!(target(&Action::Account("b".into()), &order, None).as_deref(), Some("b"));
        assert_eq!(target(&Action::Account("z".into()), &order, None), None);
        assert_eq!(target(&Action::Next, &[], None), None);
    }

    fn game(account: &str, window: Option<isize>, minimized: bool) -> Candidate { Candidate { account: account.into(), window, minimized } }

    #[test]
    fn the_cycle_keys_step_through_open_windows_in_home_order() {
        let games = [game("a", Some(10), false), game("b", Some(20), false), game("c", Some(30), false)];
        let to = |action: Action, current: Option<&str>| cycle(&action, &games, current).map(|s| (s.index, s.hwnd));
        assert_eq!(to(Action::Next, Some("a")), Some((2, 20)));
        assert_eq!(to(Action::Next, Some("c")), Some((1, 10)), "wraps to the first");
        assert_eq!(to(Action::Previous, Some("a")), Some((3, 30)), "wraps to the last");
        assert_eq!(to(Action::Previous, Some("c")), Some((2, 20)));
        // From no game (or one Crystal didn't start): the first, or the last going back.
        assert_eq!(to(Action::Next, None), Some((1, 10)));
        assert_eq!(to(Action::Previous, None), Some((3, 30)));
        assert_eq!(to(Action::Account("b".into()), Some("a")), Some((2, 20)));
    }

    #[test]
    fn games_without_a_window_are_skipped_and_minimised_ones_restored() {
        // b is still loading (no window yet), d closed its window; c is minimised.
        let games = [game("a", Some(10), false), game("b", None, false), game("c", Some(30), true), game("d", None, false), game("e", Some(50), false)];
        let next = cycle(&Action::Next, &games, Some("a")).unwrap();
        assert_eq!(next, Switch { account: "c".into(), hwnd: 30, index: 3, restore: true }, "b skipped, c restored");
        assert_eq!(cycle(&Action::Next, &games, Some("c")).map(|s| s.index), Some(5), "d skipped");
        assert_eq!(cycle(&Action::Next, &games, Some("e")).map(|s| s.index), Some(1));
        assert_eq!(cycle(&Action::Previous, &games, Some("a")).map(|s| s.index), Some(5));
        assert_eq!(cycle(&Action::Previous, &games, Some("e")).map(|s| (s.index, s.restore)), Some((3, true)));
        assert!(!cycle(&Action::Next, &games, Some("c")).unwrap().restore);
        // A game with no window can't be the target of its account hotkey either.
        assert_eq!(cycle(&Action::Account("b".into()), &games, Some("a")), None);
        // One window: it is its own next. None: nowhere to go.
        let one = [game("a", Some(10), false), game("b", None, false)];
        assert_eq!(cycle(&Action::Next, &one, Some("a")).map(|s| s.hwnd), Some(10));
        assert_eq!(cycle(&Action::Next, &[game("a", None, false)], None), None);
        assert_eq!(cycle(&Action::Next, &[], None), None);
    }

    #[test]
    fn no_hotkey_is_set_by_default_and_the_old_defaults_are_cleared_once() {
        let s = Settings::default();
        assert_eq!((s.cycle, s.cycle_back, s.accounts.len()), (None, None, 0));
        // Both fields are always written: a Crystal up to 1.2.4 reads a missing one as Ctrl+Tab.
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"cycle":null,"cycle_back":null}"#);
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), s, "a missing field is no hotkey");
        // What 1.2.4 wrote for a user who never touched the hotkeys: cleared.
        let mut old: Settings = serde_json::from_str(r#"{"cycle":"Ctrl+Tab","cycle_back":"Ctrl+Shift+Tab","accounts":{"k":"Ctrl+Alt+1"}}"#).unwrap();
        assert_eq!((old.cycle, old.cycle_back), (Some(OLD_CYCLE), Some(OLD_CYCLE_BACK)));
        assert!(old.clear_old_defaults());
        assert_eq!((old.cycle, old.cycle_back), (None, None));
        assert_eq!(old.accounts["k"], Hotkey::parse("Ctrl+Alt+1").unwrap(), "an account's hotkey was always the user's");
        assert!(!old.clear_old_defaults(), "nothing left to clear");
        // A chord the user chose is kept; the other key, still the old default, is cleared.
        let mut own: Settings = serde_json::from_str(r#"{"cycle":"Ctrl+Alt+Right","cycle_back":"Ctrl+Shift+Tab"}"#).unwrap();
        assert!(own.clear_old_defaults());
        assert_eq!((own.cycle, own.cycle_back), (Hotkey::parse("Ctrl+Alt+Right"), None));
        // The old defaults the other way round were the user's doing.
        let mut swapped = Settings { cycle: Some(OLD_CYCLE_BACK), cycle_back: Some(OLD_CYCLE), ..Settings::default() };
        assert!(!swapped.clear_old_defaults());
        assert_eq!((swapped.cycle, swapped.cycle_back), (Some(OLD_CYCLE_BACK), Some(OLD_CYCLE)));
        // Off (null) stays off.
        let mut off = Settings::default();
        assert!(!off.clear_old_defaults());
    }
}
