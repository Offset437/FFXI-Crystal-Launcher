//! Game windows, found by their process (never by title), the monitors they sit on, and the few
//! window operations the features need: read and set a placement, and bring a window forward.
//!
//! Coordinates are physical pixels: every call that reads or moves a window runs under a
//! per-monitor-v2 DPI context (`PhysicalDpi`), so a placement saved on a 150% monitor means the
//! same thing when read back, whatever the game's own DPI awareness is.

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};

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

/// The SetWindowPos flags of a move or resize: the window keeps its place in the z-order and is
/// not activated. The taskbar decides from the window in front and the top window of each monitor
/// whether a full-screen game has the screen, so a move must change neither.
pub(crate) const MOVE_ONLY: u32 = {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER};
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER
};

/// `apply`, also putting back `frame` when the window's frame differs from it (the styles are
/// set first and the move carries SWP_FRAMECHANGED, so the new frame takes effect with it).
/// Nothing here activates a window or changes the z-order. A window is never maximised: one that
/// is maximised already and stays so is left alone, any other is put at the normal rectangle.
pub(crate) fn apply_frame(hwnd: isize, rect: Rect, shown: Shown, frame: Option<Frame>) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, GetWindowPlacement, GetWindowRect, IsIconic, IsWindow, IsZoomed, SetWindowLongW, SetWindowPlacement, SetWindowPos, ShowWindow,
        GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, WINDOWPLACEMENT,
    };
    let _dpi = PhysicalDpi::enter();
    let hwnd = hwnd as HWND;
    // SAFETY: window calls on a handle checked to exist; the window may vanish meanwhile, which
    // makes the calls fail harmlessly.
    unsafe {
        if IsWindow(hwnd) == 0 { return false; }
        // Maximised and to stay maximised with the same normal rectangle: nothing to do, and
        // restoring it only to maximise it again would activate it.
        let framed = frame.is_none_or(|f| f == Frame::from_styles(GetWindowLongW(hwnd, GWL_STYLE) as u32, GetWindowLongW(hwnd, GWL_EXSTYLE) as u32));
        if shown == Shown::Maximized && IsZoomed(hwnd) != 0 && framed && normal_rect(hwnd) == Some(rect) { return true; }
        if IsIconic(hwnd) != 0 || IsZoomed(hwnd) != 0 {
            // Back to the normal rectangle through the window's placement: ShowWindow would bring
            // a minimised window to the top of the z-order.
            let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            if GetWindowPlacement(hwnd, &mut placement) != 0 {
                placement.showCmd = SW_SHOWNOACTIVATE as u32;
                SetWindowPlacement(hwnd, &placement);
            }
        }
        let mut flags = MOVE_ONLY;
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
            // Not SW_MINIMIZE: that one activates the next window in the z-order.
            Shown::Minimized => { ShowWindow(hwnd, SW_SHOWMINNOACTIVE); }
            Shown::Maximized | Shown::Normal => {}
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

/// What decides whether the taskbar gets out of a game window's way, in one line for the launch
/// history: where the window is against its monitor (to the pixel: the taskbar stays on top of a
/// window that does not cover the screen), its styles, whether it is always on top or in front,
/// and how Windows scales it (a process that is not DPI aware on a scaled display is given
/// scaled coordinates, so its window need not be where it believes). No name, no title.
pub(crate) fn describe(hwnd: isize) -> Option<String> {
    use windows_sys::Win32::UI::HiDpi::{GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetWindowDpiAwarenessContext};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_EXSTYLE, GWL_STYLE};
    let p = capture(hwnd)?;
    let _dpi = PhysicalDpi::enter();
    // SAFETY: read-only queries on a window handle `capture` just read.
    let (style, ex_style, awareness, dpi) = unsafe {
        (GetWindowLongW(hwnd as HWND, GWL_STYLE) as u32, GetWindowLongW(hwnd as HWND, GWL_EXSTYLE) as u32,
            GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd as HWND)), GetDpiForWindow(hwnd as HWND))
    };
    Some(describe_placement(&p, style, ex_style, awareness, dpi, foreground().is_some_and(|(front, _)| front == hwnd)))
}

