//! A fixed /shutdown request runs on the client's own input thread. The normal
//! game command handles its countdown and restrictions. The owner's retention
//! setting decides whether POL stays signed in after the game returns. The
//! command entry and the globals it reads come from game_layout's signature
//! scan (any client build), re-checked in live memory before each call.
use crate::{game_layout::Resolution, log};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE, HWND},
    System::{
        LibraryLoader::{
            GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        },
        Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_IMAGE, PAGE_GUARD},
        SystemInformation::GetTickCount64,
        Threading::GetCurrentThreadId,
    },
};

use crate::game_layout::{
    controls::{SHUTDOWN_BODY, SHUTDOWN_CALLER, SHUTDOWN_CALLER_CALL as CALLER_CALL, SHUTDOWN_CALLER_LEN, SHUTDOWN_COMMAND, SHUTDOWN_PROLOGUE},
    Shutdown,
};
use crate::sigscan::Pattern;
const UNAVAILABLE: &str = "Background shutdown is unavailable for this game session. Launch it again with the current Crystal build.";
const EXPIRED: &str = "The shutdown request expired before the game could run it.";
const EXITED: &str = "The game closed before the shutdown request could run.";
static NEXT_GAME: AtomicU64 = AtomicU64::new(1);
pub(crate) fn last_game() -> u64 { NEXT_GAME.load(Ordering::Acquire).saturating_sub(1) }

use crate::mem::read;
fn pointer(address: usize) -> Option<usize> { crate::mem::pointer(address, 0x1000) }
/// Live re-check of the scanned entry: the signature (opcodes and stack
/// layout exact), and the operands this module reads relocated to `base`.
/// The caller is ABI evidence only; Crystal calls the entry directly, so
/// another tool may redirect that one call (input hooks do)
/// without changing what the shutdown runs.
fn body_valid(base: usize, layout: &Shutdown, code: &[u8; SHUTDOWN_BODY], caller: &[u8; SHUTDOWN_CALLER_LEN]) -> bool {
    let (Some(body), Some(call)) = (Pattern::parse(SHUTDOWN_COMMAND), Pattern::parse(SHUTDOWN_CALLER)) else {
        return false;
    };
    let operand = |at: usize, rva: usize| crate::sigscan::operand_at(base, code, at, rva);
    body.matches(code, SHUTDOWN_PROLOGUE.len())
        && operand(0x22, layout.context)
        && operand(0x30, layout.enabled)
        && operand(0x40, layout.enabled + 8)
        && operand(0x89, layout.manager)
        && call.matches(caller, 0)
        && caller[CALLER_CALL] == 0xE8
}

struct HostLease(HMODULE);
impl Drop for HostLease {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}
fn host_target(address: usize) -> Option<HostLease> {
    let mut module = std::ptr::null_mut();
    if unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            address as *const u16,
            &mut module,
        )
    } == 0
    {
        return None;
    }
    let lease = HostLease(module);
    let filename = crate::win::module_path(module)?.to_string_lossy().into_owned();
    let name = filename.rsplit(['\\', '/']).next()?;
    if !name.eq_ignore_ascii_case("Hook.dll") && !name.eq_ignore_ascii_case("Ashita.dll") {
        return None;
    }
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let mapped = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            std::mem::size_of_val(&info),
        )
    } != 0;
    (mapped
        && info.Type == MEM_IMAGE
        && info.AllocationBase == module.cast()
        && info.Protect & PAGE_GUARD == 0
        && info.Protect & 0xF0 != 0)
        .then_some(lease)
}

