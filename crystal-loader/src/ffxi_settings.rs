//! Per-process FINAL FANTASY XI settings (`ffxi_settings` in the session).
//!
//! FFXiMain reads `HKLM\SOFTWARE\PlayOnline<region>\SquareEnix\FinalFantasyXI\0000..0045`
//! itself, through its own ADVAPI32 imports, once, inside `FFXiGameMain`.
//! It opens the key with `RegCreateKeyExA`. So the loader
//! rewrites FFXiMain's import entries for the registry calls as soon as FFXi.dll has loaded it
//! and answers queries for the session's names on that key from memory. Everything else goes
//! through to whatever the import entry held before (the real registry, or an injector's own
//! hook). Nothing outside this process changes.

use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use windows_sys::core::{GUID, HRESULT, PCSTR};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, REG_DWORD, REG_SAM_FLAGS, REG_SZ};

use crate::log;

/// At most this many values, names of at most this many bytes, strings of at most this many.
pub const MAX_SETTINGS: usize = 256;
pub const MAX_NAME: usize = 64;
pub const MAX_STRING: usize = 1024;

const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_MORE_DATA: u32 = 234;

/// One value as the session gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// A JSON number: `REG_DWORD` (negative numbers are stored as their two's complement).
    Dword(u32),
    /// A JSON string: `REG_SZ`, in the active ANSI code page.
    Text(String),
}

/// The session's `ffxi_settings`, validated: names unique ignoring ASCII case, as registry
/// value names are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FfxiSettings {
    entries: Vec<(String, SettingValue)>,
}

impl FfxiSettings {
    pub fn new(entries: Vec<(String, SettingValue)>) -> Result<Self, &'static str> {
        if entries.len() > MAX_SETTINGS {
            return Err("too many values");
        }
        for (i, (name, value)) in entries.iter().enumerate() {
            if !valid_name(name) {
                return Err("value names must be 1-64 printable ASCII characters without a backslash");
            }
            if entries[..i].iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                return Err("a value name appears twice");
            }
            if let SettingValue::Text(s) = value {
                if s.len() > MAX_STRING || s.contains('\0') {
                    return Err("strings must be at most 1024 bytes, without NUL");
                }
            }
        }
        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&SettingValue> {
        self.entries.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(n, _)| n.as_str())
    }
}

pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len()) && name.bytes().all(|b| (b.is_ascii_graphic() && b != b'\\') || b == b' ')
}

/// Whether `sub` (opened under `HKEY_LOCAL_MACHINE`) is FFXI's settings key:
/// `SOFTWARE\PlayOnline{,US,EU}\{Square,SquareEnix,SQUARE}\FinalFantasyXI`, any case, with or
/// without `WOW6432Node`.
pub fn is_settings_key(sub: &[u8]) -> bool {
    let mut parts: Vec<&[u8]> = sub.split(|&b| b == b'\\').filter(|p| !p.is_empty()).collect();
    if parts.get(1).is_some_and(|p| p.eq_ignore_ascii_case(b"WOW6432Node")) {
        parts.remove(1);
    }
    matches!(parts.as_slice(), [software, root, vendor, game]
        if software.eq_ignore_ascii_case(b"SOFTWARE")
            && [&b"PlayOnline"[..], b"PlayOnlineUS", b"PlayOnlineEU"].iter().any(|r| root.eq_ignore_ascii_case(r))
            && (vendor.eq_ignore_ascii_case(b"SquareEnix") || vendor.eq_ignore_ascii_case(b"Square"))
            && game.eq_ignore_ascii_case(b"FinalFantasyXI"))
}

// --- what the wrappers serve -------------------------------------------------

/// One value as `RegQueryValueExA` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegData {
    name: String,
    ty: u32,
    data: Vec<u8>,
}

struct Store {
    values: Vec<RegData>,
    /// HKEYs FFXiMain opened on the settings key, still open.
    keys: Vec<usize>,
    reads: usize,
    writes: usize,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);
static FFXIMAIN_DONE: AtomicBool = AtomicBool::new(false);
static REGISTRY_DONE: AtomicBool = AtomicBool::new(false);
static REGISTRY_SLOTS: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];
static KEY_LOGGED: AtomicBool = AtomicBool::new(false);

fn store() -> std::sync::MutexGuard<'static, Option<Store>> {
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

fn reg_data(name: &str, value: &SettingValue, ansi: &dyn Fn(&str) -> Vec<u8>) -> RegData {
    match value {
        SettingValue::Dword(v) => RegData { name: name.to_owned(), ty: REG_DWORD, data: v.to_le_bytes().to_vec() },
        SettingValue::Text(s) => {
            let mut data = ansi(s);
            data.push(0);
            RegData { name: name.to_owned(), ty: REG_SZ, data }
        }
    }
}

/// Empty settings track the key while delegating all values to the prior
/// registry/tool functions. Live controller changes can add overrides later.
pub fn install(settings: Option<FfxiSettings>) {
    let Some(settings) = settings else { return };
    install_with(&settings, &to_ansi);
}

fn install_with(settings: &FfxiSettings, ansi: &dyn Fn(&str) -> Vec<u8>) {
    let values = settings.entries.iter().map(|(n, v)| reg_data(n, v, ansi)).collect();
    *store() = Some(Store { values, keys: Vec::new(), reads: 0, writes: 0 });
}

