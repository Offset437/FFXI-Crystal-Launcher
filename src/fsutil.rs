//! Whole-file replacement: a reader sees the previous file or the new one, never part of either.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Which step of [`write_atomic`] failed. The previous file is intact either way.
#[derive(Debug)]
pub(crate) struct WriteError {
    /// The new bytes were staged and flushed; only replacing the target failed.
    pub(crate) replacing: bool,
    pub(crate) error: io::Error,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.error.fmt(f) }
}

impl From<WriteError> for io::Error {
    fn from(error: WriteError) -> Self { error.error }
}

/// Writes `bytes` to a staging file beside `path` that belongs to this write alone, flushes
/// it, then replaces `path` with it. The staging file is removed whenever the write fails.
/// The parent folder must exist.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let staging = |error| WriteError { replacing: false, error };
    // A shared staging name would let concurrent writers replace or delete each other's data.
    let (temporary, mut file) = loop {
        let mut name = path.file_name()
            .ok_or_else(|| staging(io::Error::new(io::ErrorKind::InvalidInput, "missing file name")))?
            .to_os_string();
        name.push(format!(".{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let temporary = path.with_file_name(name);
        // create_new never follows or overwrites a planted path.
        match OpenOptions::new().write(true).create_new(true).open(&temporary) {
            Ok(file) => break (Staged(temporary), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(staging(error)),
        }
    };
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(staging)?;
    drop(file);
    replace(&temporary.0, path).map_err(|error| WriteError { replacing: true, error })
}

/// Removes the staging file unless it was moved into place.
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

/// Replaces `target` with the flushed file `source` in one step, writing the rename through.
/// A scanner or reader without delete sharing may hold the target for a moment, so access and
/// sharing failures are retried for up to 150 ms; any other failure returns at once. The
/// previous `target` is kept whenever this fails.
pub(crate) fn replace(source: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let wide_path = |path: &Path| -> io::Result<Vec<u16>> {
        let name = path.file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        // Canonicalizing supplies the extended drive/UNC prefix for long paths. Only the
        // parent is resolved: the target need not exist, and replacing a link must replace
        // that directory entry rather than what it points to.
        Ok(crate::win::wide_path(&fs::canonicalize(parent)?.join(name)))
    };
    let source = wide_path(source)?;
    let target = wide_path(target)?;
    let deadline = Instant::now() + Duration::from_millis(150);
    let mut pause = Duration::from_millis(5);
    loop {
        // SAFETY: both NUL-terminated buffers live through the synchronous call.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
        if !matches!(error.raw_os_error(), Some(5 | 32 | 33)) || Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(pause.min(deadline.saturating_duration_since(Instant::now())));
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crystal-fsutil-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staging_left(dir: &Path) -> bool {
        fs::read_dir(dir).unwrap().any(|e| e.unwrap().path().extension().is_some_and(|x| x == "tmp"))
    }

    #[test]
    fn write_atomic_creates_and_replaces_without_leaving_staging_files() {
        let dir = folder("replace");
        let path = dir.join("store.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert!(!staging_left(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_replace_keeps_the_previous_file_and_says_so() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let dir = folder("held");
        let path = dir.join("store.json");
        write_atomic(&path, b"old").unwrap();
        let held = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE).open(&path).unwrap();
        let error = write_atomic(&path, b"new").unwrap_err();
        assert!(error.replacing, "{error}");
        drop(held);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(!staging_left(&dir));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_fails_while_staging() {
        let dir = folder("missing");
        let error = write_atomic(&dir.join("absent").join("store.json"), b"x").unwrap_err();
        assert!(!error.replacing);
        let _ = fs::remove_dir_all(&dir);
    }
}
