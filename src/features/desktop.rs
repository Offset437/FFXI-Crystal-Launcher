//! The one desktop thread: a message-only window that owns Crystal's global hotkeys
//! (RegisterHotKey) and the WinEvent hooks that tell the features when a tracked game's window
//! appears, is moved or resized, minimised, renamed, or when the foreground window changes.
//!
//! It exists only while something needs it (a hotkey is set, or a game is tracked by a feature
//! that watches windows) and sleeps in GetMessage the rest of the time: no timers, no polling.
//! Hooks are out-of-context, so nothing is loaded into the game. The per-game hooks are scoped
//! to that game's processes; the one global hook is the foreground change, which only fires when
//! the user switches windows. That hook also arms the in-game hotkeys (the cycle keys): they are
//! registered while a tracked game's window is in front and released on any other change, and
//! for the same span the low-level keyboard hook (`keyhook`) takes exactly those chords and the
//! account hotkeys Windows registered for Crystal, since a game reading the keyboard through
//! DirectInput keeps registered hotkeys from arriving.
//!
//! Registrations are the window's own: they last through sleep, a lock and an Explorer
//! restart, and while the launcher's window is closed to the notification area (this thread
//! does not depend on it). Windows does not say which program owns a chord it refuses
//! (ERROR_HOTKEY_ALREADY_REGISTERED), only that one does; `probe` asks that for a chord before
//! it is needed.

use super::hotkeys::{Gate, Hotkey, Registrar};
use super::keyhook::{Fired, KeyHook, OWN_INPUT, WM_HOOK_HOTKEY, WM_HOOK_RELEASE};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};

/// How a hotkey press arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    /// WM_HOTKEY (RegisterHotKey): this thread received the input.
    Hotkey,
    /// The low-level keyboard hook took the chord over a game (`keyhook`).
    Hook,
}

impl Via {
    pub(crate) fn name(self) -> &'static str {
        match self { Self::Hotkey => "WM_HOTKEY", Self::Hook => "hook" }
    }
}

/// What the desktop thread reports, on the desktop thread itself (so a hotkey's handler may set
/// the foreground window: the hotkey's input belongs to this thread).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// `key`: the key event that fired it, when the hook took it.
    Hotkey { id: i32, via: Via, key: Option<Fired> },
    /// A line for the launch history (`hotkeys: ...`).
    Log(String),
    /// The hotkeys after a `Command::Hotkeys`: each id with Ok, or the Windows error.
    Registered(Vec<(i32, Result<(), u32>)>),
    Foreground { hwnd: isize, pid: u32 },
    MoveSizeEnd { hwnd: isize, pid: u32 },
    Minimized { hwnd: isize, pid: u32, start: bool },
    Shown { hwnd: isize, pid: u32 },
    TitleChanged { hwnd: isize, pid: u32 },
}

pub(crate) type Sink = Arc<dyn Fn(Event) + Send + Sync>;

pub(crate) enum Command {
    /// Replace the hotkeys registered for as long as they are set.
    Hotkeys(Vec<(i32, Hotkey)>),
    /// Replace the hotkeys registered only while a tracked process's window is in the foreground
    /// (`hotkeys::Gate`). Their ids must differ from `Hotkeys`' ids.
    GameHotkeys(Vec<(i32, Hotkey)>),
    /// Watch these processes' windows (replaces the set).
    Track(Vec<u32>),
    /// Tests: run on the desktop thread.
    #[cfg(test)]
    Run(Box<dyn FnOnce() + Send>),
}

const CLASS: &str = "CrystalFeaturesDesktop";
const WM_COMMANDS: u32 = 0x8000 + 0x40; // WM_APP + 0x40
const MOD_NOREPEAT: u32 = 0x4000;

const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
const EVENT_OBJECT_SHOW: u32 = 0x8002;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const WINEVENT_OUTOFCONTEXT: u32 = 0;
const WINEVENT_SKIPOWNPROCESS: u32 = 2;
const OBJID_WINDOW: i32 = 0;