/// True when the session carried settings to serve.
pub fn active() -> bool {
    store().is_some()
}

/// The game's own enable flag, rather than Windower's background-input flag.
/// Unknown or malformed values keep the normal DirectInput discovery path.
pub(crate) fn controller_enabled() -> Option<bool> {
    let state = store();
    controller_enabled_value(&state.as_ref()?.values)
}

fn controller_enabled_value(values: &[RegData]) -> Option<bool> {
    let value = values.iter().find(|value| value.name.eq_ignore_ascii_case("padmode000"))?;
    if value.ty != REG_SZ { return None; }
    let text = std::str::from_utf8(value.data.split(|byte| *byte == 0).next()?).ok()?;
    let fields: Vec<_> = text.split(',').map(str::trim).collect();
    if fields.len() < 6 || !fields[..6].iter().all(|field| matches!(*field, "0" | "1")) { return None; }
    Some(fields[0] == "1")
}

pub(crate) fn controller_ready() -> bool {
    if !REGISTRY_DONE.load(Ordering::Acquire) || !active() { return false; }
    let wrappers = [w_open_key as *const () as usize, w_open_key_ex as *const () as usize,
        w_create_key_ex as *const () as usize, w_query_value_ex as *const () as usize,
        w_set_value_ex as *const () as usize, w_close_key as *const () as usize];
    REGISTRY_SLOTS.iter().zip(wrappers).all(|(slot, wrapper)| import_matches(slot.load(Ordering::Acquire), wrapper))
}

fn import_matches(slot: usize, expected: usize) -> bool {
    // A later injector must not make the stock reload bypass our overrides.
    slot != 0 && crate::mem::read::<usize>(slot) == Some(expected)
}

/// The only values a live controller request may carry.
pub(crate) const CONTROLLER_KEYS: [&str; 3] = ["padmode000", "padsin000", "padguid000"];

pub(crate) fn is_controller_key(name: &str) -> bool {
    CONTROLLER_KEYS.iter().any(|key| name.eq_ignore_ascii_case(key))
}

/// Replace only the live controller's process-local values. The game's
/// already-open registry handle remains tracked; reinstalling the whole store
/// here would lose it and make the reload fall through to global settings.
pub(crate) fn update_controller_values(settings: &FfxiSettings, key: usize) -> Result<(), &'static str> {
    if !controller_ready() {
        return Err("This session cannot reload controller settings; restart the game from Crystal.");
    }
    let mut guard = store();
    let state = guard.as_mut().ok_or("This session has no controller settings override; restart the game from Crystal.")?;
    update_controller_store(state, settings, key)
}

fn update_controller_store(state: &mut Store, settings: &FfxiSettings, key: usize) -> Result<(), &'static str> {
    if !state.keys.contains(&key) {
        return Err("The game's controller settings handle is no longer available.");
    }
    // Keep this boundary narrow even if a future caller omits its validation.
    if settings.entries.iter().any(|(name, value)| {
        !is_controller_key(name)
            || !matches!(value, SettingValue::Text(text) if text.is_ascii())
    }) {
        return Err("The live request contains unsupported controller settings.");
    }
    let values: Vec<_> = settings.entries.iter().map(|(name, value)| reg_data(name, value, &|s| s.as_bytes().to_vec())).collect();
    for value in values {
        if let Some(old) = state.values.iter_mut().find(|old| old.name.eq_ignore_ascii_case(&value.name)) {
            *old = value;
        } else {
            state.values.push(value);
        }
    }
    Ok(())
}

fn to_ansi(s: &str) -> Vec<u8> {
    use windows_sys::Win32::Globalization::{WideCharToMultiByte, CP_ACP};
    if s.is_ascii() {
        return s.as_bytes().to_vec();
    }
    let wide: Vec<u16> = s.encode_utf16().collect();
    // SAFETY: lengths match the buffers.
    unsafe {
        let n = WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), wide.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut());
        let mut out = vec![0u8; n.max(0) as usize];
        WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), wide.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut());
        out
    }
}

/// `RegQueryValueExA`'s output rules for one value: `Some(status)` when `key`/`name` is one of
/// ours, `None` to pass the call through.
///
/// # Safety
/// The out pointers are the caller's, as `RegQueryValueExA` documents them.
unsafe fn query(key: usize, name: PCSTR, ty: *mut u32, data: *mut u8, size: *mut u32) -> Option<u32> {
    if name.is_null() {
        return None;
    }
    let name = CStr::from_ptr(name.cast()).to_bytes();
    let mut guard = store();
    let s = guard.as_mut()?;
    if !s.keys.contains(&key) {
        return None;
    }
    let v = s.values.iter().find(|v| v.name.as_bytes().eq_ignore_ascii_case(name))?;
    s.reads += 1;
    let needed = v.data.len() as u32;
    if !ty.is_null() {
        *ty = v.ty;
    }
    if data.is_null() {
        if !size.is_null() {
            *size = needed;
        }
        return Some(ERROR_SUCCESS);
    }
    if size.is_null() {
        return Some(ERROR_INVALID_PARAMETER);
    }
    if *size < needed {
        *size = needed;
        return Some(ERROR_MORE_DATA);
    }
    std::ptr::copy_nonoverlapping(v.data.as_ptr(), data, v.data.len());
    *size = needed;
    Some(ERROR_SUCCESS)
}

