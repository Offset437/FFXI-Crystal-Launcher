//! Game windows, found by their process (never by title), the monitors they sit on, and the few
//! window operations the features need: read and set a placement, and bring a window forward.
//!
//! Coordinates are physical pixels: every call that reads or moves a window runs under a
//! per-monitor-v2 DPI context (`PhysicalDpi`), so a placement saved on a 150% monitor means the
//! same thing when read back, whatever the game's own DPI awareness is.

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};

/// A rectangle in physical screen pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub(crate) const fn new(x: i32, y: i32, w: i32, h: i32) -> Self { Self { x, y, w, h } }
    pub(crate) fn right(&self) -> i32 { self.x + self.w }
    pub(crate) fn bottom(&self) -> i32 { self.y + self.h }
    pub(crate) fn area(&self) -> i64 { i64::from(self.w.max(0)) * i64::from(self.h.max(0)) }
    pub(crate) fn intersection(&self, other: &Rect) -> Option<Rect> {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        let (r, b) = (self.right().min(other.right()), self.bottom().min(other.bottom()));
        (r > x && b > y).then(|| Rect::new(x, y, r - x, b - y))
    }
    pub(crate) fn contains(&self, x: i32, y: i32) -> bool { x >= self.x && x < self.right() && y >= self.y && y < self.bottom() }
    fn from_win(r: &RECT) -> Self { Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top) }
}

/// One display, as Windows reports it now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Monitor {
    /// The GDI device name (`\\.\DISPLAY1`): stable while the display stays connected.
    pub device: String,
    pub rect: Rect,
    /// The rectangle minus the taskbar and docked bars.
    pub work: Rect,
    /// Effective DPI (96 = 100%).
    pub dpi: u32,
    pub primary: bool,
}

/// How a window is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Shown {
    #[default]
    Normal,
    Minimized,
    Maximized,
}

/// The game's window mode as its window shows it (FFXI's `0034`: 1 window, 3 borderless; 0
/// fullscreen is a borderless window that covers its monitor while it has the display).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Mode {
    #[default]
    Windowed,
    Borderless,
    /// Borderless and exactly covering its monitor: Crystal moves it between monitors at most.
    Fullscreen,
}

/// A window's frame: the style bits FFXI's window modes differ in (`0034` 1 windowed:
/// WS_OVERLAPPEDWINDOW with client and window edges; 2 and 3 borderless: WS_POPUP). The other bits
/// (visible, minimised, clipping) are the window's own state: never read into this, never written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Frame {
    pub style: u32,
    pub ex_style: u32,
}

impl Frame {
    /// WS_POPUP, WS_CAPTION (border and dialog frame), WS_THICKFRAME, WS_SYSMENU, the minimise and
    /// maximise boxes.
    const STYLE: u32 = 0x8000_0000 | 0x00C0_0000 | 0x0004_0000 | 0x0008_0000 | 0x0002_0000 | 0x0001_0000;
    /// WS_EX_DLGMODALFRAME, WS_EX_WINDOWEDGE, WS_EX_CLIENTEDGE, WS_EX_STATICEDGE.
    const EX_STYLE: u32 = 0x0000_0001 | 0x0000_0100 | 0x0000_0200 | 0x0002_0000;

    pub(crate) fn from_styles(style: u32, ex_style: u32) -> Self {
        Self { style: style & Self::STYLE, ex_style: ex_style & Self::EX_STYLE }
    }

    /// `style` and `ex_style` with their frame bits replaced by this frame's.
    fn applied_to(self, style: u32, ex_style: u32) -> (u32, u32) {
        ((style & !Self::STYLE) | self.style, (ex_style & !Self::EX_STYLE) | self.ex_style)
    }
}

/// What `capture` reads from a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    /// The window's own rectangle (frame included) while shown normally.
    pub rect: Rect,
    /// Its client (render) size.
    pub client: (i32, i32),
    pub shown: Shown,
    pub mode: Mode,
    pub frame: Frame,
    pub monitor: Monitor,
}

/// Whether a game placed like `placement` may hold its monitor's display, so a resize would
/// fight the game: `Mode::Fullscreen` only says the window is borderless and exactly covers its
/// monitor, which is also FFXI's borderless mode (`0034` = 3) at the monitor's resolution. A
/// fullscreen game gives the display up and minimises when another window comes in front, so one
/// that covers its monitor while shown behind another window is a borderless window and can be
/// arranged. In front, or minimised, it can't be told apart and is left alone.
pub(crate) fn holds_display(placement: &Placement, in_front: bool) -> bool {
    placement.mode == Mode::Fullscreen && (in_front || placement.shown == Shown::Minimized)
}

