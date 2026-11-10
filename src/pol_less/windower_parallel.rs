//! Parallel Windower startup from a saved snapshot of Windower's own startup
//! record (the profile data Windower.exe hands Hook.dll). No account or
//! authentication data is ever captured; the record is DPAPI-protected for
//! this Windows user in Crystal's data folder.
//!
//! When a snapshot is used: it was captured today (local calendar day) from a
//! Windower.exe this launcher started, and every input it depends on is
//! unchanged: the Windower folder, the profile name, the game folder,
//! settings.xml, Windower.exe and Hook.dll (by content), the plugin DLLs (by
//! size and time), Windower's update manifests and the screen size. The
//! first launch of each day, any change, and any missing, corrupt or
//! unreadable snapshot run Windower.exe instead, which self-updates and
//! supplies a fresh snapshot.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use zeroize::{Zeroize, Zeroizing};
#[path = "../../crystal-loader/src/windower_bootstrap_contract.rs"]
#[allow(dead_code)] // Native-only image offsets share this checked record layout.
pub(crate) mod layout;

/// Bumped whenever `Stored` or the fingerprint inputs change meaning.
const FORMAT: u32 = 2;
const MAX_FILE: u64 = 256 * 1024;
/// Other snapshots (another profile, older inputs) are removed after this.
const PRUNE_AFTER: Duration = Duration::from_secs(36 * 60 * 60);

/// Why a launch runs Windower.exe instead of a private startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Miss {
    /// No snapshot was saved for these exact Windower inputs.
    Missing,
    /// Saved on an earlier day: the first launch of the day runs Windower.exe.
    OtherDay,
    /// A Windower input changed after this launch was prepared.
    Changed,
    /// Unreadable, undecryptable, malformed or failed validation (removed).
    Corrupt,
}
impl Miss {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Miss::Missing => "no saved startup snapshot matches the current Windower files and profile",
            Miss::OtherDay => "first launch today, so Windower.exe checks for updates",
            Miss::Changed => "Windower's files or profile changed during preparation",
            Miss::Corrupt => "the saved startup snapshot was unreadable and was removed",
        }
    }
}

#[derive(Clone)]
pub(crate) struct Key {
    folder: PathBuf,
    profile: String,
    install: PathBuf,
    fingerprint: String,
    path: PathBuf,
}

/// The host's own check, run on the same bytes: Hook's startup objects and
/// record layout are found in the file by signature, any build.
fn supported_hook(bytes: &[u8]) -> bool {
    layout::hook_startup(bytes).is_ok()
}

