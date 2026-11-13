//! Where the game's settings live, and reading, writing and backing them up.
//!
//! Retail installs (and Windower, Ashita, the official client) read the
//! registry: `HKLM\SOFTWARE\WOW6432Node\<vendor>\SquareEnix\FinalFantasyXI`,
//! `<vendor>` being PlayOnlineUS or PlayOnlineEU; Japan's is
//! `…\PlayOnline\SQUARE\FinalFantasyXI` (`win::reg::ffxi_settings_key`).
use super::model::{self, Data, Values};
use crate::i18n::{t, tf};
use std::path::{Path, PathBuf};

/// The region whose registry key the page edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    Us,
    Eu,
    Jp,
}

impl Region {
    pub(crate) const ALL: [Region; 3] = [Region::Us, Region::Eu, Region::Jp];
    pub(crate) fn vendor(self) -> &'static str {
        match self {
            Region::Us => "PlayOnlineUS",
            Region::Eu => "PlayOnlineEU",
            Region::Jp => "PlayOnline",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Region::Us => t("US"),
            Region::Eu => t("Europe"),
            Region::Jp => t("Japan"),
        }
    }
    /// Backup file prefix; unchanged from when the page also had a native target.
    pub(crate) fn slug(self) -> String {
        format!("retail-{}", self.vendor().to_ascii_lowercase())
    }
    pub(crate) fn target_label(self) -> String {
        tf("Windows registry · {region}", &[("region", &self.label())])
    }
    pub(crate) fn location(self) -> String {
        format!("HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\{}\\{}\\FinalFantasyXI", self.vendor(), crate::win::reg::publisher(self.vendor()))
    }
}

/// The launcher's own files, from its configuration.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub launcher: PathBuf,
}

impl Paths {
    pub(crate) fn from_config(config: &crate::backend::Config) -> Self {
        Self { launcher: config.experiment.join("iced-launcher") }
    }
    pub(crate) fn backups(&self) -> PathBuf {
        self.launcher.join("game-config-backups")
    }
    fn menu_file(&self, region: Region) -> PathBuf {
        self.launcher.join(format!("menu-{}.json", region.vendor().to_ascii_lowercase()))
    }
}
/// A menu size explicitly applied in Crystal survives an injector rewriting the
/// shared registry. An absent preference leaves each launch profile in control.
pub(crate) fn menu_override(paths: &Paths, region: Region) -> Result<Option<(u32, u32)>, String> {
    use std::io::Read;
    let path = paths.menu_file(region);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(tf("Could not read {path}.", &[("path", &path.display())])),
    };
    let mut bytes = Vec::new();
    file.take(1025).read_to_end(&mut bytes)
        .map_err(|_| tf("Could not read {path}.", &[("path", &path.display())]))?;
    let size: (u32, u32) = serde_json::from_slice(&bytes)
        .map_err(|_| tf("The saved menu resolution in {path} is invalid.", &[("path", &path.display())]))?;
    if bytes.len() > 1024 || size.0 < model::MIN_MENU.0 || size.1 < model::MIN_MENU.1 {
        return Err(tf("The saved menu resolution in {path} is invalid.", &[("path", &path.display())]));
    }
    Ok(Some(size))
}

fn save_menu(paths: &Paths, region: Region, size: (u32, u32)) -> Result<(), String> {
    let path = paths.menu_file(region);
    if size == (0, 0) {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(tf("Could not save {path}.", &[("path", &path.display())])),
        };
    }
    std::fs::create_dir_all(&paths.launcher)
        .map_err(|_| tf("Could not create {path}.", &[("path", &paths.launcher.display())]))?;
    crate::fsutil::write_atomic(&path, &serde_json::to_vec(&size).expect("integer pair"))
        .map_err(|_| tf("Could not save {path}.", &[("path", &path.display())]))
}

fn retain_menu(values: &mut Values, size: Option<(u32, u32)>) {
    if let Some((w, h)) = size {
        values.insert(model::MENU_W.into(), Data::Dword(w));
        values.insert(model::MENU_H.into(), Data::Dword(h));
    }
}

