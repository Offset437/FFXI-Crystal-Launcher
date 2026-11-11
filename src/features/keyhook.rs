//! Crystal's hotkeys while a game is in front, through a low-level keyboard hook.
//!
//! RegisterHotKey alone does not reach Crystal while FFXI is in front: the game reads the
//! keyboard through DirectInput, which takes keys in a low-level hook (or raw input) of its own,
//! and Windows matches registered hotkeys only after the low-level hooks let a key through. A
//! low-level hook installed later than the game's runs first, so this one sees the chord before
//! the game does. That holds for every registered hotkey, so the hook is given the account
//! hotkeys as well as the cycle keys.
//!
//! Kept to the minimum, so no other program's keys are touched:
//! - The hook exists only while a Crystal game window is in front (`arm`/`disarm`, driven by the
//!   desktop thread's foreground hook). With any other window in front there is no hook at all.
//! - It takes exactly the chords it was given (the hotkeys the user set), with
//!   exactly those modifiers, and only while the window in front belongs to a tracked game: it
//!   then swallows that key's press, repeats and release, and posts the chord's id to the
//!   desktop window. Every other key passes at once: a key no chord uses costs two short list
//!   scans and no system call (`Matcher::event`).
//! - It runs on a thread of its own, above normal priority, that does nothing else: a busy
//!   desktop or UI thread can't delay the keyboard, and Windows has no cause to drop the hook
//!   (it silently removes one that keeps exceeding LowLevelHooksTimeout).
//! - It is installed again shortly after each arm, and every few seconds for as long as it is
//!   armed (`REHOOK_EVERY`): the game's own hook, which DirectInput installs when the game takes
//!   the keyboard, would otherwise get ahead of it, and a hook Windows dropped comes back.
//! - Keys sent by software (a Stream Deck, AutoHotkey) are keys like any other: the same chord
//!   does the same thing. Only Crystal's own sent keys (`OWN_INPUT`) are let through unread.
//! - Nothing is recorded or logged about keys but the chord key's own scan code, flags and
//!   timing.
//!
//! The number pad's keys are matched by scan code (`hotkeys::canonical`), so a number pad chord
//! fires whatever Num Lock says, and the number pad's Enter is not the main one.
//!
//! One physical press is one chord. A low-level hook is not told which key-downs are
//! auto-repeats, so the matcher keeps each chord key's press from its first key-down to its
//! release (`Matcher`), and that press outlives everything the switch itself causes: the hook
//! armed again for the game that came in front, its re-installs, and a spell with the hook off.

use super::hotkeys::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use std::cell::RefCell;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::HHOOK;

/// Posted to the desktop window: the chord's id in `wParam`, the key event in `lParam`
/// (`Fired::pack`).
pub(crate) const WM_HOOK_HOTKEY: u32 = 0x8000 + 0x41; // WM_APP + 0x41
/// Posted to the desktop window when the key of a chord that fired is released: the repeats
/// swallowed in `wParam`, how long it was held (ms) in `lParam`.
pub(crate) const WM_HOOK_RELEASE: u32 = 0x8000 + 0x42;

const WM_ARM: u32 = 0x8000 + 0x01;
const WM_DISARM: u32 = 0x8000 + 0x02;
/// After an arm, the hook is installed again at these delays (ms), each after the last.
const REHOOK_AFTER: [u32; 2] = [300, 1200];
/// And then at this interval (ms) for as long as the hook is armed.
const REHOOK_EVERY: u32 = 5000;
/// `dwExtraInfo` of the keys Crystal itself sends (`desktop`'s passed-on key): never a chord.
pub(crate) const OWN_INPUT: usize = 0x4352_5953;
/// A held key repeats at least this often (Windows' slowest repeat delay is 1 s): a press with
/// no event for longer was released while the hook was off.
const RELEASE_MISSED_AFTER: u32 = 1500;

/// What came before a chord's key-down, for the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Before {
    /// Nothing seen of this key since the hook's thread started.
    First,
    /// Its last release, this many ms earlier.
    Released(u32),
    /// A press whose release was never seen (the hook was off), last heard of this many ms earlier.
    Unreleased(u32),
}

/// What the hook does with one key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    Swallow,
    /// A chord was pressed: swallow the key and run the chord with this id.
    Fire { id: i32, before: Before },
    /// The key of a chord that fired was released (swallowed): how long it was held (ms) and how
    /// many auto-repeats were swallowed meanwhile.
    Release { held: u32, repeats: u32 },
}

/// A chord key that is down.
#[derive(Clone, Copy, Debug)]
struct Down {
    /// The key as chords name it (`hotkeys::canonical`).
    key: u16,
    /// It fired a chord: its repeats and release are swallowed. Otherwise they pass (the press
    /// went to the game or to Windows), and never fire.
    fired: bool,
    /// When it was pressed, and its last key-down (event times, ms).
    since: u32,
    last: u32,
    repeats: u32,
}

