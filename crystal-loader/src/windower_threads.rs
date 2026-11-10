//! Faster thread lists for Windower's Hook during private startup.
//!
//! Before patching code, Hook freezes its own process: it takes a Toolhelp
//! thread snapshot, walks it for this process's threads, suspends them,
//! patches, and resumes them. A Toolhelp thread snapshot holds every thread
//! on the PC (thousands), so each walk costs ~30 ms; Hook does 37 of them while
//! it starts (measured: 1.12 s of a 1.2 s readiness wait, 2026-10-22).
//!
//! While the startup wait lasts, Hook's own `CreateToolhelp32Snapshot` for
//! threads is answered from a fresh Process Snapshotting (PSS) capture of this
//! process alone, taken at the same moment the call is made. Hook then walks
//! exactly the threads it would have kept (Toolhelp's other processes are the
//! ones it skips), and still opens, suspends and resumes them itself. Nothing
//! is cached between calls. Any other request, any failure and every call
//! after startup goes to the original functions.
//!
//! Hook calls `CloseHandle` while it holds other threads suspended, so the
//! `CloseHandle`/`Thread32*` wrappers never lock or allocate: cursors live in
//! a fixed table found by atomic compare.

use std::{
    cell::UnsafeCell,
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, SetLastError, ERROR_BAD_LENGTH, ERROR_NO_MORE_FILES, ERROR_NO_MORE_ITEMS, HANDLE},
    System::{
        Diagnostics::{
            ProcessSnapshotting::{
                PssCaptureSnapshot, PssFreeSnapshot, PssWalkMarkerCreate, PssWalkMarkerFree, PssWalkSnapshot,
                HPSS, HPSSWALK, PSS_CAPTURE_THREADS, PSS_THREAD_ENTRY, PSS_WALK_THREADS,
            },
            ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD},
        },
        Threading::{CreateEventW, GetCurrentProcess, GetCurrentProcessId},
    },
};

/// More threads than this in the host: use Toolhelp (the game has ~50).
const MAX_THREADS: usize = 1024;
/// Concurrent walks served at once; Hook walks one at a time.
const CURSORS: usize = 8;
/// A slot being filled, never a real handle value.
const FILLING: usize = 1;

#[derive(Clone, Copy)]
struct Thread {
    id: u32,
    priority: i32,
}
struct Cursor {
    /// 0 free, FILLING, else the handle Hook was given.
    handle: AtomicUsize,
    len: AtomicUsize,
    next: AtomicUsize,
    threads: UnsafeCell<[Thread; MAX_THREADS]>,
}
// SAFETY: `threads` is written only by the thread that moved `handle` from 0
// to FILLING, and read only after `handle` holds the published value.
unsafe impl Sync for Cursor {}

static TABLE: [Cursor; CURSORS] = [const {
    Cursor {
        handle: AtomicUsize::new(0),
        len: AtomicUsize::new(0),
        next: AtomicUsize::new(0),
        threads: UnsafeCell::new([Thread { id: 0, priority: 0 }; MAX_THREADS]),
    }
}; CURSORS];
/// Hook's previous import targets: snapshot, first, next, close.
static ORIGINAL: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
/// One install attempt per process; `INSTALLED` once all four are in place.
static ATTEMPTED: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Per-phase counters, logged and reset by `report` (all lock-free).
static SERVED: AtomicU64 = AtomicU64::new(0);
static SERVED_MICROS: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Logs the thread walks Hook made since the previous report, under `phase`.
/// Nothing is logged for a host without private Windower startup.
pub(crate) fn report(phase: &str) {
    if !INSTALLED.load(Ordering::Acquire) {
        return;
    }
    let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
    let (served, served_us, fallbacks) = (take(&SERVED), take(&SERVED_MICROS), take(&FALLBACKS));
    crate::log!("Windower Hook thread walks ({phase}): process-only={served} ({:.1} ms), fallbacks={fallbacks}, serving={}",
        served_us as f64 / 1000.0, ENABLED.load(Ordering::Acquire));
}

/// Which checkpoints this host has seen: a game started before, and this
/// game's warning, title and return.
const STARTED: u8 = 1;
const WARNING: u8 = 2;
const TITLE: u8 = 4;
const RETURNED: u8 = 8;