/// The current thread runs per-monitor-v2 DPI aware until this is dropped.
pub(crate) struct PhysicalDpi(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
impl PhysicalDpi {
    pub(crate) fn enter() -> Self {
        use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        // SAFETY: a documented constant context; the previous one is restored on drop.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
impl Drop for PhysicalDpi {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: restores the context the thread had before `enter`.
            unsafe { windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

/// Every monitor now, primary first.
pub(crate) fn monitors() -> Vec<Monitor> {
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    unsafe extern "system" fn each(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
        // SAFETY: `data` is the Vec passed below, alive for the enumeration.
        let list = unsafe { &mut *(data as *mut Vec<Monitor>) };
        if let Some(info) = monitor_info(monitor) { list.push(info); }
        1
    }
    let _dpi = PhysicalDpi::enter();
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only touches `list`, which outlives the call.
    unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut list as *mut _ as LPARAM) };
    list.sort_by_key(|m| (!m.primary, m.rect.x, m.rect.y));
    list
}

fn monitor_info(monitor: windows_sys::Win32::Graphics::Gdi::HMONITOR) -> Option<Monitor> {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFOEXW};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    // SAFETY: a zeroed MONITORINFOEXW with its size set, as the API requires.
    unsafe {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info as *mut _ as _) == 0 { return None; }
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) != 0 { x = 96; }
        let end = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
        Some(Monitor {
            device: String::from_utf16_lossy(&info.szDevice[..end]),
            rect: Rect::from_win(&info.monitorInfo.rcMonitor),
            work: Rect::from_win(&info.monitorInfo.rcWork),
            dpi: x,
            primary: info.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
        })
    }
}

/// The monitor `hwnd` is mostly on (the nearest when off-screen).
fn monitor_of(hwnd: HWND) -> Option<Monitor> {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    // SAFETY: MonitorFromWindow accepts any handle and returns a monitor or null.
    monitor_info(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
}

/// The process that owns `hwnd`, and its thread.
pub(crate) fn window_process(hwnd: isize) -> Option<(u32, u32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    // SAFETY: an out value for any window handle; 0 when the window is gone.
    let thread = unsafe { GetWindowThreadProcessId(hwnd as HWND, &mut pid) };
    (thread != 0).then_some((pid, thread))
}

/// Whether `hwnd` could be a game's main window: visible, unowned, not a tool window, and of a
/// real size (FFXI's window, or the borderless one Windower and dgVoodoo leave it).
fn main_window_candidate(hwnd: HWND) -> Option<i64> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindow, GetWindowLongW, GetWindowRect, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW};
    // SAFETY: read-only queries on a window handle from EnumWindows.
    unsafe {
        if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() { return None; }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 { return None; }
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 { return None; }
        let r = Rect::from_win(&rect);
        (r.w >= 160 && r.h >= 120).then(|| r.area())
    }
}

/// The main window of the game running in one of `pids` (a host and its children): the largest
/// visible, unowned top-level window they own. A minimised window counts by its normal size.
pub(crate) fn find_main_window(pids: &[u32]) -> Option<isize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, IsIconic};
    struct Search<'a> { pids: &'a [u32], best: Option<(i64, isize)> }
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> i32 {
        // SAFETY: `data` is the Search below, alive for the enumeration.
        let search = unsafe { &mut *(data as *mut Search) };
        let Some((pid, _)) = window_process(hwnd as isize) else { return 1 };
        if !search.pids.contains(&pid) { return 1; }
        // SAFETY: read-only query.
        let area = if unsafe { IsIconic(hwnd) } != 0 { normal_rect(hwnd).map(|r| r.area()) } else { main_window_candidate(hwnd) };
        if let Some(area) = area.filter(|a| *a > 0) {
            if search.best.is_none_or(|(best, _)| area > best) { search.best = Some((area, hwnd as isize)); }
        }
        1
    }
    if pids.is_empty() { return None; }
    let _dpi = PhysicalDpi::enter();
    let mut search = Search { pids, best: None };
    // SAFETY: the callback only touches `search`, which outlives the call.
    unsafe { EnumWindows(Some(each), &mut search as *mut _ as LPARAM) };
    search.best.map(|(_, hwnd)| hwnd)
}

/// `pid` and its direct children: Ashita and Windower start the game from the host they launch.
pub(crate) fn process_family(pid: u32) -> Vec<u32> {
    let mut family = vec![pid];
    if let Some(list) = crate::win::process::processes() {
        family.extend(list.iter().filter(|e| e.parent == pid && e.pid != pid).map(|e| e.pid));
    }
    family
}

