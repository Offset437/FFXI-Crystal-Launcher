//! Recording a hotkey by pressing it (the Hotkeys page's fields).
//!
//! `Recorder` is the decision, kept apart from Windows so it can be tested: the next key pressed,
//! with the modifiers held, is the chord. Modifiers alone record nothing (they are shown as they
//! are held); Escape cancels; Backspace or Delete clears the hotkey; Tab leaves the field.
//!
//! `Capture` is the Windows side: a low-level keyboard hook on a thread of its own, installed
//! when a field starts recording and removed the moment it ends. Slint's own key events can't
//! do this job: they carry text, not keys, so a number pad 0 is a "0" or an Insert like any
//! other; a chord a program has registered (Crystal's own account hotkeys among them) never
//! reaches the window at all; and Win chords go to the shell. The hook sees the virtual key,
//! the scan code and the extended-key flag of every key (`hotkeys::canonical` tells the number
//! pad's keys from them), and takes the recorded key before any program or registered hotkey
//! gets it. It acts only while Crystal's window is the one in front, passes every key it is
//! not recording, and records or logs nothing but the one chord it reports. Windows calls it
//! there only while the process is not registered for raw keyboard input (`RawKeyboard`).
//!
//! The page reads its own key events as well (no number pad distinction): a key that reaches
//! the field is one the hook did not take, because it is not installed or was not called.

use super::hotkeys::{self, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use std::cell::RefCell;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};

/// The modifier keys: (virtual key, its bit in `Recorder::held`, its `MOD_*`). Left and right
/// are held apart, so letting one of two go keeps the modifier.
const MODIFIER_KEYS: [(u16, u8, u8); 8] = [
    (0xA0, 1, MOD_SHIFT), (0xA1, 2, MOD_SHIFT), (0xA2, 4, MOD_CONTROL), (0xA3, 8, MOD_CONTROL),
    (0xA4, 16, MOD_ALT), (0xA5, 32, MOD_ALT), (0x5B, 64, MOD_WIN), (0x5C, 128, MOD_WIN),
];
const WIN_KEYS: u8 = 64 | 128;

/// `vk`'s bit in `Recorder::held`, when it is a modifier key (a side-less Shift, Ctrl or Alt
/// counts as the left one).
fn modifier_bit(vk: u16) -> Option<u8> {
    let vk = match vk { 0x10 => 0xA0, 0x11 => 0xA2, 0x12 => 0xA4, other => other };
    MODIFIER_KEYS.iter().find(|(key, _, _)| *key == vk).map(|(_, bit, _)| *bit)
}

/// Keys that are never a chord's key: none at all (0, 0xFF), the mouse buttons and Break (what
/// Ctrl makes of Pause), Caps Lock and Num Lock (they change what the other keys are) and the
/// input method's keys.
fn never_a_key(vk: u16) -> bool {
    matches!(vk, 0 | 0x01..=0x06 | 0x14 | 0x15..=0x1A | 0x1C..=0x1F | 0x90 | 0xE5 | 0xE7 | 0xFF..)
}

/// One key event as the hook sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Raw {
    pub vk: u16,
    pub scan: u16,
    /// The extended-key flag.
    pub extended: bool,
}

/// How a recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The chord pressed.
    Set(Hotkey),
    /// Backspace or Delete: no hotkey.
    Clear,
    /// Escape, or Tab leaving the field: the hotkey stays as it was.
    Cancel,
}

/// What the field shows next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The modifiers held changed (`MOD_*`; 0 is none).
    Modifiers(u8),
    /// The key that ends the recording, as the hook saw it, and whether the hook took it (no
    /// program gets it, the page's field included). Sent just before its `Done`.
    Key { key: Raw, taken: bool },
    Done(Outcome),
}

/// What a recording's hook saw, for the launch history: counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    /// Key events while it was recording.
    pub keys: u32,
    /// Those that passed unread because another window was in front.
    pub away: u32,
}

/// What becomes of one key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    /// The key is the recorder's: no program gets it.
    pub swallow: bool,
    pub event: Option<Event>,
}

const PASS: Step = Step { swallow: false, event: None };

