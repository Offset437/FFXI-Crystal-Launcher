//! Content-free timings of the game's local boot (logged for its first 60 s).
//! Forward the same calls and arguments, with one exception: the game's own
//! game-controller query skips discovery when the account's game settings
//! disable the gamepad (the same answer as no attached controller), and
//! otherwise may be answered from the launcher's verified catalog (controller_catalog.rs).
use std::{ffi::c_void, sync::{atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}, Mutex}, time::{Duration, Instant}};
use windows_sys::{core::GUID, Win32::{Foundation::{GetLastError, SetLastError, HWND}, System::Threading::{GetCurrentProcess, GetProcessTimes, GetProcessIoCounters, IO_COUNTERS}}};

static STARTED: Mutex<Option<Instant>> = Mutex::new(None);
static ORIGINAL: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
static DEVICE: AtomicUsize = AtomicUsize::new(0);
static INPUT_DEVICE: AtomicUsize = AtomicUsize::new(0);
static INPUT_ENUM: AtomicUsize = AtomicUsize::new(0);
static INPUT_ENUM_ANSI: AtomicBool = AtomicBool::new(false);
static GAME_INPUT: AtomicUsize = AtomicUsize::new(0);
static GAME_INPUT_ANSI: AtomicBool = AtomicBool::new(false);
static INPUT_ENUM_CALLS: AtomicUsize = AtomicUsize::new(0);
static INPUT_DEVICE_CALLS: AtomicUsize = AtomicUsize::new(0);
static SLEEPS: AtomicU64 = AtomicU64::new(0);
static SLEEP_US: AtomicU64 = AtomicU64::new(0);
static LONGEST_SLEEP: AtomicU64 = AtomicU64::new(0);
static INSTALL: Mutex<()> = Mutex::new(());

pub(crate) struct Run;
impl Run {
    pub(crate) fn begin() -> Self {
        SLEEPS.store(0, Ordering::Relaxed);
        SLEEP_US.store(0, Ordering::Relaxed);
        LONGEST_SLEEP.store(0, Ordering::Relaxed);
        INPUT_DEVICE_CALLS.store(0, Ordering::Relaxed);
        INPUT_ENUM_CALLS.store(0, Ordering::Relaxed);
        GAME_INPUT.store(0, Ordering::Release);
        *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        checkpoint("game-start");
        Self
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        GAME_INPUT.store(0, Ordering::Release);
        checkpoint("game-return");
        *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn active() -> bool {
    STARTED.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|at| at.elapsed() < Duration::from_secs(60))
}

pub(crate) fn checkpoint(stage: &'static str) {
    let saved = unsafe { GetLastError() };
    crate::windower_threads::checkpoint(stage);
    let elapsed = STARTED.lock().unwrap_or_else(|e| e.into_inner()).map(|at| at.elapsed());
    if let Some(elapsed) = elapsed {
        let mut created = Default::default();
        let mut ended = Default::default();
        let mut kernel = Default::default();
        let mut user = Default::default();
        let mut io = IO_COUNTERS::default();
        let cpu_ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut ended, &mut kernel, &mut user) } != 0;
        let io_ok = unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut io) } != 0;
        let micros = |t: windows_sys::Win32::Foundation::FILETIME| (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) / 10;
        crate::log!("local boot timing: {stage} elapsed_ms={} cpu_valid={cpu_ok} user_us={} kernel_us={} io_valid={io_ok} read_ops={} read_bytes={} sleep_calls={} sleep_us={} max_sleep_us={}",
            elapsed.as_millis(), micros(user), micros(kernel), io.ReadOperationCount, io.ReadTransferCount,
            SLEEPS.load(Ordering::Relaxed), SLEEP_US.load(Ordering::Relaxed), LONGEST_SLEEP.load(Ordering::Relaxed));
    }
    unsafe { SetLastError(saved) };
}

fn timed<T>(name: &'static str, call: impl FnOnce() -> T) -> T {
    let incoming = unsafe { GetLastError() };
    let measuring = active();
    let start = Instant::now();
    unsafe { SetLastError(incoming) };
    let result = call();
    let outgoing = unsafe { GetLastError() };
    if measuring && (name != "DirectInput8.CreateDevice" || INPUT_DEVICE_CALLS.fetch_add(1, Ordering::Relaxed) < 8 || start.elapsed() >= Duration::from_millis(50)) {
        crate::log!("local boot timing: {name} duration_us={}", start.elapsed().as_micros());
    }
    unsafe { SetLastError(outgoing) };
    result
}