/// A window's normal (restored) rectangle in screen coordinates, also while it is minimised or
/// maximised. `GetWindowPlacement` reports workspace coordinates: they are screen coordinates
/// shifted by the work area's offset on the window's monitor.
fn normal_rect(hwnd: HWND) -> Option<Rect> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GetWindowPlacement, GWL_EXSTYLE, WINDOWPLACEMENT, WS_EX_TOOLWINDOW};
    // SAFETY: a sized WINDOWPLACEMENT for a window handle.
    unsafe {
        let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut placement) == 0 { return None; }
        let mut rect = Rect::from_win(&placement.rcNormalPosition);
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW == 0 {
            if let Some(monitor) = monitor_of(hwnd) {
                rect.x += monitor.work.x - monitor.rect.x;
                rect.y += monitor.work.y - monitor.rect.y;
            }
        }
        Some(rect)
    }
}

/// The window's placement now. `None` when it is gone.
pub(crate) fn capture(hwnd: isize) -> Option<Placement> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowLongW, GetWindowRect, IsIconic, IsWindow, IsZoomed, GWL_EXSTYLE, GWL_STYLE, WS_CAPTION};
    let _dpi = PhysicalDpi::enter();
    let hwnd = hwnd as HWND;
    // SAFETY: read-only queries on a window handle that is checked to exist first.
    unsafe {
        if IsWindow(hwnd) == 0 { return None; }
        let shown = if IsIconic(hwnd) != 0 { Shown::Minimized } else if IsZoomed(hwnd) != 0 { Shown::Maximized } else { Shown::Normal };
        let rect = if shown == Shown::Normal {
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut r) == 0 { return None; }
            Rect::from_win(&r)
        } else { normal_rect(hwnd)? };
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let monitor = monitor_of(hwnd)?;
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let captioned = style & WS_CAPTION == WS_CAPTION;
        let mode = if captioned { Mode::Windowed } else if rect == monitor.rect { Mode::Fullscreen } else { Mode::Borderless };
        let frame = Frame::from_styles(style, GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
        Some(Placement { rect, client: (client.right - client.left, client.bottom - client.top), shown, mode, frame, monitor })
    }
}

/// Move and size `hwnd` to `rect` and show it as `shown`, without activating it. A window
/// already at `rect` is left alone, so a restore that repeats changes nothing.
pub(crate) fn apply(hwnd: isize, rect: Rect, shown: Shown) -> bool { apply_frame(hwnd, rect, shown, None) }

/// `apply`, also putting back `frame` when the window's frame differs from it (the styles are
/// set first and the move carries SWP_FRAMECHANGED, so the new frame takes effect with it).
pub(crate) fn apply_frame(hwnd: isize, rect: Rect, shown: Shown, frame: Option<Frame>) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, GetWindowRect, IsIconic, IsWindow, IsZoomed, SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWNOACTIVATE,
    };
    let _dpi = PhysicalDpi::enter();
    let hwnd = hwnd as HWND;
    // SAFETY: window calls on a handle checked to exist; the window may vanish meanwhile, which
    // makes the calls fail harmlessly.
    unsafe {
        if IsWindow(hwnd) == 0 { return false; }
        if IsIconic(hwnd) != 0 || IsZoomed(hwnd) != 0 { ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
        let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        if let Some(frame) = frame {
            let (style, ex_style) = (GetWindowLongW(hwnd, GWL_STYLE) as u32, GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
            let (want_style, want_ex) = frame.applied_to(style, ex_style);
            if (want_style, want_ex) != (style, ex_style) {
                SetWindowLongW(hwnd, GWL_STYLE, want_style as i32);
                SetWindowLongW(hwnd, GWL_EXSTYLE, want_ex as i32);
                flags |= SWP_FRAMECHANGED;
            }
        }
        let mut now: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut now);
        let moved = (Rect::from_win(&now) == rect && flags & SWP_FRAMECHANGED == 0)
            || SetWindowPos(hwnd, std::ptr::null_mut(), rect.x, rect.y, rect.w, rect.h, flags) != 0;
        match shown {
            Shown::Maximized => { ShowWindow(hwnd, SW_MAXIMIZE); }
            Shown::Minimized => { ShowWindow(hwnd, SW_MINIMIZE); }
            Shown::Normal => {}
        }
        moved
    }
}

