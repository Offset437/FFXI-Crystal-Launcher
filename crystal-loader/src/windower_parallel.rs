//! Private, parallel Windower startup. Only the two startup object names in
//! the verified, installed Hook image are redirected, in this process's memory.
//! Its startup data, initialization code and the native POL owner are unchanged.

use crate::windower_bootstrap_contract as layout;
use std::{
    ffi::c_void,
    io::Read,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, ERROR_ALREADY_EXISTS, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        LibraryLoader::{
            GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
        },
        Memory::{
            CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, VirtualProtect, FILE_MAP_WRITE,
            PAGE_READWRITE,
        },
        Threading::{CreateEventW, WaitForSingleObject},
    },
};

use crate::win::wide;

struct Objects {
    /// Held open until Hook has read the settings.
    _mapping: OwnedHandle,
    event: OwnedHandle,
}
impl Objects {
    fn new(settings: &[u8], map: &str, event: &str) -> Result<Self, String> {
        let map = wide(map);
        let event = wide(event);
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                layout::SETTINGS_LEN as u32,
                map.as_ptr(),
            );
            if handle.is_null() {
                return Err("Could not create the private Windower settings mapping.".into());
            }
            let existed = GetLastError() == ERROR_ALREADY_EXISTS;
            let mapping = OwnedHandle::from_raw_handle(handle);
            if existed {
                return Err("The private Windower mapping name is already in use.".into());
            }
            let view = MapViewOfFile(
                mapping.as_raw_handle(),
                FILE_MAP_WRITE,
                0,
                0,
                layout::SETTINGS_LEN,
            );
            if view.Value.is_null() {
                return Err("Could not map private Windower settings.".into());
            }
            std::ptr::copy_nonoverlapping(settings.as_ptr(), view.Value as *mut u8, settings.len());
            UnmapViewOfFile(view);
            let handle = CreateEventW(std::ptr::null(), 0, 0, event.as_ptr());
            if handle.is_null() {
                return Err("Could not create the private Windower completion event.".into());
            }
            let existed = GetLastError() == ERROR_ALREADY_EXISTS;
            let event = OwnedHandle::from_raw_handle(handle);
            if existed {
                return Err("The private Windower event name is already in use.".into());
            }
            Ok(Self { _mapping: mapping, event })
        }
    }
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}
#[repr(C)]
struct Loaded {
    flags: u32,
    full_name: *const UnicodeString,
    base_name: *const UnicodeString,
    base: *mut c_void,
    size: u32,
}
/// What the load callback needs, built before Hook loads. `result`: 0 not
/// seen, 1 redirected, 2 refused.
struct Context<'a> {
    startup: &'a layout::HookStartup,
    map: &'a [u8],
    event: &'a [u8],
    result: AtomicU32,
}
type Notify = unsafe extern "system" fn(u32, *const Loaded, *mut c_void);
type Register = unsafe extern "system" fn(u32, Notify, *mut c_void, *mut *mut c_void) -> i32;
type Unregister = unsafe extern "system" fn(*mut c_void) -> i32;

// Windows invokes this under its loader lock, before Hook's own initialization
// runs. It touches no heap, takes no locks, loads no DLLs and cannot panic: it
// only calls VirtualProtect on two data strings in the image being loaded.
unsafe extern "system" fn notification(reason: u32, data: *const Loaded, context: *mut c_void) {
    if reason != 1 || data.is_null() || context.is_null() {
        return;
    }
    let data = unsafe { &*data };
    if data.base_name.is_null() {
        return;
    }
    let name = unsafe { &*data.base_name };
    if name.length != 16 || name.buffer.is_null() {
        return;
    }
    let expected = *b"hook.dll";
    let mut index = 0;
    while index < 8 {
        let mut ch = unsafe { std::ptr::read_volatile(name.buffer.add(index)) };
        if (65..=90).contains(&ch) {
            ch += 32;
        }
        if ch != expected[index] as u16 {
            return;
        }
        index += 1;
    }
    let context = unsafe { &*(context as *const Context) };
    // Only the first hook.dll image is examined; a later module with the same
    // base name is never touched and cannot overwrite the first result.
    if context.result.load(Ordering::Acquire) != 0 {
        return;
    }
    let ok = unsafe { redirect(data.base as usize, data.size as usize, context.startup, context.map, context.event) };
    let _ = context.result.compare_exchange(0, if ok { 1 } else { 2 }, Ordering::AcqRel, Ordering::Acquire);
}

fn padded(text: &str, slot: usize) -> Vec<u8> {
    let mut b = vec![0; slot + 1];
    b[..text.len()].copy_from_slice(text.as_bytes());
    b
}