fn word(address: usize) -> Option<usize> { crate::mem::read::<usize>(address) }

/// Replace only a still-owned pointer. An injector installed before us remains
/// the forwarded target; an injector installed later is never overwritten.
fn replace(slot: usize, original: &AtomicUsize, wrapper: usize) -> bool {
    let Some(current) = word(slot).filter(|v| *v != 0) else { return false };
    if current == wrapper { return original.load(Ordering::Acquire) != 0; }
    let previous = original.load(Ordering::Acquire);
    if previous != 0 && previous != current { return false; }
    // Published before the swap: the game may call the wrapper at once.
    original.store(current, Ordering::Release);
    match crate::iat::swap(slot, current, wrapper) {
        Ok(done) => done.changed,
        Err(_) => { original.store(previous, Ordering::Release); false }
    }
}

pub(crate) fn install(base: usize) {
    let saved = unsafe { GetLastError() };
    let _one = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    for (dll, name, wrapper, index) in [
        ("KERNEL32.dll", "Sleep", sleep as *const () as usize, 0),
        ("d3d8.dll", "Direct3DCreate8", graphics as *const () as usize, 1),
        ("DINPUT8.dll", "DirectInput8Create", input as *const () as usize, 2),
    ] {
        // The initialized FFXiMain module supplied by ffxi_settings; this parser
        // validates its PE headers and never follows another module's imports.
        match unsafe { crate::iat::import_slots(base, dll) }.iter().find(|s| s.name.as_deref() == Some(name)) {
            Some(slot) if replace(slot.slot as usize, &ORIGINAL[index], wrapper) => crate::log!("local boot timing: {name} forwarding installed"),
            Some(_) => crate::log!("note: local boot timing: {name} import is already redirected by another module; left as is"),
            None => crate::log!("note: local boot timing: FFXiMain imports no {dll}!{name}; that stage is not measured"),
        }
    }
    unsafe { SetLastError(saved) };
}

unsafe extern "system" fn sleep(ms: u32) {
    let original: unsafe extern "system" fn(u32) = unsafe { std::mem::transmute(ORIGINAL[0].load(Ordering::Acquire)) };
    let incoming = unsafe { GetLastError() };
    let measuring = active();
    let start = Instant::now();
    unsafe { SetLastError(incoming); original(ms); }
    let outgoing = unsafe { GetLastError() };
    if measuring {
        let elapsed = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
        SLEEPS.fetch_add(1, Ordering::Relaxed);
        SLEEP_US.fetch_add(elapsed, Ordering::Relaxed);
        LONGEST_SLEEP.fetch_max(elapsed, Ordering::Relaxed);
        if elapsed >= 250_000 { crate::log!("local boot timing: Sleep requested_ms={ms} duration_us={elapsed}"); }
    }
    unsafe { SetLastError(outgoing) };
}

unsafe extern "system" fn graphics(version: u32) -> *mut c_void {
    let original: unsafe extern "system" fn(u32) -> *mut c_void = unsafe { std::mem::transmute(ORIGINAL[1].load(Ordering::Acquire)) };
    let result = timed("Direct3DCreate8", || unsafe { original(version) });
    let saved = unsafe { GetLastError() };
    if let Some(table) = word(result as usize) {
        let _one = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
        // IDirect3D8::CreateDevice is slot 15, after IUnknown and its adapter methods.
        replace(table + 15 * std::mem::size_of::<usize>(), &DEVICE, create_device as *const () as usize);
    }
    unsafe { SetLastError(saved) };
    result
}

unsafe extern "system" fn create_device(this: *mut c_void, adapter: u32, kind: i32, window: HWND, flags: u32, parameters: *mut c_void, out: *mut *mut c_void) -> i32 {
    let original: unsafe extern "system" fn(*mut c_void, u32, i32, HWND, u32, *mut c_void, *mut *mut c_void) -> i32 = unsafe { std::mem::transmute(DEVICE.load(Ordering::Acquire)) };
    timed("Direct3D8.CreateDevice", || unsafe { original(this, adapter, kind, window, flags, parameters, out) })
}

