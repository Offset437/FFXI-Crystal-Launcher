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

/// What `capture` reads from a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    /// The window's own rectangle (frame included) while shown normally.
    pub rect: Rect,
    /// Its client (render) size.
    pub client: (i32, i32),
    pub shown: Shown,
    pub mode: Mode,
    pub monitor: Monitor,
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
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowLongW, GetWindowRect, IsIconic, IsWindow, IsZoomed, GWL_STYLE, WS_CAPTION};
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
        let captioned = GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CAPTION == WS_CAPTION;
        let mode = if captioned { Mode::Windowed } else if rect == monitor.rect { Mode::Fullscreen } else { Mode::Borderless };
        Some(Placement { rect, client: (client.right - client.left, client.bottom - client.top), shown, mode, monitor })
    }
}

/// Move and size `hwnd` to `rect` and show it as `shown`, without activating it. A window
/// already at `rect` is left alone, so a restore that repeats changes nothing.
pub(crate) fn apply(hwnd: isize, rect: Rect, shown: Shown) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsIconic, IsWindow, IsZoomed, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
        SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWNOACTIVATE,
    };
    let _dpi = PhysicalDpi::enter();
    let hwnd = hwnd as HWND;
    // SAFETY: window calls on a handle checked to exist; the window may vanish meanwhile, which
    // makes the calls fail harmlessly.
    unsafe {
        if IsWindow(hwnd) == 0 { return false; }
        if IsIconic(hwnd) != 0 || IsZoomed(hwnd) != 0 { ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
        let mut now: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut now);
        let moved = Rect::from_win(&now) == rect
            || SetWindowPos(hwnd, std::ptr::null_mut(), rect.x, rect.y, rect.w, rect.h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER) != 0;
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

/// Bring `hwnd` forward for one of Crystal's own actions (a hotkey press). A hotkey's receiver may
/// set the foreground window; when Windows still refuses (the foreground lock), attach to the
/// foreground thread's input for the call, then fall back to a single Alt tap, which Windows
/// treats as the user's input. Returns whether `hwnd` is now in the foreground.
pub(crate) fn bring_forward(hwnd: isize) -> bool {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MENU};
    use windows_sys::Win32::UI::WindowsAndMessaging::{BringWindowToTop, GetForegroundWindow, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};
    let target = hwnd as HWND;
    let forward = || foreground().is_some_and(|(now, _)| now == hwnd);
    // SAFETY: window and input calls on our own thread; the attach is always undone.
    unsafe {
        if IsIconic(target) != 0 { ShowWindow(target, SW_RESTORE); }
        if SetForegroundWindow(target) != 0 && forward() { return true; }
        let current = GetCurrentThreadId();
        if let Some((_, thread)) = window_process(GetForegroundWindow() as isize).filter(|(_, t)| *t != current) {
            if AttachThreadInput(current, thread, 1) != 0 {
                BringWindowToTop(target);
                SetForegroundWindow(target);
                AttachThreadInput(current, thread, 0);
                if forward() { return true; }
            }
        }
        let key = |flags| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_MENU, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } } };
        let tap = [key(0), key(KEYEVENTF_KEYUP)];
        SendInput(tap.len() as u32, tap.as_ptr(), std::mem::size_of::<INPUT>() as i32);
        SetForegroundWindow(target);
        forward()
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
