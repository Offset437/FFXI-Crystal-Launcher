//! The game's settings as values: the numbered entries under
//! `...\SquareEnix\FinalFantasyXI`, their meanings and defaults.
//!
//! Meanings come from the community's documentation of the key (Ashita v4's
//! boot-config reference, FFXIclopedia's registry notes) and from what the
//! native client's own sources do with them (host64 `modern.c`, `signin.c`,
//! `user32.h`). Values the page does not manage are read, kept and backed up
//! but never changed.
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One stored value, as the registry types it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Data {
    Dword(u32),
    Text(String),
    Binary(Vec<u8>),
}

/// Value name ("0001", "padmode000", ...) to its data.
pub(crate) type Values = BTreeMap<String, Data>;

// ---- the managed entries ----
pub(crate) const MIP_MAPPING: &str = "0000";
pub(crate) const WINDOW_W: &str = "0001";
pub(crate) const WINDOW_H: &str = "0002";
pub(crate) const BACKGROUND_W: &str = "0003";
pub(crate) const BACKGROUND_H: &str = "0004";
pub(crate) const SOUND: &str = "0007";
pub(crate) const ENV_ANIMATION: &str = "0011";
pub(crate) const BUMP_MAPPING: &str = "0017";
pub(crate) const TEXTURE_COMPRESSION: &str = "0018";
pub(crate) const MAP_COMPRESSION: &str = "0019";
pub(crate) const HARDWARE_MOUSE: &str = "0021";
pub(crate) const OPENING_MOVIE: &str = "0022";
pub(crate) const SIMPLE_CREATION: &str = "0023";
pub(crate) const GAMMA: &str = "0028";
pub(crate) const MAX_SOUNDS: &str = "0029";
pub(crate) const WINDOW_MODE: &str = "0034";
pub(crate) const SOUND_UNFOCUSED: &str = "0035";
pub(crate) const FONT_TYPE: &str = "0036";
pub(crate) const MENU_W: &str = "0037";
pub(crate) const MENU_H: &str = "0038";
pub(crate) const STABILIZATION: &str = "0040";
pub(crate) const SCREENSHOT_DIR: &str = "0042";
pub(crate) const SCREENSHOT_SCREEN_RES: &str = "0043";
pub(crate) const KEEP_ASPECT: &str = "0044";

/// Every entry the page edits. Anything else under the key is left alone.
pub(crate) const MANAGED: [&str; 24] = [
    MIP_MAPPING,
    WINDOW_W,
    WINDOW_H,
    BACKGROUND_W,
    BACKGROUND_H,
    SOUND,
    ENV_ANIMATION,
    BUMP_MAPPING,
    TEXTURE_COMPRESSION,
    MAP_COMPRESSION,
    HARDWARE_MOUSE,
    OPENING_MOVIE,
    SIMPLE_CREATION,
    GAMMA,
    MAX_SOUNDS,
    WINDOW_MODE,
    SOUND_UNFOCUSED,
    FONT_TYPE,
    MENU_W,
    MENU_H,
    STABILIZATION,
    SCREENSHOT_DIR,
    SCREENSHOT_SCREEN_RES,
    KEEP_ASPECT,
];

