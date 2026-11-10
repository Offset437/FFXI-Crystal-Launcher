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
//! windows of an elevated game, and the foreground lock: a step Windows refuses stops the
//! activations instead of flashing taskbar buttons, the same order is then set through the z-order
//! alone (each window stacked under the window in front, nothing activated), and focus is still
//! given back. After a refusal the runtime's automatic passes only stack. A switching hotkey
//! pressed during a pass ends it (`apply`'s `stop`): the pass's own activations, and the
//! foreground it gives back at the end, would undo the hotkey's switch. Local only: nothing
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
    /// Skip the activations and only stack the windows (`stack`): Windows refused an activation
    /// earlier, so trying again would only flash taskbar buttons.
    pub stack_only: bool,
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
    Plan { taskbar, activate, restore, stack_only: false }
}

/// What `apply` managed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// Taskbar buttons re-added.
    pub taskbar: usize,
    /// Windows activated (windows that closed midway are skipped).
    pub activated: usize,
    /// Windows refused an activation, which ended the activations there.
    pub refused: bool,
    /// The refused activation's steps (`game_window::Activation::describe`).
    pub refusal: Option<String>,
    /// The windows were stacked in Home order under the window in front instead (after a
    /// refusal, or for a `stack_only` plan): whether every move worked. None when not tried.
    pub stacked: Option<bool>,
    /// Whether the window the user had in front has it again (true when nothing needed it).
    pub restored: bool,
    /// A switching hotkey was pressed meanwhile (`apply`'s `stop`): the pass ended there, and
    /// gave no window the foreground after it (the hotkey's window keeps it).
    pub stopped: bool,
}

impl Outcome {
    /// Whether the Alt+Tab order was set one way or the other.
    pub(crate) fn alt_tab_set(&self) -> bool { !self.stopped && (!self.refused || self.stacked == Some(true)) && self.restored }

    /// One line for the launch history.
    pub(crate) fn describe(&self, windows: usize) -> String {
        let mut line = format!("{windows} windows, taskbar {}, activated {}", self.taskbar, self.activated);
        if self.stopped { line.push_str(", stopped for a hotkey"); }
        if let Some(refusal) = &self.refusal { line.push_str(&format!(", refused ({refusal})")); }
        match self.stacked {
            Some(true) => line.push_str(", stacked under the window in front"),
            Some(false) => line.push_str(", stacking failed"),
            None => {}
        }
        if !self.restored { line.push_str(", focus not given back"); }
        line
    }
}

/// Carry out `plan`. `stop` is asked before every step that gives a window the foreground or
/// moves one: once it says so (a switching hotkey was pressed), nothing more is activated,
/// stacked or given the foreground back, so the pass can't undo the hotkey's switch.
pub(crate) fn apply(plan: &Plan, stop: &dyn Fn() -> bool) -> Outcome {
    let taskbar = if plan.taskbar.is_empty() { 0 } else { reorder_taskbar(&plan.taskbar) };
    let (mut activated, mut refused, mut refusal) = (0, false, None);
    if !plan.stack_only {
        for hwnd in &plan.activate {
            if stop() { break; }
            if !super::game_window::exists(*hwnd) { continue; } // closed midway
            let result = super::game_window::activate(*hwnd, false, false);
            if !result.ok() {
                // Stop rather than flash buttons.
                refused = true;
                refusal = Some(result.describe());
                break;
            }
            activated += 1;
        }
    }
    // Alt+Tab lists windows front to back: without activations, the same order comes from the
    // z-order alone, set under the window in front so nothing visible changes.
    let stacked = ((refused || plan.stack_only) && !stop()).then(|| {
        let home: Vec<isize> = plan.activate.iter().rev().copied().collect();
        stack(&home)
    });
    let stopped = stop();
    let restored = match plan.restore {
        Some(hwnd) if !stopped && activated > 0 && super::game_window::exists(hwnd) => super::game_window::activate(hwnd, false, false).ok(),
        _ => true,
    };
    Outcome { taskbar, activated, refused, refusal, stacked, restored, stopped }
}

/// The z-order `stack` sets: each window (Home order) directly under the one before it, the first
/// under the window in front (`front`), which keeps its place. `front` itself, when one of
/// `windows`, is skipped. `None` for the anchor means the top of the non-topmost windows.
pub(crate) fn stacking(windows: &[isize], front: Option<isize>) -> Vec<(isize, Option<isize>)> {
    let mut after = front;
    let mut moves = Vec::new();
    for hwnd in windows {
        if Some(*hwnd) == front || *hwnd == 0 || moves.iter().any(|(h, _)| h == hwnd) { continue; }
        moves.push((*hwnd, after));
        after = Some(*hwnd);
    }
    moves
}

