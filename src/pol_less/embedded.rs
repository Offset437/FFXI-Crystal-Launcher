//! The 32-bit loader (`pol.exe`) travels inside the launcher exe (`build.rs` embeds it) and is
//! written to `<data dir>\crystal\pol.exe` when a matching installed host is unavailable.
//! The loader writes its own log beside itself, so both locations stay user-writable.
//!
//! The file is rewritten only when its SHA-256 differs from the embedded one, through a
//! temporary file renamed into place (`fsutil::write_atomic`): sign-ins starting in parallel
//! never see or run a half-written loader, and the worker that loses the race finds the
//! winner's identical file. A running game keeps the old `pol.exe` open, so it cannot be
//! replaced; the new build then goes to `crystal\<hash8>\pol.exe` and the old one stays until
//! nothing runs it.

use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

static BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded-pol.exe"));
static SHA256: &str = include_str!(concat!(env!("OUT_DIR"), "/embedded-pol.sha256"));

/// The embedded loader and its SHA-256 (lower-case hex), or `None` for a build without one
/// (a debug build without a loader, or an `installed-host` build, which runs the installed copy).
pub(crate) fn embedded() -> Option<(&'static [u8], &'static str)> {
    (!BYTES.is_empty() && !SHA256.is_empty()).then_some((BYTES, SHA256.trim()))
}

/// The SHA-256 of the loader this launcher was built with, embedded or not: an installed
/// `crystal\pol.exe` must match it.
pub(crate) fn expected_hash() -> Option<&'static str> {
    (!SHA256.trim().is_empty()).then(|| SHA256.trim())
}

/// Lower-case hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        text.push(char::from(DIGITS[usize::from(b >> 4)]));
        text.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    text
}

/// The launcher's per-user data folder, where `crystal\` lives.
pub(crate) fn data_dir() -> PathBuf {
    crate::backend::Config::default().experiment.join("iced-launcher")
}

/// Extracts the embedded loader under the launcher's data folder; `None` when this build embeds
/// none or the folder can't be written.
pub(crate) fn extracted() -> Option<PathBuf> {
    // Unit tests never write into the real data folder.
    if cfg!(test) {
        return None;
    }
    let (bytes, hash) = embedded()?;
    extract_into(&data_dir(), bytes, hash).ok()
}

pub(super) fn has_hash(path: &Path, hash: &str) -> bool {
    fs::read(path).map(|b| sha256_hex(&b) == hash).unwrap_or(false)
}

/// Puts `bytes` (SHA-256 `hash`) at `path`, whole or not at all. A replace that failed because
/// another worker had just put the same loader there (and may already run it) is success.
fn write(path: &Path, bytes: &[u8], hash: &str) -> io::Result<()> {
    fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("no parent folder"))?)?;
    match crate::fsutil::write_atomic(path, bytes) {
        Err(_) if has_hash(path, hash) => Ok(()),
        result => Ok(result?),
    }
}

/// Puts the loader in `<root>\crystal` and returns the path to run. The standard
/// `crystal\pol.exe` when it already matches or can be replaced, else `crystal\<hash8>\pol.exe`.
pub(crate) fn extract_into(root: &Path, bytes: &[u8], hash: &str) -> io::Result<PathBuf> {
    let crystal = root.join("crystal");
    let standard = crystal.join("pol.exe");
    let path = if has_hash(&standard, hash) || write(&standard, bytes, hash).is_ok() {
        standard
    } else {
        // In use by a running game (or otherwise unreplaceable): a versioned copy beside it.
        let versioned = crystal.join(&hash[..8.min(hash.len())]).join("pol.exe");
        if !has_hash(&versioned, hash) {
            write(&versioned, bytes, hash)?;
        }
        versioned
    };
    prune_versioned(&crystal, path.parent().unwrap_or(&crystal));
    Ok(path)
}

