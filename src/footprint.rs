//! Process footprint: read and trim working sets, and run an idle signed-in session's worker
//! and game host at below-normal CPU priority. Nothing here sends, delays or reshapes any
//! network traffic: the POL keepalive and pacing run exactly as before, only the pages a
//! sleeping process is not touching go back to Windows (they fault back in on use).
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// A process's memory as Task Manager shows it: working set and private (commit) bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Memory {
    pub working_set: usize,
    pub private: usize,
}

impl Memory {
    /// `ws 123.4 MB, private 56.7 MB`
    pub(crate) fn text(self) -> String {
        format!("ws {:.1} MB, private {:.1} MB", mb(self.working_set), mb(self.private))
    }
}

pub(crate) fn mb(bytes: usize) -> f64 { bytes as f64 / (1024.0 * 1024.0) }

/// `pid` None: this process.
fn open(pid: Option<u32>, access: u32) -> Option<Handle> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess};
    match pid {
        None => Some(Handle { raw: unsafe { GetCurrentProcess() }, owned: false }),
        Some(pid) => {
            let raw = unsafe { OpenProcess(access, 0, pid) };
            (!raw.is_null()).then_some(Handle { raw, owned: true })
        }
    }
}

struct Handle { raw: windows_sys::Win32::Foundation::HANDLE, owned: bool }
impl Drop for Handle {
    fn drop(&mut self) {
        if self.owned { unsafe { windows_sys::Win32::Foundation::CloseHandle(self.raw); } }
    }
}

/// The process's working set and private bytes.
pub(crate) fn memory(pid: Option<u32>) -> Option<Memory> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ};
    let handle = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
        .or_else(|| open(pid, PROCESS_QUERY_LIMITED_INFORMATION))?;
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let ok = unsafe {
        GetProcessMemoryInfo(handle.raw, (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(), counters.cb)
    };
    (ok != 0).then_some(Memory { working_set: counters.WorkingSetSize, private: counters.PrivateUsage })
}

/// Give the process's resident pages back to Windows (they stay on the standby list and
/// fault back in cheaply when touched). `pid` None also returns the heap's free blocks.
pub(crate) fn trim(pid: Option<u32>) -> bool {
    use windows_sys::Win32::System::Threading::{SetProcessWorkingSetSize, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA};
    if pid.is_none() {
        use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
        unsafe { HeapCompact(GetProcessHeap(), 0); }
    }
    let Some(handle) = open(pid, PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION) else { return false };
    unsafe { SetProcessWorkingSetSize(handle.raw, usize::MAX, usize::MAX) != 0 }
}

/// Below-normal (`true`) or normal CPU priority. Only ever between those two: a process
/// some other tool raised is left alone.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn set_below_normal(pid: Option<u32>, below: bool) -> bool {
    use windows_sys::Win32::System::Threading::{GetPriorityClass, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
        NORMAL_PRIORITY_CLASS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION};
    let Some(handle) = open(pid, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION) else { return false };
    let (from, to) = if below { (NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS) } else { (BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS) };
    let current = unsafe { GetPriorityClass(handle.raw) };
    if current == to { return true; }
    current == from && unsafe { SetPriorityClass(handle.raw, to) } != 0
}

/// This session worker's game host (one per worker process), from its verified hello.
static HOST: AtomicU32 = AtomicU32::new(0);

pub(crate) fn set_host(pid: u32) { HOST.store(pid, Ordering::Relaxed); }

#[cfg_attr(test, allow(dead_code))]
fn host() -> Option<u32> { Some(HOST.load(Ordering::Relaxed)).filter(|pid| *pid != 0) }

/// How long a signed-in session sits at an idle main menu before it is trimmed and lowered:
/// long enough that a Play soon after a sign-in (the usual pre-sign-in case) never meets it.
pub(crate) const IDLE_AFTER: Duration = Duration::from_secs(45);

/// What the session worker's watch loop decided this turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change { None, Lower, Restore }

/// An idle signed-in session's footprint (worker side): idle at the main menu for IDLE_AFTER
/// → trim the worker and its host and run both below normal; anything that may start the game
/// (a Play, a batch, the session handed to FFXI) → normal again first.
#[derive(Debug, Default)]
pub(crate) struct IdleSession {
    since: Option<Instant>,
    lowered: bool,
}

