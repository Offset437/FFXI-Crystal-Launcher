//! The per-account Windower/Ashita profile picker, without a toolkit.
//!
//! The selection itself already lives on the `Account` (`windower_profile`, `ashita_boot_ini`;
//! `None` follows the global default). This module lists what can be picked, says what an
//! account will actually launch with, and shows the one value that bit us: whether that profile
//! turns background gamepad on. A fallback to the global default is flagged
//! (`uses_global_default`) so it can never again be silent.

use crate::backend::Account;
use crate::i18n::{t, tf};
use crate::pol_less::{ffxi_settings, tools};
use crate::backend::LaunchWith;
use std::path::Path;

/// How the game is started, as far as profiles go. Stock (vanilla) and the native client have
/// no profile; they read the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    Windower,
    Ashita,
    Native,
}

impl Tool {
    fn of(launch_with: LaunchWith) -> Self {
        match launch_with {
            LaunchWith::Windower => Tool::Windower,
            LaunchWith::Ashita => Tool::Ashita,
            LaunchWith::Native | LaunchWith::Vanilla => Tool::Native,
        }
    }
}

/// Where a background-gamepad value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GamepadSource {
    /// The profile says so (Windower element, Ashita ini key).
    Profile,
    /// A Windower profile without the element: off (Windower's own default of on is not followed).
    NotSet,
    /// Nothing in the profile; the registry's current value applies.
    Registry,
    /// The registry could not be read, or the selection is missing: treated as off.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundGamepad {
    pub on: bool,
    pub source: GamepadSource,
}

impl BackgroundGamepad {
    fn off(source: GamepadSource) -> Self {
        Self { on: false, source }
    }

    /// The registry's `padmode000`, or `Unknown` (off) when it can't be read.
    fn registry(padmode: Option<&str>) -> Self {
        padmode.map_or(Self::off(GamepadSource::Unknown), |p| Self { on: ffxi_settings::padmode_background(p), source: GamepadSource::Registry })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProfileEntry {
    /// What is stored on the account: the profile name or the `.ini` file name. "" is Windower's
    /// default profile.
    pub name: String,
    /// What the picker shows.
    pub label: String,
    pub is_default: bool,
    pub background_gamepad: BackgroundGamepad,
}

/// Larger than any real Windower settings.xml (matches the launch path's cap).
const MAX_SETTINGS_XML: u64 = 4 * 1024 * 1024;
/// Larger than any real boot ini (matches the launch path's cap).
const MAX_BOOT_INI: u64 = 1024 * 1024;

fn read_capped(path: &Path, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    // One byte past the cap tells "exactly the cap" from "larger".
    file.take(max + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max).then_some(bytes))
}

/// Windower's profiles from `<windower>\settings.xml`: the default first (always present, as
/// Windower creates it), then the named ones in file order. A missing file is just the default.
/// Names a launch could not use are left out, as are repeats (Windower matches names ignoring
/// case and takes the first).
pub(crate) fn windower_profiles(windower_dir: &Path) -> Result<Vec<ProfileEntry>, String> {
    let path = windower_dir.join("settings.xml");
    let summaries = match read_capped(&path, MAX_SETTINGS_XML) {
        Ok(Some(xml)) => ffxi_settings::profile_summaries(&xml)?,
        Ok(None) => return Err(tf("{path} is too large to be Windower's settings.", &[("path", &path.display())])),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return Err(t("Could not read Windower's settings").into()),
    };
    let mut entries: Vec<ProfileEntry> = Vec::new();
    let mut add = |name: String, gamepad: Option<bool>| {
        if entries.iter().any(|e| e.name.to_lowercase() == name.to_lowercase()) || !tools::valid_profile(&name) {
            return;
        }
        let is_default = name.is_empty();
        let background_gamepad = match gamepad {
            Some(on) => BackgroundGamepad { on, source: GamepadSource::Profile },
            None => BackgroundGamepad::off(GamepadSource::NotSet),
        };
        let label = if is_default { t("Default profile").to_owned() } else { name.clone() };
        entries.push(ProfileEntry { name, label, is_default, background_gamepad });
    };
    // The default profile exists even when settings.xml has none (`Profile.Default`).
    let default = summaries.iter().find(|p| p.name.is_empty()).and_then(|p| p.gamepad);
    add(String::new(), default);
    for p in summaries.into_iter().filter(|p| !p.name.is_empty()) {
        add(p.name, p.gamepad);
    }
    Ok(entries)
}

/// The value of `key` in `[section]` of an ini, the way Ashita reads it: names ignore case,
/// `;` and `#` start comment lines, a BOM is ignored, and the first occurrence wins.
fn ini_value(ini: &[u8], section: &str, key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(ini);
    let mut inside = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with([';', '#']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name.trim().eq_ignore_ascii_case(section);
        } else if inside {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case(key) {
                    return Some(v.trim().to_owned());
                }
            }
        }
    }
    None
}