/// A running desktop thread. Dropping it ends the thread (hooks and hotkeys are released).
pub(crate) struct Desktop {
    hwnd: isize,
    queue: Arc<Mutex<VecDeque<Command>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct State {
    sink: Sink,
    queue: Arc<Mutex<VecDeque<Command>>>,
    /// The message-only window the hotkeys belong to.
    window: HWND,
    hotkeys: Vec<i32>,
    /// The `Hotkeys` Windows registered, with their chords: the keyboard hook takes these too
    /// while a game is in front. One another program owns stays that program's.
    always: Vec<(i32, Hotkey)>,
    /// What registering `Hotkeys` returned, reported together with the gate's results.
    results: Vec<(i32, Result<(), u32>)>,
    /// The in-game hotkeys, armed by the foreground hook.
    gate: Gate,
    /// Per tracked process: its scoped hooks.
    tracked: BTreeMap<u32, Vec<HWINEVENTHOOK>>,
    /// The tracked processes, shared with the keyboard hook's thread.
    pids: Arc<Mutex<Vec<u32>>>,
    foreground_hook: Option<HWINEVENTHOOK>,
    /// A tracked game's window is in front (as last seen by the foreground hook).
    in_game: bool,
    /// The keyboard hook's thread, started the first time the in-game keys are armed.
    keyhook: Option<KeyHook>,
    /// Work waiting for its timer (`later`), by timer id.
    timers: Vec<(usize, Box<dyn FnOnce() + Send>)>,
    next_timer: usize,
}

/// Run `work` on the desktop thread `ms` from now (one timer on its window: nothing polls).
/// Only the desktop thread may ask, from one of its handlers; false anywhere else.
pub(crate) fn later(ms: u32, work: Box<dyn FnOnce() + Send>) -> bool {
    STATE.with(|s| {
        let Ok(mut state) = s.try_borrow_mut() else { return false };
        let Some(state) = state.as_mut() else { return false };
        state.next_timer += 1;
        let id = state.next_timer;
        // SAFETY: a timer on this thread's own window, killed when it fires or with the window.
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetTimer(state.window, id, ms, None) } == 0 { return false; }
        state.timers.push((id, work));
        true
    })
}

thread_local! { static STATE: std::cell::RefCell<Option<State>> = const { std::cell::RefCell::new(None) }; }

impl Desktop {
    pub(crate) fn start(sink: Sink) -> Result<Self, String> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let (ready, wait) = std::sync::mpsc::channel();
        let thread_queue = queue.clone();
        let thread = std::thread::Builder::new().name("features-desktop".into()).stack_size(256 * 1024)
            .spawn(move || run(sink, thread_queue, ready))
            .map_err(|_| crate::i18n::t("Could not start the hotkey and window watcher.").to_owned())?;
        match wait.recv() {
            Ok(Some(hwnd)) => Ok(Self { hwnd, queue, thread: Some(thread) }),
            _ => {
                let _ = thread.join();
                Err(crate::i18n::t("Could not start the hotkey and window watcher.").into())
            }
        }
    }

    pub(crate) fn send(&self, command: Command) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(command);
        // SAFETY: posting to our own window; fails harmlessly once it is gone.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(self.hwnd as HWND, WM_COMMANDS, 0, 0) };
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: as in `send`; WM_CLOSE destroys the window, whose WM_DESTROY ends the loop.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(self.hwnd as HWND, windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE, 0, 0) };
        if let Some(thread) = self.thread.take() {
            // A drop on the desktop thread itself (from a handler) must not wait for itself.
            if thread.thread().id() != std::thread::current().id() { let _ = thread.join(); }
        }
    }
}

