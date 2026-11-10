//! Native DirectInput discovery outside the game-start critical path. A
//! background scan during sign-in (or a saved scan from an earlier launch)
//! records the system provider's attached game controllers; the game's own
//! query then gets that record replayed, but only with the same live device
//! fingerprint and a successful attachment check for every device. Other
//! queries and changed/unknown hardware keep their native enumeration.
//! Local only: nothing here touches the network.
use std::{ffi::c_void, fs::{self, File, OpenOptions}, io::{Read, Write}, os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf}, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use windows_sys::{core::GUID, Win32::{
    Foundation::{FreeLibrary, GetLastError, SetLastError},
    Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
    Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING},
    System::LibraryLoader::{GetModuleHandleExW,
        GetModuleHandleW, GetProcAddress, LoadLibraryExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT},
    UI::Input::{GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME},
}};

const SIZE: usize = 580; // DIDEVICEINSTANCEA (DirectInput 8)
const MAX_DEVICES: usize = 128;
const MAX_FILE: usize = 8 + 32 + 8 + 4 + SIZE * MAX_DEVICES + 32;
const MAGIC: &[u8; 8] = b"CRDI8A01";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const IID: GUID = GUID::from_u128(0xbf798030_483a_4da2_aa99_5d64ed369700);
static STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq, Eq)]
#[repr(C, align(4))]
struct Instance([u8; SIZE]);
type Callback = unsafe extern "system" fn(*const c_void, *mut c_void) -> i32;
pub(crate) type Enumerate = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut c_void, u32) -> i32;

fn hash(bytes: &[u8]) -> Option<[u8; 32]> {
    let mut result = [0; 32];
    (unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, bytes.as_ptr(), u32::try_from(bytes.len()).ok()?, result.as_mut_ptr(), 32) } >= 0).then_some(result)
}
fn root() -> Option<PathBuf> {
    std::env::var_os("CRYSTAL_DATA_ROOT").or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(|p| PathBuf::from(p).join("Crystal Launcher/controller-discovery"))
}
fn system_dll() -> Option<PathBuf> {
    crate::win::system_dir().map(|dir| dir.join("dinput8.dll"))
}
fn wide(path: &Path) -> Vec<u16> { crate::win::wide_os(path) }