/// What an Ashita boot ini's background gamepad is: its `[ffxi.registry] padmode000` if it has
/// one (Ashita applies it as-is), else whatever the registry holds.
fn ashita_gamepad(ini: Option<&[u8]>, registry: Option<&str>) -> BackgroundGamepad {
    match ini.and_then(|ini| ini_value(ini, "ffxi.registry", "padmode000")) {
        Some(pad) => BackgroundGamepad { on: ffxi_settings::padmode_background(&pad), source: GamepadSource::Profile },
        None => BackgroundGamepad::registry(registry),
    }
}

/// Ashita's boot inis (`config\boot\*.ini`, minus Crystal's generated ones). `registry` is the
/// registry's `padmode000` for inis that don't set their own; pass
/// `ffxi_settings::registry_padmode().as_deref()` outside tests.
pub(crate) fn ashita_profiles_with(ashita_dir: &Path, registry: Option<&str>) -> Vec<ProfileEntry> {
    tools::boot_inis(ashita_dir)
        .into_iter()
        .map(|name| {
            let ini = read_capped(&ashita_dir.join("config").join("boot").join(&name), MAX_BOOT_INI).ok().flatten();
            let is_default = name.eq_ignore_ascii_case(tools::DEFAULT_BOOT_INI);
            ProfileEntry { label: name.clone(), background_gamepad: ashita_gamepad(ini.as_deref(), registry), is_default, name }
        })
        .collect()
}

pub(crate) fn ashita_profiles(ashita_dir: &Path) -> Vec<ProfileEntry> {
    ashita_profiles_with(ashita_dir, ffxi_settings::registry_padmode().as_deref())
}

/// Everything the picker lists, read once.
#[derive(Clone, Debug, Default)]
pub(crate) struct Lists {
    pub windower: Vec<ProfileEntry>,
    /// Why `windower` is empty when settings.xml could not be read.
    pub windower_error: Option<String>,
    pub ashita: Vec<ProfileEntry>,
    /// The registry's `padmode000`; what the native client and stock launches use.
    pub registry_padmode: Option<String>,
}

impl Lists {
    pub(crate) fn load(windower_dir: Option<&Path>, ashita_dir: Option<&Path>) -> Self {
        Self::load_with(windower_dir, ashita_dir, ffxi_settings::registry_padmode())
    }

    pub(crate) fn load_with(windower_dir: Option<&Path>, ashita_dir: Option<&Path>, registry_padmode: Option<String>) -> Self {
        let (windower, windower_error) = match windower_dir.map(windower_profiles) {
            Some(Ok(list)) => (list, None),
            Some(Err(e)) => (Vec::new(), Some(e)),
            None => (Vec::new(), None),
        };
        let ashita = ashita_dir.map(|dir| ashita_profiles_with(dir, registry_padmode.as_deref())).unwrap_or_default();
        Self { windower, windower_error, ashita, registry_padmode }
    }