fn run(sink: Sink, queue: Arc<Mutex<VecDeque<Command>>>, ready: std::sync::mpsc::Sender<Option<isize>>) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    // Window positions are read and set in physical pixels on this thread.
    let _dpi = super::game_window::PhysicalDpi::enter();
    // SAFETY: a window class and message-only window owned by this thread, and its message loop.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = crate::win::wide(CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(window_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc); // fails harmlessly when a previous thread registered it
        let hwnd = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), instance, std::ptr::null());
        if hwnd.is_null() { let _ = ready.send(None); return; }
        STATE.with(|s| *s.borrow_mut() = Some(State {
            sink, queue, window: hwnd, hotkeys: Vec::new(), always: Vec::new(), results: Vec::new(), gate: Gate::default(),
            tracked: BTreeMap::new(), pids: Arc::new(Mutex::new(Vec::new())), foreground_hook: None, in_game: false, keyhook: None,
            timers: Vec::new(), next_timer: 0,
        }));
        let _ = ready.send(Some(hwnd as isize));
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        STATE.with(|s| s.borrow_mut().take());
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    match message {
        WM_HOTKEY => { hotkey(wparam as i32, (lparam >> 16) as u16); 0 }
        WM_HOOK_HOTKEY => { emit(Event::Hotkey { id: wparam as i32, via: Via::Hook, key: Some(Fired::unpack(lparam)) }); 0 }
        WM_HOOK_RELEASE => {
            // A per-press line for debugging the key hook; release builds leave it out.
            if cfg!(debug_assertions) {
                emit(Event::Log(format!("hotkeys: chord key released after {} ms, {} repeats swallowed", lparam as u32, wparam as u32)));
            }
            0
        }
        WM_TIMER => {
            // SAFETY: a timer `later` set on this window.
            unsafe { KillTimer(hwnd, wparam) };
            let work = STATE.with(|s| s.borrow_mut().as_mut().and_then(|state| {
                let at = state.timers.iter().position(|(id, _)| *id == wparam)?;
                Some(state.timers.swap_remove(at).1)
            }));
            if let Some(work) = work { work(); } // outside the state: it may call back in
            0
        }
        WM_COMMANDS => { drain(hwnd); 0 }
        WM_DESTROY => {
            let keyhook = STATE.with(|s| s.borrow_mut().as_mut().and_then(|state| state.keyhook.take()));
            drop(keyhook); // its thread ends; joined outside the state
            STATE.with(|s| {
                if let Some(state) = s.borrow_mut().as_mut() {
                    state.gate.disarm(&mut Windows(hwnd));
                    // SAFETY: releasing what this thread registered and hooked.
                    for id in state.hotkeys.drain(..) { unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey(hwnd, id) }; }
                    for hooks in std::mem::take(&mut state.tracked).into_values() { for hook in hooks { unsafe { UnhookWinEvent(hook) }; } }
                    if let Some(hook) = state.foreground_hook.take() { unsafe { UnhookWinEvent(hook) }; }
                }
            });
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The sink, taken out of the thread state so a handler may call back into `Desktop`.
fn emit(event: Event) {
    if let Some(sink) = STATE.with(|s| s.borrow().as_ref().map(|state| state.sink.clone())) { sink(event); }
}

/// Every hotkey's registration result: the ones registered while set, then the in-game ones.
fn report() {
    let results = STATE.with(|s| s.borrow().as_ref().map(|state| {
        state.results.iter().chain(state.gate.results()).cloned().collect::<Vec<_>>()
    }));
    if let Some(results) = results { emit(Event::Registered(results)); }
}

/// RegisterHotKey on the desktop thread's window. A chord only the keyboard hook can tell
/// (`Hotkey::registration`) is not registered, and reads as fine.
struct Windows(HWND);
impl Registrar for Windows {
    fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), u32> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
        let Some((modifiers, vk)) = hotkey.registration() else { return Ok(()) };
        // SAFETY: a hotkey on our own window.
        let ok = unsafe { RegisterHotKey(self.0, id, modifiers | MOD_NOREPEAT, vk) } != 0;
        if ok { Ok(()) } else { Err(unsafe { windows_sys::Win32::Foundation::GetLastError() }) }
    }
    fn unregister(&mut self, id: i32) {
        // SAFETY: as above.
        unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey(self.0, id) };
    }
}

