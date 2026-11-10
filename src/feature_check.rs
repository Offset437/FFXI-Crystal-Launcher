//! Settings > System: the loader's per-build feature check. crystal-loader
//! checks the PlayOnline core's contract before each sign-in and saves
//! `crystal-pol.json`; it resolves FFXiMain (and Windower's Hook.dll) when a
//! game starts and saves
//! `crystal-features.json` beside its log, rewritten only when the build pair
//! or a result changes. This card shows the newest one: which game and
//! Windower builds it covers and every Crystal feature they switch off, with
//! the reason. Nothing here reads the game.
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

pub(crate) const FILE: &str = "crystal-features.json";

#[derive(Deserialize)]
struct Saved {
    v: u32,
    ffximain: String,
    hook: Option<SavedHook>,
    features: Vec<SavedFeature>,
}

#[derive(Deserialize)]
struct SavedHook {
    timestamp: String,
    known: Option<String>,
}

#[derive(Deserialize)]
struct SavedFeature {
    name: String,
    ok: bool,
    reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Check {
    /// e.g. "FINAL FANTASY XI 6AC37846 · Windower Hook 4.7.9.3".
    pub builds: String,
    pub total: usize,
    /// (feature, reason) for every feature that is off.
    pub off: Vec<(String, String)>,
}

fn parse(bytes: &[u8]) -> Option<Check> {
    let saved: Saved = serde_json::from_slice(bytes).ok()?;
    if saved.v != 1 || saved.features.is_empty() {
        return None;
    }
    let hook = match &saved.hook {
        Some(SavedHook { known: Some(label), .. }) => format!(" · Windower Hook {label}"),
        Some(SavedHook { timestamp, known: None }) => format!(" · Windower Hook {timestamp}"),
        None => String::new(),
    };
    Some(Check {
        builds: format!("FINAL FANTASY XI {}{hook}", saved.ffximain),
        total: saved.features.len(),
        off: saved.features.into_iter().filter(|f| !f.ok)
            .map(|f| (f.name, f.reason.unwrap_or_default())).collect(),
    })
}

/// The game host's PlayOnline core check, saved before each sign-in (so a core
/// that fails its check shows here with the reason): the core build, its
/// features and the host's 4 GB memory.
pub(crate) const POL_FILE: &str = "crystal-pol.json";

#[derive(Deserialize)]
struct SavedPol {
    v: u32,
    polcore: Option<String>,
    known: Option<String>,
    features: Vec<SavedFeature>,
}

fn parse_pol(bytes: &[u8]) -> Option<Check> {
    let saved: SavedPol = serde_json::from_slice(bytes).ok()?;
    if saved.v != 1 || saved.features.is_empty() {
        return None;
    }
    let builds = match (&saved.polcore, &saved.known) {
        (Some(core), Some(label)) => format!("PlayOnline {label} ({core})"),
        (Some(core), None) => format!("PlayOnline core {core}"),
        (None, _) => "PlayOnline core".to_string(),
    };
    Some(Check {
        builds,
        total: saved.features.len(),
        off: saved.features.into_iter().filter(|f| !f.ok).map(|f| (f.name, f.reason.unwrap_or_default())).collect(),
    })
}

/// The newest `file` among the folders a loader runs from, parsed.
fn newest_of(dirs: &[PathBuf], file: &str, parse: fn(&[u8]) -> Option<Check>) -> Option<Check> {
    dirs.iter()
        .filter_map(|dir| {
            let path = dir.join(file);
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .and_then(|(_, path): (SystemTime, PathBuf)| std::fs::read(path).ok())
        .and_then(|bytes| parse(&bytes))
}

/// The newest saved checks among the folders a loader runs from: the
/// PlayOnline core's first, then the game's and Windower's.
pub(crate) fn newest(dirs: &[PathBuf]) -> Option<Check> {
    match (newest_of(dirs, POL_FILE, parse_pol), newest_of(dirs, FILE, parse)) {
        (Some(pol), Some(game)) => Some(Check {
            builds: format!("{} · {}", pol.builds, game.builds),
            total: pol.total + game.total,
            off: pol.off.into_iter().chain(game.off).collect(),
        }),
        (pol, game) => pol.or(game),
    }
}

static CACHE: Mutex<Option<(Instant, Option<Check>)>> = Mutex::new(None);

/// The view runs often: look at the files at most every two seconds.
pub(crate) fn current(root: &Path, loader: &str) -> Option<Check> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, check)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(2) {
            return check.clone();
        }
    }
    let check = newest(&crate::dgvoodoo::loader_dirs(root, loader));
    *cache = Some((Instant::now(), check.clone()));
    check
}
#[cfg(test)]
mod tests {
    use super::*;

