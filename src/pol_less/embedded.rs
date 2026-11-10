//! The 32-bit loader (`pol.exe`) travels inside the launcher exe (`build.rs` embeds it) and is
//! written to `<data dir>\crystal\pol.exe` when a matching installed host is unavailable.
//! The loader writes its own log beside itself, so both locations stay user-writable.
//!
//! The file is rewritten only when its SHA-256 differs from the embedded one, in place under its
//! own name (no temp file renamed into an executable: that drop-then-rename pattern is what
//! antivirus heuristics flag). Nothing runs it until its hash is checked again, so a write cut
//! short is just rewritten on the next launch. A running game keeps the old `pol.exe` open, so
//! it cannot be rewritten; the new build then goes to `crystal\<hash8>\pol.exe` and the old one
//! stays until nothing runs it.

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

/// Writes `bytes` to `path` under its final name. Callers check the hash before running it,
/// and a failed write leaves no partial file behind.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent folder"))?;
    fs::create_dir_all(dir)?;
    let result = fs::write(path, bytes);
    if result.is_err() && !has_hash(path, &sha256_hex(bytes)) {
        // A file another process holds open fails before writing anything; leave it be.
        if fs::metadata(path).is_ok_and(|m| m.len() != bytes.len() as u64) {
            let _ = fs::remove_file(path);
        }
    }
    result
}

/// Puts the loader in `<root>\crystal` and returns the path to run. The standard
/// `crystal\pol.exe` when it already matches or can be replaced, else `crystal\<hash8>\pol.exe`.
pub(crate) fn extract_into(root: &Path, bytes: &[u8], hash: &str) -> io::Result<PathBuf> {
    let standard = root.join("crystal").join("pol.exe");
    if has_hash(&standard, hash) {
        return Ok(standard);
    }
    if write_atomic(&standard, bytes).is_ok() && has_hash(&standard, hash) {
        return Ok(standard);
    }
    // In use by a running game (or otherwise unreplaceable): a versioned copy beside it.
    let versioned = root.join("crystal").join(&hash[..8.min(hash.len())]).join("pol.exe");
    if has_hash(&versioned, hash) {
        return Ok(versioned);
    }
    write_atomic(&versioned, bytes)?;
    Ok(versioned)
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
    fn the_embedded_loader_matches_its_recorded_hash() {
        if let Some((bytes, hash)) = embedded() {
            assert_eq!(sha256_hex(bytes), hash);
        }
    }
}