/// `describe`'s line from what was read.
fn describe_placement(p: &Placement, style: u32, ex_style: u32, awareness: i32, dpi: u32, in_front: bool) -> String {
    let show = |r: &Rect| format!("({},{} {}x{})", r.x, r.y, r.w, r.h);
    let m = &p.monitor;
    let fit = match super::layouts::covers(&p.rect, &m.rect) {
        Some((0, 0, 0, 0)) => "covers it exactly".to_owned(),
        Some((l, t, r, b)) => format!("covers it and reaches {l}/{t}/{r}/{b} px past its left/top/right/bottom"),
        None => format!("does NOT cover it (left {}, top {}, right {}, bottom {} px inside)",
            p.rect.x - m.rect.x, p.rect.y - m.rect.y, m.rect.right() - p.rect.right(), m.rect.bottom() - p.rect.bottom()),
    };
    let scaling = match awareness {
        0 => "not DPI aware (Windows scales it)",
        1 => "system DPI aware",
        2 => "per-monitor DPI aware",
        _ => "DPI awareness unknown",
    };
    let shown = match p.shown { Shown::Normal => "", Shown::Minimized => ", minimised", Shown::Maximized => ", maximised" };
    format!("{} client {}x{} on monitor {} at {} dpi: {fit}{shown}; style {style:08X} ex {ex_style:08X}{}{}; {scaling}, window dpi {dpi}",
        show(&p.rect), p.client.0, p.client.1, show(&m.rect), m.dpi,
        if ex_style & 0x8 != 0 { ", ALWAYS ON TOP" } else { "" }, if in_front { ", in front" } else { ", not in front" })
}

/// Whose window is in front when `activate` is asked: it decides which call Windows allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Front {
    /// A window of this process: it is the foreground process and may hand the foreground on.
    Own,
    /// Another process's window (a game's, when a cycle key is pressed).
    Other,
    /// No window, or one that can't be read (a game between two windows of its own).
    None,
}

/// The order `activate` tries its methods in. A SetForegroundWindow that Windows refuses flashes
/// the target's taskbar button, and a flashing button brings the taskbar up: over a full-screen
/// game, and out of hiding when it hides itself. So the call Windows allows for certain goes
/// first, and a bare call from the background (allowed or not by rules that can't be read) is
/// made only when there is no better one:
/// - this thread received the hotkey itself (`own_input`, a WM_HOTKEY), or this process has the
///   foreground: the bare call is allowed;
/// - another thread's window is in front (a cycle key the keyboard hook took over a game): the
///   call is made as that thread, which is allowed to give its own foreground away;
/// - no window in front: only the bare call is left.
pub(crate) fn attempts(own_input: bool, front: Front) -> &'static [Method] {
    match (own_input, front) {
        (true, _) | (false, Front::Own) => &[Method::Direct, Method::Attached, Method::Switch],
        (false, Front::Other) => &[Method::Attached, Method::Direct, Method::Switch],
        (false, Front::None) => &[Method::Direct, Method::Switch],
    }
}

/// End a flash of `hwnd`'s taskbar button and caption (FlashWindowEx with FLASHW_STOP: nothing
/// is ever flashed by Crystal). Windows starts one when it refuses a foreground change; left
/// alone, the button stays lit until the window is activated, and the taskbar stays up with it.
/// A window that is not flashing is not touched; one that is not responding is left alone.
pub(crate) fn stop_flash(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FlashWindowEx, IsHungAppWindow, FLASHWINFO, FLASHW_STOP};
    // SAFETY: a sized FLASHWINFO for a window handle; fails harmlessly for a window that is gone.
    unsafe {
        if !exists(hwnd) || IsHungAppWindow(hwnd as HWND) != 0 { return; }
        let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: hwnd as HWND, dwFlags: FLASHW_STOP, uCount: 0, dwTimeout: 0 };
        FlashWindowEx(&info);
    }
}

