//! Stock pol.exe's power and screen-saver policy around the game (pol.exe
//! 1.18.13: SetThreadExecutionState(ES_CONTINUOUS|ES_SYSTEM_REQUIRED) at
//! 0x43532e, SPI_GETSCREENSAVEACTIVE then SPI_SETSCREENSAVEACTIVE 0 after it,
//! the saved value restored and ES_CONTINUOUS at 0x435513). The screen-saver
//! flag is session-wide and several hosts run at once, so they share one
//! count: the first host in turns the saver off (only when it was on) and the
//! last host out turns it back on. No host undoes another's change.

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0},
    System::{
        Memory::{CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE},
        Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED},
        Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
    UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETSCREENSAVEACTIVE, SPI_SETSCREENSAVEACTIVE},
};

/// [hosts in a game, 1 when the first of them turned the saver off].
type State = [u32; 2];

/// Held on the game thread for one game; every exit path restores.
pub(crate) struct Awake(Option<Shared>);

impl Awake {
    pub(crate) fn begin() -> Self {
        // SAFETY: plain flag calls on this thread.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        let shared = Shared::open();
        let saver = shared.as_ref().and_then(|s| s.with(|state| enter(state, saver_active, || set_saver(false))));
        crate::log!("stage: sleep held for the game; screen saver {}", saver.unwrap_or("left as is (no shared state)"));
        Self(shared)
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        if let Some(shared) = &self.0 { shared.with(|state| leave(state, || { set_saver(true); })); }
        // SAFETY: as in begin.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

fn enter(state: &mut State, active: impl FnOnce() -> bool, off: impl FnOnce() -> bool) -> &'static str {
    state[0] += 1;
    if state[0] > 1 { return "already held by another game"; }
    if !active() { return "already off"; }
    if !off() { return "could not be turned off"; }
    state[1] = 1;
    "off until the last game ends"
}

fn leave(state: &mut State, on: impl FnOnce()) {
    state[0] = state[0].saturating_sub(1);
    if state[0] == 0 && std::mem::take(&mut state[1]) == 1 { on(); }
}

fn saver_active() -> bool {
    let mut active = 0i32;
    // SAFETY: the out value is a live BOOL.
    unsafe { SystemParametersInfoW(SPI_GETSCREENSAVEACTIVE, 0, (&mut active as *mut i32).cast(), 0) != 0 && active != 0 }
}

/// Like stock, in this session only (no fWinIni: nothing is written to the profile).
fn set_saver(on: bool) -> bool {
    // SAFETY: SPI_SETSCREENSAVEACTIVE takes its value in uiParam.
    unsafe { SystemParametersInfoW(SPI_SETSCREENSAVEACTIVE, u32::from(on), std::ptr::null_mut(), 0) != 0 }
}

/// The hosts' shared count, behind a session-wide mutex.
struct Shared { lock: HANDLE, map: HANDLE, view: MEMORY_MAPPED_VIEW_ADDRESS }

impl Shared {
    fn open() -> Option<Self> {
        let lock_name = crate::win::wide("Local\\crystal-loader-awake-lock");
        let map_name = crate::win::wide("Local\\crystal-loader-awake");
        // SAFETY: NUL-terminated names, default security; handles owned below.
        unsafe {
            let lock = CreateMutexW(std::ptr::null(), 0, lock_name.as_ptr());
            if lock.is_null() { return None; }
            let map = CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, std::mem::size_of::<State>() as u32, map_name.as_ptr());
            if map.is_null() { CloseHandle(lock); return None; }
            let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, std::mem::size_of::<State>());
            if view.Value.is_null() { CloseHandle(map); CloseHandle(lock); return None; }
            Some(Self { lock, map, view })
        }
    }

    fn with<T>(&self, work: impl FnOnce(&mut State) -> T) -> Option<T> {
        // SAFETY: a live mutex owned by this value.
        if !matches!(unsafe { WaitForSingleObject(self.lock, 2000) }, WAIT_OBJECT_0 | WAIT_ABANDONED) { return None; }
        // SAFETY: a zero-initialised page-aligned view of State, written only under the lock.
        let result = work(unsafe { &mut *self.view.Value.cast::<State>() });
        unsafe { ReleaseMutex(self.lock) };
        Some(result)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: handles and view owned by this value.
        unsafe { UnmapViewOfFile(self.view); CloseHandle(self.map); CloseHandle(self.lock); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_game_turns_the_saver_off_and_only_the_last_restores_it() {
        let mut state = [0; 2];
        let mut restored = 0;
        assert_eq!(enter(&mut state, || true, || true), "off until the last game ends");
        assert_eq!(enter(&mut state, || panic!("not asked"), || panic!("not changed")), "already held by another game");
        leave(&mut state, || restored += 1); // the first host leaves early
        assert_eq!(restored, 0, "never under another running game");
        leave(&mut state, || restored += 1);
        assert_eq!((restored, state), (1, [0, 0]));
        // A user who keeps the saver off is never turned on.
        assert_eq!(enter(&mut state, || false, || panic!("not changed")), "already off");
        leave(&mut state, || panic!("not restored"));
        assert_eq!(enter(&mut state, || true, || false), "could not be turned off");
        leave(&mut state, || panic!("not restored"));
        assert_eq!(state, [0, 0]);
    }
}