/// Today's local calendar day, the unit of the daily Windower.exe run.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn fingerprint(folder: &Path, profile: &str, install: &Path, supported: fn(&[u8]) -> bool) -> Result<String, &'static str> {
    let mut hash = Sha256::new();
    hash.update(FORMAT.to_le_bytes());
    for value in [
        folder.to_string_lossy(),
        profile.into(),
        install.to_string_lossy(),
    ] {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    for name in ["settings.xml", "Windower.exe", "Hook.dll"] {
        let path = folder.join(name);
        let meta = fs::metadata(&path).map_err(|_| "a Windower file is missing")?;
        if meta.len() > 8 * 1024 * 1024 {
            return Err("a Windower file is unexpectedly large");
        }
        let bytes = fs::read(path).map_err(|_| "a Windower file could not be read")?;
        if name == "Hook.dll" && !supported(&bytes) {
            return Err("this Windower Hook.dll build is not supported for private startup");
        }
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    for (name, stamp) in super::tools::windower_files(folder)
        .into_iter()
        .filter(|(name, _)| name.starts_with("plugins\\") || name.starts_with("plugins/"))
    {
        hash.update(name.as_bytes());
        if let Some((length, modified)) = stamp {
            hash.update(length.to_le_bytes());
            hash.update(modified.to_le_bytes());
        }
    }
    for name in [
        r"updates\manifest.xml",
        r"updates\addons.json",
        r"updates\resources.xml",
    ] {
        hash.update(name.as_bytes());
        match fs::metadata(folder.join(name)) {
            Ok(meta) if meta.len() <= 16 * 1024 * 1024 => {
                hash.update(fs::read(folder.join(name)).map_err(|_| "a Windower update manifest could not be read")?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => hash.update([0]),
            _ => return Err("a Windower update manifest could not be read"),
        }
    }
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
        };
        hash.update(GetSystemMetrics(SM_CXSCREEN).to_le_bytes());
        hash.update(GetSystemMetrics(SM_CYSCREEN).to_le_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Serialize, Deserialize)]
struct Stored {
    format: u32,
    /// Local calendar day of the Windower.exe run that supplied `data`.
    day: String,
    fingerprint: String,
    data: String,
}
impl Drop for Stored {
    fn drop(&mut self) {
        self.data.zeroize();
    }
}
fn check(stored: &Stored, fingerprint: &str, today: &str) -> Result<(), Miss> {
    if stored.format != FORMAT || stored.fingerprint != fingerprint {
        return Err(Miss::Corrupt);
    }
    if stored.day != today {
        return Err(Miss::OtherDay);
    }
    Ok(())
}
pub(crate) struct Snapshot {
    bytes: Zeroizing<Vec<u8>>,
}
impl Snapshot {
    pub(crate) fn viewer_directory(&self) -> Result<PathBuf, String> {
        layout::text(&self.bytes, 640, 4096)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or("Invalid Windower startup directory.".into())
    }
    #[cfg(test)]
    pub(crate) fn label(&self) -> Option<String> {
        layout::text(&self.bytes, 0, 256)
    }
    pub(crate) fn line(&self, folder: &Path, nonce: &str) -> Zeroizing<String> {
        let mut encoded = Vec::new();
        super::pipe::base64_into(&mut encoded, &self.bytes);
        let data = Zeroizing::new(String::from_utf8(encoded).expect("base64"));
        Zeroizing::new(format!(
            "{{\"type\":\"windower_startup\",\"v\":1,\"hook\":{},\"nonce\":{},\"data\":\"{}\"}}\n",
            serde_json::to_string(&folder.join("Hook.dll")).unwrap(),
            serde_json::to_string(nonce).unwrap(),
            data.as_str()
        ))
    }
}
impl Key {
    /// `store`: the folder in Crystal's data folder that holds the snapshots.
    /// `Err` (with a log reason) when private startup can't apply at all.
    pub(crate) fn new(folder: &Path, profile: &str, install: &Path, store: &Path) -> Result<Self, &'static str> {
        Self::with_hook_check(folder, profile, install, store, supported_hook)
    }
    fn with_hook_check(folder: &Path, profile: &str, install: &Path, store: &Path, supported_hook: fn(&[u8]) -> bool) -> Result<Self, &'static str> {
        let fingerprint = fingerprint(folder, profile, install, supported_hook)?;
        let path = store.join(format!("{fingerprint}.dpapi"));
        Ok(Self {
            folder: folder.into(),
            profile: profile.into(),
            install: install.into(),
            fingerprint,
            path,
        })
    }
    #[cfg(test)]
    pub(crate) fn file_stem(&self) -> String {
        self.fingerprint.clone()
    }
    /// The SHA-256 over Hook.dll's bytes proves it is the file that already passed the build check in with_hook_check.
    fn unchanged(&self) -> bool {
        fingerprint(&self.folder, &self.profile, &self.install, |_| true).as_deref()
            == Ok(self.fingerprint.as_str())
    }
    /// Today's snapshot for these unchanged inputs. A corrupt file is removed,
    /// so the Windower.exe run that follows writes a fresh one.
    pub(crate) fn load(&self) -> Result<Snapshot, Miss> {
        self.load_on(&today())
    }
    pub(super) fn load_on(&self, today: &str) -> Result<Snapshot, Miss> {
        let encrypted = match fs::metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Miss::Missing),
            Ok(meta) if meta.len() <= MAX_FILE => fs::read(&self.path).ok(),
            _ => None,
        };
        if !self.unchanged() {
            return Err(Miss::Changed);
        }
        let parsed = encrypted
            .and_then(|bytes| crate::backend::unprotect(&bytes).ok())
            .and_then(|plain| serde_json::from_slice::<Stored>(&plain).ok());
        let result = match parsed {
            Some(stored) => check(&stored, &self.fingerprint, today).and_then(|()| {
                let bytes = Zeroizing::new(super::pipe::base64_decode(&stored.data).ok_or(Miss::Corrupt)?);
                layout::validate(&bytes).map_err(|_| Miss::Corrupt)?;
                Ok(Snapshot { bytes })
            }),
            None => Err(Miss::Corrupt),
        };
        if result.as_ref().err() == Some(&Miss::Corrupt) {
            self.discard();
        }
        result
    }
    /// Removes this key's snapshot, e.g. after a private startup failed with it.
    pub(crate) fn discard(&self) {
        let _ = fs::remove_file(&self.path);
    }
    /// Saves a record Windower.exe just supplied, atomically (temporary file,
    /// then rename), and removes stale snapshots of other inputs.
    pub(crate) fn save(&self, bytes: &[u8]) -> Result<(), String> {
        self.save_on(bytes, &today())
    }
    pub(super) fn save_on(&self, bytes: &[u8], day: &str) -> Result<(), String> {
        if !self.unchanged() {
            return Err("Windower inputs changed during startup; the startup snapshot was not saved.".into());
        }
        layout::validate(bytes).map_err(str::to_owned)?;
        let mut encoded = Vec::new();
        super::pipe::base64_into(&mut encoded, bytes);
        let stored = Stored {
            format: FORMAT,
            day: day.into(),
            fingerprint: self.fingerprint.clone(),
            data: String::from_utf8(encoded).expect("base64"),
        };
        let encoded = Zeroizing::new(
            serde_json::to_vec(&stored)
                .map_err(|_| "Could not encode the startup snapshot.")?,
        );
        let encrypted = crate::backend::protect(&encoded)?;
        let parent = self
            .path
            .parent()
            .ok_or("Missing startup snapshot directory.")?;
        fs::create_dir_all(parent)
            .map_err(|_| "Could not create the startup snapshot directory.")?;
        crate::fsutil::write_atomic(&self.path, &encrypted)
            .map_err(|_| "Could not save the startup snapshot.")?;
        prune(parent, &self.path);
        Ok(())
    }
}

