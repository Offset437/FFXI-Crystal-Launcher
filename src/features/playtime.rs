//! Play time per client, kept locally in `iced-launcher/playtime.json`. Nothing leaves the
//! machine.
//!
//! The interface reports a client's start and end; the tracker adds the difference. There is no
//! timer: a session's length is `stop - start`, and the file is written only on those two
//! events. A session still open when Crystal is killed cannot be timed (its end is unknown), so
//! open sessions are never saved and simply do not count; only `last_played` survives.

use super::ClientKey;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Discord rich presence through its local IPC pipe. Off by default.
    pub discord: bool,
    /// Record play time per client.
    pub record: bool,
}

impl Default for Settings {
    fn default() -> Self { Self { discord: false, record: true } }
}

/// The saved record of one client.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    total_secs: u64,
    sessions: u64,
    /// Unix seconds of the latest start.
    last_played: u64,
    character: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    clients: BTreeMap<String, Stored>,
}

/// One client's totals, as the play-time screen lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub key: ClientKey,
    pub total_secs: u64,
    pub sessions: u64,
    pub last_played: u64,
    pub character: Option<String>,
}

pub(crate) struct Tracker {
    path: PathBuf,
    data: File,
    open: HashMap<ClientKey, u64>,
}

/// A single session counts for at most this long, so a clock jump cannot add months.
const MAX_SESSION_SECS: u64 = 7 * 24 * 60 * 60;
const LIMIT: u64 = 1024 * 1024;
const MAX_LABEL: usize = 64;

pub(crate) fn path(config: &crate::backend::Config) -> PathBuf {
    config.experiment.join("iced-launcher").join("playtime.json")
}