fn system_provider(function: usize) -> bool {
    let Some(expected) = system_dll() else { return false };
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

/// Connected XInput slots, from the system xinput1_4.dll. Loaded on first use
/// so a host whose gamepad is disabled never loads it; unavailable means no
/// fingerprint, and so the game's own discovery.
fn xinput_mask() -> Option<u8> {
    type GetState = unsafe extern "system" fn(u32, *mut [u32; 4]) -> u32;
    static GET_STATE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let address = (*GET_STATE.get_or_init(|| {
        let path = crate::win::system_dir()?.join("xinput1_4.dll");
        // Kept loaded for the process lifetime, like the game's own input DLLs.
        let module = unsafe { LoadLibraryExW(wide(&path).as_ptr(), std::ptr::null_mut(), 0) };
        if module.is_null() { return None; }
        unsafe { GetProcAddress(module, c"XInputGetState".as_ptr().cast()) }.map(|f| f as usize)
    }))?;
    let get_state: GetState = unsafe { std::mem::transmute(address) };
    let mut mask = 0u8;
    for i in 0..4 {
        // XINPUT_STATE: a u32 packet number and the 12-byte XINPUT_GAMEPAD.
        let mut state = [0u32; 4];
        match unsafe { get_state(i, &mut state) } { 0 => mask |= 1 << i, 1167 => {}, _ => return None }
    }
    Some(mask)
}

/// Device names are used only as hash input, never logged or stored in the
/// cache. Sorting removes enumeration-order changes. XInput connection bits
/// cover controllers which do not appear as ordinary Raw Input HID devices.
fn fingerprint() -> Option<[u8; 32]> {
    let mut count = 0;
    let size = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
    if unsafe { GetRawInputDeviceList(std::ptr::null_mut(), &mut count, size) } == u32::MAX || count > 2048 { return None; }
    let mut devices: Vec<_> = (0..count).map(|_| RAWINPUTDEVICELIST::default()).collect();
    if count != 0 {
        let n = unsafe { GetRawInputDeviceList(devices.as_mut_ptr(), &mut count, size) };
        if n == u32::MAX || n as usize > devices.len() { return None; }
        devices.truncate(n as usize);
    }
    let mut names = Vec::new();
    for device in devices {
        let mut length = 0;
        if unsafe { GetRawInputDeviceInfoW(device.hDevice, RIDI_DEVICENAME, std::ptr::null_mut(), &mut length) } == u32::MAX
            || length == 0 || length > 4096 { return None; }
        let mut name = vec![0u16; length as usize + 1];
        let n = unsafe { GetRawInputDeviceInfoW(device.hDevice, RIDI_DEVICENAME, name.as_mut_ptr().cast(), &mut length) };
        if n == u32::MAX || n as usize > name.len() { return None; }
        name.truncate(n as usize);
        let mut bytes = device.dwType.to_le_bytes().to_vec();
        bytes.extend(name.iter().flat_map(|n| n.to_le_bytes()));
        names.push(bytes);
    }
    names.sort();
    let mut bytes = Vec::new();
    for name in names { bytes.extend((name.len() as u32).to_le_bytes()); bytes.extend(name); }
    bytes.push(xinput_mask()?);
    let metadata = fs::metadata(system_dll()?).ok()?;
    bytes.extend(metadata.len().to_le_bytes());
    bytes.extend(metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos().to_le_bytes());
    hash(&bytes)
}

/// Normalize the two strings instead of copying uninitialized bytes after
/// their terminators from a native callback buffer.
fn normalized(raw: &[u8; SIZE]) -> Option<Instance> {
    if u32::from_le_bytes(raw[..4].try_into().ok()?) != SIZE as u32 { return None; }
    let mut result = Instance([0; SIZE]);
    result.0[..40].copy_from_slice(&raw[..40]);
    for start in [40, 300] {
        let n = raw[start..start + 260].iter().position(|b| *b == 0)?;
        result.0[start..start + n].copy_from_slice(&raw[start..start + n]);
    }
    result.0[560..].copy_from_slice(&raw[560..]);
    Some(result)
}

struct Capture { devices: Vec<Instance>, invalid: bool }
unsafe extern "system" fn capture(device: *const c_void, context: *mut c_void) -> i32 {
    let state = unsafe { &mut *(context as *mut Capture) };
    if state.devices.len() == MAX_DEVICES { state.invalid = true; return 0; }
    let mut raw = [0u8; SIZE];
    if !crate::mem::read_into(device as usize, &mut raw) { state.invalid = true; return 0; }
    match normalized(&raw) { Some(instance) => state.devices.push(instance), None => { state.invalid = true; return 0; } }
    1
}

fn encode(fingerprint: &[u8; 32], devices: &[Instance], now: u64) -> Option<Vec<u8>> {
    if devices.len() > MAX_DEVICES { return None; }
    let mut bytes = MAGIC.to_vec(); bytes.extend(fingerprint); bytes.extend(now.to_le_bytes()); bytes.extend((devices.len() as u32).to_le_bytes());
    for device in devices { bytes.extend(&device.0); }
    bytes.extend(hash(&bytes)?); Some(bytes)
}
fn decode(bytes: &[u8], fingerprint: &[u8; 32], now: u64) -> Option<Vec<Instance>> {
    if bytes.len() < 84 || bytes.len() > MAX_FILE || &bytes[..8] != MAGIC || &bytes[8..40] != fingerprint { return None; }
    let created = u64::from_le_bytes(bytes[40..48].try_into().ok()?);
    if now.checked_sub(created)? > MAX_AGE.as_secs() { return None; }
    let count = u32::from_le_bytes(bytes[48..52].try_into().ok()?) as usize;
    if count > MAX_DEVICES || bytes.len() != 84 + count * SIZE { return None; }
    if hash(&bytes[..bytes.len() - 32])?.as_slice() != &bytes[bytes.len() - 32..] { return None; }
    bytes[52..bytes.len() - 32].as_chunks::<SIZE>().0.iter().map(normalized).collect()
}
fn now() -> Option<u64> { Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs()) }
fn cached(root: &Path, fingerprint: &[u8; 32]) -> Option<Vec<Instance>> {
    let file = File::open(root.join("attached.bin")).ok()?;
    let mut bytes = Vec::new(); file.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes).ok()?;
    decode(&bytes, fingerprint, now()?)
}
fn turn(root: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(root.join("scan.lock"))
}
fn busy(root: &Path) -> bool { turn(root).is_err_and(|e| matches!(e.raw_os_error(), Some(32 | 33))) }
fn save(root: &Path, fingerprint: &[u8; 32], devices: &[Instance]) -> Option<()> {
    let bytes = encode(fingerprint, devices, now()?)?;
    let temp = root.join(format!("attached-{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(&temp)?;
        file.write_all(&bytes)?; file.sync_all()?; drop(file);
        if unsafe { MoveFileExW(wide(&temp).as_ptr(), wide(&root.join("attached.bin")).as_ptr(), MOVEFILE_REPLACE_EXISTING) } == 0 { return Err(std::io::Error::last_os_error()); }
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(temp); }
    result.ok()
}

pub(crate) fn prefetch() {
    if crate::ffxi_settings::controller_enabled() == Some(false) {
        crate::log!("controller discovery: gamepad disabled; no background controller scan needed");
        return;
    }
    if STARTED.swap(true, Ordering::AcqRel) { return; }
    let _ = std::thread::Builder::new().name("controller-discovery".into()).spawn(|| {
        let Some(root) = root() else { return };
        if fs::create_dir_all(&root).is_err() { return; }
        let Some(before) = fingerprint() else { return };
        if cached(&root, &before).is_some() { crate::log!("controller discovery: saved native catalog matches the connected devices"); return; }
        // Another host may hold the scan turn; its catalog serves this host too.
        let Ok(_turn) = turn(&root) else { return };
        if cached(&root, &before).is_some() { crate::log!("controller discovery: another host just saved a matching native catalog"); return; }
        let Some(path) = system_dll() else { return };
        // Absolute system DLL, never a game-folder replacement or another injector.
        let module = unsafe { LoadLibraryExW(wide(&path).as_ptr(), std::ptr::null_mut(), 0) };
        if module.is_null() { return; }
        let Some(address) = (unsafe { GetProcAddress(module, c"DirectInput8Create".as_ptr().cast()) }) else { unsafe { FreeLibrary(module) }; return };
        let create: unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(address) };
        let mut object = std::ptr::null_mut();
        if unsafe { create(GetModuleHandleW(std::ptr::null()), 0x800, &IID, &mut object, std::ptr::null_mut()) } >= 0 && !object.is_null() {
            let table = unsafe { *(object as *const *const usize) };
            let enumerate: Enumerate = unsafe { std::mem::transmute(*table.add(4)) };
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*table.add(2)) };
            let mut state = Capture { devices: Vec::new(), invalid: false };
            let started = Instant::now();
            let result = unsafe { enumerate(object, 4, capture as *const () as *mut c_void, (&mut state as *mut Capture).cast(), 1) };
            unsafe { release(object) };
            if result == 0 && !state.invalid && fingerprint() == Some(before) && save(&root, &before, &state.devices).is_some() {
                crate::log!("controller discovery: native attached catalog ready; count={} elapsed_ms={}", state.devices.len(), started.elapsed().as_millis());
            }
        }
        unsafe { FreeLibrary(module) };
    });
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
            // A scan already overlapping sign-in may be about to finish. Never
            // start a second full scan alongside it; retain the native fallback
            // if it fails, is unsupported, or exceeds its bounded wait.
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
                    if self::fingerprint() != Some(fingerprint) {
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
    fn instance() -> Instance {
        let mut raw = [0xaau8; SIZE]; raw[..4].copy_from_slice(&(SIZE as u32).to_le_bytes());
        raw[40..45].copy_from_slice(b"test\0"); raw[300..305].copy_from_slice(b"test\0");
        normalized(&raw).unwrap()
    }
    #[test]
    fn native_catalog_rejects_changed_hardware_expiry_damage_and_truncation() {
        let devices = vec![instance()]; let fingerprint = [4; 32];
        let bytes = encode(&fingerprint, &devices, 100).unwrap();
        assert_eq!(decode(&bytes, &fingerprint, 101), Some(devices));
        assert!(decode(&bytes, &[5; 32], 101).is_none());
        assert!(decode(&bytes, &fingerprint, 99).is_none());
        assert!(decode(&bytes, &fingerprint, 100 + MAX_AGE.as_secs() + 1).is_none());
        let mut damaged = bytes.clone(); damaged[60] ^= 1;
        assert!(decode(&damaged, &fingerprint, 101).is_none());
        assert!(decode(&bytes[..bytes.len()-1], &fingerprint, 101).is_none());
        let empty = encode(&fingerprint, &[], 100).unwrap();
        assert_eq!(decode(&empty, &fingerprint, 100), Some(vec![]));
    }
    #[test]
    fn callback_strings_are_normalized_without_uninitialized_tails() {
        let device = instance();
        assert!(device.0[45..300].iter().chain(&device.0[305..560]).all(|b| *b == 0));
        let mut bad = device.0; bad[40..300].fill(1);
        assert!(normalized(&bad).is_none());
        let mut bad = device.0; bad[..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(normalized(&bad).is_none());
    }
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
    #[ignore = "read-only local controller enumeration; never starts a game or signs in"]
    fn local_catalog_matches_the_current_system_provider() {
        prefetch();
        let until = Instant::now() + Duration::from_secs(40);
        let root = root().unwrap(); let fingerprint = fingerprint().expect("current hardware fingerprint");
        while cached(&root, &fingerprint).is_none() {
            assert!(Instant::now() < until, "controller discovery did not produce a catalog");
            std::thread::sleep(Duration::from_millis(50));
        }
        let started = Instant::now(); assert!(cached(&root, &fingerprint).is_some());
        println!("verified cached controller read: {} us", started.elapsed().as_micros());
        let expected = cached(&root, &fingerprint).unwrap().len();
        unsafe extern "system" fn count(_device: *const c_void, context: *mut c_void) -> i32 {
            unsafe { *(context as *mut usize) += 1 }; 1
        }
        // Exercise the actual interception path with a real system object,
        // including provider validation, current hardware and callback replay.
        let module = unsafe { LoadLibraryExW(wide(&system_dll().unwrap()).as_ptr(), std::ptr::null_mut(), 0) };
        assert!(!module.is_null());
        let address = unsafe { GetProcAddress(module, c"DirectInput8Create".as_ptr().cast()) }.unwrap();
        let create: unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(address) };
        let mut object = std::ptr::null_mut();
        assert_eq!(unsafe { create(GetModuleHandleW(std::ptr::null()), 0x800, &IID, &mut object, std::ptr::null_mut()) }, 0);
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
