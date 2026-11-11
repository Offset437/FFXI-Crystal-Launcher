//! The features' saved choices: `iced-launcher/features.json`, beside settings.json and kept the
//! same way (serde defaults for every field, a 1 MiB cap, atomic replacement, a damaged file
//! preserved before it is replaced). It is a separate file so that settings.json stays exactly as
//! older Crystals read and write it: an older Crystal ignores this file, and a newer field is
//! dropped by no one, because unknown fields are kept (`extra`).

use super::{backups, hotkeys, layouts, notify, perf, playtime, recovery};
use crate::backend::Config;
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Every feature's saved settings. Each section's defaults are what a missing file means.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FeatureSettings {
    #[serde(default, deserialize_with = "section")]
    pub layouts: layouts::Settings,
    #[serde(default, deserialize_with = "section")]
    pub hotkeys: hotkeys::Settings,
    #[serde(default, deserialize_with = "section")]
    pub recovery: recovery::Settings,
    #[serde(default, deserialize_with = "section")]
    pub backups: backups::Settings,
    #[serde(default, deserialize_with = "section")]
    pub notify: notify::Settings,
    #[serde(default, deserialize_with = "section")]
    pub perf: perf::Settings,
    #[serde(default, deserialize_with = "section")]
    pub playtime: playtime::Settings,
    /// The cycle keys are the user's own. Missing (false) in a file a Crystal up to 1.2.4
    /// wrote, where Ctrl+Tab and Ctrl+Shift+Tab may be the defaults those versions set by
    /// themselves: `load` clears them, once, and sets this. It is a field of the file, not of
    /// the hotkeys, so an older Crystal saving the file keeps it (`extra`) and a chord chosen
    /// since is not cleared again.
    #[serde(default)]
    pub no_default_hotkeys: bool,
    /// Fields a newer Crystal wrote: kept as they were on every save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One section of the file: one that does not read (an unknown value in an enum, say) falls back
/// to its defaults instead of failing the whole file.
fn section<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    Ok(T::deserialize(serde_json::Value::deserialize(d)?).unwrap_or_default())
}

impl Default for FeatureSettings {
    fn default() -> Self {
        Self {
            layouts: Default::default(),
            hotkeys: Default::default(),
            recovery: Default::default(),
            backups: Default::default(),
            notify: Default::default(),
            perf: Default::default(),
            playtime: Default::default(),
            no_default_hotkeys: true,
            extra: Default::default(),
        }
    }
}

const LIMIT: usize = 1024 * 1024;

pub(crate) fn path(config: &Config) -> PathBuf {
    config.experiment.join("iced-launcher/features.json")
}

/// The saved settings; defaults when the file is missing. `Err` for a file that exists but
/// cannot be used: the caller keeps defaults in memory and must not save over it unasked.
pub(crate) fn load(config: &Config) -> Result<FeatureSettings, String> {
    load_from(&path(config))
}

pub(crate) fn load_from(path: &Path) -> Result<FeatureSettings, String> {
    load_noting(path).map(|(settings, _)| settings)
}

/// The saved settings, and whether the old default cycle keys were just cleared from them
/// (`FeatureSettings::no_default_hotkeys`): the caller saves them so that it happens once.
pub(crate) fn load_noting(path: &Path) -> Result<(FeatureSettings, bool), String> {
    let Some(bytes) = read(path)? else { return Ok((FeatureSettings::default(), false)) };
    let mut settings: FeatureSettings = serde_json::from_slice(&bytes).map_err(|_| t("Feature settings could not be read."))?;
    let first = !std::mem::replace(&mut settings.no_default_hotkeys, true);
    let cleared = first && settings.hotkeys.clear_old_defaults();
    Ok((settings, cleared))
}

pub(crate) fn save(config: &Config, settings: &FeatureSettings) -> Result<(), String> {
    save_to(&path(config), settings)
}