#[derive(Clone, Copy)]
struct Layout {
    base: usize,
    window: usize,
    keyboard: usize,
    shutdown: Shutdown,
}
impl Layout {
    /// The scanned groups this needs, then a live re-check. Every refusal
    /// names its reason (logged by the caller); the user sees UNAVAILABLE.
    fn resolve(resolution: &Resolution) -> Result<Self, String> {
        let group = |what: &str, reason: &String| format!("{what} unavailable: {reason}");
        let layout = Self {
            base: resolution.base,
            window: resolution.menus.as_ref().map_err(|e| group("login menus (game window)", e))?.window,
            keyboard: resolution.input.as_ref().map_err(|e| group("key input", e))?.keyboard,
            shutdown: *resolution.shutdown.as_ref().map_err(|e| group("shutdown command", e))?,
        };
        layout.verify().map_err(|_| "the scanned shutdown command differs in live memory".to_string())?;
        Ok(layout)
    }
    // The complete argument/source setup and an independent caller establish
    // the cdecl ABI. Keep an existing supported host hook in the call path.
    fn verify(self) -> Result<Option<HostLease>, &'static str> {
        let entry = self.base + self.shutdown.entry;
        let code = read::<[u8; SHUTDOWN_BODY]>(entry).ok_or(UNAVAILABLE)?;
        let caller = read::<[u8; SHUTDOWN_CALLER_LEN]>(self.base + self.shutdown.caller).ok_or(UNAVAILABLE)?;
        if !body_valid(self.base, &self.shutdown, &code, &caller) {
            return Err(UNAVAILABLE);
        }
        if code[..6] == SHUTDOWN_PROLOGUE {
            return Ok(None);
        }
        if code[0] != 0xE9 || !matches!(code[5], 0 | 0x90 | 0xCC) {
            return Err(UNAVAILABLE);
        }
        let relative = i32::from_le_bytes(code[1..5].try_into().unwrap()) as isize;
        let target = (entry + 5)
            .checked_add_signed(relative)
            .ok_or(UNAVAILABLE)?;
        host_target(target).map(Some).ok_or(UNAVAILABLE)
    }
    fn context(self, data: *mut u8) -> bool {
        crate::auto_login::frame_keyboard(self.base, self.keyboard, data).is_some()
            && read::<u32>(self.base + self.window).is_some_and(|window| {
                crate::auto_login::owned_window(window as usize as HWND, unsafe { GetCurrentThreadId() })
            })
    }
    fn in_game(self) -> bool {
        pointer(self.base + self.shutdown.context)
            .and_then(read::<u32>)
            .is_some()
            // The client's command entry tests this DWORD for nonzero. World
            // entry sets it to 1; interpreting it as an address rejects every
            // logged-in character.
            && read::<u32>(self.base + self.shutdown.enabled).is_some_and(|enabled| enabled != 0)
            && pointer(self.base + self.shutdown.manager)
                .and_then(read::<[u8; 0xC8]>)
                .is_some()
    }
}

#[derive(Clone, Copy)]
struct Request {
    id: u64,
    game: u64,
    deadline: u64,
}
impl Request {
    fn valid(self, game: u64, now: u64) -> bool {
        self.id != 0 && self.game == game && self.deadline > now && self.deadline - now <= 5000
    }
}
struct State {
    game: u64,
    layout: Option<Layout>,
    pending: Option<Request>,
    last_attempt: Option<Instant>,
}
static STATE: Mutex<Option<State>> = Mutex::new(None);
static EXECUTING: Mutex<()> = Mutex::new(());
fn result(id: u64, game: u64, ok: bool, message: &str) {
    crate::events::send(crate::proto::game_shutdown_result(id, game, ok, message));
}
pub(crate) fn submit(id: u64, game: u64, deadline: u64) {
    let request = Request { id, game, deadline };
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        result(id, game, false, EXITED);
        return;
    };
    if !request.valid(state.game, unsafe { GetTickCount64() }) {
        result(id, game, false, EXPIRED);
        return;
    }
    if state.layout.is_none() {
        result(id, game, false, UNAVAILABLE);
        return;
    }
    if state.pending.is_some()
        || state
            .last_attempt
            .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
    {
        result(
            id,
            game,
            false,
            "Shutdown was already requested. Wait for the game to close.",
        );
        return;
    }
    state.pending = Some(request);
}

