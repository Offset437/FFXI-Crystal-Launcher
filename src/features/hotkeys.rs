//! Client switching hotkeys: one to cycle through the running game windows (and one to cycle
//! back), Ctrl+Tab and Ctrl+Shift+Tab by default, plus optional per-account hotkeys that bring
//! that account's game forward.
//!
//! They are registered with RegisterHotKey on the desktop thread's message-only window
//! (`desktop`), never with a low-level keyboard hook: Windows delivers only these chords to
//! Crystal, nothing else is seen, and a chord another program already owns is reported back as a
//! conflict instead of being stolen. The cycle keys are registered only while a Crystal-launched
//! game window is in the foreground (`Gate`), so Ctrl+Tab stays with browsers and editors.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const MOD_ALT: u8 = 1;
pub(crate) const MOD_CONTROL: u8 = 2;
pub(crate) const MOD_SHIFT: u8 = 4;
pub(crate) const MOD_WIN: u8 = 8;

/// A chord: modifiers (`MOD_*`, the RegisterHotKey bits) and a virtual-key code. Saved as text,
/// e.g. `"Ctrl+Alt+F1"`.
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

/// Key names and their virtual-key codes (letters and digits are their own codes).
const NAMES: &[(&str, u16)] = &[
    ("Tab", 0x09), ("Enter", 0x0D), ("Pause", 0x13), ("Space", 0x20), ("PageUp", 0x21), ("PageDown", 0x22),
    ("End", 0x23), ("Home", 0x24), ("Left", 0x25), ("Up", 0x26), ("Right", 0x27), ("Down", 0x28),
    ("Insert", 0x2D), ("Delete", 0x2E), ("Num0", 0x60), ("Num1", 0x61), ("Num2", 0x62), ("Num3", 0x63),
    ("Num4", 0x64), ("Num5", 0x65), ("Num6", 0x66), ("Num7", 0x67), ("Num8", 0x68), ("Num9", 0x69),
    ("Num*", 0x6A), ("Num+", 0x6B), ("Num-", 0x6D), ("Num.", 0x6E), ("Num/", 0x6F),
    (";", 0xBA), ("=", 0xBB), (",", 0xBC), ("-", 0xBD), (".", 0xBE), ("/", 0xBF), ("`", 0xC0),
    ("[", 0xDB), ("\\", 0xDC), ("]", 0xDD), ("'", 0xDE),
];

impl Hotkey {
    pub(crate) const fn new(modifiers: u8, key: u16) -> Self { Self { modifiers, key } }

    /// `"Ctrl+Alt+F1"`-style text; modifiers in any order, names ignore case.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut modifiers = 0u8;
        let mut key = None;
        for part in text.split('+').map(str::trim) {
            // "Num+" splits into "Num" and "": put the plus back.
            let part = if part.is_empty() && key == Some(u16::MAX) { "Num+" } else { part };
            let bit = match part.to_ascii_lowercase().as_str() {
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
            if part.eq_ignore_ascii_case("num") { key = Some(u16::MAX); continue; }
            key = Some(key_code(part)?);
        }
        let key = key.filter(|k| *k != u16::MAX)?;
        Some(Self { modifiers, key })
    }

    /// Why this chord can't be a Crystal hotkey, if it can't: it needs a modifier (bare keys
    /// belong to the game; F13-F24 are free), and must not be a chord Windows keeps for itself.
    pub(crate) fn problem(&self) -> Option<Problem> {
        let function_high = (0x7C..=0x87).contains(&self.key); // F13-F24
        if self.modifiers == 0 && !function_high { return Some(Problem::NeedsModifier); }
        if self.modifiers == MOD_SHIFT && !function_high { return Some(Problem::NeedsModifier); }
        let reserved = (self.modifiers & MOD_WIN != 0 && self.modifiers & (MOD_CONTROL | MOD_ALT | MOD_SHIFT) == 0)
            || (self.modifiers == MOD_ALT && matches!(self.key, 0x09 | 0x1B | 0x73)) // Alt+Tab, Alt+Esc, Alt+F4
            || (self.modifiers == MOD_CONTROL | MOD_ALT && self.key == 0x2E) // Ctrl+Alt+Del
            || (self.modifiers == MOD_CONTROL && self.key == 0x1B); // Ctrl+Esc
        reserved.then_some(Problem::Reserved)
    }
}

fn key_code(name: &str) -> Option<u16> {
    if let Some((_, code)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) { return Some(*code); }
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() { return Some(u16::from(bytes[0])); }
    let number = upper.strip_prefix('F')?.parse::<u16>().ok().filter(|n| (1..=24).contains(n))?;
    Some(0x6F + number) // VK_F1 = 0x70
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
        for (bit, name) in [(MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Win")] {
            if self.modifiers & bit != 0 { write!(f, "{name}+")?; }
        }
        f.write_str(&key_name(self.key))
    }
}

/// The default cycle chords: Ctrl+Tab forward, Ctrl+Shift+Tab back. Neither Windower 4's nor
/// Ashita v4's stock binds use them, and FFXI's own Tab binding (switch target) is unmodified Tab
/// and Shift+Tab.
pub(crate) const DEFAULT_CYCLE: Hotkey = Hotkey::new(MOD_CONTROL, 0x09);
pub(crate) const DEFAULT_CYCLE_BACK: Hotkey = Hotkey::new(MOD_CONTROL | MOD_SHIFT, 0x09);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Bring the next game window forward, while a game window is in the foreground. None
    /// (saved as `null`): off. A missing field means the default, Ctrl+Tab.
    pub cycle: Option<Hotkey>,
    /// The previous one (default Ctrl+Shift+Tab).
    pub cycle_back: Option<Hotkey>,
    /// Bring this account's game window forward, from anywhere, by account key.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, Hotkey>,
}