unsafe extern "system" fn input(module: *mut c_void, version: u32, iid: *const GUID, out: *mut *mut c_void, outer: *mut c_void) -> i32 {
    let original: unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(ORIGINAL[2].load(Ordering::Acquire)) };
    let result = timed("DirectInput8Create", || unsafe { original(module, version, iid, out, outer) });
    let saved = unsafe { GetLastError() };
    // Both IDirectInput8A and IDirectInput8W share these first two method ABIs.
    if result >= 0 && !iid.is_null() && version == 0x800 && input_interface(unsafe { &*iid }) {
        if let Some((object, table)) = word(out as usize).and_then(|object| word(object).map(|table| (object, table))) {
            let _one = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
            replace(table + 3 * std::mem::size_of::<usize>(), &INPUT_DEVICE, input_device as *const () as usize);
            let first = INPUT_ENUM.load(Ordering::Acquire) == 0;
            if replace(table + 4 * std::mem::size_of::<usize>(), &INPUT_ENUM, input_enum as *const () as usize) {
                let ansi = unsafe { (*iid).data1 } == 0xbf798030;
                if first { INPUT_ENUM_ANSI.store(ansi, Ordering::Release); }
                GAME_INPUT_ANSI.store(ansi, Ordering::Release);
                GAME_INPUT.store(object, Ordering::Release);
            }
        }
    }
    unsafe { SetLastError(saved) };
    result
}

fn input_interface(iid: &GUID) -> bool {
    matches!(iid.data1, 0xbf798030 | 0xbf798031) && iid.data2 == 0x483a && iid.data3 == 0x4da2
        && iid.data4 == [0xaa, 0x99, 0x5d, 0x64, 0xed, 0x36, 0x97, 0x00]
}

unsafe extern "system" fn input_device(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void, outer: *mut c_void) -> i32 {
    let original: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(INPUT_DEVICE.load(Ordering::Acquire)) };
    timed("DirectInput8.CreateDevice", || unsafe { original(this, iid, out, outer) })
}
unsafe extern "system" fn input_enum(this: *mut c_void, kind: u32, callback: *mut c_void, context: *mut c_void, flags: u32) -> i32 {
    let original: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut c_void, u32) -> i32 = unsafe { std::mem::transmute(INPUT_ENUM.load(Ordering::Acquire)) };
    timed("DirectInput8.EnumDevices", || {
        let incoming = unsafe { GetLastError() };
        let owner = GAME_INPUT.load(Ordering::Acquire);
        let ours = owner != 0 && owner == this as usize;
        let enabled = crate::ffxi_settings::controller_enabled();
        if INPUT_ENUM_CALLS.fetch_add(1, Ordering::Relaxed) < 4 {
            crate::log!("controller discovery: game query kind={kind} flags={flags} owned={ours} ansi={} enabled={enabled:?}", GAME_INPUT_ANSI.load(Ordering::Acquire));
        }
        unsafe { SetLastError(incoming) };
        if skip_disabled_controller_scan(ours, kind, flags, !callback.is_null(), enabled) {
            crate::log!("controller discovery: gamepad disabled; skipped game-controller scan");
            unsafe { SetLastError(incoming) };
            return 0;
        }
        if ours && GAME_INPUT_ANSI.load(Ordering::Acquire) && INPUT_ENUM_ANSI.load(Ordering::Acquire) {
            return unsafe { crate::controller_catalog::enumerate(original, this, kind, callback, context, flags) };
        }
        unsafe { original(this, kind, callback, context, flags) }
    })
}

