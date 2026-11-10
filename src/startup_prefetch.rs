//! Startup file pre-read: warm the OS file cache with the files FFXI reads between
//! GameStart and the title screen, so a cold launch (first after a reboot, or after the
//! standby list was repurposed) does not wait on the disk.
//!
//! Measured on the owner's machine (NVMe, SysMain disabled, 2026-10-22): launches whose
//! `GameMain DLL load` was cold (0.17-0.35 s instead of 0.009 s) reached the warning screen
//! 1.2-1.7 s later and the title 0.3 s later than warm ones. The ~71 MB the game read up to
//! character select took 1.28 s to read cold and 17 ms warm.
//!
//! Which files: a fixed seed (the FFXI DLLs, cursors and the VTABLE/FTABLE of every ROM
//! folder) plus a learned list. Learning is passive and read-only: some time after a game
//! start, the game folder's directory entries are listed and every file whose NTFS last
//! access time falls inside the start window is added. Nothing hooks the game, and nothing
//! is ever written to the game folder (reads only, shared with every writer).
//!
//! Limits: one pre-read at a time, at most `MAX_BYTES` in total and `MAX_FILE_BYTES` per
//! file, low I/O priority, sequential-scan reads, skipped on battery, when free memory is
//! short, or when it ran within `REPEAT_AFTER`; cancelled by `cancel()` (an update or repair
//! starting, Crystal closing).
//!
//! Our own reads update last access times too, but they finish seconds before the game
//! starts (outside the learning window), and any file read inside the window by us was
//! already listed, so learning only ever adds what the game read. The learned list is
//! dropped whenever the game version changes (FFXiMain.dll or VTABLE.DAT size/time).

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Total bytes one pre-read reads, at most.
pub(crate) const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Bytes read from any one file, at most (the start reads parts of the big DATs).
pub(crate) const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Learned files kept per game folder, at most.
const MAX_FILES: usize = 4000;
/// A pre-read of the same folder within this long is skipped (the cache is still warm).
const REPEAT_AFTER: Duration = Duration::from_secs(5 * 60);
/// NTFS updates a last access time only when the stored one is over an hour old, so one
/// learning pass per hour per folder is all that can see new files.
const LEARN_EVERY: Duration = Duration::from_secs(65 * 60);
/// The learning window around the observed game start: GameStart happens shortly before
/// the launcher's tick sees the game, and the title plus character select follow within
/// ~15 s of it on the measured launches. A quick character login can put some zone data
/// inside too; the list is ordered by read time and capped at `MAX_BYTES`, so the title's
/// files always come first.
const WINDOW_BEFORE: Duration = Duration::from_secs(10);
const WINDOW_AFTER: Duration = Duration::from_secs(25);
/// Folders the game writes or that hold nothing the start reads.
const SKIPPED_DIRS: [&str; 4] = ["user", "temp", "sys", "mov"];
/// Free physical memory a pre-read leaves, at least, beyond what it reads.
const MIN_FREE_MEMORY: u64 = 1024 * 1024 * 1024;

