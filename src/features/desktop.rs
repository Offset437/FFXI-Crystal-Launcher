//! The one desktop thread: a message-only window that owns Crystal's global hotkeys
//! (RegisterHotKey) and the WinEvent hooks that tell the features when a tracked game's window
//! appears, is moved or resized, minimised, renamed, or when the foreground window changes.
//!
//! It exists only while something needs it (a hotkey is set, or a game is tracked by a feature
//! that watches windows) and sleeps in GetMessage the rest of the time: no timers, no polling.
//! Hooks are out-of-context, so nothing is loaded into the game. The per-game hooks are scoped
//! to that game's processes; the one global hook is the foreground change, which only fires when
//! the user switches windows. That hook also arms the in-game hotkeys (the cycle keys): they are
//! registered while a tracked game's window is in front and released on any other change.

use super::hotkeys::{Gate, Hotkey, Registrar};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};

/// What the desktop thread reports, on the desktop thread itself (so a hotkey's handler may set
/// the foreground window: the hotkey's input belongs to this thread).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Hotkey(i32),
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
    /// Run on the desktop thread (window calls that need its input state).
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
    /// What registering `Hotkeys` returned, reported together with the gate's results.
    results: Vec<(i32, Result<(), u32>)>,
    /// The in-game hotkeys, armed by the foreground hook.
    gate: Gate,
    /// Per tracked process: its scoped hooks.
    tracked: BTreeMap<u32, Vec<HWINEVENTHOOK>>,
    foreground_hook: Option<HWINEVENTHOOK>,
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

    /// Whether the caller is the desktop thread.
    pub(crate) fn is_current(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| t.thread().id() == std::thread::current().id())
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
            sink, queue, window: hwnd, hotkeys: Vec::new(), results: Vec::new(), gate: Gate::default(),
            tracked: BTreeMap::new(), foreground_hook: None,
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
        WM_COMMANDS => { drain(hwnd); 0 }
        WM_DESTROY => {
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

/// RegisterHotKey on the desktop thread's window.
struct Windows(HWND);
impl Registrar for Windows {
    fn register(&mut self, id: i32, hotkey: Hotkey) -> Result<(), u32> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
        // SAFETY: a hotkey on our own window.
        let ok = unsafe { RegisterHotKey(self.0, id, u32::from(hotkey.modifiers) | MOD_NOREPEAT, u32::from(hotkey.key)) } != 0;
        if ok { Ok(()) } else { Err(unsafe { windows_sys::Win32::Foundation::GetLastError() }) }
    }
    fn unregister(&mut self, id: i32) {
        // SAFETY: as above.
        unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey(self.0, id) };
    }
}

/// Whether `pid` is one of the tracked games' processes.
fn is_game(state: &State, pid: Option<u32>) -> bool { pid.is_some_and(|pid| state.tracked.contains_key(&pid)) }

/// The foreground process now (one query, on a change this thread was told about; never polled).
fn foreground_pid() -> Option<u32> { super::game_window::foreground().map(|(_, pid)| pid) }

/// Arm or disarm the in-game hotkeys for the process now in front. Whether to report.
fn gate_focus(pid: Option<u32>) -> bool {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(state) = state.as_mut() else { return false };
        let game = is_game(state, pid);
        let window = state.window;
        state.gate.focus(game, &mut Windows(window))
    })
}

/// A hotkey was pressed. An in-game one that arrives after the foreground left the games (its
/// WinEvent not yet handled) is released at once and its key passed on, so the program now in
/// front still gets its Ctrl+Tab.
fn hotkey(id: i32, key: u16) {
    let stale = STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(state) = state.as_mut() else { return false };
        if !state.gate.is_key(id) || is_game(state, foreground_pid()) { return false; }
        let window = state.window;
        state.gate.disarm(&mut Windows(window));
        true
    });
    if !stale { emit(Event::Hotkey(id)); return; }
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP};
    // The modifiers are still held: only the key itself is sent again.
    let input = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: key, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
    let tap = [input(0), input(KEYEVENTF_KEYUP)];
    // SAFETY: a sized array of keyboard inputs.
    unsafe { SendInput(tap.len() as u32, tap.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}

fn drain(hwnd: HWND) {
    let mut changed = false;
    loop {
        let Some(command) = STATE.with(|s| s.borrow().as_ref().and_then(|state| state.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front())) else { break };
        match command {
            Command::Hotkeys(list) => { register(hwnd, &list); changed = true; }
            Command::GameHotkeys(list) => {
                let game = is_game_now();
                STATE.with(|s| if let Some(state) = s.borrow_mut().as_mut() { state.gate.set_keys(list, game, &mut Windows(hwnd)); });
                changed = true;
            }
            Command::Track(pids) => {
                track(&pids);
                // A game that ended in front, or one whose window is already in front.
                changed |= gate_focus(foreground_pid());
            }
            Command::Run(work) => work(),
        }
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
    let registered: Vec<i32> = results.iter().filter(|(_, r)| r.is_ok()).map(|(id, _)| *id).collect();
    STATE.with(|s| if let Some(state) = s.borrow_mut().as_mut() { state.hotkeys = registered; state.results = results; });
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