impl Default for Settings {
    fn default() -> Self { Self { cycle: Some(DEFAULT_CYCLE), cycle_back: Some(DEFAULT_CYCLE_BACK), accounts: BTreeMap::new() } }
}

/// What a hotkey does.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Action {
    Next,
    Previous,
    Account(String),
}

impl Action {
    /// Whether the hotkey is registered only while a game window is in the foreground (the cycle
    /// keys, so a chord like Ctrl+Tab stays with browsers and editors everywhere else). Account
    /// hotkeys are registered for as long as they are set.
    pub(crate) fn in_game_only(&self) -> bool { matches!(self, Self::Next | Self::Previous) }
}

/// Why a hotkey is not active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// Bare keys (and Shift+key) belong to the game.
    NeedsModifier,
    /// Windows keeps this chord for itself.
    Reserved,
    /// Another of Crystal's hotkeys has the same chord.
    Duplicate,
    /// Another program registered it first.
    InUse,
    /// RegisterHotKey failed for another reason.
    Failed(u32),
}

impl Problem {
    pub(crate) fn text(&self) -> String {
        use crate::i18n::{t, tf};
        match self {
            Self::NeedsModifier => t("Add Ctrl, Alt or Win to this hotkey.").into(),
            Self::Reserved => t("Windows reserves this hotkey.").into(),
            Self::Duplicate => t("Another Crystal hotkey uses this.").into(),
            Self::InUse => t("Another program already uses this hotkey.").into(),
            Self::Failed(code) => tf("Could not register this hotkey (error {code}).", &[("code", code)]),
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
        let keys = vec![(1, DEFAULT_CYCLE), (2, DEFAULT_CYCLE_BACK)];
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
        let mut windows = Fake { owned_elsewhere: vec![DEFAULT_CYCLE], ..Fake::default() };
        let mut gate = Gate::default();
        gate.set_keys(vec![(1, DEFAULT_CYCLE), (2, DEFAULT_CYCLE_BACK)], false, &mut windows);
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

    #[test]
    fn bare_and_reserved_chords_are_refused() {
        assert_eq!(Hotkey::parse("F1").unwrap().problem(), Some(Problem::NeedsModifier));
        assert_eq!(Hotkey::parse("Shift+A").unwrap().problem(), Some(Problem::NeedsModifier));
        assert_eq!(Hotkey::parse("F13").unwrap().problem(), None);
        assert_eq!(Hotkey::parse("Alt+Tab").unwrap().problem(), Some(Problem::Reserved));
        assert_eq!(Hotkey::parse("Alt+F4").unwrap().problem(), Some(Problem::Reserved));
        assert_eq!(Hotkey::parse("Win+L").unwrap().problem(), Some(Problem::Reserved));
        assert_eq!(Hotkey::parse("Ctrl+Alt+Delete").unwrap().problem(), Some(Problem::Reserved));
        assert_eq!(Hotkey::parse("Ctrl+Alt+Right").unwrap().problem(), None);
        assert_eq!(Hotkey::parse("Win+Alt+1").unwrap().problem(), None);
    }

    #[test]
    fn the_plan_registers_each_chord_once_and_reports_the_rest() {
        let mut s = Settings { cycle: None, cycle_back: None, ..Settings::default() };
        assert_eq!(plan(&s), (vec![], vec![]));
        s.cycle = Hotkey::parse("Ctrl+Alt+Right");
        s.cycle_back = Hotkey::parse("Ctrl+Alt+Right");
        s.accounts.insert("b".into(), Hotkey::parse("Ctrl+Alt+2").unwrap());
        s.accounts.insert("a".into(), Hotkey::parse("F1").unwrap());
        let (register, refused) = plan(&s);
        assert_eq!(register.iter().map(|(id, a, _)| (*id, a.clone())).collect::<Vec<_>>(), vec![(1, Action::Next), (2, Action::Account("b".into()))]);
        assert_eq!(refused.len(), 2);
        assert_eq!(refused[0].problem, Some(Problem::Duplicate));
        assert_eq!(refused[1], Status { action: Action::Account("a".into()), hotkey: Hotkey::parse("F1").unwrap(), problem: Some(Problem::NeedsModifier) });
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

    #[test]
    fn cycling_is_ctrl_tab_by_default_and_turning_it_off_is_kept() {
        let s = Settings::default();
        assert_eq!((s.cycle, s.cycle_back), (Hotkey::parse("Ctrl+Tab"), Hotkey::parse("Ctrl+Shift+Tab")));
        assert_eq!(s.cycle.unwrap().problem(), None);
        assert_eq!(s.cycle_back.unwrap().problem(), None);
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"cycle":"Ctrl+Tab","cycle_back":"Ctrl+Shift+Tab"}"#);
        // A missing field is the default; an explicit null is off and stays off.
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), s);
        let off = Settings { cycle: None, cycle_back: None, ..Settings::default() };
        let json = serde_json::to_string(&off).unwrap();
        assert_eq!(json, r#"{"cycle":null,"cycle_back":null}"#);
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), off);
        let back: Settings = serde_json::from_str(r#"{"cycle":"Ctrl+Alt+Right","accounts":{"k":"Ctrl+Alt+1"}}"#).unwrap();
        assert_eq!(back.cycle, Hotkey::parse("Ctrl+Alt+Right"));
        assert_eq!(back.cycle_back, Some(DEFAULT_CYCLE_BACK));
        assert_eq!(back.accounts["k"], Hotkey::parse("Ctrl+Alt+1").unwrap());
        assert!(Action::Next.in_game_only() && Action::Previous.in_game_only() && !Action::Account("k".into()).in_game_only());
    }
}