/// Whether Windows would register `hotkey` now: it is registered for the calling thread and
/// released at once. `Err(1409)` (ERROR_HOTKEY_ALREADY_REGISTERED) when a program owns it;
/// Crystal's own registrations count as one, so ask only for a chord Crystal does not hold.
pub(crate) fn probe(hotkey: Hotkey) -> Result<(), u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
    const ID: i32 = 0xBFFE;
    let Some((modifiers, vk)) = hotkey.registration() else { return Ok(()) };
    // SAFETY: a hotkey of this thread's own, released before returning.
    unsafe {
        if RegisterHotKey(std::ptr::null_mut(), ID, modifiers | MOD_NOREPEAT, vk) == 0 { return Err(windows_sys::Win32::Foundation::GetLastError()); }
        UnregisterHotKey(std::ptr::null_mut(), ID);
    }
    Ok(())
}

/// The chords the keyboard hook takes while a game is in front: the in-game ones, and the
/// others Windows registered for Crystal.
fn hook_keys(state: &State) -> Vec<(i32, Hotkey)> { state.gate.keys().iter().chain(&state.always).copied().collect() }

/// Whether `pid` is one of the tracked games' processes.
fn is_game(state: &State, pid: Option<u32>) -> bool { pid.is_some_and(|pid| state.tracked.contains_key(&pid)) }

/// The foreground process now (one query, on a change this thread was told about; never polled).
fn foreground_pid() -> Option<u32> { super::game_window::foreground().map(|(_, pid)| pid) }

/// Arm or disarm the in-game hotkeys for the process now in front. Whether to report.
fn gate_focus(pid: Option<u32>) -> bool {
    let (report, line) = STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(state) = state.as_mut() else { return (false, None) };
        let game = is_game(state, pid);
        let window = state.window;
        let report = state.gate.focus(game, &mut Windows(window));
        (report, set_in_game(state, game))
    });
    if let Some(line) = line { emit(Event::Log(line)); }
    report
}

/// Whether a game is in front changed to `game`: arm or disarm the keyboard hook to match the
/// chords set, and describe the change for the history (nothing while no hotkey is set).
fn set_in_game(state: &mut State, game: bool) -> Option<String> {
    if game && state.in_game {
        // Game to game: that game takes the keyboard (and DirectInput's own hook) as it comes in
        // front, so the hook is installed again to stay ahead of it. The key that made the switch
        // is usually still down: the hook keeps its press (`keyhook::Matcher`), so its repeats
        // switch nothing. Quietly: only a failure is told.
        let keys = hook_keys(state);
        let result = state.keyhook.as_ref().filter(|_| !keys.is_empty()).map(|hook| hook.arm(keys));
        return match result {
            Some(Err(code)) => Some(format!("hotkeys: game to game: hook failed (error {code})")),
            _ => None,
        };
    }
    if game == state.in_game { return None; }
    state.in_game = game;
    if !game {
        if let Some(hook) = &state.keyhook { hook.disarm(); }
        return (!hook_keys(state).is_empty()).then(|| "hotkeys: game left: cycle keys unregistered, hook off".to_owned());
    }
    let keys = hook_keys(state);
    if keys.is_empty() { return None; }
    let mut registered: Vec<String> = state.gate.keys().iter().map(|(id, hotkey)| {
        match state.gate.results().iter().find(|(i, _)| i == id).map(|(_, r)| *r) {
            Some(Ok(())) => format!("{hotkey} registered"),
            Some(Err(code)) => format!("{hotkey} not registered (error {code})"),
            None => format!("{hotkey} not tried"),
        }
    }).collect();
    if !state.always.is_empty() { registered.push(format!("{} account hotkeys", state.always.len())); }
    if state.keyhook.is_none() {
        match KeyHook::start(state.window as isize, state.pids.clone()) {
            Ok(hook) => state.keyhook = Some(hook),
            Err(error) => return Some(format!("hotkeys: game in front: {}; hook unavailable ({error})", registered.join(", "))),
        }
    }
    let hook = match state.keyhook.as_ref().map(|hook| hook.arm(keys)) {
        Some(Ok(())) => "hook on".to_owned(),
        Some(Err(code)) => format!("hook failed (error {code})"),
        None => "hook unavailable".to_owned(),
    };
    Some(format!("hotkeys: game in front: {}; {hook}", registered.join(", ")))
}