/// The foreground window and its process.
pub(crate) fn foreground() -> Option<(isize, u32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    // SAFETY: no arguments; null when no window is in the foreground.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() { return None; }
    Some((hwnd as isize, window_process(hwnd as isize)?.0))
}

/// How long a foreground change may take to complete before it counts as refused.
pub(crate) const ACTIVATION_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether `hwnd` is the foreground window within `limit`. Giving another thread's window the
/// foreground completes when that thread handles its activation (a game, between frames); until
/// then the foreground reads as the old window or none, so a switch checked at once looks refused.
pub(crate) fn wait_foreground(hwnd: isize, limit: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if foreground().is_some_and(|(now, _)| now == hwnd) { return true; }
        if start.elapsed() >= limit || !exists(hwnd) { return false; }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

pub(crate) fn is_minimized(hwnd: isize) -> bool {
    // SAFETY: read-only query on any handle.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(hwnd as HWND) != 0 }
}

pub(crate) fn exists(hwnd: isize) -> bool {
    // SAFETY: read-only query on any handle.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd as HWND) != 0 }
}

/// The window's title, at most 256 characters (a name hint only; windows are never found by it).
pub(crate) fn title(hwnd: isize) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buffer = [0u16; 256];
    // SAFETY: a buffer of the length passed.
    let n = unsafe { GetWindowTextW(hwnd as HWND, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..n.max(0) as usize])
}

/// How one attempt to give a window the foreground was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// SetForegroundWindow on its own: allowed for the thread that received the hotkey, or while
    /// Crystal is in front.
    Direct,
    /// Attached to the input of the thread in front, BringWindowToTop and SetForegroundWindow,
    /// then detached (Windows then treats the call as that thread's own).
    Attached,
    /// AttachThreadInput itself failed; nothing was tried.
    AttachRefused,
    /// SwitchToThisWindow, what Alt+Tab itself uses (the last resort, never sends a key).
    Switch,
}

impl Method {
    fn name(self) -> &'static str {
        match self { Self::Direct => "set", Self::Attached => "attach+set", Self::AttachRefused => "attach refused", Self::Switch => "switch" }
    }
}

/// One attempt and how it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub method: Method,
    /// What the call returned (SetForegroundWindow's BOOL; true for SwitchToThisWindow, which
    /// returns nothing).
    pub returned: bool,
    /// GetLastError right after the call (0 when Windows set none).
    pub error: u32,
    /// Whether the window was the foreground within `ACTIVATION_SETTLE` (GetForegroundWindow).
    pub front: bool,
}

/// What `activate` did, for the launch history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Activation {
    /// It was minimised and was restored first (SW_RESTORE).
    pub restored: bool,
    /// It was in front already: nothing was called.
    pub already: bool,
    /// It (or the window in front, for the attach) is not responding: nothing was called.
    pub hung: bool,
    pub steps: Vec<Step>,
}

impl Activation {
    pub(crate) fn ok(&self) -> bool { self.already || self.steps.iter().any(|s| s.front) }

    /// `restore; set ret=0 err=0 front=no; attach+set ret=1 err=0 front=yes`.
    pub(crate) fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.restored { parts.push("restore".into()); }
        if self.already { parts.push("already in front".into()); }
        if self.hung { parts.push("not responding".into()); }
        for s in &self.steps {
            parts.push(format!("{} ret={} err={} front={}", s.method.name(), u8::from(s.returned), s.error, if s.front { "yes" } else { "no" }));
        }
        if parts.is_empty() { "nothing tried".into() } else { parts.join("; ") }
    }
}