/// Tell the taskbar that `hwnd` is a full-screen window, or no longer one
/// (ITaskbarList2::MarkFullscreenWindow). The taskbar goes behind the window in front when it
/// takes that window for a full-screen one; it works that out by itself from the window's size
/// and state, and that reading can be lost (it was, whenever a game's taskbar button was removed
/// and added again). Told outright, a game that covers its monitor keeps the taskbar behind it
/// while it is in front. Nothing else is asked of the taskbar: no button is added, removed,
/// moved, activated or flashed, and the window itself is not touched. Whether the taskbar took it.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn mark_fullscreen(hwnd: isize, on: bool) -> bool {
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    /// ITaskbarList2's vtable as far as it is used: IUnknown, HrInit, the four methods before
    /// MarkFullscreenWindow (never called, so they have no names here), and it.
    #[repr(C)]
    struct TaskbarList2 {
        query_interface: usize,
        add_ref: usize,
        release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
        hr_init: unsafe extern "system" fn(*mut core::ffi::c_void) -> i32,
        unused: [usize; 4],
        mark_fullscreen_window: unsafe extern "system" fn(*mut core::ffi::c_void, HWND, i32) -> i32,
    }
    const IID_ITASKBARLIST2: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x602d4995_b13a_429b_a66e_1935e44f4317);
    if !exists(hwnd) { return false; }
    // SAFETY: COM initialised for this call (balanced below when it was this call's doing); the
    // object is used through its documented vtable and released.
    unsafe {
        let init = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        // S_OK and S_FALSE are ours to balance; RPC_E_CHANGED_MODE means COM is already set up.
        let balance = init >= 0;
        let mut object: *mut core::ffi::c_void = std::ptr::null_mut();
        let created = CoCreateInstance(&windows_sys::Win32::UI::Shell::TaskbarList, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ITASKBARLIST2, &mut object);
        let mut taken = false;
        if created >= 0 && !object.is_null() {
            let vtbl = &**(object as *mut *const TaskbarList2);
            taken = (vtbl.hr_init)(object) >= 0 && (vtbl.mark_fullscreen_window)(object, hwnd as HWND, i32::from(on)) >= 0;
            (vtbl.release)(object);
        }
        if balance { CoUninitialize(); }
        taken
    }
}

/// How one attempt to give a window the foreground was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// SetForegroundWindow on its own: allowed for the thread that received the hotkey, or while
    /// Crystal is in front.
    Direct,
    /// Attached to the input of the thread in front and SetForegroundWindow, then detached (Windows then treats the call as that thread's own).
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