/// A write of one of our names on our key: kept in this process only. `None` passes through.
///
/// # Safety
/// `data` is valid for `len` bytes, as `RegSetValueExA` documents.
unsafe fn set(key: usize, name: PCSTR, ty: u32, data: *const u8, len: u32) -> Option<u32> {
    if name.is_null() {
        return None;
    }
    let name = CStr::from_ptr(name.cast()).to_bytes();
    let mut guard = store();
    let s = guard.as_mut()?;
    if !s.keys.contains(&key) {
        return None;
    }
    let v = s.values.iter_mut().find(|v| v.name.as_bytes().eq_ignore_ascii_case(name))?;
    if data.is_null() && len != 0 {
        return Some(ERROR_INVALID_PARAMETER);
    }
    v.ty = ty;
    v.data = if len == 0 { Vec::new() } else { std::slice::from_raw_parts(data, len as usize).to_vec() };
    s.writes += 1;
    Some(ERROR_SUCCESS)
}

/// After a successful open: remember the handle if it is the settings key.
///
/// # Safety
/// `sub` is null or a NUL-terminated string; `out` is the open's out pointer.
unsafe fn opened(parent: HKEY, sub: PCSTR, out: *mut HKEY) {
    if parent != HKEY_LOCAL_MACHINE || sub.is_null() || out.is_null() {
        return;
    }
    if !is_settings_key(CStr::from_ptr(sub.cast()).to_bytes()) {
        return;
    }
    let key = *out as usize;
    let mut guard = store();
    let Some(s) = guard.as_mut() else { return };
    if !s.keys.contains(&key) {
        s.keys.push(key);
    }
    let n = s.values.len();
    drop(guard);
    if !KEY_LOGGED.swap(true, Ordering::Relaxed) {
        log!("ffxi_settings: FFXiMain opened its settings key; serving {n} values from the session");
    }
}

fn closed(key: usize) {
    if let Some(s) = store().as_mut() {
        s.keys.retain(|&k| k != key);
    }
}

/// Logs what the game read (after `GameStart` returns).
pub fn report() {
    let guard = store();
    let Some(s) = guard.as_ref() else { return };
    if FFXIMAIN_DONE.load(Ordering::Acquire) {
        log!("ffxi_settings: served {} reads and kept {} writes in-process", s.reads, s.writes);
    } else {
        log!("ffxi_settings: NOT applied - FFXiMain.dll was never seen loading; the game used the registry");
    }
}

// --- the wrappers -------------------------------------------------------------

type RegOpenKeyA = unsafe extern "system" fn(HKEY, PCSTR, *mut HKEY) -> u32;
type RegOpenKeyExA = unsafe extern "system" fn(HKEY, PCSTR, u32, REG_SAM_FLAGS, *mut HKEY) -> u32;
type RegCreateKeyExA = unsafe extern "system" fn(HKEY, PCSTR, u32, PCSTR, u32, REG_SAM_FLAGS, *const c_void, *mut HKEY, *mut u32) -> u32;
type RegQueryValueExA = unsafe extern "system" fn(HKEY, PCSTR, *const u32, *mut u32, *mut u8, *mut u32) -> u32;
type RegSetValueExA = unsafe extern "system" fn(HKEY, PCSTR, u32, u32, *const u8, u32) -> u32;
type RegCloseKey = unsafe extern "system" fn(HKEY) -> u32;
type CoCreateInstance = unsafe extern "system" fn(*const GUID, *mut c_void, u32, *const GUID, *mut *mut c_void) -> HRESULT;

const I_OPEN: usize = 0;
const I_OPEN_EX: usize = 1;
const I_CREATE_EX: usize = 2;
const I_QUERY: usize = 3;
const I_SET: usize = 4;
const I_CLOSE: usize = 5;
const I_COCREATE: usize = 6;
const I_MAIN_COCREATE: usize = 7;

/// What each import entry held before it was redirected here.
static ORIGINAL: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];

/// The original, or the system's own function if none was recorded.
fn original(i: usize) -> usize {
    match ORIGINAL[i].load(Ordering::Acquire) {
        0 => system_fn(i),
        f => f,
    }
}

fn system_fn(i: usize) -> usize {
    use windows_sys::Win32::System::Com::CoCreateInstance;
    use windows_sys::Win32::System::Registry as r;
    match i {
        I_OPEN => r::RegOpenKeyA as *const () as usize,
        I_OPEN_EX => r::RegOpenKeyExA as *const () as usize,
        I_CREATE_EX => r::RegCreateKeyExA as *const () as usize,
        I_QUERY => r::RegQueryValueExA as *const () as usize,
        I_SET => r::RegSetValueExA as *const () as usize,
        I_CLOSE => r::RegCloseKey as *const () as usize,
        _ => CoCreateInstance as *const () as usize,
    }
}

