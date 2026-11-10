//! Game windows follow the Home account order: on the taskbar and in Alt+Tab.
//!
//! - Taskbar: each game window's button is removed and added again in Home order through the
//!   shell's own taskbar interface (ITaskbarList DeleteTab/AddTab), so each lands after the ones
//!   before it. The window itself is never hidden, moved, resized or activated, so nothing
//!   flashes and a fullscreen game keeps its display. (Hiding and re-showing each window does the
//!   same but deactivates the one in front and makes the game rebuild its device; it is only the
//!   fallback for a button the shell would not add back.)
//! - Alt+Tab: the windows are activated in reverse Home order, so the first account's is the most
//!   recent, then the window the user had in front gets it back. Minimised windows are skipped
//!   (activating one would restore it or leave it active while hidden).
//!
//! The plan is pure (`plan`); `apply` carries it out and tolerates windows that close midway,
//! windows of an elevated game, and the foreground lock: a step Windows refuses stops the Alt+Tab
//! pass instead of flashing taskbar buttons, and focus is still given back. Local only: nothing
//! reaches the game but ordinary activation, and nothing goes over the network.

/// A game window to order, in Home order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub hwnd: isize,
    pub minimized: bool,
}

/// What `apply` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Taskbar buttons re-added in this order (Home order).
    pub taskbar: Vec<isize>,
    /// Windows activated in this order (reverse Home order).
    pub activate: Vec<isize>,
    /// The window given the foreground back afterwards.
    pub restore: Option<isize>,
}

/// The plan for `windows` (Home order) with `foreground` in front now. Fewer than two windows
/// have no order to set; a repeated window counts once, where it first appears.
pub(crate) fn plan(windows: &[Window], foreground: Option<isize>) -> Plan {
    let mut unique: Vec<Window> = Vec::with_capacity(windows.len());
    for w in windows {
        if w.hwnd != 0 && !unique.iter().any(|u| u.hwnd == w.hwnd) { unique.push(*w); }
    }
    if unique.len() < 2 { return Plan::default(); }
    let taskbar = unique.iter().map(|w| w.hwnd).collect();
    let shown: Vec<isize> = unique.iter().filter(|w| !w.minimized).map(|w| w.hwnd).collect();
    if shown.len() < 2 { return Plan { taskbar, ..Plan::default() }; }
    let activate: Vec<isize> = shown.into_iter().rev().collect();
    // The first account's window ends up in front anyway when the user had it there.
    let restore = foreground.filter(|f| activate.last() != Some(f));
    Plan { taskbar, activate, restore }
}

/// What `apply` managed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// Taskbar buttons re-added.
    pub taskbar: usize,
    /// Windows activated (windows that closed midway are skipped).
    pub activated: usize,
    /// Windows refused an activation, which ended the Alt+Tab pass there.
    pub refused: bool,
    /// Whether the window the user had in front has it again (true when nothing needed it).
    pub restored: bool,
}

/// Carry out `plan`.
pub(crate) fn apply(plan: &Plan) -> Outcome {
    let taskbar = if plan.taskbar.is_empty() { 0 } else { reorder_taskbar(&plan.taskbar) };
    let (mut activated, mut refused) = (0, false);
    for hwnd in &plan.activate {
        if !super::game_window::exists(*hwnd) { continue; } // closed midway
        if !activate(*hwnd) { refused = true; break; } // stop rather than flash buttons
        activated += 1;
    }
    let restored = match plan.restore {
        Some(hwnd) if activated > 0 && super::game_window::exists(hwnd) => activate(hwnd),
        _ => true,
    };
    Outcome { taskbar, activated, refused, restored }
}

// ---- taskbar ---------------------------------------------------------------------------------

type Hresult = i32;

/// ITaskbarList's vtable: IUnknown, then HrInit, AddTab, DeleteTab (the rest unused).
#[repr(C)]
struct TaskbarListVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    hr_init: unsafe extern "system" fn(*mut core::ffi::c_void) -> Hresult,
    add_tab: unsafe extern "system" fn(*mut core::ffi::c_void, windows_sys::Win32::Foundation::HWND) -> Hresult,
    delete_tab: unsafe extern "system" fn(*mut core::ffi::c_void, windows_sys::Win32::Foundation::HWND) -> Hresult,
}

const IID_ITASKBARLIST: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x56fdf342_fd6d_11d0_958a_006097c9a090);

/// Re-add each window's taskbar button in order. How many were re-added.
fn reorder_taskbar(list: &[isize]) -> usize {
    use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOWNA};
    // SAFETY: COM initialised for this call (balanced below when it was this call's doing); the
    // object is used through its documented vtable and released.
    unsafe {
        let init = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        // S_OK and S_FALSE are ours to balance; RPC_E_CHANGED_MODE means COM is already set up.
        let balance = init >= 0;
        let mut object: *mut core::ffi::c_void = std::ptr::null_mut();
        let created = CoCreateInstance(&windows_sys::Win32::UI::Shell::TaskbarList, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ITASKBARLIST, &mut object);
        let mut done = 0;
        if created >= 0 && !object.is_null() {
            let vtbl = &**(object as *mut *const TaskbarListVtbl);
            if (vtbl.hr_init)(object) >= 0 {
                for hwnd in list {
                    let window = *hwnd as windows_sys::Win32::Foundation::HWND;
                    if !super::game_window::exists(*hwnd) { continue; } // closed midway
                    if (vtbl.delete_tab)(object, window) < 0 { continue; } // left where it is
                    if (vtbl.add_tab)(object, window) >= 0 {
                        done += 1;
                    } else if super::game_window::exists(*hwnd) {
                        // The button must come back: showing the window again adds it.
                        ShowWindow(window, SW_HIDE);
                        ShowWindow(window, SW_SHOWNA);
                        done += 1;
                    }
                }
            }
            (vtbl.release)(object);
        }
        if balance { CoUninitialize(); }
        done
    }
}