const STORE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Store {
    version: u32,
    #[serde(default)]
    folders: BTreeMap<String, Folder>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Folder {
    /// The game version these files belong to (`version_stamp`).
    stamp: String,
    /// Relative paths in the order the game first read them.
    files: Vec<Learned>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Learned {
    path: String,
    /// Milliseconds after the window start the game first read it (orders the pre-read).
    at_ms: u64,
    /// Its size when learned (bounds the list).
    #[serde(default)]
    bytes: u64,
}

/// The game version: FFXiMain.dll and VTABLE.DAT size and write time. A patch changes both.
fn version_stamp(game: &Path) -> Option<String> {
    let part = |name: &str| -> Option<String> {
        let meta = fs::metadata(game.join(name)).ok()?;
        let written = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some(format!("{}:{written}", meta.len()))
    };
    Some(format!("{}|{}", part("FFXiMain.dll")?, part("VTABLE.DAT")?))
}

fn folder_key(game: &Path) -> String { game.to_string_lossy().to_lowercase() }

fn load(store: &Path) -> Store {
    fs::read(store).ok()
        .and_then(|bytes| serde_json::from_slice::<Store>(&bytes).ok())
        .filter(|store| store.version == STORE_VERSION)
        .unwrap_or(Store { version: STORE_VERSION, folders: BTreeMap::new() })
}

fn save(store_path: &Path, store: &Store) {
    let Ok(bytes) = serde_json::to_vec(store) else { return };
    if let Some(parent) = store_path.parent() { let _ = fs::create_dir_all(parent); }
    let _ = crate::fsutil::write_atomic(store_path, &bytes);
}

/// A stored path is used only when it is plain and relative: no root, drive, `..` or `.`.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let relative = PathBuf::from(path);
    if path.is_empty() || !relative.components().all(|part| matches!(part, Component::Normal(_))) { return None; }
    Some(relative)
}

/// Files every FFXI start reads, whether or not anything was learned yet.
fn seed(game: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ["FFXiMain.dll", "FFXi.dll", "VTABLE.DAT", "FTABLE.DAT"].iter().map(PathBuf::from).collect();
    for n in 2..=9 {
        files.push(PathBuf::from(format!("ROM{n}")).join(format!("VTABLE{n}.DAT")));
        files.push(PathBuf::from(format!("ROM{n}")).join(format!("FTABLE{n}.DAT")));
    }
    if let Ok(entries) = fs::read_dir(game) {
        let mut cursors: Vec<PathBuf> = entries.flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| PathBuf::from(entry.file_name()))
            .filter(|name| name.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("ani")))
            .collect();
        cursors.sort();
        files.extend(cursors);
    }
    files
}

/// What one pre-read would read, in order, without duplicates.
fn plan(game: &Path, store: &Store) -> Vec<PathBuf> {
    let mut files = seed(game);
    if let Some(folder) = store.folders.get(&folder_key(game)) {
        if version_stamp(game).as_deref() == Some(folder.stamp.as_str()) {
            files.extend(folder.files.iter().filter_map(|file| safe_relative(&file.path)));
        }
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|file| seen.insert(file.to_string_lossy().to_lowercase()));
    files
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub files: usize,
    pub bytes: u64,
    pub cancelled: bool,
}

/// Read `files` (relative to `game`) into the file cache, stopping at the byte caps or when
/// `cancelled` says so. Only regular files under `game` are opened, read-only and shared with
/// every writer and deleter, so an update or the game itself is never blocked.
fn read_files(game: &Path, files: &[PathBuf], cancelled: &dyn Fn() -> bool, max_bytes: u64) -> Report {
    let mut report = Report::default();
    let mut buffer = vec![0u8; 1024 * 1024];
    for relative in files {
        if report.bytes >= max_bytes { break; }
        let path = game.join(relative);
        // No reparse points: a link inside the game folder could point anywhere.
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if !meta.is_file() { continue; }
        let Ok(mut file) = open_for_prefetch(&path) else { continue };
        let limit = MAX_FILE_BYTES.min(max_bytes - report.bytes);
        let mut read = 0u64;
        while read < limit {
            if cancelled() { report.cancelled = true; return report; }
            let want = buffer.len().min((limit - read) as usize);
            match file.read(&mut buffer[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n as u64,
            }
        }
        report.bytes += read;
        report.files += 1;
    }
    report
}

fn open_for_prefetch(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileIoPriorityHintInfo, SetFileInformationByHandle, FILE_FLAG_SEQUENTIAL_SCAN, FILE_IO_PRIORITY_HINT_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)?;
    // Low I/O priority: the game's own reads, and everything else, go first.
    let hint = FILE_IO_PRIORITY_HINT_INFO { PriorityHint: 1 /* IoPriorityHintLow */ };
    // SAFETY: a live handle owned by `file` and a correctly sized, initialised struct.
    unsafe {
        SetFileInformationByHandle(file.as_raw_handle() as _, FileIoPriorityHintInfo,
            (&hint as *const FILE_IO_PRIORITY_HINT_INFO).cast(), std::mem::size_of::<FILE_IO_PRIORITY_HINT_INFO>() as u32);
    }
    Ok(file)
}

