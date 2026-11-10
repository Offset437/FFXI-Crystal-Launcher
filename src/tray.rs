//! Background mode's notification-area icon (Open / Quit), on its own thread with a hidden
//! window that is never shown. The same window lets a second Crystal start bring the window
//! back when Crystal runs without one (`ask_running_instance_to_open`). Plain Win32
//! (Shell_NotifyIconW): no extra dependency, no polling — the thread sleeps in GetMessage.
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// Show the window (icon click, its menu, or a second Crystal start).
    Open,
    /// Quit Crystal (the icon's menu).
    Quit,
}

type Sender = tokio::sync::mpsc::UnboundedSender<Command>;
pub(crate) type Receiver = tokio::sync::mpsc::UnboundedReceiver<Command>;

struct Channel {
    sender: Sender,
    receiver: Mutex<Option<Receiver>>,
}

fn channel() -> &'static Channel {
    static CHANNEL: OnceLock<Channel> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        Channel { sender, receiver: Mutex::new(Some(receiver)) }
    })
}

fn send(command: Command) {
    let _ = channel().sender.send(command);
}

/// The icon's and a second start's commands, for the front end that runs the window loop:
/// handed out once (`None` after that).
pub(crate) fn take_commands() -> Option<Receiver> {
    channel().receiver.lock().ok().and_then(|mut receiver| receiver.take())
}

/// The hidden window's class; a second Crystal start finds the first one by it.
const CLASS: &str = "CrystalLauncherBackground";
/// The icon's callback.
const WM_TRAY: u32 = 0x8000 + 1; // WM_APP + 1
const WM_SHOW_ICON: u32 = 0x8000 + 2;
const WM_HIDE_ICON: u32 = 0x8000 + 3;
/// Posted by a second Crystal start: open the window.
const WM_OPEN: u32 = 0x8000 + 4;
/// The icon menu's Quit, for scripts (and the background-mode smoke check).
const WM_QUIT_REQUEST: u32 = 0x8000 + 5;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;

static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The icon is currently added.
static SHOWN: AtomicBool = AtomicBool::new(false);
/// The icon should be shown (between WM_SHOW_ICON and WM_HIDE_ICON), whether or not the
/// shell accepted it yet.
static WANTED: AtomicBool = AtomicBool::new(false);
/// Failed adds since the last WM_SHOW_ICON or TaskbarCreated.
static RETRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const RETRY_TIMER: usize = 1;
const RETRY_LIMIT: u32 = 5;
static STARTED: OnceLock<()> = OnceLock::new();

/// Start the icon thread and its hidden window (once). The icon itself appears only while
/// Crystal has no window (`set_icon(true)`).
pub(crate) fn start() {
    STARTED.get_or_init(|| {
        let (ready, wait) = std::sync::mpsc::channel();
        if std::thread::Builder::new().name("tray".into()).spawn(move || run(ready)).is_ok() {
            // The window exists before the first `set_icon` is posted to it.
            let _ = wait.recv_timeout(std::time::Duration::from_secs(2));
        }
    });
}

/// Show (`true`) or remove the notification-area icon.
pub(crate) fn set_icon(visible: bool) {
    if cfg!(test) { return; }
    if visible { start(); }
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd == 0 { return; }
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, SendMessageW};
        // Removal waits for the icon thread, so an exit right after leaves no stale icon.
        if visible { PostMessageW(hwnd as _, WM_SHOW_ICON, 0, 0); } else { SendMessageW(hwnd as _, WM_HIDE_ICON, 0, 0); }
    }
}

/// main(), once the window loop has ended: remove the icon and destroy the hidden window (its
/// thread then ends), so a Crystal started during the close cleanup waits for this one
/// (`claim_instance`) instead of handing its open to a loop that is gone. An open request
/// that reaches the window meanwhile is answered `OpenAnswer::Closing`.
pub(crate) fn shutdown() {
    if cfg!(test) { return; }
    CLOSING.store(true, Ordering::Release);
    set_icon(false);
    let hwnd = WINDOW.swap(0, Ordering::AcqRel);
    if hwnd != 0 {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(hwnd as _, windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE, 0, 0) };
    }
}