/// Give `hwnd` the foreground, for a switching hotkey (the only thing Crystal ever activates a
/// game window for): restore it when minimised (`restore`), then the methods in `attempts`'
/// order, each waiting for a switch still completing (`wait_foreground`) before the next is tried:
/// SetForegroundWindow on its own; the same as the thread in front (attached to its input for the
/// call); and last SwitchToThisWindow. `own_input`: this thread received
/// the hotkey itself (a WM_HOTKEY), so its own call is allowed. No key is ever sent: a synthetic
/// Alt would reach the game. A refused call flashes the target's taskbar button, so one that
/// would be refused is not made first, a bare call is not repeated after the call as the thread
/// in front was refused, and whatever flash a refusal started is ended (`stop_flash`).
pub(crate) fn activate(hwnd: isize, restore: bool, own_input: bool) -> Activation {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsHungAppWindow, IsIconic, SetForegroundWindow, ShowWindow, SwitchToThisWindow, SW_RESTORE,
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
        let front = match foreground() {
            None => Front::None,
            Some((_, pid)) if pid == GetCurrentProcessId() => Front::Own,
            Some(_) => Front::Other,
        };
        // The call as the thread in front was made and refused: a bare one would be too.
        let mut refused_as_front = false;
        for method in attempts(own_input, front) {
            match method {
                Method::Direct => {
                    if refused_as_front { continue; }
                    SetLastError(0);
                    let returned = SetForegroundWindow(target) != 0;
                    let error = GetLastError();
                    let front = if returned { wait_foreground(hwnd, ACTIVATION_SETTLE) } else { in_front() };
                    out.steps.push(Step { method: Method::Direct, returned, error, front });
                    if front { return out; }
                    stop_flash(hwnd);
                }
                Method::Attached | Method::AttachRefused => {
                    let current = GetCurrentThreadId();
                    let front_window = GetForegroundWindow();
                    let thread = (!front_window.is_null()).then(|| window_process(front_window as isize)).flatten().map(|(_, t)| t).filter(|t| *t != current);
                    let Some(thread) = thread else { continue };
                    if IsHungAppWindow(front_window) != 0 { out.hung = true; return out; }
                    SetLastError(0);
                    if AttachThreadInput(current, thread, 1) != 0 {
                        SetLastError(0);
                        let returned = SetForegroundWindow(target) != 0;
                        let error = GetLastError();
                        AttachThreadInput(current, thread, 0);
                        let front = if returned { wait_foreground(hwnd, ACTIVATION_SETTLE) } else { in_front() };
                        out.steps.push(Step { method: Method::Attached, returned, error, front });
                        if front { return out; }
                        stop_flash(hwnd);
                        refused_as_front = true;
                    } else {
                        out.steps.push(Step { method: Method::AttachRefused, returned: false, error: GetLastError(), front: false });
                    }
                }
                // Alt+Tab's own switch.
                Method::Switch => {
                    SetLastError(0);
                    SwitchToThisWindow(target, 1);
                    let error = GetLastError();
                    let front = wait_foreground(hwnd, ACTIVATION_SETTLE);
                    out.steps.push(Step { method: Method::Switch, returned: true, error, front });
                    if !front { stop_flash(hwnd); }
                }
            }
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
#[cfg(test)]
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
        assert_eq!(activate(gone, true, false), Activation::default());
        stop_flash(gone); // nothing to stop on a window that is gone
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
    fn the_history_says_whether_a_game_window_covers_its_monitor() {
        let monitor = Monitor { device: r"\\.\DISPLAY1".into(), rect: Rect::new(0, 0, 3840, 2160), work: Rect::new(0, 0, 3840, 2088), dpi: 144, primary: true };
        let p = |rect: Rect| Placement { rect, client: (rect.w, rect.h), shown: Shown::Normal, mode: Mode::Borderless, frame: Frame::default(), monitor: monitor.clone() };
        // The owner's clients (2026-11-10): borderless at the monitor's size, system DPI aware.
        assert_eq!(describe_placement(&p(monitor.rect), 0x9400_0000, 0, 1, 144, true),
            "(0,0 3840x2160) client 3840x2160 on monitor (0,0 3840x2160) at 144 dpi: covers it exactly; style 94000000 ex 00000000, in front; system DPI aware, window dpi 144");
        // Put inside the work area (what a restore used to do to a window a pixel larger than
        // the screen): the taskbar's strip shows, and the line says by how much.
        let short = describe_placement(&p(monitor.work), 0x9400_0000, 0x8, 0, 96, false);
        assert!(short.contains("does NOT cover it (left 0, top 0, right 0, bottom 72 px inside)"), "{short}");
        assert!(short.contains("ALWAYS ON TOP, not in front; not DPI aware (Windows scales it), window dpi 96"), "{short}");
        // A game Windows scales by 150% that asked for the monitor's own size.
        let scaled = describe_placement(&p(Rect::new(0, 0, 5760, 3240)), 0x9400_0000, 0, 0, 96, true);
        assert!(scaled.contains("covers it and reaches 0/0/1920/1080 px past its left/top/right/bottom"), "{scaled}");
        // A real window reads without a name or a title.
        let Some(game) = FakeGame::covering(&monitors().into_iter().next().unwrap_or(monitor.clone())) else { return };
        let line = describe(game.handle()).expect("readable");
        assert!(line.contains("covers it exactly") && line.contains("not in front"), "{line}");
        assert_eq!(describe(0x7FFF_FFF1), None, "a window that is gone has no line");
    }

    #[test]
    fn a_foreground_call_windows_would_refuse_is_never_made_first() {
        use Method::{Attached, Direct, Switch};
        // A WM_HOTKEY on this thread, or Crystal in front: the bare call is allowed.
        for front in [Front::Own, Front::Other, Front::None] {
            assert_eq!(attempts(true, front)[0], Direct, "{front:?}");
        }
        assert_eq!(attempts(false, Front::Own), &[Direct, Attached, Switch]);
        // A cycle key the keyboard hook took over a game (the owner's history, 2026-11-10: 11 of
        // 61 such presses had the bare call refused, each flashing the target's taskbar button):
        // asked as the game's thread first.
        assert_eq!(attempts(false, Front::Other), &[Attached, Direct, Switch]);
        // No window in front: there is no thread to ask as.
        assert_eq!(attempts(false, Front::None), &[Direct, Switch]);
    }

    #[test]
    fn moves_never_activate_and_never_make_a_window_always_on_top() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW};
        assert_eq!(MOVE_ONLY & (SWP_NOACTIVATE | SWP_NOZORDER), SWP_NOACTIVATE | SWP_NOZORDER, "a move keeps the z-order and the active window");
        assert_eq!(MOVE_ONLY & SWP_SHOWWINDOW, 0, "nothing is shown by a move");
        // A covering stand-in moved to a tile and back: exactly the monitor again, never always on
        // top, and never given the foreground.
        let Some(monitor) = monitors().into_iter().next() else { return };
        let Some(game) = FakeGame::covering(&monitor) else { panic!("no stand-in window") };
        let hwnd = game.handle();
        for rect in [Rect::new(monitor.work.x + 3, monitor.work.y + 5, 641, 479), monitor.rect, monitor.rect] {
            assert!(apply(hwnd, rect, Shown::Normal));
            assert_eq!(capture(hwnd).unwrap().rect, rect, "pixel for pixel");
            assert!(!is_topmost(hwnd));
            assert!(foreground().is_none_or(|(front, _)| front != hwnd), "not activated");
        }
        assert_eq!(capture(hwnd).unwrap().mode, Mode::Fullscreen, "covers its monitor exactly");
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