/// The hook's decision, kept apart from Windows so it can be tested.
#[derive(Debug, Default)]
pub(crate) struct Matcher {
    keys: Vec<(i32, Hotkey)>,
    /// The chord keys that are down, each from its first key-down to its release. A chord fires
    /// on a key's first key-down only: every later key-down of the same press is an auto-repeat.
    down: Vec<Down>,
    /// When each chord key was last released.
    released: Vec<(u16, u32)>,
}

impl Matcher {
    /// Replace the chords. Keys that are down stay down: the hook is armed again on every switch
    /// between games, while the chord's key is often still held.
    pub(crate) fn set_keys(&mut self, keys: Vec<(i32, Hotkey)>) { self.keys = keys; }

    /// The hook was installed after being off: `is_down` tells which keys Windows has down now.
    /// A chord key already down belongs to whatever had it (its repeats pass and never fire); a
    /// passed press released meanwhile is forgotten. A press that fired stays: Windows never saw
    /// it, so only its release or `RELEASE_MISSED_AFTER` ends it.
    pub(crate) fn armed(&mut self, now: u32, is_down: impl Fn(u16) -> bool) {
        self.down.retain(|d| d.fired || is_down(d.key));
        for d in self.down.iter_mut().filter(|d| !d.fired) { d.last = now; }
        for (_, hotkey) in &self.keys {
            if is_down(hotkey.key) && !self.down.iter().any(|d| d.key == hotkey.key) {
                self.down.push(Down { key: hotkey.key, fired: false, since: now, last: now, repeats: 0 });
            }
        }
    }

    /// One key event for a key that is the same under either name (every key but the number
    /// pad's): see `event`.
    #[cfg(test)]
    pub(crate) fn key(&mut self, key: u16, down: bool, now: u32, modifiers: impl FnOnce() -> u8, game_in_front: impl FnOnce() -> bool) -> Verdict {
        self.event(key, key, down, now, modifiers, game_in_front)
    }

    /// One key event: `key` (as chords name it: `hotkeys::canonical`) and its plain virtual key
    /// `vk`, pressed or released, at `now` (the event's time, ms). `modifiers` (the `MOD_*` bits
    /// held now) and `game_in_front` are asked only for a new press of a key that could
    /// complete a chord; any other key is passed after two short scans.
    ///
    /// A chord on a number pad key matches that key whatever Num Lock makes its virtual key; a
    /// chord on a plain virtual key also matches the number pad key Windows reports under it
    /// (Insert for the number pad's 0 with Num Lock off, Enter for its Enter), as the same
    /// chord registered with RegisterHotKey does.
    pub(crate) fn event(&mut self, key: u16, vk: u16, down: bool, now: u32, modifiers: impl FnOnce() -> u8, game_in_front: impl FnOnce() -> bool) -> Verdict {
        let mut before = None;
        if let Some(at) = self.down.iter().position(|d| d.key == key) {
            let quiet = now.wrapping_sub(self.down[at].last);
            if quiet <= RELEASE_MISSED_AFTER {
                if down {
                    // An auto-repeat of a press already decided.
                    let press = &mut self.down[at];
                    press.last = now;
                    press.repeats = press.repeats.saturating_add(1);
                    return if press.fired { Verdict::Swallow } else { Verdict::Pass };
                }
                let press = self.down.swap_remove(at);
                self.note_release(key, now);
                return if press.fired { Verdict::Release { held: now.wrapping_sub(press.since), repeats: press.repeats } } else { Verdict::Pass };
            }
            // Its release was missed: this event starts over.
            self.down.swap_remove(at);
            before = Some(Before::Unreleased(quiet));
        }
        if !self.keys.iter().any(|(_, h)| h.key == key || h.key == vk) { return Verdict::Pass; }
        if !down {
            self.note_release(key, now);
            return Verdict::Pass;
        }
        let before = before.unwrap_or_else(|| {
            self.released.iter().find(|(k, _)| *k == key).map_or(Before::First, |(_, at)| Before::Released(now.wrapping_sub(*at)))
        });
        let held = modifiers();
        let chord = |named: u16| self.keys.iter().find(|(_, h)| h.key == named && h.modifiers == held).map(|(id, _)| *id);
        let id = chord(key).or_else(|| chord(vk)).filter(|_| game_in_front());
        self.down.push(Down { key, fired: id.is_some(), since: now, last: now, repeats: 0 });
        id.map_or(Verdict::Pass, |id| Verdict::Fire { id, before })
    }

    fn note_release(&mut self, key: u16, now: u32) {
        match self.released.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = now,
            None => self.released.push((key, now)),
        }
    }
}

/// The key event that fired a chord, for the history: the chord key's scan code and the hook's
/// flags for it (`KBDLLHOOKSTRUCT`), and what came before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fired {
    pub scan: u8,
    pub flags: u8,
    pub before: Before,
}

impl Fired {
    const GAP_MAX: u32 = 0x3FFF;