/// One recording, from the field starting it to the key that ends it.
#[derive(Debug, Default)]
pub(crate) struct Recorder {
    /// The modifier keys held (`MODIFIER_KEYS` bits).
    held: u8,
    /// The Windows keys whose press was taken: their release is too. (Passed on, a Windows key
    /// let go after a chord the shell never saw would open Start.)
    taken: u8,
    /// Keys that were down when recording began (the Return that started it): their repeats and
    /// release are not this recording's.
    before: Vec<u16>,
    /// The modifiers last reported.
    shown: u8,
    done: bool,
}

impl Recorder {
    /// `held`: the modifier keys down now (`MODIFIER_KEYS` bits); `before`: the other keys down.
    pub(crate) fn new(held: u8, before: Vec<u16>) -> Self {
        let mut recorder = Self { held, before, ..Self::default() };
        recorder.shown = recorder.modifiers();
        recorder
    }

    /// The modifiers held (`MOD_*`).
    pub(crate) fn modifiers(&self) -> u8 {
        MODIFIER_KEYS.iter().filter(|(_, bit, _)| self.held & bit != 0).fold(0, |all, (_, _, modifier)| all | modifier)
    }

    pub(crate) fn done(&self) -> bool { self.done }

    /// One key event from the hook.
    pub(crate) fn raw(&mut self, key: Raw, down: bool) -> Step {
        if self.done { return PASS; }
        if let Some(bit) = modifier_bit(key.vk) {
            let windows = bit & WIN_KEYS != 0;
            let swallow = if down {
                self.held |= bit;
                if windows { self.taken |= bit; }
                windows
            } else {
                self.held &= !bit;
                let taken = self.taken & bit != 0;
                self.taken &= !bit;
                taken
            };
            let modifiers = self.modifiers();
            return Step { swallow, event: self.show(modifiers) };
        }
        if never_a_key(key.vk) { return PASS; }
        if let Some(at) = self.before.iter().position(|vk| *vk == key.vk) {
            if !down { self.before.swap_remove(at); }
            return PASS;
        }
        if !down { return PASS; }
        self.press(self.modifiers(), hotkeys::canonical(key.vk, key.scan, key.extended))
    }

    /// The modifiers held now, from a source that reports them whole (the page's own key
    /// events).
    pub(crate) fn hold(&mut self, modifiers: u8) -> Step {
        if self.done { return PASS; }
        Step { swallow: true, event: self.show(modifiers) }
    }

    /// A key that is not a modifier went down with `modifiers` held: recording ends.
    pub(crate) fn press(&mut self, modifiers: u8, key: u16) -> Step {
        if self.done { return PASS; }
        self.done = true;
        let outcome = match (modifiers, key) {
            (0, 0x1B) => Outcome::Cancel,
            (0, 0x08 | 0x2E) => Outcome::Clear,
            // Tab and Shift+Tab move on, as in any field.
            (0 | MOD_SHIFT, 0x09) => return Step { swallow: false, event: Some(Event::Done(Outcome::Cancel)) },
            _ => Outcome::Set(Hotkey::new(modifiers, key)),
        };
        Step { swallow: true, event: Some(Event::Done(outcome)) }
    }

    fn show(&mut self, modifiers: u8) -> Option<Event> {
        (std::mem::replace(&mut self.shown, modifiers) != modifiers).then_some(Event::Modifiers(modifiers))
    }
}

/// Where the hook's thread reports (called on that thread).
pub(crate) type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// A recording's keyboard hook. Dropping it removes the hook and ends its thread; the thread
/// removes the hook itself as soon as the recording is done.
pub(crate) struct Capture {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<Seen>>,
    /// Put back when the hook is removed.
    raw: RawKeyboard,
}

/// This process's registration for raw keyboard input, taken off for as long as a recording's
/// hook is in.
///
/// The window toolkit's event loop (winit) registers for raw keyboard input when it starts,
/// for device events Crystal never reads. While a process is registered, Windows does not
/// call that process's own low-level keyboard hook for keys that go to its own window in
/// front: the hook was installed and never called with Crystal's window in front, which is
/// the only place a recording runs. (The in-game hook, `keyhook`, is not affected: the window
/// in front there is the game's.) The registration is put back as it was when the recording
/// ends.
struct RawKeyboard(Vec<windows_sys::Win32::UI::Input::RAWINPUTDEVICE>);

// SAFETY: the window handle in a registration is only handed back to Windows.
unsafe impl Send for RawKeyboard {}