/// Best effort: deletes the versioned `crystal\<hash8>\` folders (their old `pol.exe`, the
/// dgVoodoo files beside it, its log) other than `keep`. A folder whose `pol.exe` cannot be
/// deleted (Windows locks a running image) is left whole, and so is one used or written in the
/// last hour, which another launch, possibly in another Crystal process, may be preparing.
fn prune_versioned(crystal: &Path, keep: &Path) {
    let recent = |p: &Path| {
        fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().map_or(true, |age| age < std::time::Duration::from_secs(3600)))
    };
    for entry in fs::read_dir(crystal).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path == keep || name.len() != 8 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // A plain folder only: never follow a link out of Crystal's own folder.
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
            continue;
        }
        let host = path.join("pol.exe");
        if recent(&host) || recent(&path) || (host.exists() && fs::remove_file(&host).is_err()) {
            continue;
        }
        let _ = fs::remove_dir_all(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crystal-embedded-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fresh_extraction_writes_the_standard_path() {
        let root = temp("fresh");
        let bytes = b"loader one";
        let path = extract_into(&root, bytes, &sha256_hex(bytes)).unwrap();
        assert_eq!(path, root.join("crystal").join("pol.exe"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read_dir(root.join("crystal")).unwrap().count(), 1, "no temp file is left");
        // The same hash is not rewritten.
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(extract_into(&root, bytes, &sha256_hex(bytes)).unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        // A changed hash replaces the file.
        let new = b"loader two, newer";
        assert_eq!(extract_into(&root, new, &sha256_hex(new)).unwrap(), path);
        assert_eq!(fs::read(&path).unwrap(), new);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_locked_loader_gets_a_versioned_copy_and_stays_untouched() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = temp("locked");
        let old = b"loader one";
        let new = b"loader two, newer";
        let standard = extract_into(&root, old, &sha256_hex(old)).unwrap();
        // A running image can be read but not replaced or deleted (share mode: read only).
        let _held = fs::OpenOptions::new().read(true).share_mode(1).open(&standard).unwrap();
        let hash = sha256_hex(new);
        let path = extract_into(&root, new, &hash).unwrap();
        assert_eq!(path, root.join("crystal").join(&hash[..8]).join("pol.exe"));
        assert_eq!(fs::read(&path).unwrap(), new);
        assert_eq!(fs::read(&standard).unwrap(), old, "the running file is left alone");
        // A later launch reuses the versioned copy.
        assert_eq!(extract_into(&root, new, &hash).unwrap(), path);
        drop(_held);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parallel_extractions_all_get_the_whole_loader() {
        let root = temp("parallel");
        let bytes = vec![7u8; 4 << 20];
        let hash = sha256_hex(&bytes);
        let paths: Vec<_> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..8).map(|_| s.spawn(|| extract_into(&root, &bytes, &hash).unwrap())).collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        for path in paths {
            assert!(has_hash(&path, &hash), "{}", path.display());
        }
        assert!(has_hash(&root.join("crystal").join("pol.exe"), &hash));
        assert!(fs::read_dir(root.join("crystal")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stale_versioned_hosts_are_pruned_and_running_or_new_ones_kept() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::{Duration, SystemTime};
        let root = temp("prune");
        let crystal = root.join("crystal");
        let age = |path: &Path, when: SystemTime| {
            // FILE_WRITE_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS opens a folder too.
            fs::OpenOptions::new().access_mode(0x0100).custom_flags(0x0200_0000).open(path).unwrap().set_modified(when).unwrap();
        };
        let old = SystemTime::now() - Duration::from_secs(7200);
        let make = |name: &str, when: SystemTime| {
            let dir = crystal.join(name);
            fs::create_dir_all(&dir).unwrap();
            for file in ["pol.exe", "D3D8.dll", "dgVoodoo.conf"] {
                fs::write(dir.join(file), file).unwrap();
                age(&dir.join(file), when);
            }
            age(&dir, when);
            dir
        };
        let stale = make("0badc0de", old);
        let running = make("deadbeef", old);
        let fresh = make("cafef00d", SystemTime::now());
        let foreign = make("my-notes", old);
        // A running image can be read but not deleted.
        let held = fs::OpenOptions::new().read(true).share_mode(1).open(running.join("pol.exe")).unwrap();
        let bytes = b"loader one";
        let path = extract_into(&root, bytes, &sha256_hex(bytes)).unwrap();
        assert_eq!(path, crystal.join("pol.exe"));
        assert!(!stale.exists(), "an old host and its dgVoodoo files go");
        assert!(running.join("pol.exe").exists() && running.join("D3D8.dll").exists());
        assert!(fresh.join("pol.exe").exists());
        assert!(foreign.join("pol.exe").exists());
        drop(held);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_embedded_loader_matches_its_recorded_hash() {
        if let Some((bytes, hash)) = embedded() {
            assert_eq!(sha256_hex(bytes), hash);
        }
    }
}
