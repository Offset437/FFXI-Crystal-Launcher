//! The FFXI settings a Windower launch gets in-process (`ffxi_settings` in the session).
//!
//! Windower 4 writes a profile's video and sound values into the shared
//! `HKLM\...\PlayOnlineUS\SquareEnix\FinalFantasyXI` key just before each start, and the game
//! reads them as it starts. Here the same values are computed from Windower's settings.xml and
//! handed to crystal-loader, which serves them to that game alone.
//! Windower still writes the registry; these games no
//! longer read it for these values, so starts with different profiles can overlap.

use crate::i18n::{t, tf};
use std::path::Path;

use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowMode {
    BorderlessWindow,
    Window,
    Fullscreen,
}

/// The profile fields `Profile.Apply` and `Injector.Launch` use.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowerProfile {
    window_mode: WindowMode,
    width: i32,
    height: i32,
    mip_map_level: i32,
    ui_scale: f32,
    gamma: f32,
    hardware_mouse: bool,
    super_sampling: f32,
    bump_mapping: bool,
    sound: bool,
    always_play_sound: bool,
    always_enable_gamepad: bool,
}

impl WindowerProfile {
    /// `new Profile(name)`: a profile settings.xml doesn't have.
    fn constructed() -> Self {
        Self {
            window_mode: WindowMode::BorderlessWindow,
            width: 0,
            height: 0,
            mip_map_level: 0,
            ui_scale: 0.0,
            gamma: 0.0,
            hardware_mouse: true,
            super_sampling: 1.0,
            bump_mapping: false,
            sound: true,
            always_play_sound: true,
            always_enable_gamepad: false,
        }
    }

    /// `Profile.Load`: each element parsed as Windower does, its default when missing or bad.
    fn loaded(fields: &[(String, String)]) -> Self {
        let get = |name: &str| fields.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        let int = |name: &str, default: i32| get(name).and_then(|v| v.trim().parse().ok()).unwrap_or(default);
        let float = |name: &str, default: f32| get(name).and_then(|v| v.trim().parse().ok()).unwrap_or(default);
        let boolean = |name: &str, default: bool| get(name).and_then(parse_bool).unwrap_or(default);
        let window_mode = match get("windowmode").map(str::to_ascii_lowercase).as_deref() {
            Some("window") => WindowMode::Window,
            Some("fullscreen") => WindowMode::Fullscreen,
            _ => WindowMode::BorderlessWindow,
        };
        Self {
            window_mode,
            width: int("width", 0),
            height: int("height", 0),
            mip_map_level: int("mipmaplevel", 0),
            gamma: float("gamma", 0.0),
            ui_scale: float("uiscale", 1.0),
            hardware_mouse: boolean("hardwaremouse", true),
            super_sampling: float("supersampling", 1.0),
            bump_mapping: boolean("bumpmapping", false),
            sound: boolean("sound", true),
            always_play_sound: boolean("alwaysplaysound", true),
            always_enable_gamepad: explicit_gamepad(fields) == Some(true),
        }
    }
}

/// `bool.Parse` as Windower reads its elements: case-insensitive `true`/`false`, trimmed.
fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// What a profile says about `<alwaysenablegamepad>`: `None` when the element is missing or not
/// a boolean. Windower itself defaults to on; Crystal never turns background gamepad on unless
/// the profile says so, because that setting reaches every game the profile starts.
fn explicit_gamepad(fields: &[(String, String)]) -> Option<bool> {
    fields.iter().find(|(n, _)| n == "alwaysenablegamepad").and_then(|(_, v)| parse_bool(v))
}

/// One `<profile>`: its trimmed name (`None` without a `name` attribute) and the text of each
/// child element, first occurrence first.
struct XmlProfile {
    name: String,
    fields: Vec<(String, String)>,
}

/// Windower's profiles are exactly the root's `<profile>` children (case-sensitive name).
/// `element_depth`: 1 for the root element, 2 for its children.
pub(super) fn is_root_profile(element_depth: usize, e: &quick_xml::events::BytesStart) -> bool {
    element_depth == 2 && e.name().as_ref() == b"profile"
}

/// A `<profile>`'s trimmed `name` attribute; empty without one (the default profile).
pub(super) fn profile_name(e: &quick_xml::events::BytesStart) -> String {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == b"name")
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.trim().to_owned()))
        .unwrap_or_default()
}