#[cfg(target_arch = "x86")]
unsafe fn invoke(entry: usize) {
    let command: unsafe extern "C" fn(*const u8, u32) -> i32 = std::mem::transmute(entry);
    // ASCII, NUL terminated, source 1: the same two arguments as the ordinary
    // UI caller. A return acknowledges dispatch, not completed logout.
    command(c"/shutdown".as_ptr().cast(), 1);
}
pub(crate) fn on_input(data: *mut u8) {
    let Ok(mut guard) = STATE.try_lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else { return };
    let Some(request) = state.pending else { return };
    let Some(layout) = state.layout else { return };
    if !layout.context(data) {
        return;
    }
    let Ok(_executing) = EXECUTING.try_lock() else {
        return;
    };
    state.pending = None;
    if !request.valid(state.game, unsafe { GetTickCount64() }) || crate::events::pipe_closed() {
        result(request.id, request.game, false, EXPIRED);
        return;
    }
    let lease = match layout.verify() {
        Ok(lease) => lease,
        Err(error) => {
            result(request.id, request.game, false, error);
            return;
        }
    };
    if !layout.in_game() {
        result(
            request.id,
            request.game,
            false,
            "Log into a character before requesting shutdown from Crystal.",
        );
        return;
    }
    if !request.valid(state.game, unsafe { GetTickCount64() }) {
        result(request.id, request.game, false, EXPIRED);
        return;
    }
    state.last_attempt = Some(Instant::now());
    drop(guard); // Host command hooks may reenter the input API.
    #[cfg(target_arch = "x86")]
    unsafe {
        invoke(layout.base + layout.shutdown.entry);
    }
    drop(lease);
    result(
        request.id,
        request.game,
        true,
        "Shutdown requested. Waiting for FFXI and PlayOnline to close.",
    );
}