/// Every file under `game` (minus `SKIPPED_DIRS`) last accessed in `[from, to]`, as
/// (relative path, ms after `from`, size). Lists directory entries only; opens no file.
fn accessed_between(game: &Path, from: SystemTime, to: SystemTime) -> Vec<(String, u64, u64)> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let Ok(entries) = fs::read_dir(game.join(&relative)) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            let child = relative.join(&name);
            if kind.is_dir() {
                let top = relative.as_os_str().is_empty();
                if !(top && SKIPPED_DIRS.iter().any(|skip| name.to_string_lossy().eq_ignore_ascii_case(skip))) {
                    pending.push(child);
                }
                continue;
            }
            if !kind.is_file() { continue; }
            // On Windows this is the directory listing's own data: no file is opened.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() == 0 { continue; }
            let Ok(accessed) = meta.accessed() else { continue };
            if accessed < from || accessed > to { continue; }
            let at = accessed.duration_since(from).unwrap_or_default().as_millis() as u64;
            found.push((child.to_string_lossy().into_owned(), at, meta.len()));
        }
    }
    found
}

/// Merge what the game read in `[from, to]` into the folder's list. A changed game version
/// starts the list over.
fn learn(store: &mut Store, game: &Path, from: SystemTime, to: SystemTime) -> usize {
    learn_within(store, game, from, to, MAX_BYTES)
}

fn learn_within(store: &mut Store, game: &Path, from: SystemTime, to: SystemTime, max_bytes: u64) -> usize {
    let Some(stamp) = version_stamp(game) else { return 0 };
    let folder = store.folders.entry(folder_key(game)).or_default();
    if folder.stamp != stamp { *folder = Folder { stamp, files: Vec::new() }; }
    let known: std::collections::HashSet<String> = folder.files.iter().map(|file| file.path.to_lowercase()).collect();
    let seeded: std::collections::HashSet<String> = seed(game).iter().map(|file| file.to_string_lossy().to_lowercase()).collect();
    let mut added = 0;
    for (path, at_ms, bytes) in accessed_between(game, from, to) {
        let key = path.to_lowercase();
        if known.contains(&key) || seeded.contains(&key) || safe_relative(&path).is_none() { continue; }
        folder.files.push(Learned { path, at_ms, bytes });
        added += 1;
    }
    // Stable: files learned earlier keep their place among equal times. The list keeps what
    // one pre-read can read; the earliest files (the title's) are the ones kept.
    folder.files.sort_by_key(|file| file.at_ms);
    let mut total = 0u64;
    let keep = folder.files.iter().take(MAX_FILES).take_while(|file| {
        total += file.bytes.min(MAX_FILE_BYTES);
        total <= max_bytes
    }).count();
    folder.files.truncate(keep);
    if added == 0 { return 0; }
    folder.files.iter().filter(|file| !known.contains(&file.path.to_lowercase())).count()
}