/// The phase `stage` ends, once per game, or `None`. A host that keeps its
/// session plays several games: each GameStart opens a new set of phases.
fn phase(seen: &AtomicU8, stage: &str) -> Option<&'static str> {
    let first = |bit: u8| seen.fetch_or(bit, Ordering::AcqRel) & bit == 0;
    match stage {
        "game-start" => Some(if seen.swap(STARTED, Ordering::AcqRel) & STARTED == 0 {
            "after Hook startup, until GameStart"
        } else {
            "previous game return until GameStart"
        }),
        "warning" => first(WARNING).then_some("GameStart until the license warning"),
        "title" => {
            let before = seen.fetch_or(TITLE, Ordering::AcqRel);
            (before & TITLE == 0).then_some(if before & WARNING != 0 {
                "license warning until the title menu"
            } else {
                "GameStart until the title menu"
            })
        }
        "game-return" => {
            let before = seen.fetch_or(RETURNED, Ordering::AcqRel);
            (before & RETURNED == 0).then_some(if before & TITLE != 0 { "title menu until game return" } else { "GameStart until game return" })
        }
        _ => None,
    }
}

/// Boot checkpoints that end a measured phase (from `boot_timing`).
pub(crate) fn checkpoint(stage: &str) {
    static SEEN: AtomicU8 = AtomicU8::new(0);
    if let Some(phase) = phase(&SEEN, stage) {
        report(phase);
    }
}
fn original(index: usize, fallback: usize) -> usize {
    match ORIGINAL[index].load(Ordering::Acquire) {
        0 => fallback,
        f => f,
    }
}

fn find(handle: HANDLE) -> Option<&'static Cursor> {
    let value = handle as usize;
    if value <= FILLING {
        return None;
    }
    TABLE.iter().find(|c| c.handle.load(Ordering::Acquire) == value)
}

/// This process's threads into `out`; `None` on any PSS failure or overflow.
fn capture_into(out: &mut [Thread; MAX_THREADS]) -> Option<usize> {
    struct Snapshot(HPSS);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe { PssFreeSnapshot(GetCurrentProcess(), self.0) };
        }
    }
    struct Marker(HPSSWALK);
    impl Drop for Marker {
        fn drop(&mut self) {
            unsafe { PssWalkMarkerFree(self.0) };
        }
    }
    unsafe {
        let mut snapshot = std::ptr::null_mut();
        if PssCaptureSnapshot(GetCurrentProcess(), PSS_CAPTURE_THREADS, 0, &mut snapshot) != 0 {
            return None;
        }
        let snapshot = Snapshot(snapshot);
        let mut marker = std::ptr::null_mut();
        if PssWalkMarkerCreate(std::ptr::null(), &mut marker) != 0 {
            return None;
        }
        let marker = Marker(marker);
        let owner = GetCurrentProcessId();
        let mut count = 0;
        loop {
            let mut entry: PSS_THREAD_ENTRY = std::mem::zeroed();
            let code = PssWalkSnapshot(snapshot.0, PSS_WALK_THREADS, marker.0,
                (&mut entry as *mut PSS_THREAD_ENTRY).cast(), size_of::<PSS_THREAD_ENTRY>() as u32);
            if code == ERROR_NO_MORE_ITEMS {
                return (count > 0).then_some(count);
            }
            if code != 0 || entry.ProcessId != owner || count == MAX_THREADS {
                return None;
            }
            out[count] = Thread { id: entry.ThreadId, priority: entry.BasePriority };
            count += 1;
        }
    }
}

/// A cursor over a fresh capture, or `None` (the caller uses Toolhelp).
fn open_cursor() -> Option<HANDLE> {
    let started = std::time::Instant::now();
    let cursor = TABLE.iter().find(|c| c.handle.compare_exchange(0, FILLING, Ordering::AcqRel, Ordering::Acquire).is_ok())?;
    // SAFETY: this thread owns the slot until it publishes or frees it.
    let filled = capture_into(unsafe { &mut *cursor.threads.get() });
    let handle = filled.and_then(|_| {
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        (!handle.is_null()).then_some(handle)
    });
    let (Some(count), Some(handle)) = (filled, handle) else {
        cursor.handle.store(0, Ordering::Release);
        return None;
    };
    cursor.len.store(count, Ordering::Relaxed);
    cursor.next.store(0, Ordering::Relaxed);
    cursor.handle.store(handle as usize, Ordering::Release);
    SERVED.fetch_add(1, Ordering::Relaxed);
    SERVED_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    Some(handle)
}

