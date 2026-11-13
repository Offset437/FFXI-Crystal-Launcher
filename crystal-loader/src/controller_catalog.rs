//! The game's own controller query answered from the launcher's saved
//! DirectInput catalog (controller_record.rs), but only with the same live
//! device fingerprint and a successful attachment check for every device.
//! Other queries and changed/unknown hardware keep their native enumeration.
//! This host never creates a DirectInput object of its own (see
//! controller_record.rs for why). Local only: nothing here touches the network.
use std::{ffi::c_void, fs, time::{Duration, Instant}};
use windows_sys::{core::GUID, Win32::{
    Foundation::{GetLastError, SetLastError},
    System::LibraryLoader::{GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT},
}};
use crate::controller_record::{busy, cached, fingerprint, root, Instance};

type Callback = unsafe extern "system" fn(*const c_void, *mut c_void) -> i32;
pub(crate) type Enumerate = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut c_void, u32) -> i32;

fn system_provider(function: usize) -> bool {
    let Some(expected) = crate::win::system_dir().map(|dir| dir.join("dinput8.dll")) else { return false };
    let mut module = std::ptr::null_mut();
    if unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        function as *const u16, &mut module) } == 0 { return false; }
    let Some(actual) = crate::win::module_path(module) else { return false };
    // System32 and SysWOW64 can name the same redirected DLL to this 32-bit
    // process. Resolve filesystem aliases before comparing the provider.
    match (fs::canonicalize(expected), fs::canonicalize(actual)) {
        (Ok(expected), Ok(actual)) => expected.to_string_lossy().eq_ignore_ascii_case(&actual.to_string_lossy()),
        _ => false,
    }
}

fn attached(object: *mut c_void, devices: &[Instance]) -> bool {
    let table = unsafe { *(object as *const *const usize) };
    let status: unsafe extern "system" fn(*mut c_void, *const GUID) -> i32 = unsafe { std::mem::transmute(*table.add(5)) };
    devices.iter().all(|d| unsafe { status(object, d.0.as_ptr().add(4).cast()) } == 0)
}
fn replay(devices: &[Instance], callback: Callback, context: *mut c_void) {
    for device in devices { if unsafe { callback(device.0.as_ptr().cast(), context) } == 0 { break; } }
}

pub(crate) unsafe fn enumerate(original: Enumerate, object: *mut c_void, kind: u32, callback: *mut c_void, context: *mut c_void, flags: u32) -> i32 {
    let incoming = unsafe { GetLastError() };
    let provider = system_provider(original as *const () as usize);
    if kind == 4 && flags == 1 && !callback.is_null() && provider {
        if let (Some(root), Some(fingerprint)) = (root(), fingerprint()) {
            // The launcher's scan may still be running beside this host's start.
            // Never start a scan here; keep the native fallback if it fails,
            // is unsupported, or exceeds its bounded wait.
            let started = Instant::now();
            let reuse = |fingerprint: &[u8; 32]| cached(&root, fingerprint).filter(|d| attached(object, d));
            let mut devices = reuse(&fingerprint);
            if devices.is_none() && busy(&root) {
                // The scan saves attached.bin while it still holds scan.lock,
                // so wait on the lock probe and check the cache once. A
                // plug/unplug while waiting goes straight to the native path.
                let mut unchanged = true;
                while busy(&root) && started.elapsed() < Duration::from_secs(12) {
                    std::thread::sleep(Duration::from_millis(20));
                    if crate::controller_record::fingerprint() != Some(fingerprint) {
                        unchanged = false;
                        break;
                    }
                }
                // Reused only under the request's own, still-current fingerprint.
                if unchanged {
                    devices = reuse(&fingerprint);
                }
            }
            if let Some(devices) = devices {
                crate::log!("controller discovery: reused {} verified native device record(s); wait_ms={}", devices.len(), started.elapsed().as_millis());
                unsafe { SetLastError(incoming) };
                replay(&devices, unsafe { std::mem::transmute::<*mut c_void, Callback>(callback) }, context);
                return 0;
            }
        }
    }
    crate::log!("controller discovery: native fallback; kind={kind} flags={flags} system_provider={provider}");
    unsafe { SetLastError(incoming); original(object, kind, callback, context, flags) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_record::{tests::instance, SIZE};
    #[test]
    fn replay_runs_callbacks_on_the_calling_thread_and_honors_stop() {
        struct State { count: usize, thread: std::thread::ThreadId }
        unsafe extern "system" fn callback(device: *const c_void, state: *mut c_void) -> i32 {
            let state = unsafe { &mut *(state as *mut State) };
            assert_eq!(std::thread::current().id(), state.thread);
            assert_eq!(unsafe { *(device as *const u32) }, SIZE as u32);
            state.count += 1; 0
        }
        let mut state = State { count: 0, thread: std::thread::current().id() };
        replay(&[instance(), instance()], callback, (&mut state as *mut State).cast());
        assert_eq!(state.count, 1);
    }
    #[test]
    #[ignore = "read-only local controller enumeration after a launcher scan; never starts a game or signs in"]
    fn local_catalog_matches_the_current_system_provider() {
        use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryExW}};
        let root = root().unwrap(); let fingerprint = fingerprint().expect("current hardware fingerprint");
        let expected = cached(&root, &fingerprint).expect("run the launcher's controller_record scan first").len();
        unsafe extern "system" fn count(_device: *const c_void, context: *mut c_void) -> i32 {
            unsafe { *(context as *mut usize) += 1 }; 1
        }
        // Exercise the actual interception path with a real system object,
        // including provider validation, current hardware and callback replay.
        let path = crate::win::system_dir().unwrap().join("dinput8.dll");
        let module = unsafe { LoadLibraryExW(crate::win::wide_os(&path).as_ptr(), std::ptr::null_mut(), 0) };
        assert!(!module.is_null());
        let address = unsafe { GetProcAddress(module, c"DirectInput8Create".as_ptr().cast()) }.unwrap();
        let create: unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(address) };
        let iid = GUID::from_u128(0xbf798030_483a_4da2_aa99_5d64ed369700);
        let mut object = std::ptr::null_mut();
        assert_eq!(unsafe { create(GetModuleHandleW(std::ptr::null()), 0x800, &iid, &mut object, std::ptr::null_mut()) }, 0);
        let table = unsafe { *(object as *const *const usize) };
        let native: Enumerate = unsafe { std::mem::transmute(*table.add(4)) };
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*table.add(2)) };
        let mut received = 0usize;
        let started = Instant::now();
        assert_eq!(unsafe { enumerate(native, object, 4, count as *const () as *mut c_void, (&mut received as *mut usize).cast(), 1) }, 0);
        let elapsed = started.elapsed();
        assert_eq!(received, expected);
        unsafe { release(object); FreeLibrary(module); }
        println!("real DirectInput cached enumeration: {} us, {received} device(s)", elapsed.as_micros());
        assert!(elapsed < Duration::from_millis(100), "the cached query still blocked on native discovery");
    }
}