/// Other profiles' snapshots stay usable for the day; anything older is
/// removed, so changed inputs never accumulate files.
fn prune(folder: &Path, keep: &Path) {
    let now = SystemTime::now();
    for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
        let path = entry.path();
        let ours = path.extension().is_some_and(|e| e == "dpapi" || e == "tmp");
        let stale = entry.metadata().ok().and_then(|m| m.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > PRUNE_AFTER);
        if ours && stale && path != keep {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) struct Capture(std::os::windows::io::OwnedHandle);
impl Capture {
    pub(crate) fn new() -> Option<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Memory::{CreateFileMappingW, PAGE_READWRITE},
        };
        let name = crate::win::wide(layout::MAP_NAME);
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                layout::SETTINGS_LEN as u32,
                name.as_ptr(),
            )
        };
        (!handle.is_null())
            .then(|| Self(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) }))
    }
    pub(crate) fn read(&self) -> Option<Zeroizing<Vec<u8>>> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Memory::{MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ};
        unsafe {
            let view = MapViewOfFile(
                self.0.as_raw_handle(),
                FILE_MAP_READ,
                0,
                0,
                layout::SETTINGS_LEN,
            );
            if view.Value.is_null() {
                return None;
            }
            let bytes = Zeroizing::new(
                std::slice::from_raw_parts(view.Value as *const u8, layout::SETTINGS_LEN).to_vec(),
            );
            UnmapViewOfFile(view);
            layout::validate(&bytes).ok()?;
            Some(bytes)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn record(label: &str) -> Vec<u8> {
        let mut bytes = vec![0; layout::SETTINGS_LEN];
        for (offset, value) in [(0, label), (608, "4.7.9.3"), (640, r"C:\Viewer\")] {
            for (i, c) in value.encode_utf16().enumerate() {
                bytes[offset + i * 2..offset + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
            }
        }
        bytes[25248..25252].copy_from_slice(&1u32.to_le_bytes());
        for offset in [25252, 25256, 25260, 25264] {
            bytes[offset..offset + 4].copy_from_slice(&1280i32.to_le_bytes());
        }
        layout::validate(&bytes).unwrap();
        bytes
    }
    pub(crate) fn synthetic(label: &str) -> Snapshot {
        Snapshot { bytes: Zeroizing::new(record(label)) }
    }

    /// A fake Windower folder (any Hook.dll content is "supported") and its store.
    pub(crate) struct Fixture { pub(crate) root: PathBuf }
    impl Fixture {
        pub(crate) fn new() -> Self {
            let root = std::env::temp_dir().join(format!("crystal-windower-startup-{}", crate::backend::unique_id().unwrap()));
            fs::create_dir_all(root.join("windower/plugins")).unwrap();
            fs::create_dir_all(root.join("windower/updates")).unwrap();
            for (name, text) in [("settings.xml", "<settings><profile name=\"\"/></settings>"), ("Windower.exe", "exe v1"), ("Hook.dll", "hook v1"),
                (r"updates\manifest.xml", "<manifest/>"), (r"plugins\Timestamp.dll", "plugin")] {
                fs::write(root.join("windower").join(name), text).unwrap();
            }
            Self { root }
        }
        pub(crate) fn folder(&self) -> PathBuf { self.root.join("windower") }
        pub(crate) fn store(&self) -> PathBuf { self.root.join("store") }
        pub(crate) fn key(&self, profile: &str) -> Key {
            Key::with_hook_check(&self.folder(), profile, Path::new(r"C:\Game"), &self.store(), |_| true).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); }
    }

    #[test]
    fn snapshot_cache_validity_follows_the_day_and_every_windower_input() {
        let fixture = Fixture::new();
        let key = fixture.key("");
        assert_eq!(key.load_on("2026-10-22").err(), Some(Miss::Missing), "no file yet: Windower.exe runs");
        key.save_on(&record("Main"), "2026-10-22").unwrap();
        // Same day, unchanged inputs: private startup, nothing else consulted.
        let snapshot = key.load_on("2026-10-22").unwrap();
        assert_eq!(layout::text(&snapshot.bytes, 0, 256).as_deref(), Some("Main"));
        // A new local day: the first launch runs Windower.exe (and its self-update).
        assert_eq!(key.load_on("2026-10-23").err(), Some(Miss::OtherDay));
        // Each dependency changes the key; a launch prepared before the change refuses it.
        let changes: [(&str, &dyn Fn(&Path)); 6] = [
            ("profile settings", &|f| fs::write(f.join("settings.xml"), "<settings><profile name=\"Alt\"/></settings>").unwrap()),
            ("Hook.dll", &|f| fs::write(f.join("Hook.dll"), "hook v2").unwrap()),
            ("Windower.exe", &|f| fs::write(f.join("Windower.exe"), "exe v2").unwrap()),
            ("plugin", &|f| fs::write(f.join(r"plugins\Timestamp.dll"), "plugin, longer").unwrap()),
            ("new plugin", &|f| fs::write(f.join(r"plugins\Extra.dll"), "x").unwrap()),
            ("update manifest", &|f| fs::write(f.join(r"updates\manifest.xml"), "<manifest v='2'/>").unwrap()),
        ];
        for (what, change) in changes {
            let fixture = Fixture::new();
            let key = fixture.key("");
            key.save_on(&record("Main"), "2026-10-22").unwrap();
            change(&fixture.folder());
            assert_eq!(key.load_on("2026-10-22").err(), Some(Miss::Changed), "{what}");
            assert_eq!(fixture.key("").load_on("2026-10-22").err(), Some(Miss::Missing), "{what}: new inputs need Windower.exe");
            assert!(key.save_on(&record("Main"), "2026-10-22").is_err(), "{what}: a capture across a change is not saved");
        }
        // Another profile name or Windower folder is another key.
        assert_eq!(fixture.key("Alt").load_on("2026-10-22").err(), Some(Miss::Missing));
        let other = Key::with_hook_check(&fixture.folder(), "", Path::new(r"D:\Game"), &fixture.store(), |_| true).unwrap();
        assert_eq!(other.load_on("2026-10-22").err(), Some(Miss::Missing));
        // An unsupported Hook build never gets a key at all.
        assert!(Key::with_hook_check(&fixture.folder(), "", Path::new(r"C:\Game"), &fixture.store(), |_| false).is_err());
    }

    #[test]
    fn a_corrupt_or_foreign_snapshot_is_removed_and_means_windower_exe() {
        let fixture = Fixture::new();
        let key = fixture.key("");
        for damage in [b"not dpapi".to_vec(), Vec::new(), crate::backend::protect(b"{\"format\":2}").unwrap(),
            crate::backend::protect(&serde_json::to_vec(&Stored { format: FORMAT, day: "2026-10-22".into(),
                fingerprint: "another".into(), data: String::new() }).unwrap()).unwrap(),
            crate::backend::protect(&serde_json::to_vec(&Stored { format: FORMAT, day: "2026-10-22".into(),
                fingerprint: key.fingerprint.clone(), data: "AAAA".into() }).unwrap()).unwrap()] {
            fs::create_dir_all(fixture.store()).unwrap();
            fs::write(&key.path, damage).unwrap();
            assert_eq!(key.load_on("2026-10-22").err(), Some(Miss::Corrupt));
            assert!(!key.path.exists(), "a corrupt snapshot is removed");
        }
        fs::write(&key.path, vec![0; MAX_FILE as usize + 1]).unwrap();
        assert_eq!(key.load_on("2026-10-22").err(), Some(Miss::Corrupt));
        // An invalid record is never saved.
        assert!(key.save_on(&[0; 16], "2026-10-22").is_err());
        assert!(!key.path.exists());
    }

    #[test]
    fn saving_is_atomic_keeps_other_profiles_and_prunes_stale_files() {
        let fixture = Fixture::new();
        let (main, alt) = (fixture.key(""), fixture.key("Alt"));
        alt.save_on(&record("Alt"), "2026-10-22").unwrap();
        let stale = fixture.store().join("0000.dpapi");
        fs::write(&stale, b"old").unwrap();
        let old = SystemTime::now() - PRUNE_AFTER - Duration::from_secs(60);
        fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
        main.save_on(&record("Main"), "2026-10-22").unwrap();
        assert!(!stale.exists());
        let names: Vec<_> = fs::read_dir(fixture.store()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names.len(), 2, "only the two current snapshots, no temporary files: {names:?}");
        assert!(alt.load_on("2026-10-22").is_ok() && main.load_on("2026-10-22").is_ok());
        // The stored file is protected: no profile text is readable in it.
        assert!(!fs::read(&main.path).unwrap().windows(4).any(|w| w == b"Main"));
    }

    #[test]
    fn concurrent_configuration_frames_keep_distinct_profile_data() {
        let clients: Vec<_> = (0..6)
            .map(|i| {
                std::thread::spawn(move || {
                    let label = format!("Profile{i}");
                    let snapshot = synthetic(&label);
                    let line = snapshot.line(Path::new(r"C:\Windower"), &format!("{i:024x}"));
                    let decoded: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let bytes =
                        super::super::pipe::base64_decode(decoded["data"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(
                        layout::text(&bytes, 0, 256).as_deref(),
                        Some(label.as_str())
                    );
                    assert!(decoded.get("native_auth").is_none());
                    assert_eq!(&*snapshot.bytes, bytes.as_slice());
                })
            })
            .collect();
        for client in clients {
            client.join().unwrap();
        }
    }
}