fn skip_disabled_controller_scan(owned: bool, kind: u32, flags: u32, callback: bool, enabled: Option<bool>) -> bool {
    owned && kind == 4 && matches!(flags, 0 | 1) && callback && enabled == Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "read-only local DirectInput check; run alone, without game or sign-in"]
    fn local_disabled_gamepad_skips_scan_and_preserves_keyboard() {
        use windows_sys::Win32::{Foundation::FreeLibrary,
            System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW}};
        use crate::ffxi_settings::{FfxiSettings, SettingValue};
        unsafe extern "system" fn count(_: *const c_void, context: *mut c_void) -> i32 {
            unsafe { *(context as *mut usize) += 1 }; 1
        }
        let name: Vec<_> = "dinput8.dll\0".encode_utf16().collect();
        let module = unsafe { LoadLibraryW(name.as_ptr()) };
        assert!(!module.is_null());
        let create = unsafe { GetProcAddress(module, c"DirectInput8Create".as_ptr().cast()) }.unwrap();
        ORIGINAL[2].store(create as *const () as usize, Ordering::Release);
        crate::ffxi_settings::install(Some(FfxiSettings::new(vec![
            ("padmode000".into(), SettingValue::Text("0,0,0,0,1,0".into()))
        ]).unwrap()));
        let _run = Run::begin();
        let iid = GUID::from_u128(0xbf798030_483a_4da2_aa99_5d64ed369700);
        let mut object = std::ptr::null_mut();
        assert_eq!(unsafe { input(GetModuleHandleW(std::ptr::null()), 0x800, &iid, &mut object, std::ptr::null_mut()) }, 0);
        let table = word(object as usize).unwrap();
        let call: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut c_void, u32) -> i32 = unsafe { std::mem::transmute(word(table + 4 * std::mem::size_of::<usize>()).unwrap()) };
        for flags in [0, 1] {
            let mut received = 0usize;
            unsafe { SetLastError(4321) };
            let started = Instant::now();
            let lines = crate::log::capture(|| {
                assert_eq!(unsafe { call(object, 4, count as *const () as *mut c_void, (&mut received as *mut usize).cast(), flags) }, 0);
            });
            let elapsed = started.elapsed();
            assert_eq!(unsafe { GetLastError() }, 4321);
            assert_eq!(received, 0);
            assert!(lines.iter().any(|line| line.contains("gamepad disabled; skipped")));
            assert!(elapsed < Duration::from_millis(100));
            println!("disabled controller query flags={flags}: {} us", elapsed.as_micros());
        }
        // Device class 3 is keyboard. Compare the real provider with the
        // wrapped route: turning off the gamepad must not hide other inputs.
        let native: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut c_void, u32) -> i32 = unsafe { std::mem::transmute(INPUT_ENUM.load(Ordering::Acquire)) };
        let (mut expected, mut received) = (0usize, 0usize);
        let native_result = unsafe { native(object, 3, count as *const () as *mut c_void, (&mut expected as *mut usize).cast(), 1) };
        assert_eq!(unsafe { call(object, 3, count as *const () as *mut c_void, (&mut received as *mut usize).cast(), 1) }, native_result);
        assert_eq!(received, expected);
        assert_eq!(crate::ffxi_settings::controller_enabled(), Some(false));
        crate::ffxi_settings::install(Some(FfxiSettings::new(vec![
            ("padmode000".into(), SettingValue::Text("1,0,0,0,1,0".into()))
        ]).unwrap()));
        assert_eq!(crate::ffxi_settings::controller_enabled(), Some(true));
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(word(table + 2 * std::mem::size_of::<usize>()).unwrap()) };
        unsafe { release(object); FreeLibrary(module); }
    }

    #[test]
    fn disabled_scan_is_scoped_to_the_games_controller_query() {
        for flags in [0, 1] {
            assert!(skip_disabled_controller_scan(true, 4, flags, true, Some(false)));
        }
        for enabled in [None, Some(true)] {
            assert!(!skip_disabled_controller_scan(true, 4, 1, true, enabled));
        }
        for kind in [0, 1, 2, 3, 0x14, 0x15] {
            assert!(!skip_disabled_controller_scan(true, kind, 1, true, Some(false)));
        }
        assert!(!skip_disabled_controller_scan(false, 4, 1, true, Some(false)));
        assert!(!skip_disabled_controller_scan(true, 4, 1, false, Some(false)));
        assert!(!skip_disabled_controller_scan(true, 4, 0x100, true, Some(false)));
    }
    #[test]
    fn forwarding_keeps_results_and_last_error() {
        unsafe { SetLastError(2345) };
        let result = timed("test", || { assert_eq!(unsafe { GetLastError() }, 2345); unsafe { SetLastError(4567) }; -42 });
        assert_eq!(result, -42);
        assert_eq!(unsafe { GetLastError() }, 4567);
    }
    #[test]
    fn pointer_replacement_is_idempotent_and_preserves_other_hooks() {
        let slot = AtomicUsize::new(123);
        let original = AtomicUsize::new(0);
        let address = &slot as *const _ as usize;
        assert!(replace(address, &original, 456));
        assert_eq!(original.load(Ordering::Acquire), 123);
        assert_eq!(slot.load(Ordering::Acquire), 456);
        assert!(replace(address, &original, 456));
        slot.store(789, Ordering::Release);
        assert!(!replace(address, &original, 456));
        assert_eq!(slot.load(Ordering::Acquire), 789);
        assert!(!replace(0, &original, 456));
    }
}