/// Labels for the enumerated entries, in value order.
/// The text shown is translated, the stored value (the number) never is.
/// `0034`: the three modes FINAL FANTASY XI Config offers (Fullscreen, Window Mode, Borderless
/// Window), as it and Windower write them; 2 is no mode the game's tool sets, so a stored 2
/// shows as "Other (2)".
pub(crate) fn window_modes() -> [(u32, &'static str); 3] {
    [
        (0, t("Full screen")),
        (1, t("Windowed")),
        (3, t("Borderless window")),
    ]
}
pub(crate) fn mip_levels() -> [(u32, &'static str); 7] {
    [
        (0, t("Off")),
        (1, t("Level 1")),
        (2, t("Level 2")),
        (3, t("Level 3")),
        (4, t("Level 4")),
        (5, t("Level 5")),
        (6, t("Level 6 (best)")),
    ]
}
pub(crate) fn texture_levels() -> [(u32, &'static str); 3] {
    [
        (0, t("High compression")),
        (1, t("Low compression")),
        (2, t("Uncompressed (best)")),
    ]
}
pub(crate) fn map_levels() -> [(u32, &'static str); 2] {
    [(0, t("Compressed")), (1, t("Uncompressed (sharper)"))]
}
pub(crate) fn animation_levels() -> [(u32, &'static str); 3] {
    [(0, t("Off")), (1, t("Normal")), (2, t("Smooth"))]
}
pub(crate) fn font_types() -> [(u32, &'static str); 3] {
    [(0, t("Compressed")), (1, t("Uncompressed")), (2, t("High quality"))]
}

pub(crate) const MAX_SOUNDS_RANGE: (u32, u32) = (12, 20);
pub(crate) const MIN_WINDOW: (u32, u32) = (640, 480);
/// The smallest interface the native client's sign-in screen accepts.
pub(crate) const MIN_MENU: (u32, u32) = (512, 384);
/// Interface magnifications offered for the menu resolution.
pub(crate) const MENU_SCALES: [f32; 7] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

/// A sensible starting point for `desktop` (w, h): the native client's own
/// first-run defaults (`signin.c`), sized to the screen.
pub(crate) fn defaults(desktop: (u32, u32)) -> Values {
    let (w, h) = (desktop.0.max(MIN_WINDOW.0), desktop.1.max(MIN_WINDOW.1));
    let (mw, mh) = menu_for(w, h, default_menu_scale(h));
    let mut v = Values::new();
    let mut d = |k: &str, x: u32| {
        v.insert(k.into(), Data::Dword(x));
    };
    d(MIP_MAPPING, 6);
    d(WINDOW_W, w);
    d(WINDOW_H, h);
    d(BACKGROUND_W, 4096);
    d(BACKGROUND_H, 4096);
    d(SOUND, 1);
    d(ENV_ANIMATION, 1);
    d(BUMP_MAPPING, 1);
    d(TEXTURE_COMPRESSION, 1);
    d(MAP_COMPRESSION, 1);
    d(HARDWARE_MOUSE, 1);
    d(OPENING_MOVIE, 1);
    d(SIMPLE_CREATION, 0);
    d(GAMMA, 0f32.to_bits());
    d(MAX_SOUNDS, 12);
    d(WINDOW_MODE, 1);
    d(SOUND_UNFOCUSED, 1);
    d(FONT_TYPE, 0);
    d(MENU_W, mw);
    d(MENU_H, mh);
    d(STABILIZATION, 0);
    d(SCREENSHOT_SCREEN_RES, 0);
    d(KEEP_ASPECT, 1);
    v.insert(SCREENSHOT_DIR.into(), Data::Text(String::new()));
    v
}

/// The interface magnification that keeps the menus readable: about 1080
/// lines of interface at most, in the scale list's steps.
pub(crate) fn default_menu_scale(window_h: u32) -> f32 {
    let wanted = window_h as f32 / 1080.0;
    MENU_SCALES
        .iter()
        .copied()
        .rfind(|s| *s <= wanted.max(1.0) + 0.01)
        .unwrap_or(1.0)
}

/// The menu size for a window at an interface scale, rounded to even sizes as
/// the native client's Config > Display does.
pub(crate) fn menu_for(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let even = |x: f32| ((x.round() as u32) + 1) & !1;
    (even(w as f32 / scale), even(h as f32 / scale))
}

/// The interface scale a menu size gives a window (1.0 when unknown).
pub(crate) fn scale_of(window_h: u32, menu_h: u32) -> f32 {
    if menu_h == 0 {
        1.0
    } else {
        window_h as f32 / menu_h as f32
    }
}

pub(crate) fn dword(values: &Values, key: &str) -> Option<u32> {
    match values.get(key) {
        Some(Data::Dword(v)) => Some(*v),
        _ => None,
    }
}

pub(crate) fn text_value<'a>(values: &'a Values, key: &str) -> Option<&'a str> {
    match values.get(key) {
        Some(Data::Text(v)) => Some(v.as_str()),
        _ => None,
    }
}

/// Gamma is a float stored as its bits; -1.0 (darker) to 1.0 (brighter).
pub(crate) fn gamma_of(bits: u32) -> f32 {
    let g = f32::from_bits(bits);
    if g.is_finite() {
        g.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Gamma as stored: two decimals, so a slider's float noise never shows up as
/// a pending change.
pub(crate) fn gamma_bits(g: f32) -> u32 {
    let g = (g.clamp(-1.0, 1.0) * 100.0).round() / 100.0;
    // -0.0 and 0.0 are the same setting.
    if g == 0.0 {
        0
    } else {
        g.to_bits()
    }
}

/// The managed entries whose value differs between `original` and `edited`.
pub(crate) fn changed_keys(original: &Values, edited: &Values) -> Vec<&'static str> {
    MANAGED
        .iter()
        .copied()
        .filter(|k| original.get(*k) != edited.get(*k))
        .collect()
}

/// Checks a set of values before it is written.
pub(crate) fn validate(values: &Values) -> Result<(), String> {
    let get = |k| dword(values, k).unwrap_or(0);
    if get(WINDOW_W) < MIN_WINDOW.0 || get(WINDOW_H) < MIN_WINDOW.1 {
        return Err(t("The window resolution must be at least 640 × 480.").into());
    }
    // Windower uses a zero pair for automatic (1x) menus. It is a valid
    // retail setting, not a zero-sized interface; partial pairs are invalid.
    let automatic_menu = get(MENU_W) == 0 && get(MENU_H) == 0;
    if !automatic_menu && (get(MENU_W) < MIN_MENU.0 || get(MENU_H) < MIN_MENU.1) {
        return Err(t("The menu resolution must be at least 512 × 384.").into());
    }
    if get(BACKGROUND_W) < 256 || get(BACKGROUND_H) < 256 {
        return Err(t("The background resolution must be at least 256 × 256.").into());
    }
    if let Some(n) = dword(values, MAX_SOUNDS) {
        if !(MAX_SOUNDS_RANGE.0..=MAX_SOUNDS_RANGE.1).contains(&n) {
            return Err(t("Maximum sounds must be between 12 and 20.").into());
        }
    }
    if let Some(dir) = text_value(values, SCREENSHOT_DIR) {
        let dir = dir.trim();
        if !dir.is_empty() {
            let path = std::path::Path::new(dir);
            if !path.is_absolute() {
                return Err(t("Enter the screenshot folder as a full path, such as D:\\Pictures\\FFXI.").into());
            }
            if dir.len() > 200 {
                return Err(t("The screenshot folder path is too long for the game.").into());
            }
            if !path.is_dir() {
                return Err(t("The screenshot folder does not exist. Create it first, or leave the field empty to use the game's default.").into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_config_gamma_is_float_bits_with_two_decimals() {
        assert_eq!(gamma_bits(0.0), 0);
        assert_eq!(gamma_bits(-0.0), 0);
        assert_eq!(gamma_bits(1.0), 0x3f80_0000);
        assert_eq!(gamma_bits(-1.0), 0xbf80_0000);
        assert_eq!(gamma_bits(5.0), 0x3f80_0000);
        assert_eq!(gamma_of(gamma_bits(-0.4012)), -0.4);
        assert_eq!(gamma_of(0x7fc0_0000), 0.0); // NaN reads as neutral
    }

    #[test]
    fn game_config_menu_resolution_follows_interface_scale() {
        assert_eq!(menu_for(1920, 1080, 1.0), (1920, 1080));
        assert_eq!(menu_for(1920, 1080, 1.5), (1280, 720));
        assert_eq!(menu_for(3840, 2160, 2.0), (1920, 1080));
        // Odd results round up to even, as the native client stores them.
        assert_eq!(menu_for(1366, 768, 1.25), (1094, 614));
        assert_eq!(default_menu_scale(1080), 1.0);
        assert_eq!(default_menu_scale(1440), 1.25);
        assert_eq!(default_menu_scale(2160), 2.0);
        assert_eq!(default_menu_scale(720), 1.0);
        assert!((scale_of(2160, 1080) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn game_config_defaults_are_complete_and_valid() {
        let d = defaults((1920, 1080));
        for key in MANAGED {
            assert!(d.contains_key(key), "default for {key}");
        }
        assert_eq!(dword(&d, WINDOW_W), Some(1920));
        assert_eq!(dword(&d, MENU_H), Some(1080));
        validate(&d).unwrap();
        // A tiny desktop still yields a usable window.
        assert_eq!(dword(&defaults((320, 200)), WINDOW_W), Some(640));
    }

    #[test]
    fn game_config_changed_keys_only_reports_managed_differences() {
        let original: Values = [
            ("padmode000".to_string(), Data::Text("1,1,0,0,0,1".into())),
            ("bFirst".to_string(), Data::Binary(vec![0])),
            (MIP_MAPPING.to_string(), Data::Dword(6)),
            (WINDOW_W.to_string(), Data::Dword(3440)),
            (WINDOW_H.to_string(), Data::Dword(1440)),
            ("0031".to_string(), Data::Dword(0x3bc4_9ba6)),
        ]
        .into();
        let mut edited = original.clone();
        edited.insert("0031".into(), Data::Dword(1)); // unmanaged
        assert!(changed_keys(&original, &edited).is_empty());
        edited.insert(MIP_MAPPING.into(), Data::Dword(3));
        edited.insert(SOUND.into(), Data::Dword(1)); // absent before
        assert_eq!(changed_keys(&original, &edited), vec![MIP_MAPPING, SOUND]);
    }

    #[test]
    fn game_config_validation_rejects_unusable_values() {
        let mut v = defaults((1920, 1080));
        v.insert(WINDOW_W.into(), Data::Dword(320));
        assert!(validate(&v).is_err());
        let mut v = defaults((1920, 1080));
        v.insert(MAX_SOUNDS.into(), Data::Dword(40));
        assert!(validate(&v).is_err());
        let mut v = defaults((1920, 1080));
        v.insert(SCREENSHOT_DIR.into(), Data::Text("relative\\shots".into()));
        assert!(validate(&v).is_err());
        let dir = std::env::temp_dir();
        v.insert(SCREENSHOT_DIR.into(), Data::Text(dir.display().to_string()));
        validate(&v).unwrap();
        // Windower's automatic (0, 0) menu is valid; a partial pair is not.
        v.insert(MENU_W.into(), Data::Dword(0));
        v.insert(MENU_H.into(), Data::Dword(0));
        validate(&v).unwrap();
        v.insert(MENU_H.into(), Data::Dword(720));
        assert!(validate(&v).is_err());
    }
}