/// Process-wide state: one pre-read at a time, and when each folder last ran or learned.
struct State {
    last_read: BTreeMap<String, Instant>,
    last_learn: BTreeMap<String, Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { last_read: BTreeMap::new(), last_learn: BTreeMap::new() });
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Bumped by `cancel()`; a read stops once it differs from the value it started with.
static CANCEL_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Serialises the store file between the pre-read and learning threads.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Stop a running pre-read (an update or repair starts, Crystal closes).
pub(crate) fn cancel() {
    CANCEL_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Only a read that ran to the end counts as recent: a cancelled one warmed nothing.
fn record_read(key: String, report: &Report) {
    if !report.cancelled {
        STATE.lock().unwrap_or_else(|poison| poison.into_inner()).last_read.insert(key, Instant::now());
    }
}

/// Start a background pre-read of `game`'s startup files. `store` is the learned-list file;
/// `history` gets one line when the read finishes. False when it was skipped: no game, read
/// recently, already running, on battery or short of memory.
pub(crate) fn start(game: &Path, store: &Path, history: &Path) -> bool {
    // Before anything else, so a cancel() racing with this start is still seen.
    let generation = CANCEL_GENERATION.load(Ordering::Relaxed);
    if !game.join("FFXiMain.dll").is_file() { return false; }
    let key = folder_key(game);
    {
        let state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.last_read.get(&key).is_some_and(|at| at.elapsed() < REPEAT_AFTER) { return false; }
    }
    if on_battery() { return false; }
    if free_memory().is_some_and(|free| free < MAX_BYTES + MIN_FREE_MEMORY) { return false; }
    if RUNNING.swap(true, Ordering::AcqRel) { return false; }
    let (game, store, history) = (game.to_path_buf(), store.to_path_buf(), history.to_path_buf());
    let spawned = std::thread::Builder::new().name("xi-startup-prefetch".into()).spawn(move || {
        lower_thread_priority();
        let files = {
            let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            plan(&game, &load(&store))
        };
        let began = Instant::now();
        let cancelled = || CANCEL_GENERATION.load(Ordering::Relaxed) != generation;
        // A cancel between `start` and here reads nothing at all.
        let report = if cancelled() {
            Report { cancelled: true, ..Report::default() }
        } else { read_files(&game, &files, &cancelled, MAX_BYTES) };
        record_read(key, &report);
        RUNNING.store(false, Ordering::Release);
        crate::launch_log::append_history(&history, "Crystal", &format!(
            "startup pre-read: {} of {} files, {:.1} MB in {} ms{}",
            report.files, files.len(), report.bytes as f64 / (1024.0 * 1024.0), began.elapsed().as_millis(),
            if report.cancelled { ", cancelled" } else { "" }), None);
    });
    if spawned.is_err() { RUNNING.store(false, Ordering::Release); }
    true
}

/// The launcher saw a game of `game` start: some time later, add the files the start read
/// to the learned list. At most once per `LEARN_EVERY` per folder.
pub(crate) fn game_started(game: &Path, store: &Path) {
    if !game.join("FFXiMain.dll").is_file() { return; }
    let key = folder_key(game);
    {
        let mut state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.last_learn.get(&key).is_some_and(|at| at.elapsed() < LEARN_EVERY) { return; }
        state.last_learn.insert(key, Instant::now());
    }
    let seen = SystemTime::now();
    let (game, store) = (game.to_path_buf(), store.to_path_buf());
    let _ = std::thread::Builder::new().name("xi-startup-learn".into()).spawn(move || {
        lower_thread_priority();
        std::thread::sleep(WINDOW_AFTER + Duration::from_secs(5));
        let from = seen.checked_sub(WINDOW_BEFORE).unwrap_or(seen);
        let _guard = STORE_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut current = load(&store);
        let before = current.clone();
        learn(&mut current, &game, from, seen + WINDOW_AFTER);
        if current != before { save(&store, &current); }
    });
}

fn lower_thread_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST};
    // SAFETY: the pseudo handle of the calling thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST); }
}

fn on_battery() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // SAFETY: plain integers; all-zero is a valid value.
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: writes the live local struct.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 { return false; }
    // 0 = offline (on battery); 1 = online; 255 = unknown (desktops without a battery report 1).
    status.ACLineStatus == 0
}

