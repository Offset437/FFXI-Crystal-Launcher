//! Native window chrome for the undecorated launcher window: Windows 11
//! rounded corners, the system drop shadow and a theme-matched 1 px border,
//! plus the title-bar double-click rule.
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::time::Instant;

/// The launcher window's HWND, once it has opened (0 until then).
static HWND: AtomicIsize = AtomicIsize::new(0);

pub(crate) fn is_foreground() -> bool {
    let hwnd = HWND.load(Ordering::Relaxed);
    hwnd != 0 && unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } as isize == hwnd
}

/// The window closed (Crystal runs in the background): nothing is foreground any more.
pub(crate) fn detach() {
    HWND.store(0, Ordering::Relaxed);
}


/// Rounds the corners, turns on the DWM shadow and tints the border for the
/// current theme. Safe to call again; a zero handle is ignored.
pub(crate) fn attach(hwnd: isize, light: bool) {
    if hwnd == 0 {
        return;
    }
    HWND.store(hwnd, Ordering::Relaxed);
    unsafe {
        use windows_sys::Win32::Graphics::Dwm::{
            DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
        };
        use windows_sys::Win32::UI::Controls::MARGINS;
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        // DWMWCP_ROUND; ignored (harmlessly) before Windows 11.
        let round: i32 = 2;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&round as *const i32).cast(),
            4,
        );
        // A one-pixel frame extension on every side is what makes DWM draw
        // the system shadow around a window whose non-client area was removed
        // (the approach tao/Tauri use). The page paints opaque over it, so
        // nothing of the frame itself shows. Unlike winit's own
        // `undecorated_shadow`, it leaves no 1 px line along the top.
        let margins = MARGINS {
            cxLeftWidth: 1,
            cxRightWidth: 1,
            cyTopHeight: 1,
            cyBottomHeight: 1,
        };
        DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
    set_theme(light);
}

/// Matches the Windows 11 window border to the theme's hairline colour.
pub(crate) fn set_theme(light: bool) {
    let hwnd = HWND.load(Ordering::Relaxed);
    if hwnd == 0 {
        return;
    }
    unsafe {
        use windows_sys::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        };
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let line = crate::palette::colors(light).line;
        let [r, g, b, _] = line.into_rgba8();
        // COLORREF is 0x00BBGGRR.
        let colorref: u32 = (r as u32) | ((g as u32) << 8) | ((b as u32) << 16);
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            (&colorref as *const u32).cast(),
            4,
        );
        let dark: i32 = (!light).into();
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            4,
        );
    }
}

/// Milliseconds since `EPOCH` of the last press on the title area (0 = none).
static LAST_TITLE_PRESS: AtomicU64 = AtomicU64::new(0);

/// A press on the title area: true when it completes a double click (so the
/// window maximises or restores), false when it should start a window drag.
pub(crate) fn title_press_is_double() -> bool {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let now = EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1;
    let last = LAST_TITLE_PRESS.swap(now, Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) <= double_click_ms() {
        // A third quick press starts a new pair rather than toggling again.
        LAST_TITLE_PRESS.store(0, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn double_click_ms() -> u64 {
    {
        let ms = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
        if ms > 0 {
            return ms as u64;
        }
    }
    500
}