/// Set by `shutdown`: open requests are answered "closing", not opened.
static CLOSING: AtomicBool = AtomicBool::new(false);
/// WM_OPEN's answers.
const OPENING: isize = 1;
const CLOSING_ANSWER: isize = 2;

/// What a running Crystal said to a second start's open request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenAnswer {
    /// It opens (or brings forward) its window.
    Opened,
    /// Windows refused the request (a running Crystal at a higher integrity level), or its
    /// window did not answer.
    Unreachable,
    /// It is closing: wait for it to end.
    Closing,
}

/// A second Crystal start: ask the running one (which may have no window) to open its window.
/// `None` when no running Crystal's hidden window exists.
pub(crate) fn ask_running_instance_to_open() -> Option<OpenAnswer> {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowW, SendMessageTimeoutW, ASFW_ANY, SMTO_ABORTIFHUNG};
        let class = wide(CLASS);
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() { return None; }
        // This start is the foreground one: let the running Crystal's new window come forward.
        AllowSetForegroundWindow(ASFW_ANY);
        let mut answer = 0usize;
        if SendMessageTimeoutW(hwnd, WM_OPEN, 0, 0, SMTO_ABORTIFHUNG, 2000, &mut answer) == 0 {
            return Some(OpenAnswer::Unreachable);
        }
        Some(match answer as isize {
            CLOSING_ANSWER => OpenAnswer::Closing,
            _ => OpenAnswer::Opened,
        })
    }
}

use crate::win::wide;

static UI_THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// main(): the thread that runs the window's event loop.
pub(crate) fn note_ui_thread() {
    UI_THREAD.store(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, Ordering::Relaxed);
}

/// With no window left, the event loop notices a requested exit only at its next message
/// (winit waits in GetMessage): post a few empty ones so a quit from the background exits.
pub(crate) fn wake_ui_thread_soon() {
    let thread = UI_THREAD.load(Ordering::Relaxed);
    if thread == 0 || cfg!(test) { return; }
    let _ = std::thread::Builder::new().name("exit-wake".into()).spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(thread, 0, 0, 0); }
        }
    });
}

fn run(ready: std::sync::mpsc::Sender<()>) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(window_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        // A top-level window that is never shown (not a message-only one: those miss the
        // TaskbarCreated broadcast that re-adds the icon after Explorer restarts).
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), std::ptr::null(), WS_POPUP,
            0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
        if hwnd.is_null() { let _ = ready.send(()); return; }
        // Crystal runs elevated; let Explorer's (unelevated) messages through.
        for message in [WM_TRAY, WM_OPEN, taskbar_created()] {
            ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, std::ptr::null_mut());
        }
        WINDOW.store(hwnd as isize, Ordering::Release);
        let _ = ready.send(());
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn taskbar_created() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(wide("TaskbarCreated").as_ptr())
    })
}

unsafe fn notify(hwnd: windows_sys::Win32::Foundation::HWND, add: bool) -> bool {
    use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, LoadImageW, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXSMICON, SM_CYSMICON};
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    if !add { return Shell_NotifyIconW(NIM_DELETE, &data) != 0; }
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    // The exe's own icon (crystal.rc: resource 1), at the small-icon size.
    let instance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
    // Loaded fresh each time (the small-icon size follows DPI changes) and destroyed after:
    // the shell keeps its own copy.
    let icon = LoadImageW(instance, 1 as _, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR);
    data.hIcon = icon as _;
    for (slot, unit) in data.szTip.iter_mut().zip("Crystal Launcher".encode_utf16()) { *slot = unit; }
    let added = Shell_NotifyIconW(NIM_ADD, &data) != 0 || Shell_NotifyIconW(NIM_MODIFY, &data) != 0;
    if !icon.is_null() { windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(icon as _); }
    added
}