    /// As a message's `lParam` (32 bits: scan, flags, what came before, the gap capped at 16 s).
    pub(crate) fn pack(&self) -> isize {
        let (kind, gap) = match self.before {
            Before::First => (0u32, 0),
            Before::Released(ms) => (1, ms.min(Self::GAP_MAX)),
            Before::Unreleased(ms) => (2, ms.min(Self::GAP_MAX)),
        };
        (u32::from(self.scan) | u32::from(self.flags) << 8 | kind << 16 | gap << 18) as i32 as isize
    }

    pub(crate) fn unpack(lparam: isize) -> Self {
        let bits = lparam as u32;
        let gap = bits >> 18;
        let before = match (bits >> 16) & 3 {
            1 => Before::Released(gap),
            2 => Before::Unreleased(gap),
            _ => Before::First,
        };
        Self { scan: bits as u8, flags: (bits >> 8) as u8, before }
    }

    /// `scan 0x0F flags 0x00, new press 412 ms after its release`.
    pub(crate) fn describe(&self) -> String {
        let gap = |ms: u32| if ms >= Self::GAP_MAX { format!("over {} s", Self::GAP_MAX / 1000) } else { format!("{ms} ms") };
        let before = match self.before {
            Before::First => "new press, the first seen".to_owned(),
            Before::Released(ms) => format!("new press {} after its release", gap(ms)),
            Before::Unreleased(ms) => format!("press {} after one whose release was not seen", gap(ms)),
        };
        format!("scan 0x{:02X} flags 0x{:02X}, {before}", self.scan, self.flags)
    }
}

/// What the hook thread shares with the desktop thread.
struct Shared {
    keys: Mutex<Vec<(i32, Hotkey)>>,
    /// The tracked games' processes (the desktop thread keeps it current).
    pids: Arc<Mutex<Vec<u32>>>,
    /// The desktop window the chords are posted to.
    window: isize,
}

/// The hook thread. Dropping it removes the hook and ends the thread.
pub(crate) struct KeyHook {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    replies: mpsc::Receiver<Result<(), u32>>,
}

struct Running {
    shared: Arc<Shared>,
    matcher: Matcher,
    hook: HHOOK,
    timer: usize,
    /// Re-installs still due after the last arm.
    rehooks: usize,
}

thread_local! { static RUNNING: RefCell<Option<Running>> = const { RefCell::new(None) }; }

impl KeyHook {
    /// Start the thread (no hook yet). `window` receives `WM_HOOK_HOTKEY` and `WM_HOOK_RELEASE`.
    pub(crate) fn start(window: isize, pids: Arc<Mutex<Vec<u32>>>) -> Result<Self, String> {
        let shared = Arc::new(Shared { keys: Mutex::new(Vec::new()), pids, window });
        let (ready, wait) = mpsc::channel();
        let (reply, replies) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new().name("features-keys".into()).stack_size(64 * 1024)
            .spawn(move || run(thread_shared, ready, reply))
            .map_err(|e| e.to_string())?;
        match wait.recv_timeout(Duration::from_secs(5)) {
            Ok(thread_id) => Ok(Self { thread_id, thread: Some(thread), shared, replies }),
            Err(_) => Err("the key thread did not start".into()),
        }
    }

    /// Install the hook for `keys` (again, when installed). Whether Windows installed it, or its
    /// error.
    pub(crate) fn arm(&self, keys: Vec<(i32, Hotkey)>) -> Result<(), u32> {
        *self.shared.keys.lock().unwrap_or_else(|e| e.into_inner()) = keys;
        while self.replies.try_recv().is_ok() {}
        if !self.post(WM_ARM) { return Err(last_error()); }
        self.replies.recv_timeout(Duration::from_secs(1)).unwrap_or(Err(1460)) // ERROR_TIMEOUT
    }

    /// Remove the hook (the thread stays, idle in GetMessage).
    pub(crate) fn disarm(&self) { self.post(WM_DISARM); }