pub(crate) fn save_to(path: &Path, settings: &FeatureSettings) -> Result<(), String> {
    use std::io::Write;
    let data = serde_json::to_vec_pretty(settings).map_err(|_| t("Could not encode feature settings."))?;
    if data.len() > LIMIT { return Err(t("Feature settings are too large to save.").into()); }
    std::fs::create_dir_all(path.parent().ok_or(t("Could not create the launcher settings folder."))?)
        .map_err(|_| t("Could not create the launcher settings folder."))?;
    if let Some(previous) = read(path)? {
        if serde_json::from_slice::<FeatureSettings>(&previous).is_err() {
            let backup = path.with_extension(format!("invalid-{}.json", crate::backend::unique_id()?));
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(backup)
                .map_err(|_| t("Could not preserve the damaged feature settings file; it was kept unchanged."))?;
            file.write_all(&previous).and_then(|_| file.sync_all())
                .map_err(|_| t("Could not preserve the damaged feature settings file; it was kept unchanged."))?;
        }
    }
    crate::fsutil::write_atomic(path, &data).map_err(|_| t("Could not write feature settings.").into())
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(t("Feature settings are unavailable.").into()),
    };
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1).read_to_end(&mut bytes).map_err(|_| t("Feature settings are unavailable."))?;
    if bytes.len() > LIMIT { return Err(t("Feature settings file is too large; it was kept unchanged.").into()); }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xi-features-store-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_or_empty_file_means_defaults_and_unknown_fields_survive_a_save() {
        let dir = temp();
        let file = dir.join("features.json");
        assert_eq!(load_from(&file).unwrap(), FeatureSettings::default());
        std::fs::write(&file, "{}").unwrap();
        assert_eq!(load_from(&file).unwrap(), FeatureSettings::default());
        // A newer Crystal's section is kept by this one.
        std::fs::write(&file, r#"{"future":{"x":1},"recovery":{"auto_relaunch":true}}"#).unwrap();
        let loaded = load_from(&file).unwrap();
        assert!(loaded.recovery.auto_relaunch);
        save_to(&file, &loaded).unwrap();
        let again: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(again["future"]["x"], 1);
        assert_eq!(load_from(&file).unwrap(), loaded);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 1.2.5: no hotkey is set by default. A file from an older Crystal has the cycle keys
    /// that are still the old defaults cleared, once; chords chosen since are the user's.
    #[test]
    fn the_old_default_cycle_keys_are_cleared_once_from_an_older_file() {
        use crate::features::hotkeys::Hotkey;
        let dir = temp();
        let file = dir.join("features.json");
        // What 1.2.4 wrote for someone who never set a hotkey, with an account hotkey of theirs.
        std::fs::write(&file, r#"{"hotkeys":{"cycle":"Ctrl+Tab","cycle_back":"Ctrl+Shift+Tab","accounts":{"k":"Ctrl+Alt+1"}},"recovery":{"auto_relaunch":true}}"#).unwrap();
        let (loaded, cleared) = load_noting(&file).unwrap();
        assert!(cleared && loaded.no_default_hotkeys);
        assert_eq!((loaded.hotkeys.cycle, loaded.hotkeys.cycle_back), (None, None));
        assert_eq!(loaded.hotkeys.accounts["k"], Hotkey::parse("Ctrl+Alt+1").unwrap());
        assert!(loaded.recovery.auto_relaunch, "nothing else changes");
        // Until it is saved the file reads the same way again.
        assert_eq!(load_noting(&file).unwrap(), (loaded.clone(), true));
        // Saved: the file says it is done, and both keys are written as off (an older Crystal
        // reads a missing one as Ctrl+Tab).
        save_to(&file, &loaded).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(saved["no_default_hotkeys"], true);
        assert!(saved["hotkeys"]["cycle"].is_null() && saved["hotkeys"]["cycle_back"].is_null());
        assert_eq!(load_noting(&file).unwrap(), (loaded.clone(), false));
        // Ctrl+Tab chosen afterwards is the user's: it stays, start after start.
        let mut chosen = loaded;
        chosen.hotkeys.cycle = Hotkey::parse("Ctrl+Tab");
        chosen.hotkeys.cycle_back = Hotkey::parse("Ctrl+Shift+Tab");
        save_to(&file, &chosen).unwrap();
        assert_eq!(load_noting(&file).unwrap(), (chosen.clone(), false));
        // An older Crystal saving the file keeps the mark with the fields it does not know.
        let mut older: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        older.insert("notify".into(), serde_json::json!({ "enabled": false }));
        std::fs::write(&file, serde_json::to_vec(&older).unwrap()).unwrap();
        let again = load_from(&file).unwrap();
        assert_eq!((again.hotkeys.cycle, again.notify.enabled), (Hotkey::parse("Ctrl+Tab"), false));
        // A custom chord in an older file is kept; only the key still at its old default goes.
        std::fs::write(&file, r#"{"hotkeys":{"cycle":"Ctrl+Alt+Right","cycle_back":"Ctrl+Shift+Tab"}}"#).unwrap();
        let (own, cleared) = load_noting(&file).unwrap();
        assert!(cleared);
        assert_eq!((own.hotkeys.cycle, own.hotkeys.cycle_back), (Hotkey::parse("Ctrl+Alt+Right"), None));
        // Hotkeys already off, or all the user's own: nothing to clear, and the file is marked.
        for text in [r#"{"hotkeys":{"cycle":null,"cycle_back":null}}"#, r#"{"hotkeys":{"cycle":"F9","cycle_back":"Ctrl+Alt+Left"}}"#, "{}"] {
            std::fs::write(&file, text).unwrap();
            let (settings, cleared) = load_noting(&file).unwrap();
            assert!(!cleared && settings.no_default_hotkeys, "{text}");
        }
        // No file at all: nothing set, nothing to clear.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(load_noting(&file).unwrap(), (FeatureSettings::default(), false));
        assert!(FeatureSettings::default().no_default_hotkeys);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_unreadable_hotkey_or_section_does_not_wipe_the_rest() {
        let dir = temp();
        let file = dir.join("features.json");
        std::fs::write(&file, r#"{"hotkeys":{"cycle":"Ctrl+Bogus","cycle_back":"F9","accounts":{"a":"nope","b":"Ctrl+Alt+1"}},"perf":{"background":"maybe"},"recovery":{"auto_relaunch":true}}"#).unwrap();
        let loaded = load_from(&file).unwrap();
        assert_eq!(loaded.hotkeys.cycle, None);
        assert_eq!(loaded.hotkeys.cycle_back, crate::features::hotkeys::Hotkey::parse("F9"));
        assert_eq!(loaded.hotkeys.accounts.keys().collect::<Vec<_>>(), ["b"]);
        assert_eq!(loaded.perf, Default::default());
        assert!(loaded.recovery.auto_relaunch);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_is_an_error_and_is_preserved_before_a_save_replaces_it() {
        let dir = temp();
        let file = dir.join("features.json");
        std::fs::write(&file, b"{not json").unwrap();
        assert!(load_from(&file).is_err());
        save_to(&file, &FeatureSettings::default()).unwrap();
        let kept: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("invalid-")).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read(kept[0].path()).unwrap(), b"{not json");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