unsafe extern "system" fn snapshot(flags: u32, pid: u32) -> HANDLE {
    let saved = unsafe { GetLastError() };
    if ENABLED.load(Ordering::Acquire) && flags == TH32CS_SNAPTHREAD && pid == 0 {
        if let Some(handle) = open_cursor() {
            unsafe { SetLastError(saved) };
            return handle;
        }
        FALLBACKS.fetch_add(1, Ordering::Relaxed);
    }
    let f: unsafe extern "system" fn(u32, u32) -> HANDLE =
        unsafe { std::mem::transmute(original(0, CreateToolhelp32Snapshot as *const () as usize)) };
    unsafe { SetLastError(saved) };
    unsafe { f(flags, pid) }
}

/// Toolhelp's contract: FALSE with ERROR_BAD_LENGTH for a short entry, FALSE
/// with ERROR_NO_MORE_FILES at the end. Served cursors outlive the startup
/// window, so a walk already begun always finishes on the same list.
unsafe fn step(cursor: &Cursor, entry: *mut THREADENTRY32, first: bool) -> i32 {
    if entry.is_null() || unsafe { (*entry).dwSize } < size_of::<THREADENTRY32>() as u32 {
        unsafe { SetLastError(ERROR_BAD_LENGTH) };
        return 0;
    }
    if first {
        cursor.next.store(0, Ordering::Relaxed);
    }
    let index = cursor.next.load(Ordering::Relaxed);
    if index >= cursor.len.load(Ordering::Relaxed) {
        unsafe { SetLastError(ERROR_NO_MORE_FILES) };
        return 0;
    }
    // SAFETY: published before the handle was handed out; never rewritten while live.
    let thread = unsafe { (*cursor.threads.get())[index] };
    let size = unsafe { (*entry).dwSize };
    unsafe {
        *entry = THREADENTRY32 {
            dwSize: size,
            cntUsage: 0,
            th32ThreadID: thread.id,
            th32OwnerProcessID: GetCurrentProcessId(),
            tpBasePri: thread.priority,
            tpDeltaPri: 0,
            dwFlags: 0,
        };
    }
    cursor.next.store(index + 1, Ordering::Relaxed);
    1
}

unsafe extern "system" fn first(handle: HANDLE, entry: *mut THREADENTRY32) -> i32 {
    if let Some(cursor) = find(handle) {
        return unsafe { step(cursor, entry, true) };
    }
    let f: unsafe extern "system" fn(HANDLE, *mut THREADENTRY32) -> i32 =
        unsafe { std::mem::transmute(original(1, Thread32First as *const () as usize)) };
    unsafe { f(handle, entry) }
}

unsafe extern "system" fn next(handle: HANDLE, entry: *mut THREADENTRY32) -> i32 {
    if let Some(cursor) = find(handle) {
        return unsafe { step(cursor, entry, false) };
    }
    let f: unsafe extern "system" fn(HANDLE, *mut THREADENTRY32) -> i32 =
        unsafe { std::mem::transmute(original(2, Thread32Next as *const () as usize)) };
    unsafe { f(handle, entry) }
}

unsafe extern "system" fn close(handle: HANDLE) -> i32 {
    if let Some(cursor) = find(handle) {
        cursor.handle.store(0, Ordering::Release);
    }
    let f: unsafe extern "system" fn(HANDLE) -> i32 =
        unsafe { std::mem::transmute(original(3, CloseHandle as *const () as usize)) };
    unsafe { f(handle) }
}