    fn post(&self, message: u32) -> bool {
        // SAFETY: a thread message to our own thread, which created its queue before reporting ready.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(self.thread_id, message, 0, 0) != 0 }
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        self.post(windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

fn last_error() -> u32 {
    // SAFETY: no arguments.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

fn run(shared: Arc<Shared>, ready: mpsc::Sender<u32>, reply: mpsc::Sender<Result<(), u32>>) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    // SAFETY: this thread's own queue, timers and hook, all released before it ends.
    unsafe {
        // Every key on the PC waits for this thread while the hook is in: it answers ahead of
        // ordinary work, however busy the games keep the processors.
        windows_sys::Win32::System::Threading::SetThreadPriority(windows_sys::Win32::System::Threading::GetCurrentThread(), windows_sys::Win32::System::Threading::THREAD_PRIORITY_HIGHEST);
        let mut msg: MSG = std::mem::zeroed();
        // Create the queue before anyone posts to it.
        PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
        RUNNING.with(|r| *r.borrow_mut() = Some(Running { shared, matcher: Matcher::default(), hook: std::ptr::null_mut(), timer: 0, rehooks: 0 }));
        if ready.send(GetCurrentThreadId()).is_err() { return; }
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            match msg.message {
                WM_ARM => {
                    let result = RUNNING.with(|r| r.borrow_mut().as_mut().map(|run| {
                        let keys = run.shared.keys.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        run.matcher.set_keys(keys);
                        // Off until now: the keys Windows has down were not pressed over a game.
                        if run.hook.is_null() { run.matcher.armed(windows_sys::Win32::System::SystemInformation::GetTickCount(), key_down); }
                        let result = install(run);
                        stop_timer(run);
                        if result.is_ok() {
                            run.rehooks = REHOOK_AFTER.len();
                            run.timer = SetTimer(std::ptr::null_mut(), 0, rehook_delay(run.rehooks), None);
                        }
                        result
                    })).unwrap_or(Err(0));
                    let _ = reply.send(result);
                }
                WM_DISARM => RUNNING.with(|r| if let Some(run) = r.borrow_mut().as_mut() { uninstall(run); }),
                WM_TIMER if msg.hwnd.is_null() => RUNNING.with(|r| {
                    let mut running = r.borrow_mut();
                    let Some(run) = running.as_mut() else { return };
                    if msg.wParam != run.timer { return; }
                    stop_timer(run);
                    if run.hook.is_null() { return; }
                    let _ = install(run); // a failure keeps the hook already in place
                    run.rehooks = run.rehooks.saturating_sub(1);
                    run.timer = SetTimer(std::ptr::null_mut(), 0, rehook_delay(run.rehooks), None);
                }),
                _ => {}
            }
        }
        RUNNING.with(|r| if let Some(mut run) = r.borrow_mut().take() { uninstall(&mut run); });
    }
}

/// How long until the hook is installed again, with `early` of `REHOOK_AFTER`'s re-installs
/// still due; after them, `REHOOK_EVERY` for as long as the hook is armed.
fn rehook_delay(early: usize) -> u32 {
    if early == 0 { REHOOK_EVERY } else { REHOOK_AFTER[REHOOK_AFTER.len().saturating_sub(early)] }
}

/// Install the hook ahead of every other, then remove the one it replaces. The matcher is not
/// touched: a key held across the re-install is still the same press.
fn install(run: &mut Running) -> Result<(), u32> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL};
    // SAFETY: a low-level hook for this thread's message loop; the old one is ours to remove.
    unsafe {
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), GetModuleHandleW(std::ptr::null()), 0);
        if hook.is_null() { return Err(last_error()); }
        if !run.hook.is_null() { UnhookWindowsHookEx(run.hook); }
        run.hook = hook;
        Ok(())
    }
}

/// Remove the hook. The matcher keeps the keys that are down (`Matcher::armed` sorts them out
/// when the hook comes back).
fn uninstall(run: &mut Running) {
    stop_timer(run);
    run.rehooks = 0;
    if !run.hook.is_null() {
        // SAFETY: the hook this thread installed.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(run.hook) };
        run.hook = std::ptr::null_mut();
    }
}

fn stop_timer(run: &mut Running) {
    if run.timer != 0 {
        // SAFETY: a thread timer this thread set.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::KillTimer(std::ptr::null_mut(), run.timer) };
        run.timer = 0;
    }
}

/// Whether Windows has `key` down now, under any virtual key it reports it as (a key this
/// hook swallowed never reads as down).
fn key_down(key: u16) -> bool {
    // SAFETY: a key-state query.
    super::hotkeys::virtual_keys(key).iter().any(|vk| unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(*vk)) as u16 & 0x8000 != 0 })
}

/// The `MOD_*` bits held now.
fn modifiers() -> u8 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT};
    let mut bits = 0;
    if key_down(VK_CONTROL) { bits |= MOD_CONTROL; }
    if key_down(VK_MENU) { bits |= MOD_ALT; }
    if key_down(VK_SHIFT) { bits |= MOD_SHIFT; }
    if key_down(VK_LWIN) || key_down(VK_RWIN) { bits |= MOD_WIN; }
    bits
}