impl Tracker {
    /// The tracker for `path`. A missing file is empty. A damaged or oversize file is moved
    /// aside as `playtime.invalid-<id>.json` (never lost) and the tracker starts empty.
    pub(crate) fn load(path: &Path) -> Self {
        use std::io::Read;
        let mut bytes = Vec::new();
        let read = std::fs::File::open(path).map(|f| f.take(LIMIT + 1).read_to_end(&mut bytes));
        let data = match read {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Ok(Ok(_)) if bytes.len() as u64 <= LIMIT => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                Self::set_aside(path);
                File::default()
            }),
            _ => {
                Self::set_aside(path);
                File::default()
            }
        };
        Self { path: path.to_owned(), data, open: HashMap::new() }
    }

    fn set_aside(path: &Path) {
        if let Ok(id) = crate::backend::unique_id() {
            let _ = std::fs::rename(path, path.with_extension(format!("invalid-{id}.json")));
        }
    }

    /// Begins a session. A client already running keeps its original start. Saves.
    pub(crate) fn start(&mut self, key: &ClientKey, now: u64) {
        if self.open.contains_key(key) { return; }
        self.open.insert(key.clone(), now);
        self.data.clients.entry(key.id()).or_default().last_played = now;
        self.persist();
    }

    /// Ends a session and returns its seconds (at most 7 days; 0 if the clock went back).
    /// `None` if the client was not running. Saves.
    pub(crate) fn stop(&mut self, key: &ClientKey, now: u64) -> Option<u64> {
        let started = self.open.remove(key)?;
        let secs = now.saturating_sub(started).min(MAX_SESSION_SECS);
        let stored = self.data.clients.entry(key.id()).or_default();
        stored.total_secs = stored.total_secs.saturating_add(secs);
        stored.sessions = stored.sessions.saturating_add(1);
        stored.last_played = stored.last_played.max(started);
        self.persist();
        Some(secs)
    }

    /// Remembers a character label for a client that has a record. Kept in memory until the
    /// next start or stop saves it. Blank clears; long labels are cut.
    pub(crate) fn set_character(&mut self, key: &ClientKey, name: Option<String>) {
        let label = name.map(|n| n.trim().chars().take(MAX_LABEL).collect::<String>()).filter(|n| !n.is_empty());
        if let Some(stored) = self.data.clients.get_mut(&key.id()) { stored.character = label; }
    }

    /// Everyone with a record, most played first.
    pub(crate) fn entries(&self) -> Vec<Entry> {
        let mut list: Vec<_> = self.data.clients.iter().filter_map(|(id, s)| Some(Entry {
            key: ClientKey::parse(id)?,
            total_secs: s.total_secs,
            sessions: s.sessions,
            last_played: s.last_played,
            character: s.character.clone(),
        })).collect();
        list.sort_by(|a, b| b.total_secs.cmp(&a.total_secs).then(b.last_played.cmp(&a.last_played)).then_with(|| a.key.cmp(&b.key)));
        list
    }

    /// The sessions open now, with their start times.
    #[cfg(test)]
    pub(crate) fn running(&self) -> Vec<(ClientKey, u64)> {
        let mut list: Vec<_> = self.open.iter().map(|(k, s)| (k.clone(), *s)).collect();
        list.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        list
    }

    /// Writes the file atomically.
    pub(crate) fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(&self.data).map_err(|_| crate::i18n::t("Could not encode play time."))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|_| crate::i18n::t("Could not save play time."))?;
        }
        crate::fsutil::write_atomic(&self.path, &data).map_err(|_| crate::i18n::t("Could not save play time.").into())
    }

    /// Saves after a start or stop; a failure is not fatal and the next save tries again.
    fn persist(&self) { let _ = self.save(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("xi-playtime-{}", crate::backend::unique_id().unwrap()));
        (dir.join("playtime.json"), dir)
    }

    fn key(a: &str, slot: Option<u8>) -> ClientKey { ClientKey::new(a, slot) }

    #[test]
    fn start_and_stop_accumulate() {
        let (p, dir) = file();
        let mut t = Tracker::load(&p);
        let k = key("a", Some(2));
        t.start(&k, 1000);
        assert_eq!(t.running(), vec![(k.clone(), 1000)]);
        assert_eq!(t.stop(&k, 1600), Some(600));
        t.start(&k, 2000);
        assert_eq!(t.stop(&k, 2050), Some(50));
        let e = &t.entries()[0];
        assert_eq!((e.total_secs, e.sessions, e.last_played), (650, 2, 2000));
        assert!(t.running().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn double_start_is_ignored_and_stop_without_start_is_none() {
        let (p, dir) = file();
        let mut t = Tracker::load(&p);
        let k = key("a", None);
        assert_eq!(t.stop(&k, 5), None);
        assert!(t.entries().is_empty());
        t.start(&k, 100);
        t.start(&k, 900);
        assert_eq!(t.running(), vec![(k.clone(), 100)]);
        assert_eq!(t.stop(&k, 160), Some(60));
        assert_eq!(t.stop(&k, 170), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn it_persists_and_drops_sessions_open_at_load() {
        let (p, dir) = file();
        let mut t = Tracker::load(&p);
        let (a, b) = (key("a", None), key("b", Some(1)));
        t.start(&a, 10);
        t.stop(&a, 40);
        t.start(&b, 50);
        t.set_character(&a, Some("  Vana  ".into()));
        t.set_character(&key("ghost", None), Some("x".into()));
        t.start(&a, 60); // saves the label
        t.stop(&a, 70);
        // b is still open: Crystal "was killed".
        let back = Tracker::load(&p);
        assert!(back.running().is_empty());
        let entries = back.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].key.clone(), entries[0].total_secs, entries[0].sessions), (a, 40, 2));
        assert_eq!(entries[0].character.as_deref(), Some("Vana"));
        assert_eq!((entries[1].key.clone(), entries[1].total_secs, entries[1].last_played), (b, 0, 50));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_session_is_clamped_and_a_backwards_clock_adds_nothing() {
        let (p, dir) = file();
        let mut t = Tracker::load(&p);
        let k = key("a", None);
        t.start(&k, 100);
        assert_eq!(t.stop(&k, 100 + 30 * 86400), Some(7 * 86400));
        t.start(&k, 1_000_000);
        assert_eq!(t.stop(&k, 5), Some(0));
        assert_eq!(t.entries()[0].total_secs, 7 * 86400);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn entries_sort_by_total_and_a_damaged_file_is_set_aside() {
        let (p, dir) = file();
        let mut t = Tracker::load(&p);
        let (a, b) = (key("a", None), key("b", None));
        t.start(&a, 0);
        t.stop(&a, 10);
        t.start(&b, 0);
        t.stop(&b, 99);
        assert_eq!(t.entries().iter().map(|e| e.key.account.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        std::fs::write(&p, b"{broken").unwrap();
        let fresh = Tracker::load(&p);
        assert!(fresh.entries().is_empty());
        assert!(std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("invalid-")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_default_to_recording_without_discord() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings { discord: false, record: true });
        assert_eq!(serde_json::from_str::<Settings>(r#"{"discord":true}"#).unwrap(), Settings { discord: true, record: true });
    }
}