impl IdleSession {
    /// Each watch-loop turn, with whether the session is idle at the Viewer main menu.
    pub(crate) fn tend(&mut self, idle: bool, now: Instant) -> Change {
        if !idle {
            return self.wake();
        }
        let since = *self.since.get_or_insert(now);
        if !self.lowered && now.saturating_duration_since(since) >= IDLE_AFTER {
            self.lowered = true;
            return Change::Lower;
        }
        Change::None
    }

    /// When `tend` would lower an idle session; None while not idle or already lowered.
    pub(crate) fn next_change(&self) -> Option<Instant> {
        self.since.filter(|_| !self.lowered).map(|since| since + IDLE_AFTER)
    }

    /// Before anything that may start the game: normal priority now, and a fresh idle wait.
    pub(crate) fn wake(&mut self) -> Change {
        self.since = None;
        if std::mem::take(&mut self.lowered) { Change::Restore } else { Change::None }
    }

    /// Apply a change to this worker and its host; the session log line describing it.
    pub(crate) fn apply(change: Change) -> Option<String> {
        match change {
            Change::None => None,
            Change::Lower => Some(lower()),
            Change::Restore => Some(restore()),
        }
    }
}

#[cfg(not(test))]
fn lower() -> String {
    let host = host();
    let before = (memory(None), host.and_then(|pid| memory(Some(pid))));
    trim(None);
    if let Some(pid) = host { trim(Some(pid)); }
    let priority = set_below_normal(None, true) & host.is_none_or(|pid| set_below_normal(Some(pid), true));
    let after = (memory(None), host.and_then(|pid| memory(Some(pid))));
    let show = |m: Option<Memory>| m.map_or_else(|| "unknown".to_owned(), Memory::text);
    format!("Idle footprint: session idle at the main menu for {} s; worker {} -> {}; host {} -> {}; priority below normal: {}.",
        IDLE_AFTER.as_secs(), show(before.0), show(after.0), show(before.1), show(after.1), if priority { "yes" } else { "no" })
}

#[cfg(not(test))]
fn restore() -> String {
    let host = host();
    let ok = set_below_normal(None, false) & host.is_none_or(|pid| set_below_normal(Some(pid), false));
    format!("Idle footprint: normal priority again before the game{}.", if ok { "" } else { " (not confirmed)" })
}

// Unit tests drive fake hosts that are often this very process: never lower the test runner.
#[cfg(test)]
fn lower() -> String { "Idle footprint: lowered (test).".into() }
#[cfg(test)]
fn restore() -> String { "Idle footprint: restored (test).".into() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_session_lowers_once_after_the_wait_and_restores_when_it_leaves_idle() {
        let start = Instant::now();
        let mut idle = IdleSession::default();
        assert_eq!(idle.tend(true, start), Change::None);
        assert_eq!(idle.tend(true, start + IDLE_AFTER - Duration::from_millis(1)), Change::None);
        assert_eq!(idle.tend(true, start + IDLE_AFTER), Change::Lower);
        assert_eq!(idle.tend(true, start + IDLE_AFTER * 3), Change::None, "only once");
        assert_eq!(idle.tend(false, start + IDLE_AFTER * 3), Change::Restore, "the game took the session");
        assert_eq!(idle.tend(false, start + IDLE_AFTER * 4), Change::None);
        // Back at the main menu after the game: a fresh wait.
        let back = start + IDLE_AFTER * 5;
        assert_eq!(idle.tend(true, back), Change::None);
        assert_eq!(idle.tend(true, back + IDLE_AFTER), Change::Lower);
    }

    #[test]
    fn a_play_restores_first_and_restarts_the_wait() {
        let start = Instant::now();
        let mut idle = IdleSession::default();
        assert_eq!(idle.wake(), Change::None, "nothing lowered yet");
        idle.tend(true, start);
        assert_eq!(idle.tend(true, start + IDLE_AFTER), Change::Lower);
        assert_eq!(idle.wake(), Change::Restore);
        // The start was refused (an update): still idle, lowered again only after a new wait.
        let after = start + IDLE_AFTER + Duration::from_secs(1);
        assert_eq!(idle.tend(true, after), Change::None);
        assert_eq!(idle.tend(true, after + IDLE_AFTER - Duration::from_millis(1)), Change::None);
        assert_eq!(idle.tend(true, after + IDLE_AFTER), Change::Lower);
    }

    #[test]
    fn this_process_memory_reads_and_trims() {
        let before = memory(None).expect("own memory");
        assert!(before.working_set > 0 && before.private > 0);
        assert!(trim(None));
        assert!(memory(None).is_some());
    }
}