/// Redirect four of Hook's own kernel32 import slots, found by name in its
/// import directory (any Hook build), and serve its thread walks for the
/// whole session (owner 2026-10-22: saves ~0.2 s of all-PC walks during game
/// start). Called once, after Hook loaded and outside any loader callback.
/// The walk and close wrappers go in before the snapshot one, and serving
/// starts only when all four are in place, so a partial install only ever
/// forwards. On failure Hook keeps Toolhelp for this whole launch, and the
/// log says why.
pub(crate) unsafe fn install(base: usize) {
    if ATTEMPTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let saved = unsafe { GetLastError() };
    let result = (|| -> Result<(), &'static str> {
        let wanted = [
            ("CreateToolhelp32Snapshot", snapshot as *const () as usize),
            ("Thread32First", first as *const () as usize),
            ("Thread32Next", next as *const () as usize),
            ("CloseHandle", close as *const () as usize),
        ];
        let slots = unsafe { crate::iat::import_slots(base, "kernel32.dll") };
        let found: Vec<_> = wanted
            .iter()
            .map(|(name, wrapper)| slots.iter().find(|s| s.name.as_deref() == Some(*name)).map(|s| (s.slot, *wrapper)))
            .collect::<Option<_>>()
            .ok_or("Hook.dll does not import all of CreateToolhelp32Snapshot, Thread32First/Next and CloseHandle from kernel32")?;
        for (i, (slot, _)) in found.iter().enumerate() {
            if !(*slot as usize).is_multiple_of(size_of::<usize>()) {
                return Err("a Hook.dll import slot is misaligned");
            }
            let current = unsafe { (*(*slot as *const AtomicUsize)).load(Ordering::Acquire) };
            if current == 0 {
                return Err("a Hook.dll import slot is not bound");
            }
            ORIGINAL[i].store(current, Ordering::Release);
        }
        for i in [3, 2, 1, 0] {
            let (slot, wrapper) = found[i];
            let done = crate::iat::swap(slot as usize, ORIGINAL[i].load(Ordering::Acquire), wrapper)
                .map_err(|_| "a Hook.dll import slot could not be made writable")?;
            if !done.changed || done.restored.is_err() {
                return Err("a Hook.dll import slot changed during installation");
            }
        }
        INSTALLED.store(true, Ordering::Release);
        ENABLED.store(true, Ordering::Release);
        crate::log!("Windower Hook thread walks: process-only for the whole session");
        Ok(())
    })();
    unsafe { SetLastError(saved) };
    if let Err(reason) = result {
        crate::log!("note: Windower Hook thread walks keep all-PC Toolhelp: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Barrier};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    fn walk(handle: HANDLE) -> Vec<THREADENTRY32> {
        let mut out = Vec::new();
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..unsafe { std::mem::zeroed() } };
        let mut more = unsafe { first(handle, &mut entry) };
        while more != 0 {
            out.push(entry);
            more = unsafe { next(handle, &mut entry) };
        }
        assert_eq!(unsafe { GetLastError() }, ERROR_NO_MORE_FILES);
        out
    }

    /// The served list is exactly Toolhelp's list filtered to this process,
    /// and each request sees threads created after an earlier one.
    #[test]
    fn served_lists_match_toolhelp_for_this_process_and_are_fresh() {
        let _serving = ServingForTest::new();
        let pid = unsafe { GetCurrentProcessId() };
        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();
        let wait = barrier.clone();
        let before = unsafe { snapshot(TH32CS_SNAPTHREAD, 0) };
        let worker = std::thread::spawn(move || {
            tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            wait.wait();
        });
        let id = rx.recv().unwrap();
        // Other tests start and end threads meanwhile: compare against the
        // threads Toolhelp lists both just before and just after the capture.
        let toolhelp_before = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        let after = unsafe { snapshot(TH32CS_SNAPTHREAD, 0) };
        let toolhelp_after = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        let (before_list, after_list) = (walk(before), walk(after));
        let ours = |handle| walk(handle).iter().filter(|t| t.th32OwnerProcessID == pid).map(|t| t.th32ThreadID).collect::<Vec<_>>();
        let (first_list, second_list) = (ours(toolhelp_before), ours(toolhelp_after));
        barrier.wait();
        worker.join().unwrap();
        assert!(!before_list.iter().any(|t| t.th32ThreadID == id), "a new thread is not in an older list");
        assert!(after_list.iter().any(|t| t.th32ThreadID == id), "each request is a fresh capture");
        assert!(after_list.iter().any(|t| t.th32ThreadID == unsafe { GetCurrentThreadId() }));
        assert!(after_list.iter().all(|t| t.th32OwnerProcessID == pid), "only this process's threads");
        for stable in first_list.iter().filter(|id| second_list.contains(id)) {
            assert!(after_list.iter().any(|t| t.th32ThreadID == *stable), "every thread Toolhelp lists for this process is served");
        }
        for handle in [before, after, toolhelp_before, toolhelp_after] {
            assert_eq!(unsafe { close(handle) }, 1);
        }
        assert!(TABLE.iter().all(|c| c.handle.load(Ordering::Acquire) == 0));
    }

    #[test]
    fn walks_restart_reject_short_entries_and_close_without_touching_other_handles() {
        let _serving = ServingForTest::new();
        std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..CURSORS).map(|_| scope.spawn(|| unsafe {
                let handle = snapshot(TH32CS_SNAPTHREAD, 0);
                assert!(find(handle).is_some() || !handle.is_null());
                let mut short = THREADENTRY32 { dwSize: 4, ..std::mem::zeroed() };
                assert_eq!(first(handle, &mut short), 0);
                assert_eq!(GetLastError(), ERROR_BAD_LENGTH);
                let once = walk(handle);
                assert_eq!(walk(handle).len(), once.len(), "Thread32First restarts the walk");
                assert_eq!(close(handle), 1);
                let normal = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
                assert!(!normal.is_null());
                assert_eq!(close(normal), 1);
            })).collect();
            for task in tasks {
                task.join().unwrap();
            }
        });
        // Other snapshot kinds always go to Toolhelp.
        let modules = unsafe { snapshot(0x8, 0) };
        assert!(find(modules).is_none());
        unsafe { close(modules) };
    }

    #[test]
    fn a_full_table_falls_back_to_toolhelp() {
        let _serving = ServingForTest::new();
        let held: Vec<_> = (0..CURSORS).map(|_| unsafe { snapshot(TH32CS_SNAPTHREAD, 0) }).collect();
        let extra = unsafe { snapshot(TH32CS_SNAPTHREAD, 0) };
        assert!(find(extra).is_none(), "the ninth walk uses Toolhelp");
        let pid = unsafe { GetCurrentProcessId() };
        assert!(walk(extra).iter().any(|t| t.th32OwnerProcessID != pid), "a real Toolhelp list");
        for handle in held.into_iter().chain([extra]) {
            unsafe { close(handle) };
        }
    }

    #[test]
    fn every_game_in_a_kept_session_reports_its_own_phases() {
        let seen = AtomicU8::new(0);
        assert_eq!(phase(&seen, "game-start"), Some("after Hook startup, until GameStart"));
        assert_eq!(phase(&seen, "warning"), Some("GameStart until the license warning"));
        assert_eq!(phase(&seen, "title"), Some("license warning until the title menu"));
        assert_eq!(phase(&seen, "title"), None, "a revisited title menu reports once per game");
        assert_eq!(phase(&seen, "characters"), None);
        assert_eq!(phase(&seen, "game-return"), Some("title menu until game return"));
        // The second game of the same host: every phase reports again.
        assert_eq!(phase(&seen, "game-start"), Some("previous game return until GameStart"));
        assert_eq!(phase(&seen, "title"), Some("GameStart until the title menu"), "no warning seen in this game");
        assert_eq!(phase(&seen, "warning"), Some("GameStart until the license warning"));
        assert_eq!(phase(&seen, "game-start"), Some("previous game return until GameStart"));
        assert_eq!(phase(&seen, "game-return"), Some("GameStart until game return"));
    }

    /// The tests share the static table and switch; one at a time.
    struct ServingForTest(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl ServingForTest {
        fn new() -> Self {
            static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let guard = ONE.lock().unwrap_or_else(|e| e.into_inner());
            ENABLED.store(true, Ordering::Release);
            Self(guard)
        }
    }
    impl Drop for ServingForTest {
        fn drop(&mut self) {
            ENABLED.store(false, Ordering::Release);
        }
    }
}