pub(crate) fn read(paths: &Paths, region: Region) -> Result<Values, String> {
    let mut values = registry::read(region.vendor())?
        .ok_or_else(|| tf("FINAL FANTASY XI ({region}) has no settings in the registry.", &[("region", &region.label())]))?;
    retain_menu(&mut values, menu_override(paths, region)?);
    Ok(values)
}

/// Writes the changed managed values.
pub(crate) fn write(paths: &Paths, region: Region, original: &Values, edited: &Values, menu_selected: bool) -> Result<(), String> {
    let changes: Vec<(&str, &Data)> = model::changed_keys(original, edited)
        .into_iter()
        .filter_map(|k| edited.get(k).map(|d| (k, d)))
        .collect();
    registry::write(region.vendor(), &changes)?;
    if menu_selected || changes.iter().any(|(key, _)| matches!(*key, model::MENU_W | model::MENU_H)) {
        let size = (model::dword(edited, model::MENU_W).unwrap_or(0), model::dword(edited, model::MENU_H).unwrap_or(0));
        save_menu(paths, region, size)?;
    }
    Ok(())
}

/// Regions whose FinalFantasyXI key holds game settings (not just an
/// install path).
pub(crate) fn retail_regions() -> Vec<Region> {
    Region::ALL
        .into_iter()
        .filter(|r| {
            registry::read(r.vendor())
                .ok()
                .flatten()
                .is_some_and(|v| model::dword(&v, model::WINDOW_W).is_some())
        })
        .collect()
}

pub(crate) fn writable(region: Region) -> bool {
    registry::writable(region.vendor())
}

/// Every value under a vendor's FinalFantasyXI key; `None` when the key is missing.
pub(crate) fn read_key(vendor: &str) -> Result<Option<Values>, String> {
    registry::read(vendor)
}

pub(crate) fn write_key(vendor: &str, changes: &[(&str, &Data)]) -> Result<(), String> {
    registry::write(vendor, changes)
}

/// The region controller settings use: the first whose key holds game
/// settings (the Graphics page's default), else the first holding controller
/// values, else the first key that exists at all (a fresh install before the
/// game has stored anything), in the previous US, Japan, Europe order.
pub(crate) fn controller_region() -> Option<Region> {
    retail_regions().first().copied().or_else(|| {
        Region::ALL.into_iter().find(|r| {
            registry::read(r.vendor())
                .ok()
                .flatten()
                .is_some_and(|v| v.contains_key("padmode000") || v.contains_key("padsin000"))
        })
    }).or_else(|| {
        [Region::Us, Region::Jp, Region::Eu]
            .into_iter()
            .find(|r| matches!(registry::read(r.vendor()), Ok(Some(_))))
    })
}

// ---- backups ----

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
pub(crate) struct Backup {
    pub target: String,
    pub location: String,
    pub taken: String,
    pub values: Values,
}

#[derive(Clone, Debug)]
pub(crate) struct BackupEntry {
    pub path: PathBuf,
    pub label: String,
}

impl PartialEq for BackupEntry {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl std::fmt::Display for BackupEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Saves `values` as `<dir>/<slug>-<YYYYMMDD-HHMMSS>.json`, never replacing a
/// file.
pub(crate) fn save_backup(dir: &Path, region: Region, location: &str, values: &Values, now: chrono::NaiveDateTime) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|_| tf("Could not create the backup folder {path}.", &[("path", &dir.display())]))?;
    let backup = Backup {
        target: region.slug(),
        location: location.into(),
        taken: now.format("%Y-%m-%d %H:%M:%S").to_string(),
        values: values.clone(),
    };
    let json = serde_json::to_vec_pretty(&backup).map_err(|_| t("Could not encode the backup.").to_string())?;
    let stamp = now.format("%Y%m%d-%H%M%S");
    for n in 0..100 {
        let name = if n == 0 {
            format!("{}-{stamp}.json", region.slug())
        } else {
            format!("{}-{stamp}-{n}.json", region.slug())
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                return file
                    .write_all(&json)
                    .and_then(|_| file.sync_all())
                    .map(|_| path.clone())
                    .map_err(|_| tf("Could not write the backup {path}.", &[("path", &path.display())]));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(tf("Could not write a backup in {path}.", &[("path", &dir.display())])),
        }
    }
    Err(t("Too many backups were made this second.").into())
}

