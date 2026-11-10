//! The newest official versions a real version check has told Crystal, with when.
//!
//! Check files is offline on purpose (it reads the installed patch.cfg and patch.ver and
//! sends nothing to Square Enix), so the Downloads page has no official version of its own to
//! show. Versions are learned only from requests that were sent anyway: the FFXI W20 at Play
//! (also a batch Play or a session worker's), the PlayOnline W2U at sign-in, and the version
//! an Update or Repair asked for. Each one is remembered here, and the page shows the
//! remembered version with its age instead of "Not checked". Nothing in this module opens a
//! connection.
//!
//! Stored in `official-versions.json` beside the launcher's settings, not inside them: the
//! W20 and W2U run in session worker processes, which cannot save the window's settings
//! without racing its own saves (the same arrangement as `startup-prefetch.json`). One entry
//! per game folder for FFXI, one for PlayOnline.

use crate::i18n::{t, tf};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::BTreeMap, fs, path::{Path, PathBuf}, sync::Mutex};

const STORE_VERSION: u32 = 1;
/// Game folders remembered, at most (the oldest answer goes first).
const MAX_FOLDERS: usize = 16;
/// The same version told again within this long keeps its first time (the file is not
/// rewritten for every Play).
const REWRITE_AFTER_SECS: i64 = 600;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Learned {
    pub version: String,
    /// Unix seconds when a version check said so.
    pub at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Store {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    game: BTreeMap<String, Learned>,
    #[serde(default)]
    playonline: Option<Learned>,
}

/// `8 digits _ letters or digits` (a game's `30260904_1`, the Viewer's `20110829_E`): the
/// shape of a version a server really sent, not a placeholder such as "Not checked".
pub(crate) fn is_version(text: &str) -> bool {
    text.split_once('_').is_some_and(|(date, revision)|
        date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())
        && !revision.is_empty() && revision.len() <= 16 && revision.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn folder_key(folder: &Path) -> String {
    crate::backend::plain_windows_path(folder).to_string_lossy().trim_end_matches(['\\', '/']).to_ascii_lowercase()
}

/// `Some(changed)`: whether `slot` now needs saving.
fn learn(slot: &mut Option<Learned>, version: &str, now: i64) -> bool {
    match slot {
        Some(known) if known.version == version && now.saturating_sub(known.at) < REWRITE_AFTER_SECS && now >= known.at => false,
        _ => { *slot = Some(Learned { version: version.to_owned(), at: now }); true }
    }
}

impl Store {
    pub(crate) fn game(&self, folder: &Path) -> Option<&Learned> { self.game.get(&folder_key(folder)) }
    pub(crate) fn playonline(&self) -> Option<&Learned> { self.playonline.as_ref() }

    fn learn_game(&mut self, folder: &Path, version: &str, now: i64) -> bool {
        if !is_version(version) { return false; }
        let key = folder_key(folder);
        let mut slot = self.game.remove(&key);
        let changed = learn(&mut slot, version, now);
        if let Some(slot) = slot { self.game.insert(key, slot); }
        while self.game.len() > MAX_FOLDERS {
            let Some(oldest) = self.game.iter().min_by_key(|(_, learned)| learned.at).map(|(key, _)| key.clone()) else { break };
            self.game.remove(&oldest);
        }
        changed
    }

    fn learn_playonline(&mut self, version: &str, now: i64) -> bool {
        is_version(version) && learn(&mut self.playonline, version, now)
    }
}

/// The store file next to the launcher's settings, for the data folder this process uses.
pub(crate) fn default_path() -> PathBuf {
    path_for(&crate::backend::Config::default())
}

pub(crate) fn path_for(config: &crate::backend::Config) -> PathBuf {
    config.experiment.join("iced-launcher/official-versions.json")
}

pub(crate) fn load(path: &Path) -> Store {
    fs::read(path).ok()
        .filter(|bytes| bytes.len() <= 64 * 1024)
        .and_then(|bytes| serde_json::from_slice::<Store>(&bytes).ok())
        .filter(|store| store.version == STORE_VERSION)
        .unwrap_or(Store { version: STORE_VERSION, ..Store::default() })
}

fn save(path: &Path, store: &Store) {
    let Ok(bytes) = serde_json::to_vec_pretty(store) else { return };
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let _ = crate::fsutil::write_atomic(path, &bytes);
}

static WRITING: Mutex<()> = Mutex::new(());
/// How long a writer waits for another process's load-change-save of the same store.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// The store's cross-process writer lock: an exclusive open of `<store>.lock`. Session worker
/// processes learn versions concurrently, so each load-change-save holds it throughout.
/// `None` when another writer kept it for `LOCK_WAIT`.
fn lock_store(path: &Path) -> Option<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).share_mode(0).open(&name) {
            Ok(lock) => return Some(lock),
            Err(error) if error.raw_os_error() == Some(32) && std::time::Instant::now() < deadline =>
                std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
}

fn update(path: &Path, change: impl FnOnce(&mut Store) -> bool) {
    let _writing = WRITING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // A writer that cannot get the lock skips this write: the next real check writes it again.
    let Some(_lock) = lock_store(path) else { return };
    let mut store = load(path);
    if change(&mut store) { save(path, &store); }
}

pub(crate) fn now() -> i64 { chrono::Utc::now().timestamp() }

/// A real FFXI version check (or update) said the official version for `folder` is `version`.
pub(crate) fn learn_game(folder: &Path, version: &str) {
    learn_game_at(&default_path(), folder, version, now());
}

/// A real PlayOnline version check (or update) said the official version is `version`.
pub(crate) fn learn_playonline(version: &str) {
    learn_playonline_at(&default_path(), version, now());
}

pub(crate) fn learn_game_at(path: &Path, folder: &Path, version: &str, at: i64) {
    if is_version(version) { update(path, |store| store.learn_game(folder, version, at)); }
}

pub(crate) fn learn_playonline_at(path: &Path, version: &str, at: i64) {
    if is_version(version) { update(path, |store| store.learn_playonline(version, at)); }
}

/// How long ago, in words: "just now", "5 min ago", "3 h ago", "2 d ago", else the date.
pub(crate) fn when(at: i64, now: i64) -> String {
    let age = now.saturating_sub(at).max(0);
    match age {
        0..=59 => t("just now").to_owned(),
        60..=3599 => tf("{count} min ago", &[("count", &(age / 60))]),
        3600..=86_399 => tf("{count} h ago", &[("count", &(age / 3600))]),
        86_400..=2_591_999 => tf("{count} d ago", &[("count", &(age / 86_400))]),
        _ => {
            let date = chrono::DateTime::from_timestamp(at, 0)
                .map_or_else(String::new, |time| time.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string());
            tf("on {date}", &[("date", &date)])
        }
    }
}

/// What "Latest version" shows. `reported` is the version the report on screen carries
/// (a real answer only when it looks like a version); `learned` the remembered one.
pub(crate) fn latest_text(reported: Option<&str>, learned: Option<&Learned>, now: i64) -> String {
    match (reported.filter(|version| is_version(version)), learned) {
        (Some(version), Some(learned)) if learned.version == version => with_age(learned, now),
        (Some(version), _) => version.to_owned(),
        (None, Some(learned)) => with_age(learned, now),
        (None, None) => t("Press Play to check").to_owned(),
    }
}

fn with_age(learned: &Learned, now: i64) -> String {
    tf("{version} (checked {when})", &[("version", &learned.version), ("when", &when(learned.at, now))])
}

/// How the installed version stands against the remembered official one, when that can be
/// told: `Equal`, or `Less` when installed is older (an official version never goes back).
pub(crate) fn compare(installed: &str, learned: Option<&Learned>) -> Option<Ordering> {
    let learned = learned?;
    if !is_version(installed) { return None; }
    if installed == learned.version { return Some(Ordering::Equal); }
    let key = |version: &str| -> Option<(u32, u32)> {
        let (date, revision) = version.split_once('_')?;
        Some((date.parse().ok()?, revision.parse().ok()?))
    };
    match (key(installed), key(&learned.version)) {
        (Some(installed), Some(official)) if installed < official => Some(Ordering::Less),
        _ => None,
    }
}
/// "Latest version" for FFXI: the version the report carries when a real check gave one,
/// else the one the last real check remembered with its age, else a request to press Play.
pub(crate) fn latest_game_text(app: &crate::App) -> String {
    latest_text(app.patch.as_ref().map(|p| p.remote_version.as_str()), app.learned_game(), now())
}
/// The same for PlayOnline.
pub(crate) fn latest_playonline_text(app: &crate::App) -> String {
    latest_text(app.viewer.as_ref().map(|v| v.version.remote_version.as_str()), app.official.playonline(), now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crystal-official-{name}-{}-{}", std::process::id(), now()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn versions_are_remembered_per_game_folder_and_survive_a_reload() {
        let dir = temp("persist");
        let file = dir.join("official-versions.json");
        let (one, two) = (Path::new(r"C:\Games\FFXI"), Path::new(r"D:\Other\FFXI"));
        learn_game_at(&file, one, "30260905_0", 1_000);
        learn_game_at(&file, two, "30260801_2", 2_000);
        learn_playonline_at(&file, "20110829_E", 3_000);
        let store = load(&file);
        assert_eq!(store.game(one), Some(&Learned { version: "30260905_0".into(), at: 1_000 }));
        assert_eq!(store.game(two).map(|l| l.version.as_str()), Some("30260801_2"));
        assert_eq!(store.playonline().map(|l| l.at), Some(3_000));
        // Case and a trailing separator name the same folder.
        assert_eq!(store.game(Path::new(r"c:\games\ffxi\")).map(|l| l.at), Some(1_000));
        assert_eq!(store.game(Path::new(r"C:\Elsewhere")), None);
        // A newer answer replaces the version; the same one inside the rewrite window keeps its time.
        learn_game_at(&file, one, "30260906_0", 4_000);
        learn_game_at(&file, one, "30260906_0", 4_100);
        assert_eq!(load(&file).game(one), Some(&Learned { version: "30260906_0".into(), at: 4_000 }));
        learn_game_at(&file, one, "30260906_0", 5_000);
        assert_eq!(load(&file).game(one).map(|l| l.at), Some(5_000));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn placeholders_and_damaged_files_are_never_remembered() {
        let dir = temp("junk");
        let file = dir.join("official-versions.json");
        for junk in ["Not checked", "Unavailable", "", "30260905", "3026090_0", "30260905_0 ", "30260905_é"] {
            learn_game_at(&file, Path::new(r"C:\Games\FFXI"), junk, 10);
            learn_playonline_at(&file, junk, 10);
        }
        assert!(!file.exists());
        fs::write(&file, b"{ not json").unwrap();
        assert_eq!(load(&file).game(Path::new(r"C:\Games\FFXI")), None);
        learn_game_at(&file, Path::new(r"C:\Games\FFXI"), "30260905_0", 10);
        assert!(load(&file).game(Path::new(r"C:\Games\FFXI")).is_some(), "a damaged file is replaced by the next answer");
        fs::remove_dir_all(dir).unwrap();
    }

    /// Another process's load-change-save holds the store lock: this write waits for it, and
    /// is skipped (never written over the other's) when the lock stays taken.
    #[test]
    fn another_processs_write_holds_the_store_until_it_saves() {
        let dir = temp("lock");
        let file = dir.join("official-versions.json");
        let (one, two) = (Path::new(r"C:\Games\FFXI"), Path::new(r"D:\Other\FFXI"));
        learn_game_at(&file, one, "30260905_0", 1_000);
        let other = lock_store(&file).expect("free lock");
        let started = std::time::Instant::now();
        learn_game_at(&file, two, "30260905_0", 1_001);
        assert!(started.elapsed() >= LOCK_WAIT, "waited for the other writer");
        assert!(load(&file).game(two).is_none(), "skipped while the other writer holds the store");
        // The other writer's save lands, then this one merges onto it rather than replacing it.
        let mut store = load(&file);
        store.learn_playonline("20110829_E", 1_002);
        save(&file, &store);
        drop(other);
        learn_game_at(&file, two, "30260905_0", 1_003);
        let store = load(&file);
        assert!(store.game(one).is_some() && store.game(two).is_some() && store.playonline().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_oldest_folder_is_dropped_past_the_limit() {
        let mut store = Store::default();
        for n in 0..(MAX_FOLDERS as i64 + 3) {
            store.learn_game(Path::new(&format!(r"C:\g{n}")), "30260905_0", 100 + n);
        }
        assert_eq!(store.game.len(), MAX_FOLDERS);
        assert!(store.game(Path::new(r"C:\g0")).is_none() && store.game(Path::new(r"C:\g18")).is_some());
    }

    #[test]
    fn ages_read_in_words() {
        use crate::i18n::{set, Lang};
        set(Lang::En);
        let now = 10_000_000;
        assert_eq!(when(now, now), "just now");
        assert_eq!(when(now + 5, now), "just now", "a clock that went back is not in the future");
        assert_eq!(when(now - 5 * 60 - 3, now), "5 min ago");
        assert_eq!(when(now - 3 * 3600 - 59, now), "3 h ago");
        assert_eq!(when(now - 2 * 86_400, now), "2 d ago");
        assert!(when(now - 40 * 86_400, now).starts_with("on 1970-"), "{}", when(now - 40 * 86_400, now));
    }

    #[test]
    fn latest_shows_the_remembered_version_with_its_age_or_asks_for_a_play() {
        use crate::i18n::{set, Lang};
        set(Lang::En);
        let learned = Learned { version: "30260905_0".into(), at: 1_000 };
        let now = 1_000 + 2 * 86_400;
        assert_eq!(latest_text(Some("Not checked"), Some(&learned), now), "30260905_0 (checked 2 d ago)");
        assert_eq!(latest_text(None, Some(&learned), now), "30260905_0 (checked 2 d ago)");
        assert_eq!(latest_text(Some("30260905_0"), Some(&learned), now), "30260905_0 (checked 2 d ago)");
        // A report newer than the memory is shown as it is.
        assert_eq!(latest_text(Some("30260906_0"), Some(&learned), now), "30260906_0");
        // Nothing known: asks for a Play, never "Not checked".
        assert_eq!(latest_text(Some("Not checked"), None, now), "Press Play to check");
        assert_eq!(latest_text(None, None, now), "Press Play to check");
    }

    #[test]
    fn installed_is_compared_with_the_remembered_version() {
        let learned = Learned { version: "30260905_0".into(), at: 1 };
        assert_eq!(compare("30260905_0", Some(&learned)), Some(Ordering::Equal));
        assert_eq!(compare("30260904_1", Some(&learned)), Some(Ordering::Less));
        assert_eq!(compare("30260905_0", None), None);
        assert_eq!(compare("Unavailable", Some(&learned)), None);
        assert_eq!(compare("30260906_0", Some(&learned)), None, "newer than remembered says nothing");
    }
}