/// Whether the chords are the hook's to take now: a tracked game's window is in front, or no
/// window is (the hook is only installed while a game was the last window in front, and a game
/// between two windows of its own leaves the foreground empty: the game is still what is seen).
fn game_in_front(shared: &Shared) -> bool {
    super::game_window::foreground().is_none_or(|(_, pid)| shared.pids.lock().unwrap_or_else(|e| e.into_inner()).contains(&pid))
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, PostMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, WM_KEYDOWN, WM_SYSKEYDOWN};
    if code == HC_ACTION as i32 && lparam != 0 {
        // SAFETY: for HC_ACTION, lParam points at the event's KBDLLHOOKSTRUCT.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        // A key Crystal sent itself is passed unread; any other sent key is a key.
        if !(event.flags & LLKHF_INJECTED != 0 && event.dwExtraInfo == OWN_INPUT) {
            let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let vk = event.vkCode as u16;
            let key = super::hotkeys::canonical(vk, event.scanCode as u16, event.flags & LLKHF_EXTENDED != 0);
            let verdict = RUNNING.with(|r| {
                let mut running = r.try_borrow_mut().ok()?;
                let run = running.as_mut()?;
                let Running { matcher, shared, .. } = run;
                Some((matcher.event(key, vk, down, event.time, modifiers, || game_in_front(shared)), shared.window))
            });
            match verdict {
                Some((Verdict::Swallow, _)) => return 1,
                Some((Verdict::Fire { id, before }, window)) => {
                    let fired = Fired { scan: event.scanCode as u8, flags: event.flags as u8, before };
                    // SAFETY: posting to the desktop thread's window; fails harmlessly once it is gone.
                    unsafe { PostMessageW(window as HWND, WM_HOOK_HOTKEY, id as usize, fired.pack()) };
                    return 1;
                }
                Some((Verdict::Release { held, repeats }, window)) => {
                    // SAFETY: as above.
                    unsafe { PostMessageW(window as HWND, WM_HOOK_RELEASE, repeats as usize, held as i32 as isize) };
                    return 1;
                }
                _ => {}
            }
        }
    }
    // SAFETY: passing the event on, as every hook must.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::hotkeys::{OLD_CYCLE as CYCLE, OLD_CYCLE_BACK as CYCLE_BACK};

    const TAB: u16 = 0x09;
    const CTRL: u8 = MOD_CONTROL;
    const CTRL_SHIFT: u8 = MOD_CONTROL | MOD_SHIFT;

    fn keys() -> Vec<(i32, Hotkey)> { vec![(1, CYCLE), (2, CYCLE_BACK)] }

    fn matcher() -> Matcher {
        let mut m = Matcher::default();
        m.set_keys(keys());
        m
    }

    fn fired(v: Verdict) -> Option<i32> { if let Verdict::Fire { id, .. } = v { Some(id) } else { None } }

    #[test]
    fn exactly_ctrl_tab_and_ctrl_shift_tab_fire_over_a_game() {
        let mut m = matcher();
        assert_eq!(m.key(TAB, true, 1000, || CTRL, || true), Verdict::Fire { id: 1, before: Before::First });
        // Held down: the repeats and the release are the chord's, not the game's.
        assert_eq!(m.key(TAB, true, 1500, || CTRL, || true), Verdict::Swallow);
        assert_eq!(m.key(TAB, false, 1600, || CTRL, || true), Verdict::Release { held: 600, repeats: 1 });
        assert_eq!(m.key(TAB, true, 2000, || CTRL_SHIFT, || true), Verdict::Fire { id: 2, before: Before::Released(400) });
        assert_eq!(m.key(TAB, false, 2100, || 0, || true), Verdict::Release { held: 100, repeats: 0 }, "released after the modifiers");
        // Anything else passes: bare Tab and Shift+Tab (the game's target keys), Ctrl+Alt+Tab,
        // other keys with Ctrl, and releases nobody swallowed.
        for (vk, mods) in [(TAB, 0), (TAB, MOD_SHIFT), (TAB, CTRL | MOD_ALT), (TAB, CTRL | MOD_WIN), (0x41, CTRL), (0x11, CTRL)] {
            assert_eq!(m.key(vk, true, 3000, || mods, || true), Verdict::Pass, "{vk:#x} {mods}");
            assert_eq!(m.key(vk, false, 3050, || mods, || true), Verdict::Pass);
        }
    }

    /// The rule other programs rely on: with the default chords set, every key but Tab passes
    /// whatever is held, without a look at the modifiers or the window in front; and Tab passes
    /// under every modifier combination but the two chords.
    #[test]
    fn every_key_no_chord_uses_passes_untouched() {
        let mut m = matcher();
        let mut t = 1000;
        for vk in (1..=0xFEu16).filter(|vk| *vk != TAB) {
            for down in [true, true, false] {
                t += 10;
                assert_eq!(m.key(vk, down, t, || panic!("modifiers asked for {vk:#x}"), || panic!("foreground asked for {vk:#x}")), Verdict::Pass, "{vk:#x}");
            }
        }
        for mods in (0..16u8).filter(|mods| *mods != CTRL && *mods != CTRL_SHIFT) {
            t += 10;
            assert_eq!(m.key(TAB, true, t, || mods, || true), Verdict::Pass, "Tab with {mods:#x}");
            t += 10;
            assert_eq!(m.key(TAB, false, t, || mods, || true), Verdict::Pass);
        }
        // Nothing was kept of the keys that passed but the chord key's own press and release.
        assert!(m.down.is_empty());
        assert_eq!(m.released.len(), 1);
    }

    /// An account hotkey is taken over a game like a cycle key, and like it nowhere else (there
    /// RegisterHotKey delivers it).
    #[test]
    fn account_chords_are_taken_only_over_a_game() {
        let mut m = Matcher::default();
        let account = Hotkey::new(MOD_CONTROL | MOD_ALT, 0x31); // Ctrl+Alt+1
        m.set_keys(vec![(1, CYCLE), (2, CYCLE_BACK), (3, account)]);
        assert_eq!(fired(m.key(0x31, true, 100, || MOD_CONTROL | MOD_ALT, || true)), Some(3));
        assert_eq!(m.key(0x31, true, 400, || MOD_CONTROL | MOD_ALT, || true), Verdict::Swallow, "held: one switch");
        assert_eq!(m.key(0x31, false, 500, || MOD_CONTROL | MOD_ALT, || true), Verdict::Release { held: 400, repeats: 1 });
        // The game's own Ctrl+1 and Alt+1 (macros) and a bare 1 are the game's.
        for mods in [0, MOD_CONTROL, MOD_ALT, MOD_SHIFT, MOD_CONTROL | MOD_ALT | MOD_SHIFT] {
            assert_eq!(m.key(0x31, true, 1000, || mods, || true), Verdict::Pass, "{mods:#x}");
            assert_eq!(m.key(0x31, false, 1050, || mods, || true), Verdict::Pass);
        }
        // No game in front: the key goes on to Windows, which has the chord registered.
        assert_eq!(m.key(0x31, true, 2000, || MOD_CONTROL | MOD_ALT, || false), Verdict::Pass);
        assert_eq!(m.key(0x31, false, 2050, || MOD_CONTROL | MOD_ALT, || false), Verdict::Pass);
    }

    /// The hook's event for a number pad key: its name for chords, and its virtual key.
    fn pad(vk: u16, scan: u16, extended: bool) -> (u16, u16) { (crate::features::hotkeys::canonical(vk, scan, extended), vk) }

    #[test]
    fn a_number_pad_chord_fires_whatever_num_lock_says_and_only_for_the_number_pad() {
        use crate::features::hotkeys::NUM_ENTER;
        let mut m = Matcher::default();
        m.set_keys(vec![(1, Hotkey::new(CTRL, 0x60)), (2, Hotkey::new(CTRL, NUM_ENTER)), (3, Hotkey::new(0, 0x6B))]);
        let mut t = 0;
        let mut tap = |m: &mut Matcher, (key, vk): (u16, u16), mods: u8| {
            t += 100;
            let verdict = m.event(key, vk, true, t, || mods, || true);
            m.event(key, vk, false, t + 50, || mods, || true);
            verdict
        };
        // Ctrl+Num 0 with Num Lock on (VK_NUMPAD0), and off (it arrives as Insert, unextended).
        assert_eq!(fired(tap(&mut m, pad(0x60, 0x52, false), CTRL)), Some(1));
        assert_eq!(fired(tap(&mut m, pad(0x2D, 0x52, false), CTRL)), Some(1));
        // The Insert key itself, the top row's 0 and a bare Num 0 are not that chord.
        assert_eq!(tap(&mut m, pad(0x2D, 0x52, true), CTRL), Verdict::Pass);
        assert_eq!(tap(&mut m, pad(0x30, 0x0B, false), CTRL), Verdict::Pass);
        assert_eq!(tap(&mut m, pad(0x60, 0x52, false), 0), Verdict::Pass);
        // The number pad's Enter, not the main one.
        assert_eq!(fired(tap(&mut m, pad(0x0D, 0x1C, true), CTRL)), Some(2));
        assert_eq!(tap(&mut m, pad(0x0D, 0x1C, false), CTRL), Verdict::Pass);
        // A bare number pad key (an in-game chord): alone, not with Ctrl held.
        assert_eq!(fired(tap(&mut m, pad(0x6B, 0x4E, false), 0)), Some(3));
        assert_eq!(tap(&mut m, pad(0x6B, 0x4E, false), CTRL), Verdict::Pass);
        // Held: one action, and the release found under the same name with Num Lock off.
        let (key, vk) = pad(0x2D, 0x52, false);
        assert_eq!(fired(m.event(key, vk, true, 5000, || CTRL, || true)), Some(1));
        assert_eq!(m.event(key, vk, true, 5300, || CTRL, || true), Verdict::Swallow);
        assert_eq!(m.event(key, vk, false, 5400, || CTRL, || true), Verdict::Release { held: 400, repeats: 1 });
    }

    /// A chord on a plain virtual key takes the number pad key Windows reports under it too, as
    /// the same chord registered with RegisterHotKey does; a number pad chord comes first.
    #[test]
    fn a_plain_chord_matches_as_register_hot_key_would() {
        let mut m = Matcher::default();
        m.set_keys(vec![(1, Hotkey::new(CTRL | MOD_ALT, 0x2D)), (2, Hotkey::new(CTRL | MOD_ALT, 0x0D))]); // Ctrl+Alt+Insert, Ctrl+Alt+Enter
        let (key, vk) = pad(0x2D, 0x52, false);
        assert_eq!(fired(m.event(key, vk, true, 100, || CTRL | MOD_ALT, || true)), Some(1), "Num 0 with Num Lock off is Insert to Windows");
        m.event(key, vk, false, 150, || CTRL | MOD_ALT, || true);
        let (key, vk) = pad(0x0D, 0x1C, true);
        assert_eq!(fired(m.event(key, vk, true, 300, || CTRL | MOD_ALT, || true)), Some(2), "either Enter");
        m.event(key, vk, false, 350, || CTRL | MOD_ALT, || true);
        // With a chord on the number pad key itself, that one has it.
        m.set_keys(vec![(1, Hotkey::new(CTRL | MOD_ALT, 0x2D)), (3, Hotkey::new(CTRL | MOD_ALT, 0x60))]);
        let (key, vk) = pad(0x2D, 0x52, false);
        assert_eq!(fired(m.event(key, vk, true, 500, || CTRL | MOD_ALT, || true)), Some(3));
        m.event(key, vk, false, 550, || CTRL | MOD_ALT, || true);
        let (key, vk) = pad(0x2D, 0x52, true);
        assert_eq!(fired(m.event(key, vk, true, 700, || CTRL | MOD_ALT, || true)), Some(1), "the Insert key");
    }

    #[test]
    fn the_hook_is_installed_again_soon_after_an_arm_and_then_for_as_long_as_it_is_armed() {
        assert_eq!(rehook_delay(REHOOK_AFTER.len()), REHOOK_AFTER[0]);
        assert_eq!(rehook_delay(1), REHOOK_AFTER[1]);
        assert_eq!(rehook_delay(0), REHOOK_EVERY);
        assert_eq!(rehook_delay(0), REHOOK_EVERY, "and again, with no end");
    }

    #[test]
    fn nothing_is_taken_when_no_game_is_in_front() {
        let mut m = matcher();
        assert_eq!(m.key(TAB, true, 10, || CTRL, || false), Verdict::Pass, "a browser keeps its Ctrl+Tab");
        assert_eq!(m.key(TAB, false, 20, || CTRL, || false), Verdict::Pass);
        // Keys no chord uses never ask about modifiers or the foreground; nor does a release.
        assert_eq!(m.key(0x41, true, 30, || panic!("modifiers"), || panic!("foreground")), Verdict::Pass);
        assert_eq!(m.key(TAB, false, 40, || panic!("modifiers"), || panic!("foreground")), Verdict::Pass);
        // No keys (cycling off): everything passes.
        let mut off = Matcher::default();
        assert_eq!(off.key(TAB, true, 50, || CTRL, || true), Verdict::Pass);
    }

    /// The owner's burst (2026-11-10 07:18:36-38): ten switches in two seconds, around all five
    /// games twice. Their keyboard repeats after 250 ms (the shortest delay) at 30 a second or
    /// more, so any press held a little long repeats. Each switch brings another game in front,
    /// the hook is armed again for it (`set_keys`) and installed again, and the next auto-repeat
    /// used to count as a new press.
    #[test]
    fn a_held_chord_fires_once_however_often_the_hook_is_armed_again() {
        let mut m = matcher();
        let mut switches = 0;
        let mut t = 5000;
        // Pressed, then 2 s of auto-repeat at 30 a second, the hook armed again every 150 ms.
        if fired(m.key(TAB, true, t, || CTRL, || true)).is_some() { switches += 1; }
        t += 500;
        for repeat in 0..60 {
            if repeat % 5 == 0 { m.set_keys(keys()); }
            let verdict = m.key(TAB, true, t, || CTRL, || true);
            assert_eq!(verdict, Verdict::Swallow, "repeat {repeat}");
            if fired(verdict).is_some() { switches += 1; }
            t += 33;
        }
        assert_eq!(switches, 1);
        // The release is still the chord's after the last re-arm: the game never sees a Tab.
        m.set_keys(keys());
        assert_eq!(m.key(TAB, false, t, || CTRL, || true), Verdict::Release { held: t - 5000, repeats: 60 });
        // Ctrl still held, Tab tapped again: the next switch.
        assert_eq!(fired(m.key(TAB, true, t + 200, || CTRL, || true)), Some(1));
        assert_eq!(m.key(TAB, false, t + 290, || CTRL, || true), Verdict::Release { held: 90, repeats: 0 });
    }

    #[test]
    fn a_press_that_did_not_fire_never_fires_on_its_repeats() {
        let mut m = matcher();
        // Tab pressed bare (the game's target key), Ctrl added while it repeats: not a chord.
        assert_eq!(m.key(TAB, true, 100, || 0, || true), Verdict::Pass);
        assert_eq!(m.key(TAB, true, 600, || CTRL, || panic!("a repeat asks nothing")), Verdict::Pass);
        assert_eq!(m.key(TAB, false, 700, || CTRL, || true), Verdict::Pass);
        // Pressed in the instant no game was in front (a switch still completing): passed to
        // Windows, whose registered hotkey takes it once; its repeats must not fire here as well.
        assert_eq!(m.key(TAB, true, 1000, || CTRL, || false), Verdict::Pass);
        assert_eq!(m.key(TAB, true, 1500, || CTRL, || true), Verdict::Pass);
        assert_eq!(m.key(TAB, false, 1600, || CTRL, || true), Verdict::Pass);
        assert_eq!(fired(m.key(TAB, true, 1800, || CTRL, || true)), Some(1), "the next press is new");
    }

    #[test]
    fn a_fired_press_survives_the_hook_going_off_and_a_missed_release_ends_it() {
        let mut m = matcher();
        assert_eq!(fired(m.key(TAB, true, 1000, || CTRL, || true)), Some(1));
        // The hook goes off and on within the hold (Windows never saw the swallowed key down).
        m.armed(1300, |_| false);
        assert_eq!(m.key(TAB, true, 1500, || CTRL, || true), Verdict::Swallow, "still the same press");
        assert_eq!(m.key(TAB, false, 1600, || CTRL, || true), Verdict::Release { held: 600, repeats: 1 });
        // Fired, then released while the hook was off: the next press, much later, is new, and
        // says so.
        assert_eq!(fired(m.key(TAB, true, 2000, || CTRL, || true)), Some(1));
        m.armed(9000, |_| false);
        assert_eq!(m.key(TAB, true, 9500, || CTRL, || true), Verdict::Fire { id: 1, before: Before::Unreleased(7500) });
        assert_eq!(m.key(TAB, false, 9600, || CTRL, || true), Verdict::Release { held: 100, repeats: 0 });
        // A stale press's late release (another key stopped its repeats) passes and clears it.
        assert_eq!(fired(m.key(TAB, true, 20_000, || CTRL, || true)), Some(1));
        assert_eq!(m.key(TAB, false, 25_000, || CTRL, || true), Verdict::Pass);
        assert_eq!(m.key(TAB, true, 25_100, || CTRL, || true), Verdict::Fire { id: 1, before: Before::Released(100) });
    }

    #[test]
    fn a_key_already_down_when_the_hook_comes_on_is_not_a_chord() {
        let mut m = matcher();
        // Tab is down as a game comes in front (the end of an Alt+Tab, say), then Ctrl goes down:
        // its repeats pass and fire nothing; released and pressed again, it is a chord.
        m.armed(100, |vk| vk == TAB);
        assert_eq!(m.key(TAB, true, 400, || CTRL, || panic!("a repeat asks nothing")), Verdict::Pass);
        assert_eq!(m.key(TAB, false, 500, || CTRL, || true), Verdict::Pass);
        assert_eq!(m.key(TAB, true, 700, || CTRL, || true), Verdict::Fire { id: 1, before: Before::Released(200) });
        // A passed press released while the hook was off is forgotten when it comes back.
        let mut m = matcher();
        assert_eq!(m.key(TAB, true, 1000, || 0, || true), Verdict::Pass);
        m.armed(1200, |_| false);
        assert_eq!(fired(m.key(TAB, true, 1300, || CTRL, || true)), Some(1));
    }

    #[test]
    fn the_time_wraps_without_losing_a_press() {
        let mut m = matcher();
        assert_eq!(fired(m.key(TAB, true, u32::MAX - 100, || CTRL, || true)), Some(1));
        assert_eq!(m.key(TAB, true, 300, || CTRL, || true), Verdict::Swallow);
        assert_eq!(m.key(TAB, false, 400, || CTRL, || true), Verdict::Release { held: 501, repeats: 1 });
    }

    #[test]
    fn the_fired_event_survives_its_message_and_reads_well() {
        for before in [Before::First, Before::Released(0), Before::Released(412), Before::Unreleased(7500), Before::Released(Fired::GAP_MAX)] {
            for (scan, flags) in [(0x0F, 0x00), (0x4B, 0x01), (0xFF, 0xA1)] {
                let fired = Fired { scan, flags, before };
                assert_eq!(Fired::unpack(fired.pack()), fired, "{fired:?}");
            }
        }
        // A gap too long for the message is capped, and reads as "over".
        let long = Fired::unpack(Fired { scan: 0x0F, flags: 0, before: Before::Released(90_000) }.pack());
        assert_eq!(long.before, Before::Released(Fired::GAP_MAX));
        assert_eq!(long.describe(), "scan 0x0F flags 0x00, new press over 16 s after its release");
        assert_eq!(Fired { scan: 0x0F, flags: 0, before: Before::First }.describe(), "scan 0x0F flags 0x00, new press, the first seen");
        assert_eq!(Fired { scan: 0x0F, flags: 0, before: Before::Released(412) }.describe(), "scan 0x0F flags 0x00, new press 412 ms after its release");
        assert_eq!(Fired { scan: 0x0F, flags: 0x20, before: Before::Unreleased(7500) }.describe(),
            "scan 0x0F flags 0x20, press 7500 ms after one whose release was not seen");
    }

    #[test]
    fn the_thread_installs_and_removes_its_hook() {
        // The hook only ever takes a chord over a tracked game's window: this test tracks its own
        // (windowless) process, so every key passes while it is installed.
        let pids = Arc::new(Mutex::new(vec![std::process::id()]));
        let hook = KeyHook::start(0, pids).unwrap();
        assert_eq!(hook.arm(vec![(1, Hotkey::new(MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, 0x87))]), Ok(()));
        assert_eq!(hook.arm(vec![(1, Hotkey::new(MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, 0x87))]), Ok(()), "armed again");
        hook.disarm();
        drop(hook);
    }
}