impl RawKeyboard {
    /// Generic desktop page, keyboard.
    const USAGE: (u16, u16) = (1, 6);

    /// Take the registration off, remembering what it was. Nothing when there is none.
    fn take() -> Self {
        use windows_sys::Win32::UI::Input::{GetRegisteredRawInputDevices, RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_REMOVE};
        let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
        let mut count = 0u32;
        // SAFETY: a count query, then a read into a buffer of that many entries.
        let mut devices: Vec<RAWINPUTDEVICE> = unsafe {
            GetRegisteredRawInputDevices(std::ptr::null_mut(), &mut count, size);
            let mut devices = vec![std::mem::zeroed::<RAWINPUTDEVICE>(); count as usize];
            let read = GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, size);
            devices.truncate(if read == u32::MAX { 0 } else { read as usize });
            devices
        };
        devices.retain(|device| (device.usUsagePage, device.usUsage) == Self::USAGE);
        if !devices.is_empty() {
            let remove = RAWINPUTDEVICE { usUsagePage: Self::USAGE.0, usUsage: Self::USAGE.1, dwFlags: RIDEV_REMOVE, hwndTarget: std::ptr::null_mut() };
            // SAFETY: one well-formed entry. If it fails the registration stays, and so does
            // what is put back.
            unsafe { RegisterRawInputDevices(&remove, 1, size) };
        }
        Self(devices)
    }

    /// Whether a registration was taken off.
    fn taken(&self) -> bool { !self.0.is_empty() }
}

impl Drop for RawKeyboard {
    fn drop(&mut self) {
        if self.0.is_empty() { return; }
        // SAFETY: the entries Windows gave, handed back unchanged.
        unsafe { windows_sys::Win32::UI::Input::RegisterRawInputDevices(self.0.as_ptr(), self.0.len() as u32, std::mem::size_of::<windows_sys::Win32::UI::Input::RAWINPUTDEVICE>() as u32) };
    }
}

struct Running {
    recorder: Recorder,
    /// Crystal's window: keys count only while it is in front.
    window: isize,
    sink: Sink,
    seen: Seen,
}

thread_local! { static RUNNING: RefCell<Option<Running>> = const { RefCell::new(None) }; }

impl Capture {
    /// Install the hook for a recording in `window` (Crystal's own). The Windows error when it
    /// can't be installed.
    pub(crate) fn start(window: isize, sink: Sink) -> Result<Self, u32> {
        // Before the hook: Windows calls it only while this is off (`RawKeyboard`). On any
        // error below it is put back as this returns.
        let raw = RawKeyboard::take();
        let (ready, wait) = mpsc::channel();
        let thread = std::thread::Builder::new().name("features-record".into()).stack_size(64 * 1024)
            .spawn(move || run(window, sink, ready))
            .map_err(|_| 8u32)?; // ERROR_NOT_ENOUGH_MEMORY
        match wait.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(thread_id)) => Ok(Self { thread_id, thread: Some(thread), raw }),
            Ok(Err(code)) => { let _ = thread.join(); Err(code) }
            Err(_) => {
                // The thread may still install its hook: it is told to quit as soon as it says
                // who it is, instead of being left to run with the hook on.
                std::thread::spawn(move || {
                    if let Ok(Ok(thread_id)) = wait.recv() {
                        // SAFETY: a thread message to the hook's thread; fails harmlessly once it has ended.
                        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(thread_id, windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT, 0, 0) };
                    }
                    let _ = thread.join();
                });
                Err(1460) // ERROR_TIMEOUT
            }
        }
    }

    /// Remove the hook and end its thread: what it saw.
    pub(crate) fn finish(mut self) -> Seen { self.stop() }

    /// Whether this process's raw keyboard input was taken off for the hook (the launch
    /// history says so).
    pub(crate) fn raw_keyboard_off(&self) -> bool { self.raw.taken() }

    fn stop(&mut self) -> Seen {
        let Some(thread) = self.thread.take() else { return Seen::default() };
        // SAFETY: a thread message to the hook's thread; fails harmlessly once it has ended.
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(self.thread_id, windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT, 0, 0) };
        thread.join().unwrap_or_default()
    }
}

impl Drop for Capture {
    fn drop(&mut self) { self.stop(); }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: a key-state query.
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(vk)) as u16 & 0x8000 != 0 }
}