/// Give `hwnd` the foreground, in order: restore it when minimised (`restore`); SetForegroundWindow
/// (allowed at once on the thread that received a hotkey); when refused, attach to the input of
/// the thread in front for the call, with BringWindowToTop; and when `last_resort`, finally
/// SwitchToThisWindow. No key is ever sent: a synthetic Alt would reach the game. Each step waits
/// for a switch still completing (`wait_foreground`) before the next is tried. A refused
/// SetForegroundWindow may flash the taskbar button, so the window-ordering pass stops at the
/// first refusal instead of using the last resort.
pub(crate) fn activate(hwnd: isize, restore: bool, last_resort: bool) -> Activation {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, IsHungAppWindow, IsIconic, SetForegroundWindow, ShowWindow, SwitchToThisWindow, SW_RESTORE,
    };
    let target = hwnd as HWND;
    let in_front = || foreground().is_some_and(|(now, _)| now == hwnd);
    let mut out = Activation::default();
    // SAFETY: window and input-state calls; the attach is always undone before returning.
    unsafe {
        if !exists(hwnd) { return out; } // closed meanwhile
        if IsHungAppWindow(target) != 0 { out.hung = true; return out; }
        if restore && IsIconic(target) != 0 {
            ShowWindow(target, SW_RESTORE);
            out.restored = true;
        }
        if in_front() { out.already = true; return out; }
        // 1. Directly.
        SetLastError(0);
        let returned = SetForegroundWindow(target) != 0;
        let error = GetLastError();
        let front = if returned { wait_foreground(hwnd, ACTIVATION_SETTLE) } else { in_front() };
        out.steps.push(Step { method: Method::Direct, returned, error, front });
        if front { return out; }
        // 2. As the thread in front.
        let current = GetCurrentThreadId();
        let front_window = GetForegroundWindow();
        let thread = (!front_window.is_null()).then(|| window_process(front_window as isize)).flatten().map(|(_, t)| t).filter(|t| *t != current);
        if let Some(thread) = thread {
            if IsHungAppWindow(front_window) != 0 { out.hung = true; return out; }
            SetLastError(0);
            if AttachThreadInput(current, thread, 1) != 0 {
                BringWindowToTop(target);
                SetLastError(0);
                let returned = SetForegroundWindow(target) != 0;
                let error = GetLastError();
                AttachThreadInput(current, thread, 0);
                let front = if returned { wait_foreground(hwnd, ACTIVATION_SETTLE) } else { in_front() };
                out.steps.push(Step { method: Method::Attached, returned, error, front });
                if front { return out; }
            } else {
                out.steps.push(Step { method: Method::AttachRefused, returned: false, error: GetLastError(), front: false });
            }
        }
        // 3. Alt+Tab's own switch.
        if last_resort {
            SetLastError(0);
            SwitchToThisWindow(target, 1);
            let error = GetLastError();
            let front = wait_foreground(hwnd, ACTIVATION_SETTLE);
            out.steps.push(Step { method: Method::Switch, returned: true, error, front });
        }
        out
    }
}

/// Those of `windows` that exist, in z-order, front to back (Windows' own list of top-level
/// windows, walked from the top).
pub(crate) fn z_order(windows: &[isize]) -> Vec<isize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetTopWindow, GetWindow, GW_HWNDNEXT};
    let mut found = Vec::with_capacity(windows.len());
    // SAFETY: read-only walk of the top-level windows; bounded, since the list can change under it.
    unsafe {
        let mut hwnd = GetTopWindow(std::ptr::null_mut());
        let mut steps = 0;
        while !hwnd.is_null() && found.len() < windows.len() && steps < 50_000 {
            let window = hwnd as isize;
            if windows.contains(&window) && !found.contains(&window) { found.push(window); }
            hwnd = GetWindow(hwnd, GW_HWNDNEXT);
            steps += 1;
        }
    }
    found
}

/// Whether `hwnd` is always on top (WS_EX_TOPMOST): it stays above every ordinary window,
/// whichever is active.
pub(crate) fn is_topmost(hwnd: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_EXSTYLE, WS_EX_TOPMOST};
    // SAFETY: read-only query on any handle.
    unsafe { GetWindowLongW(hwnd as HWND, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0 }
}

/// Whether `hwnd` is drawn on the desktop: visible, not minimised, and not cloaked by the
/// compositor (a window on another virtual desktop, say).
pub(crate) fn is_shown(hwnd: isize) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    // SAFETY: read-only queries on any handle; the attribute is a u32 out value.
    unsafe {
        if IsWindowVisible(hwnd as HWND) == 0 || IsIconic(hwnd as HWND) != 0 { return false; }
        let mut cloaked = 0u32;
        let read = DwmGetWindowAttribute(hwnd as HWND, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, std::mem::size_of::<u32>() as u32);
        read < 0 || cloaked == 0
    }
}

/// The windows of `z` (front to back) in front of `target`; none when `target` is not in it.
pub(crate) fn in_front_of(z: &[isize], target: isize) -> &[isize] {
    match z.iter().position(|h| *h == target) {
        Some(at) => &z[..at],
        None => &[],
    }
}

/// What `raise` found and did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Raise {
    /// How long (ms) the target was given to come to the top by itself.
    pub waited: u32,
    /// The other windows still shown in front of the target after that.
    pub over: usize,
    /// Of those, the ones that had to be put behind the target one by one (always-on-top
    /// windows, which moving the target to the top does not pass).
    pub lowered: usize,
    /// The target is in front of every other one now.
    pub top: bool,
}