    const SAVED: &str = r#"{"v":1,"ffximain":"6AC37846","hook":{"timestamp":"6A5B1A74","known":"4.7.9.3"},"features":[{"name":"Auto-login","ok":true},{"name":"Background shutdown","ok":false,"reason":"shutdown.command: no match"}]}"#;

    #[test]
    fn the_loaders_saved_check_names_the_builds_and_every_feature_that_is_off() {
        let check = parse(SAVED.as_bytes()).unwrap();
        assert_eq!(check.builds, "FINAL FANTASY XI 6AC37846 · Windower Hook 4.7.9.3");
        assert_eq!(check.total, 2);
        assert_eq!(check.off, vec![("Background shutdown".to_string(), "shutdown.command: no match".to_string())]);
        let vanilla = parse(br#"{"v":1,"ffximain":"70000000","hook":null,"features":[{"name":"Auto-login","ok":true}]}"#).unwrap();
        assert_eq!((vanilla.builds.as_str(), vanilla.off.len()), ("FINAL FANTASY XI 70000000", 0));
        assert!(parse(br#"{"v":2,"ffximain":"1","hook":null,"features":[]}"#).is_none());
        assert!(parse(b"not json").is_none());
    }

    #[test]
    fn the_newest_check_across_loader_folders_wins() {
        let root = std::env::temp_dir().join(format!("crystal-feature-card-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join(FILE), SAVED).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(b.join(FILE), br#"{"v":1,"ffximain":"70000000","hook":null,"features":[{"name":"Auto-login","ok":true}]}"#).unwrap();
        let check = newest(&[a.clone(), b.clone(), root.join("missing")]).unwrap();
        assert_eq!(check.builds, "FINAL FANTASY XI 70000000");
        assert!(newest(&[root.join("missing")]).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    const POL: &str = r#"{"v":1,"polcore":"6ABA1641","known":"1.18.15f","features":[{"name":"PlayOnline sign-in","ok":true},{"name":"4 GB memory","ok":true}]}"#;

    #[test]
    fn the_playonline_core_check_comes_first_and_names_a_failed_core() {
        let pol = parse_pol(POL.as_bytes()).unwrap();
        assert_eq!((pol.builds.as_str(), pol.total, pol.off.len()), ("PlayOnline 1.18.15f (6ABA1641)", 2, 0));
        let failed = parse_pol(br#"{"v":1,"polcore":"7A000000","known":null,"features":[{"name":"PlayOnline sign-in","ok":false,"reason":"PlayOnline core check failed: service 815 (PlayOnline sign-in) is missing from the function table."},{"name":"4 GB memory","ok":true}]}"#).unwrap();
        assert_eq!(failed.builds, "PlayOnline core 7A000000");
        assert_eq!(failed.off[0].0, "PlayOnline sign-in");
        assert!(failed.off[0].1.contains("service 815"));
        assert!(parse_pol(br#"{"v":2,"polcore":null,"known":null,"features":[]}"#).is_none());

        let root = std::env::temp_dir().join(format!("crystal-feature-pol-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(POL_FILE), POL).unwrap();
        let only_pol = newest(std::slice::from_ref(&root)).unwrap();
        assert_eq!(only_pol.builds, "PlayOnline 1.18.15f (6ABA1641)");
        std::fs::write(root.join(FILE), SAVED).unwrap();
        let both = newest(std::slice::from_ref(&root)).unwrap();
        assert_eq!(both.builds, "PlayOnline 1.18.15f (6ABA1641) · FINAL FANTASY XI 6AC37846 · Windower Hook 4.7.9.3");
        assert_eq!((both.total, both.off.len()), (4, 1));
        let _ = std::fs::remove_dir_all(&root);
    }
}
