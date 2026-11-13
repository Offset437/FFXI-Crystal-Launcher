//! The saved DirectInput controller catalog the game host replays to the
//! game's own controller query (controller_catalog.rs). Shared by path with
//! the launcher, which is the only side that scans: DirectInput8Create inside
//! the host goes through Windower's Hook.dll detour (0x10064730), which creates
//! a mouse device on the calling thread (0x100647c0) and so re-homes
//! Windower's WH_MOUSE hook to a thread that then exits. The host only reads.
//!
//! The record is keyed by a fingerprint of the attached input hardware, the
//! connected XInput slots and the 32-bit system dinput8.dll the game uses.
//! Device names are hash input only, never stored. Local only.
use std::{fs::{self, File, OpenOptions}, io::Read, os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf}, time::{Duration, SystemTime, UNIX_EPOCH}};
use windows_sys::Win32::{
    Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
    System::LibraryLoader::{GetProcAddress, LoadLibraryExW},
    UI::Input::{GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME},
};

pub(crate) const SIZE: usize = 580; // DIDEVICEINSTANCEA (DirectInput 8): no pointers, the same on x64
const MAX_DEVICES: usize = 128;
const MAX_FILE: usize = 8 + 32 + 8 + 4 + SIZE * MAX_DEVICES + 32;
const MAGIC: &[u8; 8] = b"CRDI8A01";
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
#[repr(C, align(4))]
pub(crate) struct Instance(pub(crate) [u8; SIZE]);

fn hash(bytes: &[u8]) -> Option<[u8; 32]> {
    let mut result = [0; 32];
    (unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, bytes.as_ptr(), u32::try_from(bytes.len()).ok()?, result.as_mut_ptr(), 32) } >= 0).then_some(result)
}
pub(crate) fn root() -> Option<PathBuf> {
    std::env::var_os("CRYSTAL_DATA_ROOT").or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(|p| PathBuf::from(p).join("Crystal Launcher/controller-discovery"))
}
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn directory(get: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    // GetSystemWow64DirectoryW writes nothing into a 32K-character buffer.
    let mut buffer = vec![0u16; 1024];
    let n = unsafe { get(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    (n != 0 && n < buffer.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buffer[..n])))
}
/// This process's system folder (SysWOW64 as the 32-bit host sees System32).
fn system_dir() -> Option<PathBuf> {
    directory(windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW)
}
/// The dinput8.dll the 32-bit game loads, from either side.
fn game_dinput8() -> Option<PathBuf> {
    #[cfg(target_pointer_width = "64")]
    let folder = directory(windows_sys::Win32::System::SystemInformation::GetSystemWow64DirectoryW);
    #[cfg(not(target_pointer_width = "64"))]
    let folder = system_dir();
    Some(folder?.join("dinput8.dll"))
}

/// The game's own `padmode000` gamepad flag; None when malformed.
pub(crate) fn gamepad_enabled(padmode: &str) -> Option<bool> {
    let fields: Vec<_> = padmode.split(',').map(str::trim).collect();
    if fields.len() < 6 || !fields[..6].iter().all(|field| matches!(*field, "0" | "1")) { return None; }
    Some(fields[0] == "1")
}