    fn find<'a>(list: &'a [ProfileEntry], name: &str) -> Option<&'a ProfileEntry> {
        let lower = name.to_lowercase();
        list.iter().find(|e| e.name.to_lowercase() == lower)
    }
}

/// The global defaults an account without its own choice follows (Settings, Launch tools).
#[derive(Clone, Debug, Default)]
pub(crate) struct Defaults {
    pub launch_with: LaunchWith,
    pub windower_profile: String,
    pub ashita_boot_ini: String,
}

/// One account's row: what it launches with and whether that turns background gamepad on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountProfile {
    pub account_key: String,
    pub tool: Tool,
    /// The account's own choice; `None` follows the global default.
    pub selected: Option<String>,
    /// The name a launch uses ("" is Windower's default profile). Empty for the native client,
    /// and for Ashita when the default ini is used and doesn't exist.
    pub effective: String,
    /// The account has no choice of its own: it gets whatever the global default is.
    pub uses_global_default: bool,
    /// The effective choice is not in the list. A launch refuses it.
    pub missing: bool,
    pub background_gamepad: BackgroundGamepad,
}

pub(crate) fn account_profile(account: &Account, defaults: &Defaults, lists: &Lists) -> AccountProfile {
    let tool = Tool::of(account.effective_launch_with(defaults.launch_with));
    let (selected, global, list) = match tool {
        Tool::Windower => (account.windower_profile.as_deref(), defaults.windower_profile.as_str(), &lists.windower),
        Tool::Ashita => (account.ashita_boot_ini.as_deref(), defaults.ashita_boot_ini.as_str(), &lists.ashita),
        Tool::Native => (None, "", &lists.ashita),
    };
    let mut view = AccountProfile {
        account_key: crate::backend::account_key(account),
        tool,
        selected: selected.map(str::to_owned),
        effective: String::new(),
        uses_global_default: tool != Tool::Native && selected.is_none(),
        missing: false,
        background_gamepad: BackgroundGamepad::registry(lists.registry_padmode.as_deref()),
    };
    if tool == Tool::Native {
        return view;
    }
    let effective = selected.unwrap_or(global).trim();
    view.effective = effective.to_owned();
    match (tool, Lists::find(list, effective)) {
        (_, Some(entry)) => view.background_gamepad = entry.background_gamepad,
        // Ashita with no choice anywhere uses default.ini, or a minimal one when that's absent.
        (Tool::Ashita, None) if effective.is_empty() => view.effective = tools::DEFAULT_BOOT_INI.to_owned(),
        _ => {
            view.missing = true;
            view.background_gamepad = BackgroundGamepad::off(GamepadSource::Unknown);
        }
    }
    // An unnamed Ashita choice resolves to default.ini; if that exists it supplies the value.
    if tool == Tool::Ashita && effective.is_empty() {
        if let Some(entry) = Lists::find(list, tools::DEFAULT_BOOT_INI) {
            view.background_gamepad = entry.background_gamepad;
        }
    }
    view
}

