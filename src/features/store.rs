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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct FeatureSettings {
    #[serde(default)]
    pub layouts: layouts::Settings,
    #[serde(default)]
    pub hotkeys: hotkeys::Settings,
    #[serde(default)]
    pub recovery: recovery::Settings,
    #[serde(default)]
    pub backups: backups::Settings,
    #[serde(default)]
    pub notify: notify::Settings,
    #[serde(default)]
    pub perf: perf::Settings,
    #[serde(default)]
    pub playtime: playtime::Settings,
    /// Fields a newer Crystal wrote: kept as they were on every save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
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
    let Some(bytes) = read(path)? else { return Ok(FeatureSettings::default()) };
    serde_json::from_slice(&bytes).map_err(|_| t("Feature settings could not be read.").into())
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