/// Put `target` in front of `others` in the z-order, without activating anything: a window given
/// the foreground normally comes to the top by itself, but not past an always-on-top window, and
/// a window may refuse the move. The foreground can read as the target before its thread has
/// handled the activation that brings it to the top (a game does that between frames), so it is
/// first given `settle` to get there by itself, and nothing is touched when it does. Otherwise
/// the target is moved to the top of the ordinary windows; any of `others` still in front of it
/// (always on top) is then put directly behind it, which also ends its always-on-top.
pub(crate) fn raise(target: isize, others: &[isize], settle: std::time::Duration) -> Raise {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsHungAppWindow, SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE};
    let mut all = vec![target];
    all.extend(others.iter().copied().filter(|h| *h != target));
    let in_front = || -> Vec<isize> { in_front_of(&z_order(&all), target).iter().copied().filter(|h| is_shown(*h)).collect() };
    let start = std::time::Instant::now();
    let mut over = in_front();
    while !over.is_empty() && start.elapsed() < settle {
        std::thread::sleep(std::time::Duration::from_millis(5));
        over = in_front();
    }
    let waited = start.elapsed().as_millis() as u32;
    if over.is_empty() { return Raise { waited, over: 0, lowered: 0, top: true }; }
    const KEEP: u32 = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    // SAFETY: z-order calls on window handles; one that closed meanwhile makes its call fail. A
    // window that is not responding is left alone (the call would wait for it).
    unsafe {
        if IsHungAppWindow(target as HWND) == 0 { SetWindowPos(target as HWND, HWND_TOP, 0, 0, 0, 0, KEEP); }
        let still = in_front();
        let mut lowered = 0;
        for hwnd in &still {
            if IsHungAppWindow(*hwnd as HWND) == 0 && SetWindowPos(*hwnd as HWND, target as HWND, 0, 0, 0, 0, KEEP) != 0 { lowered += 1; }
        }
        let top = still.is_empty() || in_front().is_empty();
        Raise { waited, over: over.len(), lowered, top }
    }
}