pub(crate) struct Worker {
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Worker {
    pub(crate) fn start() -> Option<Self> {
        if !cfg!(target_arch = "x86") {
            return None;
        }
        let game = NEXT_GAME.fetch_add(1, Ordering::Relaxed);
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return None;
        }
        *guard = Some(State {
            game,
            layout: None,
            pending: None,
            last_attempt: None,
        });
        let (stop, receive) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("crystal-game-shutdown".into())
            .spawn(move || {
                let mut hook = None;
                // Polls only until FFXiMain resolves; then sleeps until stopped.
                while receive.recv_timeout(Duration::from_millis(100))
                    == Err(mpsc::RecvTimeoutError::Timeout)
                {
                    let Some(resolution) = crate::game_layout::ffximain() else {
                        continue;
                    };
                    let resolved = Layout::resolve(resolution).and_then(|layout| {
                        crate::auto_login::input_hook(resolution)
                            .map(|lease| (layout, lease))
                            .map_err(|reason| format!("game input could not be prepared: {reason}"))
                    });
                    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(state) = guard.as_mut().filter(|state| state.game == game) else {
                        break;
                    };
                    match resolved {
                        Ok((layout, lease)) => {
                            state.layout = Some(layout);
                            hook = Some(lease);
                            result(0, game, true, "");
                        }
                        Err(reason) => {
                            result(0, game, false, UNAVAILABLE);
                            log!("note: background shutdown unavailable: {reason}");
                        }
                    }
                    drop(guard);
                    let _ = receive.recv();
                    break;
                }
                drop(hook);
            });
        match spawned {
            Ok(thread) => Some(Self {
                stop,
                thread: Some(thread),
            }),
            Err(_) => {
                *guard = None;
                result(0, game, false, UNAVAILABLE);
                None
            }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _executing = EXECUTING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = STATE.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Some(request) = state.pending {
                result(request.id, state.game, false, EXITED);
            }
            result(0, state.game, false, EXITED);
        }
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use crate::game_layout::BUILD_6AC37846;

    fn known() -> Shutdown { BUILD_6AC37846.shutdown }

    #[test]
    #[cfg(target_arch = "x86")]
    fn game_shutdown_accepts_the_clients_scalar_command_enabled_flag() {
        let s = known();
        let mut globals = vec![0u32; (s.context + 4) / 4];
        let context = Box::new(0u32);
        let manager = Box::new([0u32; 0xC8 / 4]);
        globals[s.context / 4] = context.as_ref() as *const u32 as u32;
        globals[s.manager / 4] = manager.as_ref() as *const _ as u32;
        let layout = Layout { base: globals.as_ptr() as usize, window: 0, keyboard: 0, shutdown: s };
        // The client sets this DWORD to 1 after world entry. It is a flag,
        // although the two adjacent command dependencies are real pointers.
        globals[s.enabled / 4] = 1;
        assert!(layout.in_game());
        globals[s.enabled / 4] = 0;
        assert!(!layout.in_game());
        globals[s.enabled / 4] = 1;
        globals[s.manager / 4] = 0;
        assert!(!layout.in_game());
        globals[s.manager / 4] = manager.as_ref() as *const _ as u32;
        globals[s.context / 4] = 0;
        assert!(!layout.in_game());
    }
    #[test]
    fn game_shutdown_rejects_expired_duplicate_and_foreign_game_requests() {
        assert!(Request { id: 1, game: 7, deadline: 1500 }.valid(7, 1000));
        for request in [
            Request { id: 0, game: 7, deadline: 1500 },
            Request { id: 1, game: 8, deadline: 1500 },
            Request { id: 1, game: 7, deadline: 1000 },
            Request { id: 1, game: 7, deadline: 6001 },
        ] {
            assert!(!request.valid(7, 1000));
        }
    }
    #[test]
    fn game_shutdown_duplicate_requests_never_repeat_the_countdown_command() {
        *STATE.lock().unwrap() = Some(State {
            game: 7,
            layout: Some(Layout { base: 0, window: 0, keyboard: 0, shutdown: known() }),
            pending: None,
            last_attempt: None,
        });
        let deadline = unsafe { GetTickCount64() } + 1000;
        let replies = crate::events::capture(|| {
            submit(1, 7, deadline);
            submit(2, 7, deadline);
        });
        assert_eq!(STATE.lock().unwrap().as_ref().unwrap().pending.unwrap().id, 1);
        assert!(replies.iter().any(|reply| reply.contains("already requested")));
        {
            let mut state = STATE.lock().unwrap();
            let state = state.as_mut().unwrap();
            state.pending = None;
            state.last_attempt = Some(Instant::now());
        }
        let replies = crate::events::capture(|| submit(3, 7, deadline));
        assert!(replies.iter().any(|reply| reply.contains("already requested")));
        assert!(STATE.lock().unwrap().take().unwrap().pending.is_none());
    }
    #[test]
    #[cfg(target_arch = "x86")]
    fn game_shutdown_cdecl_call_contains_only_shutdown_and_user_source() {
        static CALLED: AtomicBool = AtomicBool::new(false);
        unsafe extern "C" fn fixture(text: *const u8, source: u32) -> i32 {
            assert_eq!(std::ffi::CStr::from_ptr(text.cast()).to_bytes(), b"/shutdown");
            assert_eq!(source, 1);
            CALLED.store(true, Ordering::Release);
            0
        }
        unsafe {
            invoke(fixture as *const () as usize);
        }
        assert!(CALLED.load(Ordering::Acquire));
    }

    /// The entry and caller bytes of the known build at preferred base
    /// 0x10000000 (operands relocated for `base`), from the unpacked dump.
    fn sites(image: &[u8], base: usize) -> ([u8; SHUTDOWN_BODY], [u8; SHUTDOWN_CALLER_LEN]) {
        let s = known();
        let mut code: [u8; SHUTDOWN_BODY] = image[s.entry..s.entry + SHUTDOWN_BODY].try_into().unwrap();
        for at in [0x22, 0x30, 0x40, 0x78, 0x81, 0x89, 0x92] {
            let value = u32::from_le_bytes(code[at..at + 4].try_into().unwrap()).wrapping_sub(0x1000_0000).wrapping_add(base as u32);
            code[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        (code, image[s.caller..s.caller + SHUTDOWN_CALLER_LEN].try_into().unwrap())
    }

    #[test]
    #[ignore = "Read-only verification of unpacked client images selected by CRYSTAL_FFXIMAIN_DUMPS"]
    fn game_shutdown_live_check_matches_both_client_builds_and_aslr() {
        let root = std::path::PathBuf::from(
            std::env::var_os("CRYSTAL_FFXIMAIN_DUMPS").expect("Select the unpacked client images"),
        );
        let s = known();
        for build in ["6A995428", "6AC37846"] {
            let image = std::fs::read(root.join(format!("FFXiMain-{build}.img"))).unwrap();
            for base in [0x1000_0000usize, 0x1800_0000] {
                let (code, caller) = sites(&image, base);
                assert!(body_valid(base, &s, &code, &caller), "{build} at {base:#x}");
                // Read at another base: the operands do not match.
                assert!(!body_valid(base ^ 0x0100_0000, &s, &code, &caller), "{build} at {base:#x}");
                // A changed opcode or stack displacement is refused.
                for at in [0x20, 0x34 + 3, 0x4B] {
                    let mut changed = code;
                    changed[at] ^= 1;
                    assert!(!body_valid(base, &s, &changed, &caller), "{build} byte {at:#x}");
                }
                // A moved branch target or callee is not a change of behaviour.
                let mut moved = code;
                moved[0x45] ^= 2;
                moved[0x2B] ^= 0x10;
                assert!(body_valid(base, &s, &moved, &caller), "{build} relative operands");
                // A host jmp in place of the prologue is still the entry.
                let mut hooked = code;
                hooked[..6].copy_from_slice(&[0xE9, 1, 2, 3, 4, 0x90]);
                assert!(body_valid(base, &s, &hooked, &caller), "{build} hooked");
                let mut changed = caller;
                changed[0] ^= 1;
                assert!(!body_valid(base, &s, &code, &changed));
                // Another tool's hook on the caller's call keeps the check valid;
                // anything but a direct call there does not.
                let mut redirected = caller;
                redirected[CALLER_CALL + 1..CALLER_CALL + 5].copy_from_slice(&0x1234_5678u32.to_le_bytes());
                assert!(body_valid(base, &s, &code, &redirected));
                let mut not_a_call = caller;
                not_a_call[CALLER_CALL] = 0xE9;
                assert!(!body_valid(base, &s, &code, &not_a_call));
            }
        }
    }
    /// The client's chat-submit callback,
    /// original and with its call redirected by an in-process hook: only the
    /// branch distance and the call target may differ.
    #[test]
    fn game_shutdown_caller_check_ignores_only_relative_operands() {
        let call = Pattern::parse(SHUTDOWN_CALLER).unwrap();
        let caller: [u8; 24] = [
            0x8B, 0x44, 0x24, 0x04, 0x85, 0xC0, 0x75, 0x0F, 0x8B, 0x44, 0x24, 0x08,
            0x6A, 0x01, 0x50, 0xE8, 0x1C, 0x8F, 0xEC, 0xFF, 0x83, 0xC4, 0x08, 0xC3,
        ];
        assert!(call.matches(&caller, 0));
        for at in (0..24).filter(|at| *at != 7 && !(CALLER_CALL + 1..CALLER_CALL + 5).contains(at)) {
            let mut changed = caller;
            changed[at] ^= 1;
            assert!(!call.matches(&changed, 0), "byte {at}");
        }
    }
}