/// Put `windows` (Home order) under the window in front, in that order, without activating any.
/// Whether every move worked.
fn stack(windows: &[isize]) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowLongW, SetWindowPos, GWL_EXSTYLE, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
        WS_EX_TOPMOST,
    };
    // SAFETY: z-order calls on window handles; a window that closed meanwhile makes its call fail.
    unsafe {
        let front = GetForegroundWindow();
        let front = (!front.is_null()).then_some(front as isize);
        // Under a topmost window (or none), "directly under" is the top of the ordinary windows: a
        // topmost anchor would make the games topmost too.
        let ordinary = front.is_some_and(|f| GetWindowLongW(f as HWND, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST == 0);
        let mut all = true;
        for (hwnd, after) in stacking(windows, front) {
            if !super::game_window::exists(hwnd) { continue; }
            let after = match after {
                Some(a) if Some(a) != front || ordinary => a as HWND,
                _ => HWND_TOP,
            };
            all &= SetWindowPos(hwnd as HWND, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER) != 0;
        }
        all
    }
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
        assert_eq!(plan, Plan { taskbar: vec![10, 20], activate: vec![], restore: None, stack_only: false });
    }

    #[test]
    fn without_activations_the_windows_are_stacked_under_the_one_in_front() {
        // Crystal (99) in front: the games go under it in Home order.
        assert_eq!(stacking(&[10, 20, 30], Some(99)), vec![(10, Some(99)), (20, Some(10)), (30, Some(20))]);
        // A game in front keeps its place; the others follow it in Home order.
        assert_eq!(stacking(&[10, 20, 30], Some(20)), vec![(10, Some(20)), (30, Some(10))]);
        // Nothing in front: the first goes to the top of the ordinary windows.
        assert_eq!(stacking(&[10, 20], None), vec![(10, None), (20, Some(10))]);
        assert_eq!(stacking(&[10, 0, 10], Some(5)), vec![(10, Some(5))], "missing and repeated windows are skipped");
    }

    #[test]
    fn a_refused_pass_reads_as_one_line_and_counts_as_set_when_stacked() {
        let refused = Outcome {
            taskbar: 3, activated: 0, refused: true, refusal: Some("set ret=0 err=0 front=no".into()), stacked: Some(true), restored: true, stopped: false,
        };
        assert!(refused.alt_tab_set());
        assert_eq!(refused.describe(3), "3 windows, taskbar 3, activated 0, refused (set ret=0 err=0 front=no), stacked under the window in front");
        assert!(!Outcome { stacked: Some(false), ..refused.clone() }.alt_tab_set());
        assert!(!Outcome { stacked: None, ..refused.clone() }.alt_tab_set());
        let fine = Outcome { taskbar: 2, activated: 2, restored: true, ..Outcome::default() };
        assert!(fine.alt_tab_set());
        assert_eq!(fine.describe(2), "2 windows, taskbar 2, activated 2");
        // A pass a hotkey stopped did not set the order, and says why.
        let stopped = Outcome { taskbar: 5, activated: 2, restored: true, stopped: true, ..Outcome::default() };
        assert!(!stopped.alt_tab_set());
        assert_eq!(stopped.describe(5), "5 windows, taskbar 5, activated 2, stopped for a hotkey");
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
        let plan = Plan { taskbar: vec![gone[0], gone[1]], activate: vec![gone[1], gone[0]], restore: Some(gone[2]), stack_only: false };
        let nothing = Outcome { restored: true, ..Outcome::default() };
        assert_eq!(apply(&plan, &|| false), nothing);
        assert_eq!(apply(&Plan::default(), &|| false), nothing);
        // Stacking windows that are gone moves nothing and reports no failure.
        assert_eq!(apply(&Plan { stack_only: true, ..plan.clone() }, &|| false), Outcome { stacked: Some(true), ..nothing.clone() });
        // A hotkey pressed meanwhile ends the pass: nothing is activated, stacked or given the
        // foreground back.
        let asked = std::cell::Cell::new(0);
        let stop = || { asked.set(asked.get() + 1); true };
        assert_eq!(apply(&plan, &stop), Outcome { stopped: true, ..nothing.clone() });
        assert!(asked.get() >= 2, "asked before the activations and before the restore");
        assert_eq!(apply(&Plan { stack_only: true, ..plan }, &|| true), Outcome { stopped: true, ..nothing });
    }
}