/// This region's backups, newest first.
pub(crate) fn list_backups(dir: &Path, region: Region) -> Vec<BackupEntry> {
    let prefix = format!("{}-", region.slug());
    let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stamp = name.strip_prefix(&prefix)?.strip_suffix(".json")?.to_owned();
            // "retail-playonline-" must not claim "retail-playonlineus-..." files.
            stamp.as_bytes().first()?.is_ascii_digit().then_some((stamp, e.path()))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries
        .into_iter()
        .map(|(stamp, path)| BackupEntry { label: backup_label(&stamp), path })
        .collect()
}

fn backup_label(stamp: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(stamp.get(..15).unwrap_or(stamp), "%Y%m%d-%H%M%S")
        .map(|t| t.format("%b %-d, %Y  %H:%M").to_string())
        .unwrap_or_else(|_| stamp.to_owned())
}

pub(crate) fn load_backup(path: &Path) -> Result<Backup, String> {
    let bytes = std::fs::read(path).map_err(|_| tf("Could not read {path}.", &[("path", &path.display())]))?;
    serde_json::from_slice(&bytes).map_err(|_| tf("{path} is not a readable backup.", &[("path", &path.display())]))
}

// ---- the registry ----

mod registry {
    use super::super::model::{Data, Values};
    use crate::win::reg::{
        sz_bytes, Key, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_32KEY, REG_BINARY,
        REG_DWORD, REG_EXPAND_SZ, REG_SZ,
    };
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::RegEnumValueW;

    /// The 32-bit view, where the 32-bit game and its tools keep the key
    /// (WOW6432Node to a 64-bit process).
    fn open(vendor: &str, access: u32) -> Result<Key, u32> {
        Key::open(HKEY_LOCAL_MACHINE, &crate::win::reg::ffxi_settings_key(vendor), access | KEY_WOW64_32KEY)
    }