/// The loaded image is the verified file (same PE identity) and still holds
/// both original names where the file scan found them; only then are the two
/// data strings replaced. Nothing else in the image changes.
unsafe fn redirect(base: usize, size: usize, startup: &layout::HookStartup, map: &[u8], event: &[u8]) -> bool {
    if size != startup.image_len {
        return false;
    }
    // Runs inside the loader callback: no indexing or arithmetic that can panic.
    let image = unsafe { std::slice::from_raw_parts(base as *const u8, size) };
    let u32_at = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = image.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    };
    let Some(pe) = u32_at(0x3c).map(|n| n as usize) else {
        return false;
    };
    if u32_at(pe) != Some(0x4550) || pe.checked_add(8).and_then(u32_at) != Some(startup.timestamp) {
        return false;
    }
    let slots = [
        (startup.map_name_rva, layout::MAP_NAME, map),
        (startup.event_name_rva, layout::EVENT_NAME, event),
    ];
    for (rva, original, replacement) in slots {
        let Some(end) = rva.checked_add(original.len()) else {
            return false;
        };
        if image.get(rva..end) != Some(original.as_bytes())
            || image.get(end) != Some(&0)
            || replacement.len() != original.len() + 1
        {
            return false;
        }
    }
    // Each name lies in a non-executable data section (checked by the scan).
    for (rva, _, replacement) in slots {
        let at = base + rva;
        let mut old = 0;
        if unsafe { VirtualProtect(at as *const c_void, replacement.len(), PAGE_READWRITE, &mut old) } == 0 {
            return false;
        }
        unsafe { std::ptr::copy_nonoverlapping(replacement.as_ptr(), at as *mut u8, replacement.len()) };
        let mut ignored = 0;
        if unsafe { VirtualProtect(at as *const c_void, replacement.len(), old, &mut ignored) } == 0 {
            return false;
        }
    }
    true
}

/// Hook readiness is established before the caller accepts any credentials.
/// Unsupported images and initialization failures end this host without auth.
pub fn initialize(hook: &Path, settings: &[u8], nonce: &str) -> Result<Duration, String> {
    initialize_with(hook, settings, nonce, |_| {})
}

/// `after_load` runs once Hook.dll is loaded (its base address), before the
/// readiness wait; the credential-free probe uses it to trace Hook's startup.
pub fn initialize_with(hook: &Path, settings: &[u8], nonce: &str, after_load: impl FnOnce(usize)) -> Result<Duration, String> {
    let verification_started = Instant::now();
    layout::validate(settings).map_err(str::to_owned)?;
    let (map_name, event_name) =
        layout::private_names(nonce).ok_or("Invalid private Windower startup nonce.")?;
    let mut verified_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(hook)
        .map_err(|_| "Could not hold the installed Windower Hook for verification.")?;
    let mut bytes = Vec::new();
    (&mut verified_file)
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the installed Windower Hook.")?;
    let startup = match layout::hook_startup(&bytes) {
        Ok(startup) => startup,
        Err(reason) => {
            log!("note: private Windower startup unavailable for this Hook.dll: {reason}");
            return Err("The installed Windower Hook is not a build private startup supports.".into());
        }
    };
    log!("private Windower timing: file verification {:.3}s (Hook {:08X}, {}, names at {:#x}/{:#x})",
        verification_started.elapsed().as_secs_f64(), startup.timestamp,
        layout::known_hook(&startup).map_or("found by signature".into(), |v| format!("known {v}, signature scan matches")),
        startup.map_name_rva, startup.event_name_rva);
    let objects = Objects::new(settings, &map_name, &event_name)?;
    let (map, event) = (
        padded(&map_name, layout::MAP_NAME.len()),
        padded(&event_name, layout::EVENT_NAME.len()),
    );
    // Outlives the callback: it is unregistered right after LoadLibraryExW.
    let context = Context { startup: &startup, map: &map, event: &event, result: AtomicU32::new(0) };
    let library = unsafe { GetModuleHandleW(wide("ntdll.dll").as_ptr()) };
    let register = unsafe { GetProcAddress(library, c"LdrRegisterDllNotification".as_ptr() as _) }
        .ok_or("DLL load notifications are unavailable.")?;
    let unregister =
        unsafe { GetProcAddress(library, c"LdrUnregisterDllNotification".as_ptr() as _) }
            .ok_or("DLL load notifications are unavailable.")?;
    let register: Register = unsafe { std::mem::transmute(register) };
    let unregister: Unregister = unsafe { std::mem::transmute(unregister) };
    let mut cookie = std::ptr::null_mut();
    if unsafe { register(0, notification, &context as *const Context as *mut c_void, &mut cookie) } < 0 {
        return Err("Could not observe Windower initialization.".into());
    }
    let path: Vec<u16> = hook.as_os_str().encode_wide().chain(Some(0)).collect();
    let began = Instant::now();
    let module = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
    };
    let loaded = began.elapsed();
    unsafe { unregister(cookie) };
    if module.is_null() {
        return Err("Windower Hook could not be loaded in this host.".into());
    }
    // Retain the loaded Hook for the process lifetime, like its normal injector.
    if context.result.load(Ordering::Acquire) != 1 {
        return Err("Windower's private startup objects could not be established.".into());
    }
    // Hook's own-process thread walks, served without listing every thread on
    // the PC (windower_threads.rs).
    unsafe { crate::windower_threads::install(module as usize) };
    after_load(module as usize);
    let waited = unsafe { WaitForSingleObject(objects.event.as_raw_handle(), 30_000) };
    crate::windower_threads::report("Hook startup wait");
    if waited != WAIT_OBJECT_0 {
        return Err("Windower did not complete its private startup handshake.".into());
    }
    log!("private Windower timing: DLL load {:.3}s, readiness wait {:.3}s",
        loaded.as_secs_f64(), began.elapsed().saturating_sub(loaded).as_secs_f64());
    Ok(began.elapsed())
}