/// The cursor position, for "the monitor the user is on".
pub(crate) fn cursor() -> Option<(i32, i32)> {
    let _dpi = PhysicalDpi::enter();
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: an out value.
    (unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_arithmetic() {
        let a = Rect::new(0, 0, 100, 50);
        assert_eq!(a.intersection(&Rect::new(50, 25, 100, 100)), Some(Rect::new(50, 25, 50, 25)));
        assert_eq!(a.intersection(&Rect::new(100, 0, 10, 10)), None);
        assert!(a.contains(99, 49) && !a.contains(100, 0));
        assert_eq!(Rect::new(0, 0, -5, 10).area(), 0);
    }

    #[test]
    fn only_a_game_in_front_or_minimised_may_hold_its_display() {
        let monitor = Monitor { device: r"\\.\DISPLAY1".into(), rect: Rect::new(0, 0, 3840, 2160), work: Rect::new(0, 0, 3840, 2088), dpi: 144, primary: true };
        let p = |mode, shown| Placement { rect: monitor.rect, client: (3840, 2160), shown, mode, frame: Frame::default(), monitor: monitor.clone() };
        // Covering the monitor behind another window: FFXI's borderless mode at monitor size.
        assert!(!holds_display(&p(Mode::Fullscreen, Shown::Normal), false));
        assert!(holds_display(&p(Mode::Fullscreen, Shown::Normal), true));
        assert!(holds_display(&p(Mode::Fullscreen, Shown::Minimized), false));
        for mode in [Mode::Windowed, Mode::Borderless] {
            for shown in [Shown::Normal, Shown::Minimized, Shown::Maximized] {
                assert!(!holds_display(&p(mode, shown), true), "{mode:?} {shown:?}");
            }
        }
    }

    /// A hidden stand-in for FFXI's borderless full-screen window (`0034` = 3: WS_POPUP at the
    /// primary monitor's size, the styles FFXiMain creates it with), owned by this test thread.
    struct FakeGame(HWND);
    impl FakeGame {
        fn covering(monitor: &Monitor) -> Option<Self> {
            use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WS_POPUP};
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let _dpi = PhysicalDpi::enter();
            let r = monitor.rect;
            // SAFETY: a system class, no parent, no extra data; destroyed on drop.
            let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, r.x, r.y, r.w, r.h, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) };
            (!hwnd.is_null()).then_some(Self(hwnd))
        }
        fn handle(&self) -> isize { self.0 as isize }
    }
    impl Drop for FakeGame {
        fn drop(&mut self) {
            // SAFETY: this thread's own window.
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) };
        }
    }

    #[test]
    fn a_borderless_monitor_size_window_moved_away_comes_back_whole_with_its_frame() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW, GWL_EXSTYLE, GWL_STYLE};
        let Some(monitor) = monitors().into_iter().next() else { return }; // a service session has no monitor
        let Some(game) = FakeGame::covering(&monitor) else { panic!("no stand-in window") };
        let hwnd = game.handle();
        let original = capture(hwnd).expect("readable");
        assert_eq!((original.rect, original.mode, original.shown), (monitor.rect, Mode::Fullscreen, Shown::Normal));
        assert_eq!(original.frame, Frame { style: 0x8000_0000, ex_style: 0 }, "WS_POPUP, no edges");
        // Arranged into a tile, as Arrange all does.
        let tile = Rect::new(monitor.work.x + 16, monitor.work.y + 16, 928, 522);
        assert!(apply(hwnd, tile, Shown::Normal));
        let arranged = capture(hwnd).unwrap();
        assert_eq!((arranged.rect, arranged.mode), (tile, Mode::Borderless));
        // Something gave it a caption meanwhile (a tool, or a windowed restart of the frame).
        // SAFETY: this thread's own window.
        unsafe {
            let style = GetWindowLongW(game.0, GWL_STYLE) as u32;
            SetWindowLongW(game.0, GWL_STYLE, ((style & !0x8000_0000) | 0x00CF_0000) as i32);
            SetWindowLongW(game.0, GWL_EXSTYLE, 0x300);
        }
        assert_eq!(capture(hwnd).unwrap().mode, Mode::Windowed);
        // Restored with its frame: the whole monitor again, borderless, exactly as it was.
        assert!(apply_frame(hwnd, original.rect, original.shown, Some(original.frame)));
        let back = capture(hwnd).unwrap();
        assert_eq!(back.rect, monitor.rect);
        assert_eq!(back.frame, original.frame);
        assert_eq!(back.mode, Mode::Fullscreen);
        // SAFETY: read-only.
        let other_bits = unsafe { GetWindowLongW(game.0, GWL_STYLE) as u32 } & !Frame::STYLE;
        assert_eq!(other_bits & 0x1000_0000, 0, "never shown by a restore of a hidden window (WS_VISIBLE untouched)");
        // A repeat changes nothing and still reports success.
        assert!(apply_frame(hwnd, original.rect, original.shown, Some(original.frame)));
        assert_eq!(capture(hwnd).unwrap(), back);
    }

    #[test]
    fn a_frame_replaces_only_its_own_bits() {
        let frame = Frame::from_styles(0x9400_0000, 0x0000_0008); // WS_POPUP|WS_VISIBLE|WS_CLIPSIBLINGS, WS_EX_TOPMOST
        assert_eq!(frame, Frame { style: 0x8000_0000, ex_style: 0 });
        // A windowed FFXI frame (WS_OVERLAPPEDWINDOW, client and window edges) put back to borderless.
        assert_eq!(frame.applied_to(0x14CF_0000, 0x0000_0308), (0x9400_0000, 0x0000_0008));
    }

    #[test]
    fn waiting_for_the_foreground_gives_up_on_a_window_that_is_gone() {
        let gone = 0x7FFF_FFF1isize;
        assert!(!exists(gone), "a test handle names a real window");
        let start = std::time::Instant::now();
        assert!(!wait_foreground(gone, std::time::Duration::from_secs(5)));
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "no wait for a window that can't come forward");
    }

    #[test]
    fn activating_a_window_that_is_gone_tries_nothing_risky_and_reads_well() {
        let gone = 0x7FFF_FFF3isize;
        assert!(!exists(gone), "a test handle names a real window");
        assert_eq!(activate(gone, true, true), Activation::default(), "nothing is called for a closed window");
        assert!(!Activation::default().ok());
        let sample = Activation {
            restored: true,
            steps: vec![
                Step { method: Method::Direct, returned: false, error: 0, front: false },
                Step { method: Method::Attached, returned: true, error: 0, front: true },
            ],
            ..Activation::default()
        };
        assert!(sample.ok());
        assert_eq!(sample.describe(), "restore; set ret=0 err=0 front=no; attach+set ret=1 err=0 front=yes");
        assert_eq!(Activation::default().describe(), "nothing tried");
        assert!(Activation { already: true, ..Activation::default() }.ok());
    }

    #[test]
    fn the_windows_in_front_of_one_are_those_before_it_in_the_z_order() {
        let z = [30, 10, 20];
        assert_eq!(in_front_of(&z, 30), &[] as &[isize]);
        assert_eq!(in_front_of(&z, 10), &[30]);
        assert_eq!(in_front_of(&z, 20), &[30, 10]);
        assert_eq!(in_front_of(&z, 99), &[] as &[isize], "a window that is gone has nothing in front");
        // Windows that are gone are in no z-order, and raising one of them touches nothing.
        let gone = [0x7FFF_FFF1isize, 0x7FFF_FFF3];
        assert!(gone.iter().all(|h| !exists(*h)), "a test handle names a real window");
        assert_eq!(z_order(&gone), Vec::<isize>::new());
        assert_eq!(raise(gone[0], &gone[1..], NO_WAIT), Raise { waited: 0, over: 0, lowered: 0, top: true });
        assert!(!is_shown(gone[0]) && !is_topmost(gone[0]));
    }

    const NO_WAIT: std::time::Duration = std::time::Duration::ZERO;

    /// A window of this test: shown without being activated, 4 pixels, off every screen, and
    /// layered with nothing drawn (so never seen).
    fn test_window(topmost: bool) -> isize {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, ShowWindow, SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        };
        let class = crate::win::wide("Static");
        let style = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | if topmost { WS_EX_TOPMOST } else { 0 };
        // SAFETY: a window of a system class, owned and destroyed by this test's thread.
        unsafe {
            let hwnd = CreateWindowExW(style, class.as_ptr(), std::ptr::null(), WS_POPUP, -30_000, -30_000, 4, 4,
                std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
            assert!(!hwnd.is_null());
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            hwnd as isize
        }
    }

    #[test]
    fn raising_puts_a_window_in_front_of_the_others_even_one_that_is_always_on_top() {
        // b is created after a, so in front of it; c is always on top, so in front of both: the
        // case where giving a window the foreground does not make it the one seen.
        let (a, b, c) = (test_window(false), test_window(false), test_window(true));
        assert!(is_shown(a) && is_shown(b) && is_shown(c));
        assert!(is_topmost(c) && !is_topmost(a));
        assert_eq!(z_order(&[a, b, c]), vec![c, b, a]);
        assert_eq!(z_order(&[a, c]), vec![c, a], "only the windows asked about");
        let raised = raise(a, &[b, c], NO_WAIT);
        assert_eq!(raised, Raise { waited: 0, over: 2, lowered: 1, top: true }, "b passed by moving a up, c put behind it");
        assert_eq!(z_order(&[a, b, c])[0], a);
        assert!(!is_topmost(c) && !is_topmost(a), "no game window is left always on top");
        // In front already: nothing is touched.
        assert_eq!(raise(a, &[b, c], std::time::Duration::from_secs(5)), Raise { waited: 0, over: 0, lowered: 0, top: true }, "and nothing is waited for");
        // Between ordinary windows moving the target up is enough.
        let last = *z_order(&[a, b, c]).last().unwrap();
        assert_eq!(raise(last, &[a, b, c], NO_WAIT), Raise { waited: 0, over: 2, lowered: 0, top: true });
        assert_eq!(z_order(&[a, b, c])[0], last);
        // A minimised or hidden window in front hides nothing: it is not counted.
        // SAFETY: this test's own windows.
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, ShowWindow, SW_HIDE};
            ShowWindow(last as HWND, SW_HIDE);
            assert!(!is_shown(last));
            let behind = *z_order(&[a, b, c]).iter().filter(|h| **h != last).nth(1).unwrap();
            // A window nothing brings to the top is given its time first, then moved.
            let slow = raise(behind, &[a, b, c], std::time::Duration::from_millis(40));
            assert!(slow.over == 1 && slow.top && slow.waited >= 40, "{slow:?}");
            for hwnd in [a, b, c] { DestroyWindow(hwnd as HWND); }
        }
    }

    #[test]
    fn this_desktop_has_a_primary_monitor_listed_first() {
        let list = monitors();
        // A service session can have none; an interactive one always has a primary first.
        if let Some(first) = list.first() {
            assert!(first.primary);
            assert!(first.rect.w > 0 && first.work.w > 0 && first.dpi >= 96);
            assert!(first.rect.intersection(&first.work).is_some());
        }
    }

    #[test]
    fn processes_without_windows_have_no_main_window() {
        assert_eq!(find_main_window(&[]), None);
        // The test runner owns no visible top-level window.
        assert_eq!(find_main_window(&[std::process::id()]), None);
        assert_eq!(process_family(std::process::id())[0], std::process::id());
    }
}