/// A hotkey was pressed. An in-game one that arrives after the foreground left the games (its
/// WinEvent not yet handled) is released at once and its key passed on, so the program now in
/// front still gets its Ctrl+Tab.
fn hotkey(id: i32, key: u16) {
    let stale = STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(state) = state.as_mut() else { return false };
        // No window in front at all is not "left": the game that had the foreground is between
        // windows of its own, and is still what the user sees.
        let front = super::game_window::foreground();
        if !state.gate.is_key(id) || front.is_none_or(|(_, pid)| is_game(state, Some(pid))) { return false; }
        let window = state.window;
        state.gate.disarm(&mut Windows(window));
        true
    });
    if !stale { emit(Event::Hotkey { id, via: Via::Hotkey, key: None }); return; }
    emit(Event::Log(format!("hotkeys: WM_HOTKEY id {id} after the game left the front: unregistered, key passed on")));
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP};
    // The modifiers are still held: only the key itself is sent again.
    // Marked as Crystal's own, so the keyboard hook (still in for a moment) lets it through.
    let input = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: key, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: OWN_INPUT } } };
    let tap = [input(0), input(KEYEVENTF_KEYUP)];
    // SAFETY: a sized array of keyboard inputs.
    unsafe { SendInput(tap.len() as u32, tap.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}

fn drain(hwnd: HWND) {
    let mut changed = false;
    // The chords changed: the hook follows them, once for the whole batch.
    let mut rearm = false;
    loop {
        let Some(command) = STATE.with(|s| s.borrow().as_ref().and_then(|state| state.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front())) else { break };
        match command {
            Command::Hotkeys(list) => { register(hwnd, &list); changed = true; rearm = true; }
            Command::GameHotkeys(list) => {
                let game = is_game_now();
                STATE.with(|s| if let Some(state) = s.borrow_mut().as_mut() { state.gate.set_keys(list, game, &mut Windows(hwnd)); });
                changed = true;
                rearm = true;
            }
            Command::Track(pids) => {
                track(&pids);
                // A game that ended in front, or one whose window is already in front.
                changed |= gate_focus(foreground_pid());
            }
            #[cfg(test)]
            Command::Run(work) => work(),
        }
    }
    if rearm {
        let game = is_game_now();
        let line = STATE.with(|s| {
            let mut state = s.borrow_mut();
            let state = state.as_mut()?;
            // Off, then armed again now when a game is in front.
            if let Some(hook) = &state.keyhook { hook.disarm(); }
            state.in_game = false;
            set_in_game(state, game)
        });
        if let Some(line) = line { emit(Event::Log(line)); }
    }
    if changed { report(); }
}

fn is_game_now() -> bool {
    let pid = foreground_pid();
    STATE.with(|s| s.borrow().as_ref().is_some_and(|state| is_game(state, pid)))
}

fn register(hwnd: HWND, list: &[(i32, Hotkey)]) {
    let previous = STATE.with(|s| s.borrow_mut().as_mut().map(|state| std::mem::take(&mut state.hotkeys))).unwrap_or_default();
    let mut windows = Windows(hwnd);
    for id in previous { windows.unregister(id); }
    let results: Vec<_> = list.iter().map(|(id, hotkey)| (*id, windows.register(*id, *hotkey))).collect();
    let always: Vec<(i32, Hotkey)> = list.iter().filter(|(id, _)| results.iter().any(|(i, r)| i == id && r.is_ok())).copied().collect();
    STATE.with(|s| if let Some(state) = s.borrow_mut().as_mut() {
        state.hotkeys = always.iter().map(|(id, _)| *id).collect();
        state.always = always;
        state.results = results;
    });
}

fn track(pids: &[u32]) {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(state) = state.as_mut() else { return };
        let gone: Vec<u32> = state.tracked.keys().filter(|p| !pids.contains(p)).copied().collect();
        for pid in gone {
            // SAFETY: hooks this thread installed.
            for hook in state.tracked.remove(&pid).unwrap_or_default() { unsafe { UnhookWinEvent(hook) }; }
        }
        for pid in pids.iter().filter(|p| !state.tracked.contains_key(p)).copied().collect::<Vec<_>>() {
            let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
            let ranges = [(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND), (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
                (EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW), (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE)];
            // SAFETY: out-of-context hooks scoped to `pid`, delivered to this thread's loop.
            let hooks = ranges.iter().map(|(min, max)| unsafe { SetWinEventHook(*min, *max, std::ptr::null_mut(), Some(on_event), pid, 0, flags) })
                .filter(|h| !h.is_null()).collect();
            state.tracked.insert(pid, hooks);
        }
        *state.pids.lock().unwrap_or_else(|e| e.into_inner()) = state.tracked.keys().copied().collect();
        match (state.tracked.is_empty(), state.foreground_hook) {
            (true, Some(hook)) => { unsafe { UnhookWinEvent(hook) }; state.foreground_hook = None; }
            (false, None) => {
                // SAFETY: as above, for every process: only foreground changes.
                let hook = unsafe { SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, std::ptr::null_mut(), Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT) };
                state.foreground_hook = (!hook.is_null()).then_some(hook);
            }
            _ => {}
        }
    });
}

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, object: i32, child: i32, _: u32, _: u32) {
    if event == EVENT_SYSTEM_FOREGROUND {
        // Every foreground change arms or disarms the in-game hotkeys, before any filtering: a
        // change to a window this can't read (or to none) disarms them.
        let pid = (!hwnd.is_null()).then(|| super::game_window::window_process(hwnd as isize).map(|(pid, _)| pid)).flatten();
        if gate_focus(pid) { report(); }
    }
    if hwnd.is_null() || object != OBJID_WINDOW || child != 0 { return; }
    let Some((pid, _)) = super::game_window::window_process(hwnd as isize) else { return };
    let hwnd = hwnd as isize;
    // Only top-level windows matter: a game's child controls and menus are skipped.
    // SAFETY: read-only query.
    let top = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetAncestor(hwnd as HWND, windows_sys::Win32::UI::WindowsAndMessaging::GA_ROOT) } as isize == hwnd;
    if !top { return; }
    emit(match event {
        EVENT_SYSTEM_FOREGROUND => Event::Foreground { hwnd, pid },
        EVENT_SYSTEM_MOVESIZEEND => Event::MoveSizeEnd { hwnd, pid },
        EVENT_SYSTEM_MINIMIZESTART => Event::Minimized { hwnd, pid, start: true },
        EVENT_SYSTEM_MINIMIZEEND => Event::Minimized { hwnd, pid, start: false },
        EVENT_OBJECT_SHOW => Event::Shown { hwnd, pid },
        EVENT_OBJECT_NAMECHANGE => Event::TitleChanged { hwnd, pid },
        _ => return,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn the_thread_registers_hotkeys_reports_conflicts_and_ends_cleanly() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let desktop = Desktop::start(Arc::new(move |e| { let _ = tx.lock().unwrap().send(e); })).unwrap();
        // An unusual chord nothing else should own.
        let chord = Hotkey::new(super::super::hotkeys::MOD_CONTROL | super::super::hotkeys::MOD_ALT | super::super::hotkeys::MOD_SHIFT, 0x87); // F24
        desktop.send(Command::Hotkeys(vec![(1, chord)]));
        let wait = std::time::Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(wait).unwrap(), Event::Registered(vec![(1, Ok(()))]));
        // Re-registering an empty set releases the chord.
        desktop.send(Command::Hotkeys(vec![]));
        assert_eq!(rx.recv_timeout(wait).unwrap(), Event::Registered(vec![]));
        // Tracking installs and removes scoped hooks without events for a windowless process.
        desktop.send(Command::Track(vec![std::process::id()]));
        desktop.send(Command::Track(vec![]));
        let (done, finished) = mpsc::channel();
        desktop.send(Command::Run(Box::new(move || { let _ = done.send(STATE.with(|s| s.borrow().as_ref().map(|st| (st.tracked.len(), st.foreground_hook.is_some())))); })));
        assert_eq!(finished.recv_timeout(wait).unwrap(), Some((0, false)));
        drop(desktop);
    }

    #[test]
    fn work_asked_for_later_runs_once_on_the_desktop_thread() {
        let desktop = Desktop::start(Arc::new(|_| {})).unwrap();
        let wait = std::time::Duration::from_secs(5);
        let (done, finished) = mpsc::channel();
        let asked = std::time::Instant::now();
        desktop.send(Command::Run(Box::new(move || {
            let thread = std::thread::current().id();
            let ran = done.clone();
            let set = later(40, Box::new(move || { let _ = ran.send((std::thread::current().id() == thread, asked.elapsed())); }));
            if !set { let _ = done.send((false, asked.elapsed())); }
        })));
        let (same_thread, after) = finished.recv_timeout(wait).unwrap();
        assert!(same_thread, "set, and run on the desktop thread");
        assert!(after >= std::time::Duration::from_millis(30), "not before its time: {after:?}");
        assert!(finished.recv_timeout(std::time::Duration::from_millis(150)).is_err(), "once");
        assert!(!later(1, Box::new(|| {})), "only the desktop thread may ask");
        drop(desktop);
    }

    #[test]
    fn in_game_hotkeys_stay_unregistered_while_no_game_is_in_front() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let desktop = Desktop::start(Arc::new(move |e| { let _ = tx.lock().unwrap().send(e); })).unwrap();
        let wait = std::time::Duration::from_secs(5);
        let chord = Hotkey::new(super::super::hotkeys::MOD_CONTROL | super::super::hotkeys::MOD_ALT | super::super::hotkeys::MOD_SHIFT | super::super::hotkeys::MOD_WIN, 0x82); // F19
        let global = Hotkey::new(super::super::hotkeys::MOD_CONTROL | super::super::hotkeys::MOD_ALT | super::super::hotkeys::MOD_SHIFT | super::super::hotkeys::MOD_WIN, 0x81); // F18
        desktop.send(Command::Hotkeys(vec![(1, global)]));
        desktop.send(Command::GameHotkeys(vec![(2, chord)]));
        // One report for both commands: the global key registered, the in-game key waiting.
        assert_eq!(rx.recv_timeout(wait).unwrap(), Event::Registered(vec![(1, Ok(())), (2, Ok(()))]));
        // This test process is tracked, but its (windowless) process is never in front.
        desktop.send(Command::Track(vec![std::process::id()]));
        let (done, finished) = mpsc::channel();
        desktop.send(Command::Run(Box::new(move || { let _ = done.send(STATE.with(|s| s.borrow().as_ref().map(|st| st.gate.armed()))); })));
        assert_eq!(finished.recv_timeout(wait).unwrap(), Some(false));
        // The in-game chord is free for anyone else meanwhile.
        let (tx2, rx2) = mpsc::channel();
        let tx2 = Mutex::new(tx2);
        let other = Desktop::start(Arc::new(move |e| { let _ = tx2.lock().unwrap().send(e); })).unwrap();
        other.send(Command::Hotkeys(vec![(5, chord)]));
        assert_eq!(rx2.recv_timeout(wait).unwrap(), Event::Registered(vec![(5, Ok(()))]));
        drop(other);
        drop(desktop);
    }

    /// Every modifier with a high function key: a chord nothing else should own.
    fn rare(vk: u16) -> Hotkey {
        use super::super::hotkeys::{MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
        Hotkey::new(MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, vk)
    }

    /// A desktop thread standing in for another program, with `chord` registered.
    fn another_program(chord: Hotkey) -> Desktop {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let other = Desktop::start(Arc::new(move |e| { let _ = tx.lock().unwrap().send(e); })).unwrap();
        other.send(Command::Hotkeys(vec![(1, chord)]));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), Event::Registered(vec![(1, Ok(()))]));
        other
    }

    #[test]
    fn a_chord_another_program_owns_is_found_before_it_is_needed() {
        let (chord, free) = (rare(0x83), rare(0x80)); // F20, F17
        assert_eq!(probe(chord), Ok(()));
        let other = another_program(chord);
        assert_eq!(probe(chord), Err(1409), "ERROR_HOTKEY_ALREADY_REGISTERED");
        // Asking registers nothing for good: a free chord is free again the next time.
        assert_eq!(probe(free), Ok(()));
        assert_eq!(probe(free), Ok(()));
        // The number pad's Enter is never registered, so there is nothing to ask.
        assert_eq!(probe(Hotkey::new(super::super::hotkeys::MOD_CONTROL, super::super::hotkeys::NUM_ENTER)), Ok(()));
        drop(other);
        assert_eq!(probe(chord), Ok(()), "released with the program");
    }

    /// Over a game the keyboard hook takes the cycle keys and the account hotkeys Windows
    /// registered for Crystal; a chord another program owns stays that program's.
    #[test]
    fn the_keyboard_hook_is_given_the_account_hotkeys_windows_registered() {
        let (mine, taken, cycle) = (rare(0x7E), rare(0x7F), rare(0x7D)); // F15, F16, F14
        let other = another_program(taken);
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let desktop = Desktop::start(Arc::new(move |e| { let _ = tx.lock().unwrap().send(e); })).unwrap();
        let wait = std::time::Duration::from_secs(5);
        desktop.send(Command::Hotkeys(vec![(1, mine), (2, taken)]));
        desktop.send(Command::GameHotkeys(vec![(3, cycle)]));
        assert_eq!(rx.recv_timeout(wait).unwrap(), Event::Registered(vec![(1, Ok(())), (2, Err(1409)), (3, Ok(()))]));
        let (done, finished) = mpsc::channel();
        desktop.send(Command::Run(Box::new(move || { let _ = done.send(STATE.with(|s| s.borrow().as_ref().map(|st| (hook_keys(st), st.keyhook.is_some())))); })));
        // No game is in front: the chords are known, and there is no hook at all.
        assert_eq!(finished.recv_timeout(wait).unwrap(), Some((vec![(3, cycle), (1, mine)], false)));
        // Clearing the account hotkeys takes them from the hook's list too.
        desktop.send(Command::Hotkeys(vec![]));
        assert_eq!(rx.recv_timeout(wait).unwrap(), Event::Registered(vec![(3, Ok(()))]));
        let (done, finished) = mpsc::channel();
        desktop.send(Command::Run(Box::new(move || { let _ = done.send(STATE.with(|s| s.borrow().as_ref().map(hook_keys))); })));
        assert_eq!(finished.recv_timeout(wait).unwrap(), Some(vec![(3, cycle)]));
        drop(desktop);
        drop(other);
    }

    #[test]
    fn a_chord_registered_twice_reports_the_windows_error() {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let first = Desktop::start(Arc::new(move |e| { let _ = tx.lock().unwrap().send(e); })).unwrap();
        let (tx2, rx2) = mpsc::channel();
        let tx2 = Mutex::new(tx2);
        let second = Desktop::start(Arc::new(move |e| { let _ = tx2.lock().unwrap().send(e); })).unwrap();
        let chord = Hotkey::new(super::super::hotkeys::MOD_CONTROL | super::super::hotkeys::MOD_ALT | super::super::hotkeys::MOD_WIN, 0x86); // F23
        let wait = std::time::Duration::from_secs(5);
        first.send(Command::Hotkeys(vec![(7, chord)]));
        assert_eq!(rx.recv_timeout(wait).unwrap(), Event::Registered(vec![(7, Ok(()))]));
        second.send(Command::Hotkeys(vec![(7, chord)]));
        assert_eq!(rx2.recv_timeout(wait).unwrap(), Event::Registered(vec![(7, Err(1409))]), "ERROR_HOTKEY_ALREADY_REGISTERED");
        drop(first);
        drop(second);
    }
}