fn run(window: isize, sink: Sink, ready: mpsc::Sender<Result<u32, u32>>) -> Seen {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    // SAFETY: this thread's own queue and hook, removed before it ends.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // Create the queue before anyone posts to it.
        PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
        let held = MODIFIER_KEYS.iter().filter(|(vk, _, _)| key_down(*vk)).fold(0, |all, (_, bit, _)| all | bit);
        let before = (0x08..=0xFEu16).filter(|vk| modifier_bit(*vk).is_none() && !never_a_key(*vk) && key_down(*vk)).collect();
        let recorder = Recorder::new(held, before);
        let modifiers = recorder.modifiers();
        RUNNING.with(|r| *r.borrow_mut() = Some(Running { recorder, window, sink: sink.clone(), seen: Seen::default() }));
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), GetModuleHandleW(std::ptr::null()), 0);
        if hook.is_null() {
            let _ = ready.send(Err(windows_sys::Win32::Foundation::GetLastError()));
            RUNNING.with(|r| r.borrow_mut().take());
            return Seen::default();
        }
        if ready.send(Ok(GetCurrentThreadId())).is_ok() {
            if modifiers != 0 { sink(Event::Modifiers(modifiers)); }
            // The hook is called from inside GetMessage; it ends the loop when the recording does.
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
        }
        UnhookWindowsHookEx(hook);
        RUNNING.with(|r| r.borrow_mut().take()).map_or_else(Seen::default, |run| run.seen)
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallNextHookEx, GetForegroundWindow, PostQuitMessage, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, WM_KEYDOWN, WM_SYSKEYDOWN};
    if code == HC_ACTION as i32 && lparam != 0 {
        // SAFETY: for HC_ACTION, lParam points at the event's KBDLLHOOKSTRUCT.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let step = RUNNING.with(|r| {
            let mut running = r.try_borrow_mut().ok()?;
            let run = running.as_mut()?;
            if run.recorder.done() { return None; }
            run.seen.keys += 1;
            // SAFETY: no arguments.
            if unsafe { GetForegroundWindow() } as isize != run.window {
                run.seen.away += 1;
                return None;
            }
            let key = Raw { vk: event.vkCode as u16, scan: event.scanCode as u16, extended: event.flags & LLKHF_EXTENDED != 0 };
            Some((run.recorder.raw(key, down), run.sink.clone(), key))
        });
        if let Some((step, sink, key)) = step {
            if let Some(event) = step.event {
                if matches!(event, Event::Done(_)) { sink(Event::Key { key, taken: step.swallow }); }
                sink(event);
                // Done: the loop ends and the hook is removed before another key is looked at.
                // SAFETY: ends this thread's message loop.
                if matches!(event, Event::Done(_)) { unsafe { PostQuitMessage(0) }; }
            }
            if step.swallow { return 1; }
        }
    }
    // SAFETY: passing the event on, as every hook must.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: Raw = Raw { vk: 0xA2, scan: 0x1D, extended: false };
    const RIGHT_CTRL: Raw = Raw { vk: 0xA3, scan: 0x1D, extended: true };
    const ALT: Raw = Raw { vk: 0xA4, scan: 0x38, extended: false };
    const SHIFT: Raw = Raw { vk: 0xA0, scan: 0x2A, extended: false };
    const WIN: Raw = Raw { vk: 0x5B, scan: 0x5B, extended: true };
    const ESC: Raw = Raw { vk: 0x1B, scan: 0x01, extended: false };
    const TAB: Raw = Raw { vk: 0x09, scan: 0x0F, extended: false };
    const ENTER: Raw = Raw { vk: 0x0D, scan: 0x1C, extended: false };
    const A: Raw = Raw { vk: 0x41, scan: 0x1E, extended: false };

    fn swallowed(event: Event) -> Step { Step { swallow: true, event: Some(event) } }
    fn passed(event: Event) -> Step { Step { swallow: false, event: Some(event) } }
    fn set(text: &str) -> Step { swallowed(Event::Done(Outcome::Set(Hotkey::parse(text).unwrap()))) }
    fn recorder() -> Recorder { Recorder::new(0, Vec::new()) }

    #[test]
    fn modifiers_alone_record_nothing_and_are_shown_as_they_are_held() {
        let mut r = recorder();
        assert_eq!(r.raw(CTRL, true), passed(Event::Modifiers(MOD_CONTROL)));
        assert_eq!(r.raw(CTRL, true), PASS, "a modifier's own repeat changes nothing");
        assert_eq!(r.raw(ALT, true), passed(Event::Modifiers(MOD_CONTROL | MOD_ALT)));
        assert_eq!(r.raw(CTRL, false), passed(Event::Modifiers(MOD_ALT)));
        assert_eq!(r.raw(ALT, false), passed(Event::Modifiers(0)), "all let go: back to the prompt");
        assert!(!r.done());
        // Both Ctrl keys: letting one go keeps Ctrl.
        r.raw(CTRL, true);
        assert_eq!(r.raw(RIGHT_CTRL, true), PASS);
        assert_eq!(r.raw(CTRL, false), PASS);
        assert_eq!(r.raw(A, true), set("Ctrl+A"));
        assert!(r.done());
        // Done: every later key is somebody else's.
        assert_eq!(r.raw(A, true), PASS);
        assert_eq!(r.raw(A, false), PASS);
        assert_eq!(r.raw(RIGHT_CTRL, false), PASS);
    }

    #[test]
    fn the_next_key_with_the_modifiers_held_is_the_chord() {
        let chord = |keys: &[Raw], key: Raw| {
            let mut r = recorder();
            for modifier in keys { r.raw(*modifier, true); }
            r.raw(key, true)
        };
        assert_eq!(chord(&[], Raw { vk: 0x74, scan: 0x3F, extended: false }), set("F5"));
        assert_eq!(chord(&[CTRL, SHIFT], TAB), set("Ctrl+Shift+Tab"));
        assert_eq!(chord(&[CTRL, ALT, SHIFT, WIN], Raw { vk: 0x87, scan: 0x76, extended: false }), set("Ctrl+Alt+Shift+Win+F24"));
        assert_eq!(chord(&[ALT], Raw { vk: 0x21, scan: 0x49, extended: true }), set("Alt+PageUp"));
        assert_eq!(chord(&[CTRL], Raw { vk: 0xBA, scan: 0x27, extended: false }), set("Ctrl+;"));
        assert_eq!(chord(&[ALT], Raw { vk: 0x13, scan: 0x45, extended: false }), set("Alt+Pause"));
        // A key with no name of its own is still a key.
        assert_eq!(chord(&[CTRL], Raw { vk: 0xAD, scan: 0, extended: true }), set("Ctrl+0xAD"));
        // A side-less modifier (sent by software) counts.
        assert_eq!(chord(&[Raw { vk: 0x11, scan: 0, extended: false }], A), set("Ctrl+A"));
        // Modifiers held before the field was clicked count too.
        let mut held = Recorder::new(4 | 16, Vec::new());
        assert_eq!(held.modifiers(), MOD_CONTROL | MOD_ALT);
        assert_eq!(held.raw(A, true), set("Ctrl+Alt+A"));
    }

    #[test]
    fn the_number_pad_is_told_from_the_digits_the_arrows_and_the_main_enter() {
        let key = |raw: Raw| { let mut r = recorder(); r.raw(CTRL, true); r.raw(raw, true) };
        // Num Lock on: VK_NUMPAD0. Off: the same key arrives as Insert, without the extended flag.
        assert_eq!(key(Raw { vk: 0x60, scan: 0x52, extended: false }), set("Ctrl+Num0"));
        assert_eq!(key(Raw { vk: 0x2D, scan: 0x52, extended: false }), set("Ctrl+Num0"));
        // The Insert key itself carries the flag; the top row's 0 is its own key.
        assert_eq!(key(Raw { vk: 0x2D, scan: 0x52, extended: true }), set("Ctrl+Insert"));
        assert_eq!(key(Raw { vk: 0x30, scan: 0x0B, extended: false }), set("Ctrl+0"));
        // 4 and the left arrow; 5 (Clear with Num Lock off); the decimal point and Delete.
        assert_eq!(key(Raw { vk: 0x25, scan: 0x4B, extended: false }), set("Ctrl+Num4"));
        assert_eq!(key(Raw { vk: 0x25, scan: 0x4B, extended: true }), set("Ctrl+Left"));
        assert_eq!(key(Raw { vk: 0x0C, scan: 0x4C, extended: false }), set("Ctrl+Num5"));
        assert_eq!(key(Raw { vk: 0x2E, scan: 0x53, extended: false }), set("Ctrl+Num."));
        assert_eq!(key(Raw { vk: 0x6E, scan: 0x53, extended: false }), set("Ctrl+Num."));
        // The operators have codes of their own; Enter is told by the flag.
        assert_eq!(key(Raw { vk: 0x6F, scan: 0x35, extended: true }), set("Ctrl+Num/"));
        assert_eq!(key(Raw { vk: 0x6A, scan: 0x37, extended: false }), set("Ctrl+Num*"));
        assert_eq!(key(Raw { vk: 0x6D, scan: 0x4A, extended: false }), set("Ctrl+Num-"));
        assert_eq!(key(Raw { vk: 0x6B, scan: 0x4E, extended: false }), set("Ctrl+Num+"));
        assert_eq!(key(Raw { vk: 0x0D, scan: 0x1C, extended: true }), set("Ctrl+NumEnter"));
        assert_eq!(key(ENTER), set("Ctrl+Enter"));
        // Bare number pad keys are chords too (allowed in game only: `Hotkey::problem`).
        assert_eq!(recorder().raw(Raw { vk: 0x60, scan: 0x52, extended: false }, true), set("Num0"));
        // A software key with no scan code is taken at its word.
        assert_eq!(key(Raw { vk: 0x25, scan: 0, extended: false }), set("Ctrl+Left"));
    }

    #[test]
    fn escape_cancels_backspace_and_delete_clear_and_tab_moves_on() {
        assert_eq!(recorder().raw(ESC, true), swallowed(Event::Done(Outcome::Cancel)));
        assert_eq!(recorder().raw(Raw { vk: 0x08, scan: 0x0E, extended: false }, true), swallowed(Event::Done(Outcome::Clear)));
        assert_eq!(recorder().raw(Raw { vk: 0x2E, scan: 0x53, extended: true }, true), swallowed(Event::Done(Outcome::Clear)));
        // Tab is not taken: the focus moves on, and the hotkey stays as it was.
        assert_eq!(recorder().raw(TAB, true), passed(Event::Done(Outcome::Cancel)));
        let mut r = recorder();
        r.raw(SHIFT, true);
        assert_eq!(r.raw(TAB, true), passed(Event::Done(Outcome::Cancel)));
        // With Ctrl, Alt or Win they are chords like any other.
        let with_ctrl = |key: Raw| { let mut r = recorder(); r.raw(CTRL, true); r.raw(key, true) };
        assert_eq!(with_ctrl(ESC), set("Ctrl+Esc"));
        assert_eq!(with_ctrl(Raw { vk: 0x08, scan: 0x0E, extended: false }), set("Ctrl+Backspace"));
        assert_eq!(with_ctrl(Raw { vk: 0x2E, scan: 0x53, extended: true }), set("Ctrl+Delete"));
        assert_eq!(with_ctrl(TAB), set("Ctrl+Tab"));
    }

    #[test]
    fn keys_down_when_recording_began_and_keys_that_are_no_key_pass() {
        // Return started the recording and is still down: its repeats and release pass, and
        // record nothing; pressed again, it is a key.
        let mut r = Recorder::new(0, vec![0x0D]);
        assert_eq!(r.raw(ENTER, true), PASS);
        assert_eq!(r.raw(ENTER, false), PASS);
        assert!(!r.done());
        r.raw(CTRL, true);
        assert_eq!(r.raw(ENTER, true), set("Ctrl+Enter"));
        // Releases, the lock keys, the input method's keys and no key at all pass.
        let mut r = recorder();
        for vk in [0x14, 0x90, 0xE5, 0xE7, 0xFF, 0x00, 0x01] {
            assert_eq!(r.raw(Raw { vk, scan: 0, extended: false }, true), PASS, "{vk:#x}");
        }
        assert_eq!(r.raw(A, false), PASS);
        assert!(!r.done());
    }

    #[test]
    fn the_windows_key_is_taken_so_start_does_not_open() {
        let mut r = recorder();
        assert_eq!(r.raw(WIN, true), swallowed(Event::Modifiers(MOD_WIN)));
        assert_eq!(r.raw(WIN, false), swallowed(Event::Modifiers(0)), "its release too: the shell saw neither");
        // One that was down before recording began is the shell's: its release passes.
        let mut r = Recorder::new(64, Vec::new());
        assert_eq!(r.raw(WIN, false), passed(Event::Modifiers(0)));
        // Ctrl, Alt and Shift pass (programs must not be left thinking they are held).
        let mut r = recorder();
        assert!(!r.raw(CTRL, true).swallow && !r.raw(ALT, true).swallow && !r.raw(SHIFT, true).swallow);
        r.raw(WIN, true);
        assert_eq!(r.raw(A, true), set("Ctrl+Alt+Shift+Win+A"));
    }

    #[test]
    fn modifiers_reported_whole_work_the_same_way() {
        let mut r = recorder();
        assert_eq!(r.hold(MOD_CONTROL), swallowed(Event::Modifiers(MOD_CONTROL)));
        assert_eq!(r.hold(MOD_CONTROL), Step { swallow: true, event: None });
        assert_eq!(r.hold(0), swallowed(Event::Modifiers(0)));
        assert_eq!(r.press(MOD_CONTROL | MOD_ALT, 0x70), set("Ctrl+Alt+F1"));
        assert_eq!(r.press(0, 0x41), PASS, "one key ends it");
        assert_eq!(r.hold(MOD_ALT), PASS);
        assert_eq!(recorder().press(0, 0x1B), swallowed(Event::Done(Outcome::Cancel)));
        assert_eq!(recorder().press(0, 0x08), swallowed(Event::Done(Outcome::Clear)));
        assert_eq!(recorder().press(MOD_SHIFT, 0x09), passed(Event::Done(Outcome::Cancel)));
    }

    /// The tests that install a hook: one at a time (the keyboard's hooks and the raw input
    /// registration are the whole process's).
    fn one_hook() -> std::sync::MutexGuard<'static, ()> {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ONE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This process's raw keyboard registration, as Windows lists it: (flags, target window).
    fn raw_keyboard() -> Option<(u32, isize)> {
        use windows_sys::Win32::UI::Input::{GetRegisteredRawInputDevices, RAWINPUTDEVICE};
        let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
        let mut count = 0u32;
        // SAFETY: a count query, then a read into a buffer of that many entries.
        unsafe {
            GetRegisteredRawInputDevices(std::ptr::null_mut(), &mut count, size);
            let mut devices = vec![std::mem::zeroed::<RAWINPUTDEVICE>(); count as usize];
            let read = GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, size);
            devices.truncate(if read == u32::MAX { 0 } else { read as usize });
            devices.iter().find(|d| (d.usUsagePage, d.usUsage) == RawKeyboard::USAGE).map(|d| (d.dwFlags, d.hwndTarget as isize))
        }
    }

    /// The cause of "recording does nothing" (2026-11-11): the window toolkit registers the
    /// process for raw keyboard input, and Windows then never calls the process's own hook
    /// while its own window is in front. The registration is off for as long as a hook is in,
    /// and back exactly as it was after.
    #[test]
    fn raw_keyboard_input_is_off_while_the_hook_is_in_and_back_as_it_was_after() {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_DEVNOTIFY, RIDEV_REMOVE};
        use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};
        let _one = one_hook();
        let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
        // None registered (a test process has no window toolkit): nothing is taken off.
        assert_eq!(raw_keyboard(), None);
        let capture = Capture::start(0, Arc::new(|_| {})).unwrap();
        assert!(!capture.raw_keyboard_off());
        drop(capture);
        assert_eq!(raw_keyboard(), None);
        // Registered as winit registers its event loop: a message-only window, hotplug notices.
        // SAFETY: a window of this thread's, registered and removed here.
        unsafe {
            let class: Vec<u16> = "Static\0".encode_utf16().collect();
            let window = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null());
            assert!(!window.is_null());
            let device = RAWINPUTDEVICE { usUsagePage: 1, usUsage: 6, dwFlags: RIDEV_DEVNOTIFY, hwndTarget: window };
            assert_ne!(RegisterRawInputDevices(&device, 1, size), 0);
            let before = raw_keyboard();
            assert_eq!(before.map(|(_, target)| target), Some(window as isize));
            let capture = Capture::start(0, Arc::new(|_| {})).unwrap();
            assert!(capture.raw_keyboard_off());
            assert_eq!(raw_keyboard(), None, "off while the hook is in");
            capture.finish();
            assert_eq!(raw_keyboard(), before, "back as it was");
            let remove = RAWINPUTDEVICE { usUsagePage: 1, usUsage: 6, dwFlags: RIDEV_REMOVE, hwndTarget: std::ptr::null_mut() };
            RegisterRawInputDevices(&remove, 1, size);
            DestroyWindow(window);
        }
        assert_eq!(raw_keyboard(), None);
    }

    #[test]
    fn the_thread_installs_its_hook_and_takes_nothing_outside_its_window() {
        let _one = one_hook();
        // No window is this recording's (0 is never in front): every key passes while the hook
        // is installed, and nothing is reported but the modifiers held as it started.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let capture = Capture::start(0, Arc::new(move |event| sink_seen.lock().unwrap().push(event))).unwrap();
        drop(capture);
        assert!(seen.lock().unwrap().iter().all(|event| matches!(event, Event::Modifiers(_))));
    }

    /// Windows calls the recording's hook for a key event. The event is this test's own (a
    /// release of F24, marked): a second hook installed first (so called last) takes it, so no
    /// window ever gets it; with no window the recording's (0), the recording passes every key,
    /// the user's included, and only counts them.
    #[test]
    fn windows_calls_the_hook_for_a_key_event() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP};
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        const MARK: usize = 0x4352_5954;
        static TAKEN: AtomicU32 = AtomicU32::new(0);
        let _one = one_hook();
        unsafe extern "system" fn guard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            // SAFETY: for HC_ACTION, lParam points at the event's KBDLLHOOKSTRUCT.
            if code == HC_ACTION as i32 && lparam != 0 && unsafe { (*(lparam as *const KBDLLHOOKSTRUCT)).dwExtraInfo } == MARK {
                TAKEN.fetch_add(1, Ordering::AcqRel);
                return 1;
            }
            // SAFETY: passing the event on.
            unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
        }
        // SAFETY: this thread's own hook and queue; the hook is removed before the test ends.
        unsafe {
            let guard_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(guard), GetModuleHandleW(std::ptr::null()), 0);
            assert!(!guard_hook.is_null());
            let capture = Capture::start(0, Arc::new(|_| {})).unwrap();
            let input = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0x87, wScan: 0, dwFlags: KEYEVENTF_KEYUP, time: 0, dwExtraInfo: MARK } } };
            SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
            // The guard is called from inside this thread's message calls.
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut msg: MSG = std::mem::zeroed();
            while TAKEN.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
                PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE);
                std::thread::sleep(Duration::from_millis(5));
            }
            let seen = capture.finish();
            UnhookWindowsHookEx(guard_hook);
            // No desktop to send keys to (a locked session), or a window in front that Windows
            // lets this process send nothing to: nothing to check.
            if TAKEN.load(Ordering::Acquire) == 0 { eprintln!("skipped: the key event could not be sent"); return; }
            assert!(seen.keys >= 1, "the hook was installed but never called: {seen:?}");
            assert_eq!(seen.away, seen.keys, "no window was the recording's");
        }
    }

    /// The hook end to end: an F24 sent to the desktop while the window in front is this
    /// recording's is taken (the window never gets it) and reported as the chord. Run by hand:
    /// it sends a key.
    #[test]
    #[ignore = "sends an F24 key press to the desktop"]
    fn a_key_pressed_over_the_window_is_taken_and_reported() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP};
        let _one = one_hook();
        // SAFETY: no arguments.
        let front = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } as isize;
        if front == 0 { return; }
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let capture = Capture::start(front, Arc::new(move |event| { let _ = tx.lock().unwrap().send(event); })).unwrap();
        let input = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0x87, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
        let tap = [input(0), input(KEYEVENTF_KEYUP)];
        // SAFETY: a sized array of keyboard inputs.
        unsafe { SendInput(tap.len() as u32, tap.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        let done = loop {
            match rx.recv_timeout(Duration::from_secs(3)).expect("the key reaches the hook") {
                Event::Modifiers(_) | Event::Key { .. } => continue,
                Event::Done(outcome) => break outcome,
            }
        };
        assert!(matches!(done, Outcome::Set(hotkey) if hotkey.key == 0x87), "{done:?}");
        drop(capture);
    }
}