/// The root's `<profile>` children. `Err` for XML that doesn't parse or ends early (Windower may
/// be rewriting the file).
fn xml_profiles(xml: &[u8]) -> Result<Vec<XmlProfile>, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buffer = Vec::new();
    let mut profiles = Vec::new();
    let mut depth = 0usize;
    // Inside a profile: the profile, and the child element whose text is being collected.
    let mut profile: Option<XmlProfile> = None;
    let mut field: Option<(String, String)> = None;
    let bad = |e: &dyn std::fmt::Display| tf("settings.xml is not valid XML ({error})", &[("error", &e)]);
    loop {
        let event = reader.read_event_into(&mut buffer).map_err(|e| bad(&e))?;
        match event {
            Event::Start(e) => {
                depth += 1;
                if is_root_profile(depth, &e) {
                    profile = Some(XmlProfile { name: profile_name(&e), fields: Vec::new() });
                } else if depth == 3 && profile.is_some() {
                    field = Some((String::from_utf8_lossy(e.name().as_ref()).into_owned(), String::new()));
                }
            }
            Event::Empty(e) => {
                if is_root_profile(depth + 1, &e) {
                    profiles.push(XmlProfile { name: profile_name(&e), fields: Vec::new() });
                } else if depth == 2 {
                    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                    if let Some(p) = profile.as_mut() {
                        p.fields.push((name, String::new()));
                    }
                }
            }
            Event::Text(t) => {
                if let Some((_, text)) = field.as_mut() {
                    text.push_str(&t.xml10_content().map_err(|e| bad(&e))?);
                }
            }
            Event::CData(t) => {
                if let Some((_, text)) = field.as_mut() {
                    text.push_str(&t.decode().map_err(|e| bad(&e))?);
                }
            }
            Event::GeneralRef(r) => {
                if let Some((_, text)) = field.as_mut() {
                    let resolved = match r.resolve_char_ref().map_err(|e| bad(&e))? {
                        Some(c) => c,
                        None => match r.decode().map_err(|e| bad(&e))?.as_ref() {
                            "lt" => '<',
                            "gt" => '>',
                            "amp" => '&',
                            "apos" => '\'',
                            "quot" => '"',
                            _ => return Err(bad(&"unknown entity")),
                        },
                    };
                    text.push(resolved);
                }
            }
            Event::End(_) => {
                if depth == 3 {
                    if let (Some(p), Some(f)) = (profile.as_mut(), field.take()) {
                        p.fields.push(f);
                    }
                } else if depth == 2 {
                    if let Some(p) = profile.take() {
                        profiles.push(p);
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if depth != 0 {
        return Err(t("settings.xml ends early").into());
    }
    Ok(profiles)
}

/// A profile as the picker shows it: its trimmed name ("" = the default) and only its explicit
/// `<alwaysenablegamepad>`. Everything else in the file (other tools keep plaintext passwords
/// there) is dropped here.
pub(crate) struct ProfileSummary {
    pub name: String,
    pub gamepad: Option<bool>,
}

/// The root's `<profile>` children in file order, reduced to [`ProfileSummary`].
pub(crate) fn profile_summaries(xml: &[u8]) -> Result<Vec<ProfileSummary>, String> {
    Ok(xml_profiles(xml)?.into_iter().map(|p| ProfileSummary { gamepad: explicit_gamepad(&p.fields), name: p.name }).collect())
}

/// What settings.xml says, independent of how it is written: every element, attribute and
/// non-blank text in order, digested. Windower's exit save rewrites the file (new mtime)
/// from its start-up snapshot; that is no edit (park fingerprint, 07d §1.5). `None` while the
/// file doesn't parse or ends early (a save in progress).
pub(crate) fn settings_digest(xml: &[u8]) -> Option<u64> {
    use quick_xml::events::Event;
    use std::hash::{Hash, Hasher};
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buffer = Vec::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut depth = 0usize;
    let element = |kind: u8, e: &quick_xml::events::BytesStart, hasher: &mut std::collections::hash_map::DefaultHasher| -> Option<()> {
        kind.hash(hasher);
        e.name().as_ref().hash(hasher);
        for attribute in e.attributes() {
            let attribute = attribute.ok()?;
            (attribute.key.as_ref(), attribute.value.as_ref()).hash(hasher);
        }
        Some(())
    };
    loop {
        match reader.read_event_into(&mut buffer).ok()? {
            Event::Start(e) => { depth += 1; element(0, &e, &mut hasher)?; }
            Event::Empty(e) => element(1, &e, &mut hasher)?,
            Event::End(e) => { depth = depth.checked_sub(1)?; (2u8, e.name().as_ref()).hash(&mut hasher); }
            Event::Text(text) => {
                let text: &[u8] = &text;
                let text = text.trim_ascii();
                if !text.is_empty() { (3u8, text).hash(&mut hasher); }
            }
            Event::CData(text) => (4u8, &*text as &[u8]).hash(&mut hasher),
            Event::GeneralRef(name) => (5u8, &*name as &[u8]).hash(&mut hasher),
            Event::Eof => break,
            // The declaration, comments and processing instructions say nothing to Windower.
            _ => {}
        }
        buffer.clear();
    }
    (depth == 0).then(|| hasher.finish())
}

/// `Profile.Get(name)` after `Profile.Load`: the profile matching `name` ignoring case, the
/// unnamed default for "". Refuse a missing explicit choice instead of accepting
/// Windower's fallback, which would silently replace the user's chosen layout.
pub(crate) fn windower_profile(xml: Option<&[u8]>, name: &str) -> Result<WindowerProfile, String> {
    let profiles = match xml {
        Some(xml) => xml_profiles(xml)?,
        None => Vec::new(),
    };
    let name = name.trim();
    let default = || profiles.iter().find(|p| p.name.is_empty());
    let chosen = if name.is_empty() { default() } else {
        let lower = name.to_lowercase();
        Some(profiles.iter().find(|p| p.name.to_lowercase() == lower)
            .ok_or_else(|| tf("Windower profile {profile} was not found in settings.xml. Choose an existing profile.", &[("profile", &name)]))?)
    };
    Ok(chosen.map_or_else(WindowerProfile::constructed, |p| WindowerProfile::loaded(&p.fields)))
}

/// A value for the session's `ffxi_settings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Dword(u32),
    Text(String),
}

/// `(int)f` on .NET Framework x86: truncation, and 0x80000000 for NaN and out-of-range values.
fn dotnet_int(f: f32) -> i32 {
    if f.is_nan() || f >= 2_147_483_648.0 || f < -2_147_483_648.0 {
        i32::MIN
    } else {
        f as i32
    }
}

/// `Convert.ToInt32(float)`: round half to even; `None` where .NET would throw.
fn convert_to_int32(f: f32) -> Option<i32> {
    let d = f64::from(f);
    if d.is_nan() || d < -2_147_483_648.5 || d >= 2_147_483_647.5 {
        return None;
    }
    Some(d.round_ties_even() as i32)
}

/// What `Injector.Launch` + `Profile.Apply` write for `profile`. `display` is the primary
/// display's registry mode, used when the profile has no resolution (without it, the
/// resolution values are left to the registry); `padmode000` is the registry's current value.
pub(crate) fn windower_values(profile: &WindowerProfile, display: Option<(i32, i32)>, padmode000: Option<&str>) -> Vec<(&'static str, Value)> {
    let mut p = profile.clone();
    let size_known = if p.width == 0 || p.height == 0 {
        match display {
            Some((w, h)) => {
                p.width = w;
                p.height = h;
                true
            }
            None => false,
        }
    } else {
        true
    };
    if dotnet_int(p.width as f32 / p.ui_scale) <= 0 || dotnet_int(p.height as f32 / p.ui_scale) <= 0 {
        p.ui_scale = 1.0;
    }
    // The SuperSampling getter: <= 0 reads as 1.
    let super_sampling = if p.super_sampling <= 0.0 { 1.0 } else { p.super_sampling };
    let dword = |v: i32| Value::Dword(v as u32);
    let mut out = vec![("0000", dword(p.mip_map_level))];
    if size_known {
        out.push(("0001", dword(p.width)));
        out.push(("0002", dword(p.height)));
        for (name, side) in [("0003", p.width), ("0004", p.height)] {
            if let Some(v) = convert_to_int32(f32::max(1.0, side as f32 * super_sampling)) {
                out.push((name, dword(v)));
            }
        }
    }
    out.push(("0007", dword(p.sound as i32)));
    out.push(("0017", dword(p.bump_mapping as i32)));
    out.push(("0021", dword(p.hardware_mouse as i32)));
    out.push(("0028", Value::Dword(p.gamma.to_bits())));
    let mode = match p.window_mode {
        WindowMode::BorderlessWindow => 3,
        WindowMode::Window => 1,
        WindowMode::Fullscreen => 0,
    };
    out.push(("0034", dword(mode)));
    out.push(("0035", dword(p.always_play_sound as i32)));
    if size_known {
        let ui = |side: i32| if p.ui_scale != 1.0 { dotnet_int(side as f32 / p.ui_scale) } else { 0 };
        out.push(("0037", dword(ui(p.width))));
        out.push(("0038", dword(ui(p.height))));
    }
    let mut pad: Vec<String> = padmode000.unwrap_or("0,0,0,0,0,0").split(',').map(str::to_owned).collect();
    while pad.len() < 6 {
        pad.push("0".into());
    }
    pad[4] = if p.always_enable_gamepad { "1" } else { "0" }.into();
    out.push(("padmode000", Value::Text(pad.join(","))));
    out
}

/// The `ffxi_settings` object's JSON text.
pub(crate) fn to_json(values: &[(&str, Value)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = values
        .iter()
        .map(|(name, value)| {
            let v = match value {
                Value::Dword(d) => serde_json::Value::from(*d),
                Value::Text(t) => serde_json::Value::from(t.as_str()),
            };
            ((*name).to_owned(), v)
        })
        .collect();
    serde_json::Value::Object(map).to_string()
}

/// A Windower launch's settings, ready for the session line.
#[derive(Debug, Clone)]
pub(crate) struct WindowerSettings {
    pub json: String,
    pub count: usize,
    /// Something the launch log should say (values left to the registry).
    pub note: Option<String>,
}

/// Reads `<windower>\settings.xml` (missing: Windower's defaults) and computes the profile's
/// values. A file that doesn't parse is read again a few times, since an exiting Windower.exe
/// rewrites it in place.
pub(crate) fn for_windower(windower: &Path, profile: &str) -> Result<WindowerSettings, String> {
    let path = windower.join("settings.xml");
    let mut attempt = 0;
    let chosen = loop {
        let read = match std::fs::read(&path) {
            Ok(b) if b.len() > 4 * 1024 * 1024 => return Err(tf("{path} is too large to be Windower's settings.", &[("path", &path.display())])),
            Ok(b) => windower_profile(Some(&b), profile),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => windower_profile(None, profile),
            Err(_) => Err(t("Could not read Windower's settings").to_owned()),
        };
        match read {
            Ok(p) => break p,
            Err(_) if attempt < 5 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(tf("{error}: {path}", &[("error", &e), ("path", &path.display())])),
        }
    };
    let display = primary_display_mode();
    let values = windower_values(&chosen, display, registry_padmode().as_deref());
    let note = (display.is_none() && !values.iter().any(|(n, _)| *n == "0001"))
        .then(|| "The profile has no resolution and the primary display's mode is unknown; the game takes its resolution from the registry.".to_owned());
    Ok(WindowerSettings { json: to_json(&values), count: values.len(), note })
}

pub(crate) fn menu_settings(size: Option<(u32, u32)>) -> WindowerSettings {
    // An empty object prepares live controller updates without overriding the
    // initial registry or a launch tool's own controller configuration.
    let values = size.map_or_else(Vec::new, |(w, h)| vec![("0037", Value::Dword(w)), ("0038", Value::Dword(h))]);
    WindowerSettings { json: to_json(&values), count: values.len(), note: None }
}

/// `settings` with the launcher features' values (`features::runtime::launch_values`: a
/// remembered window's resolution and mode, background mute) set over the computed ones. A
/// resolution that replaces a known one scales its render size (`0003`/`0004`) and a profile's
/// menu size (`0037`/`0038`, when set) by the same factor, so the profile's proportions hold.
pub(crate) fn with_overrides(settings: WindowerSettings, overrides: &[(&str, u32)]) -> WindowerSettings {
    if overrides.is_empty() { return settings; }
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(&settings.json) else { return settings };
    for (side, render, menu) in [("0001", "0003", "0037"), ("0002", "0004", "0038")] {
        let Some(&(_, new)) = overrides.iter().find(|(name, _)| *name == side) else { continue };
        if let Some(old) = map.get(side).and_then(serde_json::Value::as_u64).filter(|old| *old > 0) {
            for scaled in [render, menu] {
                if let Some(value) = map.get(scaled).and_then(serde_json::Value::as_u64).filter(|v| *v > 0) {
                    map.insert(scaled.to_owned(), serde_json::Value::from((value * u64::from(new) + old / 2) / old));
                }
            }
        }
    }
    for (name, value) in overrides { map.insert((*name).to_owned(), serde_json::Value::from(*value)); }
    let count = map.len();
    WindowerSettings { json: serde_json::Value::Object(map).to_string(), count, note: settings.note }
}

/// Adds `"ffxi_settings":<json>` to a `session` line (`{...}\n`). The old buffer is zeroized
/// when dropped; the new one is sized up front, so no reallocation leaves a copy behind.
pub(crate) fn splice(line: Zeroizing<Vec<u8>>, json: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    let body = line.strip_suffix(b"\n").unwrap_or(&line);
    let Some(open) = body.strip_suffix(b"}") else {
        return Err("The session line is not a JSON object.".into());
    };
    let field = format!(r#","ffxi_settings":{json}}}"#);
    let mut out = Zeroizing::new(Vec::with_capacity(open.len() + field.len() + 1));
    out.extend_from_slice(open);
    out.extend_from_slice(field.as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// The primary display's registry mode, as Windower's `Display.GetCurrentDisplayMode` reads it
/// (`EnumDisplaySettings(<primary>, ENUM_REGISTRY_SETTINGS)`).
pub(super) fn primary_display_mode() -> Option<(i32, i32)> {
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE, ENUM_REGISTRY_SETTINGS,
    };
    let mut index = 0;
    loop {
        let mut device: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        if unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut device, 0) } == 0 {
            return None;
        }
        index += 1;
        if device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE == 0 {
            continue;
        }
        let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if unsafe { EnumDisplaySettingsW(device.DeviceName.as_ptr(), ENUM_REGISTRY_SETTINGS, &mut mode) } == 0 {
            return None;
        }
        let (w, h) = (mode.dmPelsWidth as i32, mode.dmPelsHeight as i32);
        return (w > 0 && h > 0).then_some((w, h));
    }
}

/// Whether a `padmode000` value has its fifth field (background gamepad) on.
pub(crate) fn padmode_background(padmode: &str) -> bool {
    padmode.split(',').nth(4).is_some_and(|f| f.trim() == "1")
}

/// The registry's current `padmode000` (Windower keeps every field but the fifth).
pub(crate) fn registry_padmode() -> Option<String> {
    use crate::win::reg::{self, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY};
    let key = r"SOFTWARE\PlayOnlineUS\SquareEnix\FinalFantasyXI";
    let units = reg::string(HKEY_LOCAL_MACHINE, key, "padmode000", RRF_RT_REG_SZ | RRF_SUBKEY_WOW6432KEY, 256)?;
    Some(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(xml: &str, profile: &str, display: Option<(i32, i32)>, pad: Option<&str>) -> serde_json::Value {
        let p = windower_profile(Some(xml.as_bytes()), profile).unwrap();
        serde_json::from_str(&to_json(&windower_values(&p, display, pad))).unwrap()
    }

    const XML: &str = "<?xml version=\"1.1\" encoding=\"utf-8\" standalone=\"no\"?>\r\n<settings>\r\n  <launcher><branch>stable</branch></launcher>\r\n  <autoload><addon>gearswap</addon></autoload>\r\n  <profile name=\"\">\r\n    <consolekey>Insert</consolekey>\r\n  </profile>\r\n  <profile name=\" Main \">\r\n    <windowmode>Window</windowmode>\r\n    <width>1920</width>\r\n    <height>1080</height>\r\n    <mipmaplevel>3</mipmaplevel>\r\n    <gamma>-0.25</gamma>\r\n    <uiscale>1.5</uiscale>\r\n    <hardwaremouse>False</hardwaremouse>\r\n    <supersampling>2</supersampling>\r\n    <bumpmapping>true</bumpmapping>\r\n    <sound>false</sound>\r\n    <alwaysplaysound>false</alwaysplaysound>\r\n    <alwaysenablegamepad>false</alwaysenablegamepad>\r\n    <password>hunter2</password>\r\n  </profile>\r\n  <profile name=\"Mule\">\r\n    <windowmode>fullscreen</windowmode>\r\n    <width>1280</width>\r\n    <height>720</height>\r\n  </profile>\r\n</settings>\r\n";

    #[test]
    fn a_full_profile_maps_like_profile_apply() {
        let v = values(XML, "main", Some((2560, 1440)), Some("1,0,1,0,1,0,7"));
        assert_eq!(
            v,
            serde_json::json!({
                "0000": 3, "0001": 1920, "0002": 1080, "0003": 3840, "0004": 2160,
                "0007": 0, "0017": 1, "0021": 0, "0028": (-0.25f32).to_bits(),
                "0034": 1, "0035": 0, "0037": 1280, "0038": 720,
                "padmode000": "1,0,1,0,0,0,7"
            })
        );
        // Nothing but these names goes to the game; the profile's password never leaves.
        assert!(!to_json(&windower_values(&windower_profile(Some(XML.as_bytes()), "Main").unwrap(), None, None)).contains("hunter2"));
    }

    #[test]
    fn defaults_follow_windower() {
        // A sparse profile: Load's defaults (borderless, sound on, scale 1), except background
        // gamepad, which stays off unless the profile says so (see the incident test below).
        let v = values(XML, "MULE", None, None);
        assert_eq!(
            v,
            serde_json::json!({
                "0000": 0, "0001": 1280, "0002": 720, "0003": 1280, "0004": 720,
                "0007": 1, "0017": 0, "0021": 1, "0028": 0,
                "0034": 0, "0035": 1, "0037": 0, "0038": 0,
                "padmode000": "0,0,0,0,0,0"
            })
        );
        // No resolution: the primary display's mode, borderless.
        let v = values(XML, "", Some((2560, 1440)), Some("0,0"));
        assert_eq!((v["0001"].as_u64(), v["0002"].as_u64(), v["0034"].as_u64()), (Some(2560), Some(1440), Some(3)));
        assert_eq!(v["padmode000"], "0,0,0,0,0,0");
        // A stale explicit selection must not silently reset the user's layout.
        assert!(windower_profile(Some(XML.as_bytes()), "Nobody").is_err());
        assert!(windower_profile(None, "Main").is_err());
        let none = windower_values(&windower_profile(None, "").unwrap(), Some((800, 600)), None);
        let none: serde_json::Value = serde_json::from_str(&to_json(&none)).unwrap();
        assert_eq!((none["0001"].as_u64(), none["0037"].as_u64()), (Some(800), Some(0)));
        // Profile.Default creates the unnamed profile even when only named
        // profiles exist; it does not choose the first named profile.
        let named_only = "<settings><profile name=\"Main\"><width>1920</width><height>1080</height><uiscale>1.5</uiscale></profile></settings>";
        assert_eq!(values(named_only, "", Some((800, 600)), None), none);
        // Display unknown and no resolution: those values stay with the registry.
        let v = values(XML, "", None, None);
        for name in ["0001", "0002", "0003", "0004", "0037", "0038"] {
            assert!(v.get(name).is_none(), "{name}");
        }
        assert_eq!(v["0034"], 3);
    }

    /// Incident: every account silently got the default profile, and Windower's own default of
    /// background gamepad on came with it. Only an explicit `true` turns it on.
    #[test]
    fn background_gamepad_is_on_only_when_the_profile_says_so() {
        let pad = |xml: Option<&str>, profile: &str| {
            let p = windower_profile(xml.map(str::as_bytes), profile).unwrap();
            let v: serde_json::Value = serde_json::from_str(&to_json(&windower_values(&p, None, Some("1,1,0,0,1,0")))).unwrap();
            v["padmode000"].as_str().unwrap().to_owned()
        };
        // The unnamed default profile without the element, and a missing settings.xml.
        assert_eq!(pad(Some(XML), ""), "1,1,0,0,0,0");
        assert_eq!(pad(None, ""), "1,1,0,0,0,0");
        // A named profile settings.xml doesn't have is constructed, never on.
        assert_eq!(pad(Some("<settings/>"), ""), "1,1,0,0,0,0");
        // Explicit false, an unparsable value and an empty element are off; true in any case is on.
        let one = |inner: &str| format!("<settings><profile name=\"p\">{inner}</profile></settings>");
        for off in ["<alwaysenablegamepad>false</alwaysenablegamepad>", "<alwaysenablegamepad>yes</alwaysenablegamepad>", "<alwaysenablegamepad/>", "<alwaysenablegamepad>1</alwaysenablegamepad>"] {
            assert_eq!(pad(Some(&one(off)), "p"), "1,1,0,0,0,0", "{off}");
        }
        assert_eq!(pad(Some(&one("<alwaysenablegamepad> True </alwaysenablegamepad>")), "p"), "1,1,0,0,1,0");
        // The first element wins, as Windower reads it.
        assert_eq!(pad(Some(&one("<alwaysenablegamepad>false</alwaysenablegamepad><alwaysenablegamepad>true</alwaysenablegamepad>")), "p"), "1,1,0,0,0,0");
        // Summaries carry the explicit value only, and nothing else of the file.
        let s = profile_summaries(XML.as_bytes()).unwrap();
        assert_eq!(s.iter().map(|p| (p.name.as_str(), p.gamepad)).collect::<Vec<_>>(), [("", None), ("Main", Some(false)), ("Mule", None)]);
        assert!(padmode_background("0,0,0,0,1,0") && !padmode_background("0,0,0,0,0,0") && !padmode_background("1") && !padmode_background(""));
    }

    #[test]
    fn element_parsing_matches_dotnet() {
        let xml = "<settings><profile name=\"x\"><width> 1600 </width><height>900</height><uiscale>bad</uiscale><windowmode>BORDERLESSWINDOW</windowmode><sound> TRUE </sound><supersampling>-1</supersampling><gamma>1.5e0</gamma></profile><profile name=\"x\"><width>1</width></profile></settings>";
        let v = values(xml, "x", None, None);
        assert_eq!((v["0001"].as_u64(), v["0003"].as_u64()), (Some(1600), Some(1600)), "bad uiscale = 1, supersampling <= 0 = 1");
        assert_eq!((v["0034"].as_u64(), v["0007"].as_u64(), v["0037"].as_u64()), (Some(3), Some(1), Some(0)));
        assert_eq!(v["0028"].as_u64(), Some(u64::from(1.5f32.to_bits())));
        // A UI scale that makes the menus 0 wide is reset to 1, as Injector.Launch does.
        let tiny = "<settings><profile name=\"\"><width>100</width><height>100</height><uiscale>1000</uiscale></profile></settings>";
        assert_eq!(values(tiny, "", None, None)["0037"], 0);
        // 0003 rounds half to even, as Convert.ToInt32 does.
        let half = "<settings><profile name=\"\"><width>5</width><height>7</height><supersampling>1.5</supersampling></profile></settings>";
        let v = values(half, "", None, None);
        assert_eq!((v["0003"].as_u64(), v["0004"].as_u64()), (Some(8), Some(10)));
        assert_eq!(convert_to_int32(2.5), Some(2));
        assert_eq!(convert_to_int32(f32::NAN), None);
        assert_eq!(dotnet_int(f32::INFINITY), i32::MIN);
        // Entities and CDATA.
        let ent = "<settings><profile name=\"a&amp;b\"><width>&#49;024</width><height><![CDATA[768]]></height></profile></settings>";
        let v = values(ent, "A&B", None, None);
        assert_eq!((v["0001"].as_u64(), v["0002"].as_u64()), (Some(1024), Some(768)));
    }

    #[test]
    fn half_written_settings_are_an_error() {
        let cut = &XML[..XML.find("<profile name=\"Mule\">").unwrap() + 30];
        assert!(windower_profile(Some(cut.as_bytes()), "Main").is_err());
        assert!(windower_profile(Some(b"<settings><profile></settings>"), "").is_err());
    }

    #[test]
    fn missing_settings_file_uses_windower_defaults() {
        let dir = std::env::temp_dir().join(format!("xi-ffxi-settings-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = for_windower(&dir, "").unwrap();
        let v: serde_json::Value = serde_json::from_str(&s.json).unwrap();
        assert_eq!(v["0034"], 3);
        assert_eq!(s.count, v.as_object().unwrap().len());
        std::fs::write(dir.join("settings.xml"), XML).unwrap();
        let v: serde_json::Value = serde_json::from_str(&for_windower(&dir, "Main").unwrap().json).unwrap();
        assert_eq!((v["0001"].as_u64(), v["0034"].as_u64()), (Some(1920), Some(1)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn feature_overrides_set_resolution_mode_and_mute_keeping_the_profile_proportions() {
        let main = for_profile_json(XML, "Main");
        let out = with_overrides(main.clone(), &[("0001", 1280), ("0002", 720), ("0034", 3), ("0035", 0)]);
        let v: serde_json::Value = serde_json::from_str(&out.json).unwrap();
        // 1920x1080 with 2x supersampling and a 1.5 UI scale, moved to 1280x720.
        assert_eq!((v["0001"].as_u64(), v["0002"].as_u64()), (Some(1280), Some(720)));
        assert_eq!((v["0003"].as_u64(), v["0004"].as_u64()), (Some(2560), Some(1440)));
        assert_eq!((v["0037"].as_u64(), v["0038"].as_u64()), (Some(853), Some(480)));
        assert_eq!((v["0034"].as_u64(), v["0035"].as_u64()), (Some(3), Some(0)));
        assert_eq!(v["padmode000"], serde_json::from_str::<serde_json::Value>(&main.json).unwrap()["padmode000"]);
        assert_eq!(out.count, v.as_object().unwrap().len());
        // Other launch methods serve only the menu size: the resolution is added, nothing scaled.
        let menu = with_overrides(menu_settings(Some((1024, 576))), &[("0001", 1280), ("0002", 720)]);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&menu.json).unwrap(), serde_json::json!({"0001":1280,"0002":720,"0037":1024,"0038":576}));
        // No overrides: unchanged.
        assert_eq!(with_overrides(main.clone(), &[]).json, main.json);
    }

    fn for_profile_json(xml: &str, profile: &str) -> WindowerSettings {
        let values = windower_values(&windower_profile(Some(xml.as_bytes()), profile).unwrap(), None, None);
        WindowerSettings { json: to_json(&values), count: values.len(), note: None }
    }

    #[test]
    fn splice_adds_the_field_to_the_session_object() {
        let line = Zeroizing::new(br#"{"type":"session","v":1,"content_ids":[]}"#.iter().copied().chain(Some(b'\n')).collect::<Vec<u8>>());
        let out = splice(line, r#"{"0001":1920,"padmode000":"0,0,0,0,1,0"}"#).unwrap();
        assert_eq!(out.last(), Some(&b'\n'));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ffxi_settings"], serde_json::json!({"0001":1920,"padmode000":"0,0,0,0,1,0"}));
        assert_eq!(v["type"], "session");
        assert!(splice(Zeroizing::new(b"[1]\n".to_vec()), "{}").is_err());
    }

    #[test]
    fn fresh_windower_settings_read_saved_profile_edits_without_changing_previous_snapshot() {
        let dir = std::env::temp_dir().join(format!("xi-fresh-profile-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.xml"), XML).unwrap();
        let previous = for_windower(&dir, "Main").unwrap();
        let before: serde_json::Value = serde_json::from_str(&previous.json).unwrap();
        assert_eq!((before["0037"].as_u64(), before["0038"].as_u64()), (Some(1280), Some(720)));
        let edited = XML.replace("<uiscale>1.5</uiscale>", "<uiscale>2</uiscale>");
        std::fs::write(dir.join("settings.xml"), &edited).unwrap();
        let fresh = for_windower(&dir, "Main").unwrap();
        let after: serde_json::Value = serde_json::from_str(&fresh.json).unwrap();
        assert_eq!((after["0037"].as_u64(), after["0038"].as_u64()), (Some(960), Some(540)));
        assert_eq!(fresh.count, after.as_object().unwrap().len());
        assert_eq!(serde_json::from_str::<serde_json::Value>(&previous.json).unwrap(), before);
        assert_eq!(std::fs::read_to_string(dir.join("settings.xml")).unwrap(), edited);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retained_menu_settings_for_other_launch_methods_only_set_the_menu_size() {
        let ashita: serde_json::Value = serde_json::from_str(&menu_settings(Some((1024, 576))).json).unwrap();
        assert_eq!(ashita, serde_json::json!({"0037":1024,"0038":576}));
        let stock = menu_settings(None);
        assert_eq!(stock.json, "{}");
        assert_eq!(stock.count, 0);
    }
}