/// Show the icon, retrying from a timer while the notification area is not up yet (a
/// background start at logon can run before Explorer's taskbar exists).
unsafe fn show_icon(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};
    if notify(hwnd, true) {
        SHOWN.store(true, Ordering::Release);
        RETRIES.store(0, Ordering::Relaxed);
        KillTimer(hwnd, RETRY_TIMER);
    } else if RETRIES.fetch_add(1, Ordering::Relaxed) < RETRY_LIMIT {
        SetTimer(hwnd, RETRY_TIMER, 2000, None);
    } else {
        KillTimer(hwnd, RETRY_TIMER);
    }
}

unsafe extern "system" fn window_proc(hwnd: windows_sys::Win32::Foundation::HWND, message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM, lparam: windows_sys::Win32::Foundation::LPARAM) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    match message {
        WM_SHOW_ICON => {
            WANTED.store(true, Ordering::Release);
            RETRIES.store(0, Ordering::Relaxed);
            show_icon(hwnd);
            0
        }
        WM_HIDE_ICON => {
            SHOWN.store(false, Ordering::Release);
            KillTimer(hwnd, RETRY_TIMER);
            // A failed add can still land later: remove whenever the icon was wanted (an
            // extra delete is harmless).
            if WANTED.swap(false, Ordering::AcqRel) { notify(hwnd, false); }
            0
        }
        WM_TIMER if wparam == RETRY_TIMER => {
            if WANTED.load(Ordering::Acquire) && !SHOWN.load(Ordering::Acquire) { show_icon(hwnd); } else { KillTimer(hwnd, RETRY_TIMER); }
            0
        }
        WM_OPEN if CLOSING.load(Ordering::Acquire) => CLOSING_ANSWER,
        WM_OPEN => { send(Command::Open); OPENING }
        // `shutdown`'s WM_CLOSE: DefWindowProc destroys the window; end the thread with it.
        WM_DESTROY => {
            if WANTED.swap(false, Ordering::AcqRel) { notify(hwnd, false); }
            PostQuitMessage(0);
            0
        }
        WM_QUIT_REQUEST => { send(Command::Quit); 0 }
        WM_TRAY => {
            match (lparam & 0xffff) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(Command::Open),
                WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd),
                _ => {}
            }
            0
        }
        _ if message == taskbar_created() => {
            // Explorer (re)started: its icons are gone, or the first add came too early.
            if WANTED.load(Ordering::Acquire) {
                SHOWN.store(false, Ordering::Release);
                RETRIES.store(0, Ordering::Relaxed);
                show_icon(hwnd);
            }
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn menu(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    let menu = CreatePopupMenu();
    if menu.is_null() { return; }
    let open = wide(crate::i18n::t("Open Crystal"));
    let quit = wide(crate::i18n::t("Quit Crystal"));
    AppendMenuW(menu, MF_STRING, MENU_OPEN, open.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
    SetMenuDefaultItem(menu, MENU_OPEN as u32, 0);
    let mut at = POINT { x: 0, y: 0 };
    GetCursorPos(&mut at);
    // Without this the menu would not close when clicking elsewhere.
    SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, at.x, at.y, 0, hwnd, std::ptr::null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match chosen as usize {
        MENU_OPEN => send(Command::Open),
        MENU_QUIT => send(Command::Quit),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second start's open request is answered "closing" once `shutdown` began, and opens
    /// the window before that.
    #[test]
    fn open_requests_are_answered_closing_after_shutdown_begins() {
        assert_eq!(unsafe { window_proc(std::ptr::null_mut(), WM_OPEN, 0, 0) }, OPENING);
        CLOSING.store(true, Ordering::Release);
        assert_eq!(unsafe { window_proc(std::ptr::null_mut(), WM_OPEN, 0, 0) }, CLOSING_ANSWER);
        CLOSING.store(false, Ordering::Release);
    }
}