    pub(super) fn read(vendor: &str) -> Result<Option<Values>, String> {
        let key = match open(vendor, KEY_READ) {
            Ok(key) => key,
            Err(ERROR_FILE_NOT_FOUND) => return Ok(None),
            Err(code) => return Err(crate::i18n::tf("Windows could not read the FINAL FANTASY XI settings (error {code}).", &[("code", &code)])),
        };
        let mut values = Values::new();
        let mut name = vec![0u16; 512];
        let mut data = vec![0u8; 8192];
        for index in 0..4096u32 {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut kind = 0u32;
            // SAFETY: buffers and their lengths are valid for the call.
            let status = unsafe {
                RegEnumValueW(
                    key.raw(),
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                continue; // an oversized value: not one of the game's
            }
            let value_name = String::from_utf16_lossy(&name[..name_len as usize]);
            let bytes = &data[..data_len as usize];
            let value = match kind {
                REG_DWORD if bytes.len() == 4 => Data::Dword(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
                REG_SZ | REG_EXPAND_SZ => {
                    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
                    Data::Text(String::from_utf16_lossy(&units[..end]))
                }
                _ => Data::Binary(bytes.to_vec()),
            };
            values.insert(value_name, value);
        }
        Ok(Some(values))
    }

    /// Whether this process may set values (opening for write changes nothing).
    pub(super) fn writable(vendor: &str) -> bool {
        open(vendor, KEY_SET_VALUE | KEY_QUERY_VALUE).is_ok()
    }

    pub(super) fn write(vendor: &str, changes: &[(&str, &Data)]) -> Result<(), String> {
        let key = open(vendor, KEY_SET_VALUE | KEY_QUERY_VALUE).map_err(|code| match code {
            ERROR_ACCESS_DENIED => crate::i18n::t("Windows refused the change: saving FINAL FANTASY XI's settings needs Crystal Launcher to run as administrator. Nothing was changed.").to_string(),
            ERROR_FILE_NOT_FOUND => crate::i18n::t("FINAL FANTASY XI's settings key is missing; run the game or its Config tool once first. Nothing was changed.").to_string(),
            code => crate::i18n::tf("Windows could not open FINAL FANTASY XI's settings for writing (error {code}). Nothing was changed.", &[("code", &code)]),
        })?;
        for (done, (name, data)) in changes.iter().enumerate() {
            let (kind, bytes): (u32, Vec<u8>) = match data {
                Data::Dword(v) => (REG_DWORD, v.to_le_bytes().to_vec()),
                Data::Text(s) => (REG_SZ, sz_bytes(s)),
                Data::Binary(b) => (REG_BINARY, b.clone()),
            };
            if let Err(status) = key.set(name, kind, &bytes) {
                return Err(crate::i18n::tf(
                    "Windows refused to save setting {name} (error {status}); {done} of {total} changes were saved. Restore the backup if the game misbehaves.",
                    &[("name", name), ("status", &status), ("done", &done), ("total", &changes.len())],
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_config::model::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("game-config-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_paths(root: &Path) -> Paths {
        Paths { launcher: root.join("iced-launcher") }
    }

    #[test]
    fn game_config_backups_round_trip_and_never_overwrite() {
        let root = scratch("backup");
        let dir = root.join("backups");
        let values = defaults((2560, 1440));
        let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 3).unwrap().and_hms_opt(14, 5, 9).unwrap();
        let us = Region::Us;
        let a = save_backup(&dir, us, "HKLM\\...", &values, now).unwrap();
        let b = save_backup(&dir, us, "HKLM\\...", &values, now).unwrap();
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("retail-playonlineus-20261003-140509"));
        // A legacy native-client backup is never listed for a region.
        std::fs::write(dir.join("native-20261003-140509.json"), b"{}").unwrap();
        // The Japanese key's slug is a prefix of the US one's; listings stay apart.
        assert!(list_backups(&dir, Region::Jp).is_empty());
        let listed = list_backups(&dir, us);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].label, "Oct 3, 2026  14:05");
        let loaded = load_backup(&listed[1].path).unwrap();
        assert_eq!(loaded.values, values);
        assert_eq!(loaded.target, "retail-playonlineus");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_config_retail_write_sends_only_changed_values() {
        // The change list the registry writer receives, without touching the registry.
        let original = defaults((1920, 1080));
        let mut edited = original.clone();
        edited.insert(MIP_MAPPING.into(), Data::Dword(2));
        edited.insert("0031".into(), Data::Dword(5));
        let changes: Vec<&str> = changed_keys(&original, &edited);
        assert_eq!(changes, vec![MIP_MAPPING]);
    }

    #[test]
    fn saved_menu_survives_injector_registry_resets_and_can_be_released() {
        let root = scratch("retained-menu");
        let paths = fake_paths(&root);
        assert_eq!(menu_override(&paths, Region::Us).unwrap(), None);
        save_menu(&paths, Region::Us, (1600, 900)).unwrap();
        // A fresh reader sees the persisted size after Windower's 1x profile
        // resets the registry, without changing that registry or other values.
        let mut registry = defaults((3840, 2160));
        registry.insert(MENU_W.into(), Data::Dword(0));
        registry.insert(MENU_H.into(), Data::Dword(0));
        retain_menu(&mut registry, menu_override(&fake_paths(&root), Region::Us).unwrap());
        assert_eq!((dword(&registry, MENU_W), dword(&registry, MENU_H)), (Some(1600), Some(900)));
        assert_eq!(dword(&registry, WINDOW_W), Some(3840));
        assert_eq!(menu_override(&paths, Region::Eu).unwrap(), None);
        save_menu(&paths, Region::Us, (1280, 720)).unwrap();
        assert_eq!(menu_override(&paths, Region::Us).unwrap(), Some((1280, 720)));
        save_menu(&paths, Region::Us, (0, 0)).unwrap();
        assert_eq!(menu_override(&paths, Region::Us).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