// ---- activation ------------------------------------------------------------------------------

/// Give `hwnd` the foreground without input of our own (no Alt tap: that would reach the program
/// in front). Allowed directly while this process is in front; otherwise for the length of the
/// call this thread shares the input state of the thread in front, which Windows treats as that
/// thread's own request. Refused for a hung window, or when the window in front belongs to a
/// process this one may not attach to (an elevated one): then Windows would only flash the
/// button, so nothing is attempted. Whether `hwnd` is in front now.
fn activate(hwnd: isize) -> bool {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{BringWindowToTop, GetForegroundWindow, IsHungAppWindow, SetForegroundWindow};
    let target = hwnd as windows_sys::Win32::Foundation::HWND;
    let in_front = || super::game_window::foreground().is_some_and(|(now, _)| now == hwnd);
    // SAFETY: window and input-state calls; the attach is always undone.
    unsafe {
        if IsHungAppWindow(target) != 0 { return false; }
        if in_front() { return true; }
        let front = GetForegroundWindow();
        let owner = super::game_window::window_process(front as isize);
        let ours = front.is_null() || owner.is_some_and(|(pid, _)| pid == GetCurrentProcessId());
        if ours {
            SetForegroundWindow(target);
            return in_front();
        }
        let Some((_, thread)) = owner else { return false };
        if IsHungAppWindow(front) != 0 { return false; }
        let current = GetCurrentThreadId();
        if thread == current || AttachThreadInput(current, thread, 1) == 0 { return false; }
        BringWindowToTop(target);
        SetForegroundWindow(target);
        AttachThreadInput(current, thread, 0);
        in_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(hwnd: isize) -> Window { Window { hwnd, minimized: false } }

    #[test]
    fn the_taskbar_follows_home_order_and_alt_tab_is_activated_in_reverse() {
        let windows = [w(10), w(20), w(30)];
        let plan = plan(&windows, Some(99));
        assert_eq!(plan.taskbar, vec![10, 20, 30], "each re-added button lands after the ones before it");
        assert_eq!(plan.activate, vec![30, 20, 10], "the first account's window is activated last: the most recent");
        assert_eq!(plan.restore, Some(99), "the window the user had gets the foreground back");
    }

    #[test]
    fn the_first_window_already_in_front_needs_no_restore() {
        assert_eq!(plan(&[w(10), w(20)], Some(10)).restore, None);
        // Another game in front gets it back.
        assert_eq!(plan(&[w(10), w(20)], Some(20)).restore, Some(20));
        // Nothing in front (the desktop locked, say): nothing to give back.
        assert_eq!(plan(&[w(10), w(20)], None).restore, None);
    }

    #[test]
    fn minimised_windows_keep_their_button_order_but_are_not_activated() {
        let windows = [w(10), Window { hwnd: 20, minimized: true }, w(30)];
        let plan = plan(&windows, Some(5));
        assert_eq!(plan.taskbar, vec![10, 20, 30]);
        assert_eq!(plan.activate, vec![30, 10]);
        // Only one window shown: no Alt+Tab order to set, so focus is never touched.
        let plan = super::plan(&[w(10), Window { hwnd: 20, minimized: true }], Some(5));
        assert_eq!(plan, Plan { taskbar: vec![10, 20], activate: vec![], restore: None });
    }

    #[test]
    fn one_window_or_none_has_no_order_and_repeats_count_once() {
        assert_eq!(plan(&[], Some(1)), Plan::default());
        assert_eq!(plan(&[w(10)], Some(1)), Plan::default());
        assert_eq!(plan(&[w(10), w(10)], Some(1)), Plan::default());
        assert_eq!(plan(&[w(0), w(10)], None), Plan::default(), "a missing window is skipped");
        let plan = plan(&[w(10), w(20), w(10)], None);
        assert_eq!((plan.taskbar, plan.activate), (vec![10, 20], vec![20, 10]));
    }

    #[test]
    fn applying_to_windows_that_are_gone_does_nothing() {
        // Handles no window has: every step is skipped, focus is left alone.
        let gone = [0x7FFF_FFF1isize, 0x7FFF_FFF3, 0x7FFF_FFF5];
        assert!(gone.iter().all(|h| !super::super::game_window::exists(*h)), "a test handle names a real window");
        let plan = Plan { taskbar: vec![gone[0], gone[1]], activate: vec![gone[1], gone[0]], restore: Some(gone[2]) };
        assert_eq!(apply(&plan), Outcome { taskbar: 0, activated: 0, refused: false, restored: true });
        assert_eq!(apply(&Plan::default()), Outcome { taskbar: 0, activated: 0, refused: false, restored: true });
    }
}