/// Connected XInput slots, from the system xinput1_4.dll, loaded on first use
/// and kept; unavailable means no fingerprint, and so the game's own discovery.
fn xinput_mask() -> Option<u8> {
    type GetState = unsafe extern "system" fn(u32, *mut [u32; 4]) -> u32;
    static GET_STATE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let address = (*GET_STATE.get_or_init(|| {
        let path = system_dir()?.join("xinput1_4.dll");
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

/// Sorting removes enumeration-order changes. XInput connection bits cover
/// controllers which do not appear as ordinary Raw Input HID devices.
pub(crate) fn fingerprint() -> Option<[u8; 32]> {
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
    let metadata = fs::metadata(game_dinput8()?).ok()?;
    bytes.extend(metadata.len().to_le_bytes());
    bytes.extend(metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos().to_le_bytes());
    hash(&bytes)
}

/// Normalize the two strings instead of copying uninitialized bytes after
/// their terminators from a native callback buffer.
pub(crate) fn normalized(raw: &[u8; SIZE]) -> Option<Instance> {
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

#[cfg(any(test, target_pointer_width = "64"))]
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
pub(crate) fn cached(root: &Path, fingerprint: &[u8; 32]) -> Option<Vec<Instance>> {
    let file = File::open(root.join("attached.bin")).ok()?;
    let mut bytes = Vec::new(); file.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes).ok()?;
    decode(&bytes, fingerprint, now()?)
}
/// The scan turn: held by the scanner until its record is saved.
fn turn(root: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(root.join("scan.lock"))
}
/// Host only: a scan is saving a record now.
#[cfg(not(target_pointer_width = "64"))]
pub(crate) fn busy(root: &Path) -> bool { turn(root).is_err_and(|e| matches!(e.raw_os_error(), Some(32 | 33))) }

#[cfg(target_pointer_width = "64")]
fn save(root: &Path, fingerprint: &[u8; 32], devices: &[Instance]) -> Option<()> {
    use std::io::Write;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
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

/// Launcher only: a fresh record when the saved one no longer matches the
/// hardware. Blocks for the scan; Some(log line) when one was saved.
#[cfg(target_pointer_width = "64")]
pub(crate) fn refresh() -> Option<String> {
    use std::ffi::c_void;
    struct Capture { devices: Vec<Instance>, invalid: bool }
    unsafe extern "system" fn capture(device: *const c_void, context: *mut c_void) -> i32 {
        let state = unsafe { &mut *(context as *mut Capture) };
        if state.devices.len() == MAX_DEVICES || device.is_null() { state.invalid = true; return 0; }
        // SAFETY: DirectInput passes a live DIDEVICEINSTANCEA for the callback's duration.
        let raw = unsafe { std::ptr::read_unaligned(device as *const [u8; SIZE]) };
        match normalized(&raw) { Some(instance) => { state.devices.push(instance); 1 } None => { state.invalid = true; 0 } }
    }
    const IID: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0xbf798030_483a_4da2_aa99_5d64ed369700);
    let root = root()?;
    fs::create_dir_all(&root).ok()?;
    let before = fingerprint()?;
    if cached(&root, &before).is_some() { return None; }
    // Another launcher or an earlier start holds the turn; its record serves this one.
    let _turn = turn(&root).ok()?;
    if cached(&root, &before).is_some() { return None; }
    let module = unsafe { LoadLibraryExW(wide(&system_dir()?.join("dinput8.dll")).as_ptr(), std::ptr::null_mut(), 0) };
    if module.is_null() { return None; }
    let mut saved = None;
    if let Some(address) = unsafe { GetProcAddress(module, c"DirectInput8Create".as_ptr().cast()) } {
        let create: unsafe extern "system" fn(*mut c_void, u32, *const windows_sys::core::GUID, *mut *mut c_void, *mut c_void) -> i32 = unsafe { std::mem::transmute(address) };
        let instance = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) };
        let mut object = std::ptr::null_mut();
        if unsafe { create(instance, 0x800, &IID, &mut object, std::ptr::null_mut()) } >= 0 && !object.is_null() {
            let table = unsafe { *(object as *const *const usize) };
            let enumerate: unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut c_void, u32) -> i32 = unsafe { std::mem::transmute(*table.add(4)) };
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*table.add(2)) };
            let mut state = Capture { devices: Vec::new(), invalid: false };
            let started = std::time::Instant::now();
            // DI8DEVCLASS_GAMECTRL, DIEDFL_ATTACHEDONLY: the game's own query.
            let result = unsafe { enumerate(object, 4, capture as *const () as *mut c_void, (&mut state as *mut Capture).cast(), 1) };
            unsafe { release(object) };
            if result == 0 && !state.invalid && fingerprint() == Some(before) && save(&root, &before, &state.devices).is_some() {
                saved = Some(format!("controller discovery: catalog saved for the game; count={} elapsed_ms={}", state.devices.len(), started.elapsed().as_millis()));
            }
        }
    }
    unsafe { windows_sys::Win32::Foundation::FreeLibrary(module) };
    saved
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[allow(dead_code)] // used by the host's tests, not the launcher's
    pub(crate) fn instance() -> Instance {
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
    fn the_gamepad_flag_is_the_first_of_six_binary_fields() {
        assert_eq!(gamepad_enabled("1,0,0,0,1,0"), Some(true));
        assert_eq!(gamepad_enabled(" 0 ,1,1,1,1,1"), Some(false));
        assert_eq!(gamepad_enabled("0,0,0,0,0"), None);
        assert_eq!(gamepad_enabled("2,0,0,0,0,0"), None);
    }
    #[test]
    fn both_sides_fingerprint_the_32_bit_system_dinput8() {
        let path = game_dinput8().unwrap();
        assert!(path.is_file(), "{}", path.display());
        #[cfg(target_pointer_width = "64")]
        assert!(path.parent().unwrap().ends_with("SysWOW64"), "{}", path.display());
    }
    /// Then run the host's `local_catalog_matches_the_current_system_provider` (i686)
    /// with the same CRYSTAL_DATA_ROOT: it replays this record to a real 32-bit query.
    #[cfg(target_pointer_width = "64")]
    #[test]
    #[ignore = "read-only local controller enumeration; writes only under CRYSTAL_DATA_ROOT"]
    fn launcher_scan_saves_a_record_the_host_can_read() {
        let _ = fs::remove_file(root().unwrap().join("attached.bin"));
        let line = refresh().expect("a fresh record");
        println!("{line}");
        assert!(cached(&root().unwrap(), &fingerprint().unwrap()).is_some());
        assert!(refresh().is_none(), "a matching record is kept");
    }
}