unsafe extern "system" fn w_open_key(key: HKEY, sub: PCSTR, out: *mut HKEY) -> u32 {
    let f: RegOpenKeyA = std::mem::transmute(original(I_OPEN));
    let r = f(key, sub, out);
    if r == ERROR_SUCCESS {
        opened(key, sub, out);
    }
    r
}

unsafe extern "system" fn w_open_key_ex(key: HKEY, sub: PCSTR, options: u32, sam: REG_SAM_FLAGS, out: *mut HKEY) -> u32 {
    let f: RegOpenKeyExA = std::mem::transmute(original(I_OPEN_EX));
    let r = f(key, sub, options, sam, out);
    if r == ERROR_SUCCESS {
        opened(key, sub, out);
    }
    r
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn w_create_key_ex(key: HKEY, sub: PCSTR, reserved: u32, class: PCSTR, options: u32, sam: REG_SAM_FLAGS, security: *const c_void, out: *mut HKEY, disposition: *mut u32) -> u32 {
    let f: RegCreateKeyExA = std::mem::transmute(original(I_CREATE_EX));
    let r = f(key, sub, reserved, class, options, sam, security, out, disposition);
    if r == ERROR_SUCCESS {
        opened(key, sub, out);
    } else if !sub.is_null() && key == HKEY_LOCAL_MACHINE && is_settings_key(CStr::from_ptr(sub.cast()).to_bytes()) && active() {
        log!("ffxi_settings: FFXiMain could not open its settings key (error {r}); it will use its defaults");
    }
    r
}

unsafe extern "system" fn w_query_value_ex(key: HKEY, name: PCSTR, reserved: *const u32, ty: *mut u32, data: *mut u8, size: *mut u32) -> u32 {
    if let Some(r) = query(key as usize, name, ty, data, size) {
        return r;
    }
    let f: RegQueryValueExA = std::mem::transmute(original(I_QUERY));
    f(key, name, reserved, ty, data, size)
}

unsafe extern "system" fn w_set_value_ex(key: HKEY, name: PCSTR, reserved: u32, ty: u32, data: *const u8, len: u32) -> u32 {
    if let Some(r) = set(key as usize, name, ty, data, len) {
        return r;
    }
    let f: RegSetValueExA = std::mem::transmute(original(I_SET));
    f(key, name, reserved, ty, data, len)
}

unsafe extern "system" fn w_close_key(key: HKEY) -> u32 {
    closed(key as usize);
    let f: RegCloseKey = std::mem::transmute(original(I_CLOSE));
    f(key)
}

/// FFXi.dll's `CoCreateInstance`: after it creates `GameMain`, FFXiMain is loaded but has not
/// read its settings yet.
unsafe extern "system" fn w_co_create_instance(clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let hr = create_component(I_COCREATE, clsid, outer, ctx, iid, out);
    apply_to_ffximain();
    hr
}

unsafe extern "system" fn w_main_co_create_instance(clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    create_component(I_MAIN_COCREATE, clsid, outer, ctx, iid, out)
}

unsafe fn create_component(original_index: usize, clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if let Some(result) = crate::game::create_selected_component(clsid, outer, ctx, iid, out) {
        return result;
    }
    let f: CoCreateInstance = std::mem::transmute(original(original_index));
    f(clsid, outer, ctx, iid, out)
}

// --- import tables ------------------------------------------------------------

/// Which slots to redirect: for each wanted function (name, the system's own address), the
/// slot whose name matches, or, when the module has no name table, whose current value is the
/// system's address. Returns (slot index, wanted index).
pub fn plan(slots: &[(Option<&str>, usize)], wanted: &[(&str, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (w, &(name, system)) in wanted.iter().enumerate() {
        let found = slots.iter().position(|(n, _)| n.is_some_and(|n| n.eq_ignore_ascii_case(name))).or_else(|| {
            slots.iter().position(|&(n, current)| n.is_none() && system != 0 && current == system)
        });
        if let Some(s) = found {
            out.push((s, w));
        }
    }
    out
}

/// Redirects the `wanted` imports of `dll` in the module at `base` to (wrapper, ORIGINAL
/// index) pairs. Returns how many were redirected.
///
/// # Safety
/// `base` is a loaded module.
unsafe fn redirect(base: usize, dll: &str, wanted: &[(&str, usize, usize)]) -> usize {
    let slots = crate::iat::import_slots(base, dll);
    let view: Vec<(Option<&str>, usize)> = slots.iter().map(|s| (s.name.as_deref(), std::ptr::read_unaligned(s.slot))).collect();
    // The address the Windows loader put in the IAT (our own `system_fn` is an import stub).
    let dll_base = module(dll);
    let system: Vec<(&str, usize)> = wanted
        .iter()
        .map(|&(n, _, _)| {
            if dll_base == 0 {
                return (n, 0);
            }
            let name: Vec<u8> = n.bytes().chain(std::iter::once(0)).collect();
            let f = windows_sys::Win32::System::LibraryLoader::GetProcAddress(dll_base as _, name.as_ptr());
            (n, f.map_or(0, |f| f as usize))
        })
        .collect();
    let mut done = 0;
    for (s, w) in plan(&view, &system) {
        let (_, wrapper, index) = wanted[w];
        let current = view[s].1;
        if current == wrapper {
            if index < REGISTRY_SLOTS.len() { REGISTRY_SLOTS[index].store(slots[s].slot as usize, Ordering::Release); }
            done += 1;
            continue;
        }
        ORIGINAL[index].store(current, Ordering::Release);
        // Only over the value just read: an injector that changed the slot
        // since then keeps it.
        if crate::iat::swap(slots[s].slot as usize, current, wrapper).is_ok_and(|s| s.changed) {
            if index < REGISTRY_SLOTS.len() { REGISTRY_SLOTS[index].store(slots[s].slot as usize, Ordering::Release); }
            done += 1;
        }
    }
    done
}

use crate::win::module_base as module;

static APPLY: Mutex<()> = Mutex::new(());

/// Once FFXiMain.dll is loaded: redirect its registry imports. Cheap until then; idempotent.
pub fn apply_to_ffximain() -> bool {
    if FFXIMAIN_DONE.load(Ordering::Acquire) {
        return true;
    }
    if !active() && !crate::game::selected_game_directory() {
        return false;
    }
    let _one = APPLY.lock().unwrap_or_else(|e| e.into_inner());
    if FFXIMAIN_DONE.load(Ordering::Acquire) {
        return true;
    }
    let base = module("FFXiMain.dll");
    if base == 0 {
        return false;
    }
    crate::boot_timing::install(base);
    if crate::game::selected_game_directory() {
        // SAFETY: FFXiMain is loaded; retain its original injector/COM import separately
        // from FFXi.dll's import, which may have a different hook.
        let n = unsafe { redirect(base, "ole32.dll", &[("CoCreateInstance", w_main_co_create_instance as *const () as usize, I_MAIN_COCREATE)]) };
        if n != 1 {
            log!("selected game: FFXiMain's COM import could not be redirected");
            return false;
        }
    }
    if !active() {
        FFXIMAIN_DONE.store(true, Ordering::Release);
        return true;
    }
    let wanted: [(&str, usize, usize); 6] = [
        ("RegOpenKeyA", w_open_key as *const () as usize, I_OPEN),
        ("RegOpenKeyExA", w_open_key_ex as *const () as usize, I_OPEN_EX),
        ("RegCreateKeyExA", w_create_key_ex as *const () as usize, I_CREATE_EX),
        ("RegQueryValueExA", w_query_value_ex as *const () as usize, I_QUERY),
        ("RegSetValueExA", w_set_value_ex as *const () as usize, I_SET),
        ("RegCloseKey", w_close_key as *const () as usize, I_CLOSE),
    ];
    // SAFETY: FFXiMain.dll is loaded (GetModuleHandle found it).
    let n = unsafe { redirect(base, "ADVAPI32.dll", &wanted) };
    if n == wanted.len() {
        REGISTRY_DONE.store(true, Ordering::Release);
        FFXIMAIN_DONE.store(true, Ordering::Release);
        log!("ffxi_settings: FFXiMain's registry imports redirected ({n}/{})", wanted.len());
    } else {
        log!("ffxi_settings: only {n} of {} FFXiMain registry imports could be redirected; refusing to substitute global settings", wanted.len());
        return false;
    }
    true
}

/// FFXi.dll and FFXiMain.dll were unloaded after their game (game_modules).
/// Their import slots are gone; the next mappings are redirected afresh, and
/// nothing here keeps an address inside the old images.
pub(crate) fn module_unloaded() {
    let _one = APPLY.lock().unwrap_or_else(|e| e.into_inner());
    FFXIMAIN_DONE.store(false, Ordering::Release);
    REGISTRY_DONE.store(false, Ordering::Release);
    for slot in &REGISTRY_SLOTS { slot.store(0, Ordering::Release); }
    // Only the old images' import slots called the wrappers; zero falls back to
    // the system function until the next redirect records the new original.
    for original in &ORIGINAL { original.store(0, Ordering::Release); }
    KEY_LOGGED.store(false, Ordering::Relaxed);
    if let Some(s) = store().as_mut() { s.keys.clear(); s.reads = 0; s.writes = 0; }
}

/// Watches FFXi.dll's `CoCreateInstance`, which loads FFXiMain for `GameMain`.
pub fn hook_ffxi_dll(base: usize) -> bool {
    if (!active() && !crate::game::selected_game_directory()) || base == 0 {
        return false;
    }
    // SAFETY: the caller passes FFXi.dll's loaded base.
    let n = unsafe { redirect(base, "ole32.dll", &[("CoCreateInstance", w_co_create_instance as *const () as usize, I_COCREATE)]) };
    if n == 1 {
        log!("ffxi_settings: watching FFXi.dll for FFXiMain");
    } else {
        log!("ffxi_settings: FFXi.dll imports no CoCreateInstance; only an already-loaded FFXiMain can be served");
    }
    n == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_enable_uses_the_first_flag_and_rejects_unknown_values() {
        let enabled = |text: &str| controller_enabled_value(&[
            reg_data("padmode000", &SettingValue::Text(text.into()), &to_ansi)
        ]);
        assert_eq!(enabled("0,0,0,0,1,0"), Some(false));
        assert_eq!(enabled("1,0,0,0,0,0"), Some(true));
        assert_eq!(enabled(" 0 ,1,0,0,1,1"), Some(false));
        for value in ["", "0", "0,0,0,0,broken,0", "2,0,0,0,0,0"] {
            assert_eq!(enabled(value), None);
        }
        assert_eq!(controller_enabled_value(&[]), None);
        assert_eq!(controller_enabled_value(&[reg_data("padmode000", &SettingValue::Dword(0), &to_ansi)]), None);
    }

    #[test]
    fn live_controller_rejects_an_import_replaced_by_another_owner() {
        let slot = AtomicUsize::new(123);
        let address = &slot as *const AtomicUsize as usize;
        assert!(import_matches(address, 123));
        slot.store(456, Ordering::Release);
        assert!(!import_matches(address, 123));
        assert!(!import_matches(0, 123));
    }

    #[test]
    fn live_controller_update_keeps_open_handles_and_unrelated_values() {
        let value = |name, text: &str| reg_data(name, &SettingValue::Text(text.into()), &|s| s.as_bytes().to_vec());
        let mut state = Store { values: vec![value("0042", "other"), value("padmode000", "old"), value("padexsin000", "extended")],
            keys: vec![123, 456], reads: 7, writes: 2 };
        let settings = FfxiSettings::new(vec![("PADMODE000".into(), SettingValue::Text("1,0,0,0,1,0".into())),
            ("padsin000".into(), SettingValue::Text("1,2".into()))]).unwrap();
        update_controller_store(&mut state, &settings, 123).unwrap();
        assert_eq!(state.keys, vec![123, 456]);
        assert_eq!((state.reads, state.writes), (7, 2));
        assert_eq!(state.values.len(), 4);
        assert_eq!(state.values[0], value("0042", "other"));
        assert_eq!(state.values[2], value("padexsin000", "extended"));
        assert_eq!(state.values[1], value("PADMODE000", "1,0,0,0,1,0"));
        let before = state.values.clone();
        assert!(update_controller_store(&mut state, &settings, 999).is_err());
        let unrelated = FfxiSettings::new(vec![("0001".into(), SettingValue::Dword(1920))]).unwrap();
        assert!(update_controller_store(&mut state, &unrelated, 123).is_err());
        assert_eq!(state.values, before);
    }

    static COM_CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "system" fn fake_com(_clsid: *const GUID, _outer: *mut c_void, _ctx: u32, _iid: *const GUID, _out: *mut *mut c_void) -> HRESULT {
        COM_CALLS.fetch_add(1, Ordering::SeqCst);
        123
    }

    #[test]
    fn game_components_do_not_fall_back_to_registered_dlls_and_other_com_calls_pass_through() {
        let directory = std::env::temp_dir().join(format!("crystal-missing-game-{}", std::process::id()));
        assert!(!directory.exists());
        crate::game::select_game_directory(directory).unwrap();
        let old = ORIGINAL[I_COCREATE].swap(fake_com as *const () as usize, Ordering::SeqCst);
        let old_main = ORIGINAL[I_MAIN_COCREATE].swap(fake_com as *const () as usize, Ordering::SeqCst);
        let other = GUID::from_u128(0xE436EBB3_524F_11CE_9F53_0020AF0BA770); // DirectShow
        let game = GUID::from_u128(0x1027DC46_750D_4B1F_8834_1D25B8BEBAB8);
        let iid = GUID::from_u128(0x493BF7B9_0C3A_43B5_BFA6_28FBEE251E3D);
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(unsafe { w_co_create_instance(&other, std::ptr::null_mut(), 1, &iid, &mut out) }, 123);
        assert_eq!(unsafe { w_main_co_create_instance(&other, std::ptr::null_mut(), 1, &iid, &mut out) }, 123);
        assert_eq!(COM_CALLS.load(Ordering::SeqCst), 2);
        assert!(unsafe { w_co_create_instance(&game, std::ptr::null_mut(), 1, &iid, &mut out) } < 0);
        assert!(out.is_null());
        assert_eq!(COM_CALLS.load(Ordering::SeqCst), 2, "the missing selected DLL cannot call the registered fallback");
        ORIGINAL[I_COCREATE].store(old, Ordering::SeqCst);
        ORIGINAL[I_MAIN_COCREATE].store(old_main, Ordering::SeqCst);
    }

    #[test]
    fn names_and_values_are_validated() {
        let ok = FfxiSettings::new(vec![("0001".into(), SettingValue::Dword(1920)), ("padmode000".into(), SettingValue::Text("1,0,1,0,1,0".into()))]).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok.get("PADMODE000"), Some(&SettingValue::Text("1,0,1,0,1,0".into())));
        assert_eq!(ok.get("0002"), None);
        assert!(FfxiSettings::new(vec![("0001".into(), SettingValue::Dword(1)), ("0001".into(), SettingValue::Dword(2))]).is_err());
        assert!(FfxiSettings::new(vec![("Pad".into(), SettingValue::Dword(1)), ("pAD".into(), SettingValue::Dword(2))]).is_err());
        for bad in ["", "a\\b", "x\u{1}", &"7".repeat(65), "é"] {
            assert!(FfxiSettings::new(vec![(bad.into(), SettingValue::Dword(0))]).is_err(), "{bad:?}");
        }
        assert!(FfxiSettings::new(vec![("0042".into(), SettingValue::Text("a\0b".into()))]).is_err());
        assert!(FfxiSettings::new(vec![("0042".into(), SettingValue::Text("x".repeat(1025)))]).is_err());
        let many = (0..=MAX_SETTINGS).map(|i| (format!("{i:04}"), SettingValue::Dword(0))).collect();
        assert!(FfxiSettings::new(many).is_err());
    }

    #[test]
    fn only_the_ffxi_settings_key_matches() {
        for yes in [
            &br"SOFTWARE\PlayOnlineUS\SquareEnix\FinalFantasyXI"[..],
            br"software\playonlineus\squareenix\finalfantasyxi\",
            br"SOFTWARE\PlayOnline\SQUARE\FinalFantasyXI",
            br"SOFTWARE\PlayOnlineEU\SquareEnix\FinalFantasyXI",
            br"\SOFTWARE\WOW6432Node\PlayOnlineUS\SquareEnix\FinalFantasyXI",
        ] {
            assert!(is_settings_key(yes), "{}", String::from_utf8_lossy(yes));
        }
        for no in [
            &br"SOFTWARE\PlayOnlineUS\InstallFolder"[..],
            br"SOFTWARE\PlayOnlineUS\SquareEnix\FinalFantasyXITestClient",
            br"SOFTWARE\PlayOnlineUS\SquareEnix\FinalFantasyXI\Sub",
            br"SOFTWARE\PlayOnlineUS\SquareEnix",
            br"SOFTWARE\Other\SquareEnix\FinalFantasyXI",
            b"",
        ] {
            assert!(!is_settings_key(no), "{}", String::from_utf8_lossy(no));
        }
    }

    #[test]
    fn plan_matches_by_name_then_by_address() {
        let slots = [(Some("RegCloseKey"), 0x10), (Some("regqueryvalueexa"), 0x20), (None, 0x30), (Some("RegSetValueExA"), 0x40)];
        let wanted = [("RegQueryValueExA", 0x99), ("RegCreateKeyExA", 0x30), ("RegOpenKeyA", 0x77), ("RegCloseKey", 0x10)];
        assert_eq!(plan(&slots, &wanted), vec![(1, 0), (2, 1), (0, 3)]);
    }

    // --- the wrappers against a fake registry ---

    const FAKE_KEY: usize = 0x4242;
    static FAKE_QUERIES: AtomicUsize = AtomicUsize::new(0);
    static FAKE_SETS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn fake_create(_k: HKEY, _s: PCSTR, _r: u32, _c: PCSTR, _o: u32, _sam: REG_SAM_FLAGS, _sec: *const c_void, out: *mut HKEY, _d: *mut u32) -> u32 {
        *out = FAKE_KEY as HKEY;
        0
    }
    unsafe extern "system" fn fake_open(_k: HKEY, _s: PCSTR, out: *mut HKEY) -> u32 {
        *out = (FAKE_KEY + 1) as HKEY;
        0
    }
    unsafe extern "system" fn fake_query(_k: HKEY, _n: PCSTR, _r: *const u32, ty: *mut u32, data: *mut u8, size: *mut u32) -> u32 {
        FAKE_QUERIES.fetch_add(1, Ordering::SeqCst);
        if !ty.is_null() {
            *ty = REG_DWORD;
        }
        if !data.is_null() {
            std::ptr::copy_nonoverlapping(7u32.to_le_bytes().as_ptr(), data, 4);
        }
        if !size.is_null() {
            *size = 4;
        }
        0
    }
    unsafe extern "system" fn fake_set(_k: HKEY, _n: PCSTR, _r: u32, _t: u32, _d: *const u8, _l: u32) -> u32 {
        FAKE_SETS.fetch_add(1, Ordering::SeqCst);
        0
    }
    unsafe extern "system" fn fake_close(_k: HKEY) -> u32 {
        0
    }

    fn read_dword(key: usize, name: &[u8]) -> (u32, u32, u32) {
        let (mut ty, mut v, mut size) = (0u32, 0u32, 4u32);
        let r = unsafe { w_query_value_ex(key as HKEY, name.as_ptr(), std::ptr::null(), &mut ty, (&mut v as *mut u32).cast(), &mut size) };
        (r, ty, v)
    }

    /// FFXiMain's own sequence (RegCreateKeyExA on the settings key, DWORD and string reads, a
    /// write-back, close) through the wrappers, with a fake registry behind them.
    #[test]
    fn wrappers_serve_the_session_on_the_settings_key_only() {
        ORIGINAL[I_CREATE_EX].store(fake_create as *const () as usize, Ordering::SeqCst);
        ORIGINAL[I_OPEN].store(fake_open as *const () as usize, Ordering::SeqCst);
        ORIGINAL[I_QUERY].store(fake_query as *const () as usize, Ordering::SeqCst);
        ORIGINAL[I_SET].store(fake_set as *const () as usize, Ordering::SeqCst);
        ORIGINAL[I_CLOSE].store(fake_close as *const () as usize, Ordering::SeqCst);
        *store() = None;
        install(Some(FfxiSettings::default()));
        assert!(active(), "an empty session prepares later live updates");
        let mut tracked: HKEY = std::ptr::null_mut();
        let sub = b"SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI\0";
        assert_eq!(unsafe { w_create_key_ex(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, std::ptr::null(), 0,
            0xF003F, std::ptr::null(), &mut tracked, std::ptr::null_mut()) }, 0);
        assert!(store().as_ref().unwrap().keys.contains(&(tracked as usize)));
        let queries = FAKE_QUERIES.load(Ordering::SeqCst);
        assert_eq!(read_dword(tracked as usize, b"0001\0"), (0, REG_DWORD, 7));
        assert_eq!(FAKE_QUERIES.load(Ordering::SeqCst), queries + 1);
        let sets = FAKE_SETS.load(Ordering::SeqCst);
        let value = 1600u32.to_le_bytes();
        assert_eq!(unsafe { w_set_value_ex(tracked, c"0001".as_ptr().cast(), 0, REG_DWORD, value.as_ptr(), 4) }, 0);
        assert_eq!(FAKE_SETS.load(Ordering::SeqCst), sets + 1, "empty scope preserves ordinary writes");
        let settings = FfxiSettings::new(vec![
            ("0001".into(), SettingValue::Dword(1920)),
            ("0028".into(), SettingValue::Dword(0xBF80_0000)),
            ("padmode000".into(), SettingValue::Text("1,0,1,0,1,0".into())),
        ])
        .unwrap();
        install_with(&settings, &|s| s.as_bytes().to_vec());

        let mut key: HKEY = std::ptr::null_mut();
        let sub = b"SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI\0";
        let r = unsafe { w_create_key_ex(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, std::ptr::null(), 0, 0xF003F, std::ptr::null(), &mut key, std::ptr::null_mut()) };
        assert_eq!((r, key as usize), (0, FAKE_KEY));

        // Overridden DWORDs come from the session; others go to the registry.
        assert_eq!(read_dword(FAKE_KEY, b"0001\0"), (0, REG_DWORD, 1920));
        assert_eq!(read_dword(FAKE_KEY, b"0028\0"), (0, REG_DWORD, 0xBF80_0000));
        let before = FAKE_QUERIES.load(Ordering::SeqCst);
        assert_eq!(read_dword(FAKE_KEY, b"0002\0"), (0, REG_DWORD, 7));
        assert_eq!(FAKE_QUERIES.load(Ordering::SeqCst), before + 1);

        // A string: size query, too small, then the read (with its NUL).
        let name = b"PadMode000\0";
        let (mut ty, mut size) = (0u32, 0u32);
        assert_eq!(unsafe { w_query_value_ex(FAKE_KEY as HKEY, name.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut size) }, 0);
        assert_eq!((ty, size), (REG_SZ, 12));
        let mut buf = [0xEEu8; 0x100];
        size = 4;
        assert_eq!(unsafe { w_query_value_ex(FAKE_KEY as HKEY, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut size) }, ERROR_MORE_DATA);
        assert_eq!((size, buf[0]), (12, 0xEE));
        size = buf.len() as u32;
        assert_eq!(unsafe { w_query_value_ex(FAKE_KEY as HKEY, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut size) }, 0);
        assert_eq!(&buf[..12], b"1,0,1,0,1,0\0");
        assert_eq!(unsafe { w_query_value_ex(FAKE_KEY as HKEY, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), std::ptr::null_mut()) }, ERROR_INVALID_PARAMETER);

        // The same name on another key is the registry's.
        let mut other: HKEY = std::ptr::null_mut();
        let install = b"SOFTWARE\\PlayOnlineUS\\InstallFolder\0";
        assert_eq!(unsafe { w_open_key(HKEY_LOCAL_MACHINE, install.as_ptr(), &mut other) }, 0);
        assert_eq!(read_dword(other as usize, b"0001\0"), (0, REG_DWORD, 7));

        // A write of an overridden name stays in-process; others reach the registry.
        let sets = FAKE_SETS.load(Ordering::SeqCst);
        let v = 1280u32.to_le_bytes();
        assert_eq!(unsafe { w_set_value_ex(FAKE_KEY as HKEY, c"0001".as_ptr().cast(), 0, REG_DWORD, v.as_ptr(), 4) }, 0);
        assert_eq!(FAKE_SETS.load(Ordering::SeqCst), sets);
        assert_eq!(read_dword(FAKE_KEY, b"0001\0"), (0, REG_DWORD, 1280));
        assert_eq!(unsafe { w_set_value_ex(FAKE_KEY as HKEY, c"0005".as_ptr().cast(), 0, REG_DWORD, v.as_ptr(), 4) }, 0);
        assert_eq!(FAKE_SETS.load(Ordering::SeqCst), sets + 1);

        // After the close, the handle value is no longer ours.
        assert_eq!(unsafe { w_close_key(FAKE_KEY as HKEY) }, 0);
        let before = FAKE_QUERIES.load(Ordering::SeqCst);
        assert_eq!(read_dword(FAKE_KEY, b"0001\0"), (0, REG_DWORD, 7));
        assert_eq!(FAKE_QUERIES.load(Ordering::SeqCst), before + 1);
        let guard = store();
        let s = guard.as_ref().unwrap();
        assert_eq!((s.reads, s.writes), (7, 1));
    }
}