/// Sets the account's own choice. `None` clears it (follows the global default). The name is
/// stored as the list spells it, so a differently-cased entry doesn't create a second spelling.
/// Windower's `""` is a valid choice: the default profile.
pub(crate) fn set_selection(account: &mut Account, tool: Tool, name: Option<String>, lists: &Lists) -> Result<(), String> {
    let slot = match tool {
        Tool::Windower => &mut account.windower_profile,
        Tool::Ashita => &mut account.ashita_boot_ini,
        Tool::Native => {
            return match name {
                None => Ok(()),
                Some(_) => Err(t("The native client has no profiles.").into()),
            }
        }
    };
    let Some(name) = name else {
        *slot = None;
        return Ok(());
    };
    let name = name.trim();
    let (valid, list, unknown) = match tool {
        Tool::Windower => (tools::valid_profile(name), &lists.windower, "Windower profile {name} was not found in settings.xml. Choose an existing profile."),
        _ => (tools::valid_boot_ini_name(name), &lists.ashita, "The base boot config {name} was not found in Ashita's config\\boot folder. Choose another."),
    };
    if !valid {
        return Err(match tool {
            Tool::Windower => t("The Windower profile name can't contain quotes or control characters."),
            _ => t("The base boot config must be an .ini file name in Ashita's config\\boot folder."),
        }
        .to_owned());
    }
    let entry = Lists::find(list, name).ok_or_else(|| tf(unknown, &[("name", &name)]))?;
    *slot = Some(entry.name.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AccountMode;

    const XML: &str = "<settings><profile name=\"\"><consolekey>Insert</consolekey></profile>\
        <profile name=\"Main\"><alwaysenablegamepad>true</alwaysenablegamepad><password>hunter2</password></profile>\
        <profile name=\"Quiet\"><alwaysenablegamepad>false</alwaysenablegamepad></profile>\
        <profile name=\"Sparse\"><width>800</width></profile>\
        <profile name=\"main\"><alwaysenablegamepad>false</alwaysenablegamepad></profile>\
        <profile name=\"bad&quot;name\"/></settings>";

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xi-profiles-{tag}-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn account() -> Account {
        Account { label: "A".into(), mode: AccountMode::Retail, server: String::new(), pol_id: "ABCD1234".into(), pol_password: String::new(),
            se_id: String::new(), se_password: String::new(), region: 0, launch_with: None, ashita_boot_ini: None, windower_profile: None,
            pol_handle_names: String::new() }
    }

    fn lists(registry: Option<&str>) -> (Lists, std::path::PathBuf) {
        let root = temp("lists");
        std::fs::write(root.join("settings.xml"), XML).unwrap();
        let boot = root.join("config").join("boot");
        std::fs::create_dir_all(&boot).unwrap();
        std::fs::write(boot.join("default.ini"), "[ashita.boot]\nfile=x\n").unwrap();
        std::fs::write(boot.join("pad.ini"), b"\xef\xbb\xbf; note\r\n[FFXI.Registry]\r\n# c\r\nPadMode000 = 0,0,0,0,1,0\r\n").unwrap();
        std::fs::write(boot.join("padoff.ini"), "[ffxi.registry]\npadmode000=1,1,1,1,0,1\n[other]\npadmode000=0,0,0,0,1,0\n").unwrap();
        std::fs::write(boot.join("crystal-abc.ini"), "[ffxi.registry]\n").unwrap();
        (Lists::load_with(Some(&root), Some(&root), registry.map(str::to_owned)), root)
    }

    fn gp(on: bool, source: GamepadSource) -> BackgroundGamepad {
        BackgroundGamepad { on, source }
    }

    #[test]
    fn windower_list_has_the_default_and_each_profiles_explicit_gamepad() {
        let (l, root) = lists(None);
        let view: Vec<_> = l.windower.iter().map(|e| (e.name.as_str(), e.is_default, e.background_gamepad)).collect();
        assert_eq!(view, [
            ("", true, gp(false, GamepadSource::NotSet)),
            ("Main", false, gp(true, GamepadSource::Profile)),
            ("Quiet", false, gp(false, GamepadSource::Profile)),
            ("Sparse", false, gp(false, GamepadSource::NotSet)),
        ], "the repeat `main` and the quoted name are not pickable");
        assert_eq!(l.windower[0].label, "Default profile");
        assert_eq!(l.windower[1].label, "Main");
        // A missing settings.xml and one with no unnamed profile still offer the default.
        let empty = temp("empty");
        assert_eq!(windower_profiles(&empty).unwrap().len(), 1);
        std::fs::write(empty.join("settings.xml"), "<settings><profile name=\"X\"/></settings>").unwrap();
        assert_eq!(windower_profiles(&empty).unwrap()[0].background_gamepad, gp(false, GamepadSource::NotSet));
        // Half-written XML and an oversized file are errors, not an empty list.
        std::fs::write(empty.join("settings.xml"), "<settings><profile name=\"X\">").unwrap();
        assert!(windower_profiles(&empty).is_err());
        std::fs::write(empty.join("settings.xml"), vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
        assert!(windower_profiles(&empty).is_err());
        for d in [root, empty] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn ashita_list_reads_the_inis_padmode_or_falls_back_to_the_registry() {
        let (l, root) = lists(Some("0,0,0,0,1,0"));
        let view: Vec<_> = l.ashita.iter().map(|e| (e.name.as_str(), e.is_default, e.background_gamepad)).collect();
        assert_eq!(view, [
            ("default.ini", true, gp(true, GamepadSource::Registry)),
            ("pad.ini", false, gp(true, GamepadSource::Profile)),
            ("padoff.ini", false, gp(false, GamepadSource::Profile)),
        ], "crystal-*.ini is Crystal's own; padmode000 in another section is ignored");
        let off = ashita_profiles_with(&root, Some("0,0,0,0,0,0"));
        assert_eq!(off[0].background_gamepad, gp(false, GamepadSource::Registry));
        assert_eq!(ashita_profiles_with(&root, None)[0].background_gamepad, gp(false, GamepadSource::Unknown));
        // An oversized ini reads as one without the key.
        std::fs::write(root.join("config").join("boot").join("pad.ini"), vec![b'#'; 1024 * 1024 + 1]).unwrap();
        assert_eq!(ashita_profiles_with(&root, None)[1].background_gamepad, gp(false, GamepadSource::Unknown));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_view_shows_own_choice_global_fallback_and_missing() {
        let (l, root) = lists(Some("0,0,0,0,1,0"));
        let defaults = Defaults { launch_with: LaunchWith::Windower, windower_profile: "Quiet".into(), ashita_boot_ini: "pad.ini".into() };
        let mut a = account();
        // No choice: the global default, flagged.
        let v = account_profile(&a, &defaults, &l);
        assert_eq!((v.tool, v.effective.as_str(), v.uses_global_default, v.missing, v.background_gamepad), (Tool::Windower, "Quiet", true, false, gp(false, GamepadSource::Profile)));
        assert_eq!(v.account_key, crate::backend::account_key(&a));
        // No choice and no global: Windower's default profile, background gamepad off.
        let bare = Defaults { windower_profile: String::new(), ..defaults.clone() };
        let v = account_profile(&a, &bare, &l);
        assert_eq!((v.effective.as_str(), v.uses_global_default, v.background_gamepad), ("", true, gp(false, GamepadSource::NotSet)));
        // Own choice (case-insensitive match, as a launch does).
        a.windower_profile = Some("main".into());
        let v = account_profile(&a, &defaults, &l);
        assert_eq!((v.selected.as_deref(), v.uses_global_default, v.missing, v.background_gamepad), (Some("main"), false, false, gp(true, GamepadSource::Profile)));
        // Explicit selection that is gone: launch refuses; shown off and unknown.
        a.windower_profile = Some("Gone".into());
        let v = account_profile(&a, &defaults, &l);
        assert_eq!((v.missing, v.background_gamepad), (true, gp(false, GamepadSource::Unknown)));
        // A stale global default is missing too.
        a.windower_profile = None;
        let stale = Defaults { windower_profile: "Gone".into(), ..defaults.clone() };
        assert!(account_profile(&a, &stale, &l).missing);
        // Ashita: own ini, global ini, unnamed (default.ini), and a missing explicit one.
        a.launch_with = Some(LaunchWith::Ashita);
        let v = account_profile(&a, &defaults, &l);
        assert_eq!((v.tool, v.effective.as_str(), v.uses_global_default, v.background_gamepad), (Tool::Ashita, "pad.ini", true, gp(true, GamepadSource::Profile)));
        a.ashita_boot_ini = Some("padoff.ini".into());
        assert_eq!(account_profile(&a, &defaults, &l).background_gamepad, gp(false, GamepadSource::Profile));
        a.ashita_boot_ini = None;
        let v = account_profile(&a, &bare_ashita(), &l);
        assert_eq!((v.effective.as_str(), v.missing, v.background_gamepad), ("default.ini", false, gp(true, GamepadSource::Registry)));
        a.ashita_boot_ini = Some("nope.ini".into());
        assert!(account_profile(&a, &defaults, &l).missing);
        // No default.ini on disk and nothing chosen: not an error, the registry applies.
        std::fs::remove_file(root.join("config").join("boot").join("default.ini")).unwrap();
        let l2 = Lists::load_with(None, Some(&root), Some("0,0,0,0,1,0".into()));
        a.ashita_boot_ini = None;
        let v = account_profile(&a, &bare_ashita(), &l2);
        assert_eq!((v.effective.as_str(), v.missing, v.background_gamepad), ("default.ini", false, gp(true, GamepadSource::Registry)));
        // The native client and stock launches read the registry and have no profile.
        // (A stored `Native` choice is the retired route; it follows the global default.)
        a.launch_with = Some(LaunchWith::Vanilla);
        let v = account_profile(&a, &defaults, &l);
        assert_eq!((v.tool, v.selected, v.effective.as_str(), v.uses_global_default, v.missing, v.background_gamepad),
            (Tool::Native, None, "", false, false, gp(true, GamepadSource::Registry)));
        a.launch_with = None;
        let stock = Defaults { launch_with: LaunchWith::Vanilla, ..defaults.clone() };
        assert_eq!(account_profile(&a, &stock, &l).tool, Tool::Native);
        // Private-server accounts always use the native client.
        let mut private = account();
        private.mode = AccountMode::Private;
        assert_eq!(account_profile(&private, &defaults, &l).tool, Tool::Native);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn bare_ashita() -> Defaults {
        Defaults { launch_with: LaunchWith::Ashita, ..Defaults::default() }
    }

    #[test]
    fn set_selection_validates_and_clears() {
        let (l, root) = lists(None);
        let mut a = account();
        set_selection(&mut a, Tool::Windower, Some(" quiet ".into()), &l).unwrap();
        assert_eq!(a.windower_profile.as_deref(), Some("Quiet"), "stored as listed, trimmed");
        set_selection(&mut a, Tool::Windower, Some(String::new()), &l).unwrap();
        assert_eq!(a.windower_profile.as_deref(), Some(""), "the default profile is a choice, not a clear");
        for bad in ["Nobody", "bad\"name", "tab\there"] {
            assert!(set_selection(&mut a, Tool::Windower, Some(bad.into()), &l).is_err(), "{bad}");
        }
        assert_eq!(a.windower_profile.as_deref(), Some(""), "a refused choice changes nothing");
        set_selection(&mut a, Tool::Windower, None, &l).unwrap();
        assert_eq!(a.windower_profile, None);
        set_selection(&mut a, Tool::Ashita, Some("PAD.ini".into()), &l).unwrap();
        assert_eq!(a.ashita_boot_ini.as_deref(), Some("pad.ini"));
        for bad in ["nope.ini", "../pad.ini", "pad", "dir\\pad.ini", ""] {
            assert!(set_selection(&mut a, Tool::Ashita, Some(bad.into()), &l).is_err(), "{bad}");
        }
        set_selection(&mut a, Tool::Ashita, None, &l).unwrap();
        assert_eq!(a.ashita_boot_ini, None);
        assert!(set_selection(&mut a, Tool::Native, Some("x".into()), &l).is_err());
        assert!(set_selection(&mut a, Tool::Native, None, &l).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