fn free_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain integers; all-zero is a valid value.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `dwLength` is set on the live local struct.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 { return None; }
    Some(status.ullAvailPhys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("crystal-prefetch-{name}-{}-{}", std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    fn write(root: &Path, relative: &str, bytes: usize, accessed: SystemTime) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![7u8; bytes]).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(FileTimes::new().set_accessed(accessed)).unwrap();
    }

    fn fake_game(name: &str) -> TempDir {
        let dir = TempDir::new(name);
        let old = SystemTime::now() - Duration::from_secs(3 * 3600);
        write(&dir.0, "FFXiMain.dll", 100, old);
        write(&dir.0, "VTABLE.DAT", 10, old);
        write(&dir.0, "FTABLE.DAT", 20, old);
        write(&dir.0, "arw_n.ani", 9, old);
        dir
    }

    #[test]
    fn learning_adds_only_files_read_in_the_window_in_read_order() {
        let game = fake_game("learn");
        let start = SystemTime::now() - Duration::from_secs(2 * 3600);
        write(&game.0, "ROM/165/84.DAT", 50, start + Duration::from_secs(3));
        write(&game.0, "ROM/0/0.DAT", 50, start + Duration::from_secs(1));
        write(&game.0, "ROM/9/9.DAT", 50, start + Duration::from_secs(120)); // after the window
        write(&game.0, "ROM/9/8.DAT", 50, start - Duration::from_secs(60)); // before it
        write(&game.0, "USER/abc/setting.dat", 50, start + Duration::from_secs(2)); // written by the game
        write(&game.0, "sound/win/se/se000/se000002.spw", 50, start + Duration::from_secs(5));
        write(&game.0, "VTABLE.DAT", 10, start + Duration::from_secs(1)); // seeded already
        let mut store = Store { version: STORE_VERSION, ..Store::default() };
        assert_eq!(learn(&mut store, &game.0, start, start + WINDOW_AFTER), 3);
        let folder = &store.folders[&folder_key(&game.0)];
        let paths: Vec<_> = folder.files.iter().map(|file| file.path.replace('\\', "/")).collect();
        assert_eq!(paths, ["ROM/0/0.DAT", "ROM/165/84.DAT", "sound/win/se/se000/se000002.spw"]);
        // A second pass in the same window adds nothing twice.
        assert_eq!(learn(&mut store, &game.0, start, start + WINDOW_AFTER), 0);
    }

    #[test]
    fn the_list_keeps_the_earliest_files_within_the_byte_cap() {
        let game = fake_game("cap");
        let start = SystemTime::now() - Duration::from_secs(2 * 3600);
        write(&game.0, "ROM/0/late.DAT", 600, start + Duration::from_secs(9));
        write(&game.0, "ROM/0/first.DAT", 400, start + Duration::from_secs(1));
        write(&game.0, "ROM/0/second.DAT", 500, start + Duration::from_secs(2));
        let mut store = Store { version: STORE_VERSION, ..Store::default() };
        assert_eq!(learn_within(&mut store, &game.0, start, start + WINDOW_AFTER, 1000), 2);
        let paths: Vec<_> = store.folders[&folder_key(&game.0)].files.iter().map(|file| file.path.replace('\\', "/")).collect();
        assert_eq!(paths, ["ROM/0/first.DAT", "ROM/0/second.DAT"]);
        // A later start that read an earlier file first pushes the latest one out.
        write(&game.0, "ROM/0/zero.DAT", 300, start + Duration::from_millis(500));
        learn_within(&mut store, &game.0, start, start + WINDOW_AFTER, 1000);
        let paths: Vec<_> = store.folders[&folder_key(&game.0)].files.iter().map(|file| file.path.replace('\\', "/")).collect();
        assert_eq!(paths, ["ROM/0/zero.DAT", "ROM/0/first.DAT"]);
    }

    #[test]
    fn a_new_game_version_drops_the_learned_list() {
        let game = fake_game("version");
        let start = SystemTime::now() - Duration::from_secs(2 * 3600);
        write(&game.0, "ROM/1/1.DAT", 50, start + Duration::from_secs(1));
        let mut store = Store { version: STORE_VERSION, ..Store::default() };
        learn(&mut store, &game.0, start, start + WINDOW_AFTER);
        assert_eq!(plan(&game.0, &store).iter().filter(|file| file.starts_with("ROM")).count(), 1);
        // A patch rewrites FFXiMain.dll: the old list is no longer read.
        let main = fs::OpenOptions::new().write(true).open(game.0.join("FFXiMain.dll")).unwrap();
        main.set_len(200).unwrap();
        main.set_times(FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(60))).unwrap();
        drop(main);
        assert!(plan(&game.0, &store).iter().all(|file| !file.starts_with("ROM")));
        learn(&mut store, &game.0, start + Duration::from_secs(3600), start + Duration::from_secs(3640));
        assert!(store.folders[&folder_key(&game.0)].files.is_empty());
    }

    #[test]
    fn stored_paths_must_stay_inside_the_game_folder() {
        assert!(safe_relative("ROM\\0\\0.DAT").is_some());
        assert!(safe_relative("..\\outside.dat").is_none());
        assert!(safe_relative("ROM\\..\\..\\x").is_none());
        assert!(safe_relative("C:\\Windows\\win.ini").is_none());
        assert!(safe_relative("\\\\server\\share\\x").is_none());
        assert!(safe_relative("").is_none());
        let game = fake_game("unsafe");
        let mut store = Store { version: STORE_VERSION, ..Store::default() };
        store.folders.insert(folder_key(&game.0), Folder { stamp: version_stamp(&game.0).unwrap(),
            files: vec![Learned { path: "..\\x.dat".into(), at_ms: 0, bytes: 1 }, Learned { path: "ROM\\1\\1.DAT".into(), at_ms: 1, bytes: 1 }] });
        let files = plan(&game.0, &store);
        assert!(files.iter().all(|file| !file.to_string_lossy().contains("..")));
        assert!(files.contains(&PathBuf::from("ROM\\1\\1.DAT")));
    }

    #[test]
    fn the_seed_lists_dlls_tables_and_cursors_once() {
        let game = fake_game("seed");
        let files = plan(&game.0, &Store::default());
        assert_eq!(files[0], PathBuf::from("FFXiMain.dll"));
        assert!(files.contains(&PathBuf::from("ROM9").join("FTABLE9.DAT")));
        assert!(files.contains(&PathBuf::from("arw_n.ani")));
        let mut unique = files.clone();
        unique.dedup();
        assert_eq!(unique.len(), files.len());
    }

    #[test]
    fn reading_respects_the_byte_cap_skips_missing_files_and_writes_nothing() {
        let game = fake_game("read");
        let old = SystemTime::now() - Duration::from_secs(3 * 3600);
        write(&game.0, "ROM/1/a.DAT", 3 * 1024 * 1024, old);
        write(&game.0, "ROM/1/b.DAT", 3 * 1024 * 1024, old);
        let written_before = fs::metadata(game.0.join("ROM/1/a.DAT")).unwrap().modified().unwrap();
        let files = [PathBuf::from("missing.DAT"), PathBuf::from("ROM\\1\\a.DAT"), PathBuf::from("ROM\\1\\b.DAT"), PathBuf::from("ROM")];
        let report = read_files(&game.0, &files, &|| false, 4 * 1024 * 1024);
        assert_eq!(report, Report { files: 2, bytes: 4 * 1024 * 1024, cancelled: false });
        assert_eq!(fs::metadata(game.0.join("ROM/1/a.DAT")).unwrap().modified().unwrap(), written_before);
        assert_eq!(fs::metadata(game.0.join("ROM/1/a.DAT")).unwrap().len(), 3 * 1024 * 1024);
    }

    #[test]
    fn a_cancelled_read_stops_at_once() {
        let game = fake_game("cancel");
        write(&game.0, "ROM/1/a.DAT", 2 * 1024 * 1024, SystemTime::now());
        let report = read_files(&game.0, &[PathBuf::from("ROM\\1\\a.DAT")], &|| true, MAX_BYTES);
        assert!(report.cancelled);
        assert_eq!(report.bytes, 0);
    }

    #[test]
    fn only_a_finished_read_counts_as_recent() {
        let key = "c:\\record-read-test".to_owned();
        let recent = || STATE.lock().unwrap().last_read.contains_key(&key);
        record_read(key.clone(), &Report { files: 0, bytes: 0, cancelled: true });
        assert!(!recent(), "a cancelled pre-read must not hold off the next one");
        record_read(key.clone(), &Report { files: 3, bytes: 9, cancelled: false });
        assert!(recent());
        STATE.lock().unwrap().last_read.remove(&key);
    }

    #[test]
    fn the_store_round_trips_and_ignores_other_versions() {
        let dir = TempDir::new("store");
        let path = dir.0.join("startup-prefetch.json");
        let mut store = Store { version: STORE_VERSION, ..Store::default() };
        store.folders.insert("c:\\game".into(), Folder { stamp: "1:2|3:4".into(), files: vec![Learned { path: "ROM\\0\\0.DAT".into(), at_ms: 5, bytes: 9 }] });
        save(&path, &store);
        assert_eq!(load(&path), store);
        fs::write(&path, br#"{"version":99,"folders":{}}"#).unwrap();
        assert!(load(&path).folders.is_empty());
        fs::write(&path, b"not json").unwrap();
        assert!(load(&path).folders.is_empty());
    }

    #[test]
    fn a_folder_without_ffxi_is_never_read() {
        let dir = TempDir::new("nogame");
        assert!(!start(&dir.0, &dir.0.join("s.json"), &dir.0.join("h.log")));
    }
}
