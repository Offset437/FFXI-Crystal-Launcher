//! Serialize FFXIDB's settings transactions between Crystal hosts, and put
//! back the minimap position fields its stock save drops.
//!
//! FFXIDB keeps every character's minimap position in one FFXIDB.xml. It
//! ignores an unsuccessful XML read and then saves a fresh document, and its
//! CRT opens writers exclusively, so games starting together, or a later save,
//! can erase each other's settings. A file-open lock would be too short: it
//! must cover the read, the change and the write.
//!
//! Nothing here depends on an FFXIDB build. Only Windower's side is located:
//! Hook's plugin loader, by signature (crate::hook_layout). Through it Crystal
//! changes three kinds of data pointer, never code:
//! - Hook's GetProcAddress import, to see FFXIDB's `CreateInstance`;
//! - FFXIDB's own KERNEL32 CreateFile imports, to see when it opens a file;
//! - four slots of the plugin object's vtable, in place: Initialize, Command,
//!   Incoming and Mouse, the stdcall callbacks of Hook's plugin interface.
//!
//! Each wrapped callback (and the factory) is a scope. The first time FFXIDB
//! opens FFXIDB.xml inside a scope, that thread takes the session-wide
//! settings mutex and reads the file as it was; the mutex is held until the
//! outermost scope returns. Callbacks that never open the file (mouse moves,
//! most packets) take no lock at all. At the end, if the file changed, the
//! position fields the stock save dropped are restored ([`restore_positions`])
//! by plain XML text, then the mutex is released.
//!
//! What Crystal cannot cover is said in the loader log and, once per host, in
//! the launch history ([`report_unprotected`]): an unverified Hook loader, an
//! FFXIDB without CreateFile imports, a vtable that cannot be changed, or
//! FFXIDB opening its settings outside the callbacks above.
//!
//! FFXIDB caches game addresses from signature scans in static maps, valid
//! only for the game that was running then. A mapping that outlived its game
//! faults in the next one, but FFXIDB must not be unmapped inside the game's
//! own teardown either (Hook FreeLibrary's it under its plugin-list lock, and
//! clients hung at /logout when that unmapped it). Crystal holds one reference
//! per mapping (HELD) through the end of its game and drops it in
//! `game_starting`, before the next GameStart. If something else still keeps
//! the old copy mapped, its vtable still holds Crystal's wrappers, and the
//! factory refuses it (logged) instead of reusing the previous game's
//! addresses.

use crate::hook_layout::{OwnedModule, Plugins};
use crate::mem::read;
use std::{
    cell::RefCell,
    ffi::{c_void, CStr, OsString},
    fs::{File, OpenOptions},
    io::{Read, Write},
    marker::PhantomData,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};
use windows_sys::{
    core::{PCSTR, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, FreeLibrary, FARPROC, HANDLE, HMODULE, WAIT_ABANDONED, WAIT_OBJECT_0},
        Globalization::{MultiByteToWideChar, CP_ACP},
        Security::{
            Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
            SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{CreateFile2, CreateFileA, CreateFileW, CREATEFILE2_EXTENDED_PARAMETERS},
        System::{
            LibraryLoader::{
                GetModuleHandleExW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            },
            Threading::{
                CreateMutexExW, ReleaseMutex, WaitForSingleObject, INFINITE, MUTEX_MODIFY_STATE,
                SYNCHRONIZATION_SYNCHRONIZE,
            },
        },
    },
};

const SHARED_SETTINGS_MUTEX: &str = "Local\\CrystalLauncher.FFXIDB.settings.shared.v1";
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

// Hook's plugin loader (crate::hook_layout, any build): it pushes
// "CreateInstance" and the module, calls its KERNEL32!GetProcAddress import,
// then calls the returned cdecl factory at factory_lookup + 0x17. Initialize
// pushes (API, object) before public stdcall vtable+8.

type Resolve = unsafe extern "system" fn(HMODULE, PCSTR) -> FARPROC;
type Factory = unsafe extern "C" fn() -> *mut c_void;
type Initialize = unsafe extern "system" fn(*mut c_void, *mut c_void);
#[cfg(test)]
type Notify = unsafe extern "system" fn(*mut c_void);
type Command = unsafe extern "system" fn(*mut c_void, *const u8);
// The last argument and result are byte-sized flags, passed in four-byte
// stack slots / AL. Use u8 rather than Rust bool: preserve every bit.
type Incoming = unsafe extern "system" fn(*mut c_void, *const u8, *const u8, u32, u8) -> u8;
type Mouse = unsafe extern "system" fn(*mut c_void, u32, i32, i32, i32, u8) -> u8;

/// The vtable slots of Hook's plugin interface that are wrapped, in the order
/// of [`ORIGINAL`]: Initialize, Command, Incoming and Mouse. Their stdcall
/// signatures are Windower's plugin ABI, the same for every plugin.
const WRAPPED: [usize; 4] = [2, 7, 11, 13];
/// The stock callbacks of the mapping in use, in [`WRAPPED`] order.
static ORIGINAL: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

fn wrappers() -> [usize; 4] {
    [initialize as *const () as usize, command as *const () as usize, incoming as *const () as usize, mouse as *const () as usize]
}

struct Bridge {
    _hook: OwnedModule,
    resolve: Resolve,
}
static BRIDGE: OnceLock<Bridge> = OnceLock::new();
static INSTALL: Mutex<()> = Mutex::new(());

/// The FFXIDB mapping Crystal protects.
#[derive(Clone, Copy, Debug)]
struct Mapping {
    module: usize,
    /// The stock `CreateInstance`.
    factory: usize,
    /// The vtable Crystal changed in place; 0 until an instance is built.
    vtable: usize,
    /// The game (GAME) during which it was built.
    game: usize,
}
static MAPPING: Mutex<Option<Mapping>> = Mutex::new(None);

/// The settings mutex for one plugin path. Leaked: scopes borrow it.
struct Settings {
    plugin: PathBuf,
    mutex: SettingsMutex,
}
static SETTINGS: AtomicPtr<Settings> = AtomicPtr::new(std::ptr::null_mut());

fn settings() -> Option<&'static Settings> {
    let settings = SETTINGS.load(Ordering::Acquire);
    // SAFETY: only `protect` stores here, always a leaked, never-freed Box.
    (!settings.is_null()).then(|| unsafe { &*settings })
}

// GameStart calls made by this host. FFXIDB scan results are valid only for
// the game that was running when they were taken.
static GAME: AtomicUsize = AtomicUsize::new(0);
// Base of the FFXIDB mapping Crystal holds one module reference on (0: none).
// Taken when an instance is protected; dropped only by `release_held`.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// Keep `module` mapped until the next game starts, so Hook's FreeLibrary at
/// the end of this game only drops Hook's own reference.
fn hold(module: usize) {
    let Some(reference) = OwnedModule::retain_at(module) else {
        log!("note: FFXIDB could not be held for the end of this game; Hook unloads it while the game closes");
        return;
    };
    std::mem::forget(reference);
    let previous = HELD.swap(module, Ordering::SeqCst);
    if previous != 0 {
        // Never more than one reference: the same mapping protected twice, or
        // (not expected) an older one still held.
        unsafe { FreeLibrary(previous as HMODULE) };
    }
}

/// Drop Crystal's reference on the previous game's FFXIDB. Runs on the host
/// thread before GameStart: no game, Hook teardown or plugin callback is in
/// progress, so FFXIDB's DLL_PROCESS_DETACH runs outside any game lock.
fn release_held() {
    let held = HELD.swap(0, Ordering::SeqCst);
    if held == 0 {
        return;
    }
    log!("stage: releasing the previous game's FFXIDB copy (base {held:#x}) before GameStart");
    let started = std::time::Instant::now();
    unsafe { FreeLibrary(held as HMODULE) };
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    if mapped_ffxidb(held) {
        log!("note: the previous game's FFXIDB copy is still in memory after {elapsed:.1} ms (another reference holds it); this game refuses that copy rather than reuse its addresses");
    } else {
        log!("stage: previous game's FFXIDB copy unloaded in {elapsed:.1} ms; this game maps a fresh copy");
    }
}

/// Call immediately before every FFXiEntry::GameStart in this host.
pub(crate) fn game_starting() {
    release_held();
    GAME.fetch_add(1, Ordering::SeqCst);
}

/// `base` is still the start of a loaded FFXIDB.dll image.
fn mapped_ffxidb(base: usize) -> bool {
    let mut module = std::ptr::null_mut();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            base as *const u16,
            &mut module,
        )
    };
    ok != 0 && module as usize == base && module_path(base).is_some_and(|path| is_named(&path, "FFXIDB.dll"))
}

fn is_named(path: &Path, name: &str) -> bool {
    path.file_name().is_some_and(|file| file.to_string_lossy().eq_ignore_ascii_case(name))
}

fn pe_identity(base: usize) -> Option<(u32, u32)> {
    crate::pe::identity(base).map(|image| (image.timestamp, image.size as u32))
}

fn module_path(module: usize) -> Option<PathBuf> {
    crate::win::module_path(module as HMODULE)
}

fn sha256(bytes: &[u8]) -> Option<String> {
    let mut digest = [0u8; 32];
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null(),
            0,
            bytes.as_ptr(),
            u32::try_from(bytes.len()).ok()?,
            digest.as_mut_ptr(),
            32,
        )
    };
    (status == 0).then(|| digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Says that FFXIDB runs without protection: in the loader log each time, and
/// once per host, through the launcher, in the launch history.
pub(crate) fn report_unprotected(reason: &str) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    log!("note: FFXIDB minimap settings protection is OFF: {reason}");
    if !REPORTED.swap(true, Ordering::SeqCst) {
        crate::events::send(crate::proto::notice_line(&format!("FFXIDB minimap settings protection is off: {reason}.")));
    }
}

/// Live re-check, in this process's memory, of every byte the resolved
/// loader sites were found by and the IAT slot this adapter replaces.
fn hook_layout(base: usize, build: &Plugins) -> bool {
    let lookup = base + build.factory_lookup;
    read::<u8>(lookup) == Some(0x68)
        && read::<u32>(lookup + 1) == Some((base + build.factory_name) as u32)
        && read::<[u8; 3]>(lookup + 5) == Some([0x57, 0xFF, 0x15])
        && read::<u32>(lookup + 8) == Some((base + build.get_proc_iat) as u32)
        && read::<[u8; 15]>(base + build.factory_name) == Some(*b"CreateInstance\0")
        && read::<[u8; 2]>(lookup + 0x17) == Some([0xFF, 0xD0])
        && read::<[u8; 7]>(base + build.initialize_call)
            == Some([0x52, 0x51, 0x8B, 0x01, 0xFF, 0x50, 0x08])
}

/// The loaded Hook's plugin loader, resolved by signature and re-checked live.
fn verified_hook(base: usize) -> Result<(Plugins, String), String> {
    let layout = crate::hook_layout::hook(base).ok_or("Hook.dll layout unavailable")?;
    let plugins = layout.plugins.clone()?;
    if !hook_layout(base, &plugins) {
        return Err("the resolved plugin loader changed in memory".into());
    }
    let label = layout.known.map_or_else(|| format!("{:08X} (signature scan)", layout.timestamp), str::to_owned);
    Ok((plugins, label))
}

/// Change one aligned data pointer; never write executable code. A failed
/// page restore is reported even when the replacement is already active.
fn replace_pointer(address: usize, expected: usize, replacement: usize) -> Result<(), String> {
    if !address.is_multiple_of(std::mem::align_of::<usize>()) || read::<usize>(address) != Some(expected) {
        return Err("FFXIDB adapter pointer changed before installation".into());
    }
    let done = crate::iat::swap(address, expected, replacement)
        .map_err(|error| format!("FFXIDB adapter could not make its pointer writable: {error}"))?;
    if done.restored.is_err() {
        return Err("FFXIDB adapter could not restore pointer-page protection".into());
    }
    if !done.changed {
        return Err("FFXIDB adapter pointer was changed concurrently".into());
    }
    Ok(())
}

/// Call after Hook has loaded and before GameStart. Ok(true) means Hook's
/// factory interception is installed; the log confirms FFXIDB when it loads.
/// An unsupported Hook build is reported and left stock.
pub(crate) fn install() -> Result<bool, String> {
    let _serial = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(hook) = crate::hook_layout::retain_hook() else { return Ok(false) };
    let base = hook.0;
    let (build, version) = match verified_hook(base) {
        Ok(found) => found,
        Err(reason) => {
            report_unprotected(&format!(
                "Windower's Hook.dll plugin loader could not be verified: {reason} (loaded PE identity {:X?})",
                pe_identity(base)
            ));
            return Ok(false);
        }
    };
    let replacement = resolve as *const () as usize;
    let current = read::<usize>(base + build.get_proc_iat)
        .ok_or("Cannot read Windower GetProcAddress import")?;
    if current == replacement && BRIDGE.get().is_some() {
        return Ok(true);
    }
    // Do not bypass another tool's resolver or overwrite an unknown IAT hook.
    if current != GetProcAddress as *const () as usize {
        return Err("Windower's plugin resolver was already changed; cannot safely install FFXIDB settings protection".into());
    }
    let bridge = Bridge {
        _hook: hook,
        resolve: unsafe { std::mem::transmute::<usize, Resolve>(current) },
    };
    if let Err(other) = BRIDGE.set(bridge) {
        if BRIDGE.get().map(|b| b._hook.0) != Some(other._hook.0) {
            return Err("Windower module changed after preparing FFXIDB protection".into());
        }
    }
    replace_pointer(base + build.get_proc_iat, current, replacement)?;
    log!(
        "stage: verified Windower Hook {} plugin resolver ready for FFXIDB shared-settings protection",
        version
    );
    Ok(true)
}

/// A kernel mutex is reentrant on the owning OS thread and shared across
/// processes. A Rust Mutex would neither allow callback reentry nor cover
/// another game. The guard is intentionally !Send and !Sync.
struct TransactionMutex(usize);
struct Transaction<'a> {
    mutex: &'a TransactionMutex,
    _same_thread: PhantomData<Rc<()>>,
}
impl TransactionMutex {
    fn open(name: &str) -> Result<Self, String> {
        let name: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        // Windower isolates CreateMutexW names so multiple games can run.
        // That also separates our clients' settings locks. Use the extended
        // API, which keeps this exact kernel-object name shared between them.
        let handle = unsafe {
            CreateMutexExW(std::ptr::null(), name.as_ptr(), 0,
                MUTEX_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE)
        };
        if handle.is_null() {
            return Err(format!(
                "Cannot create FFXIDB settings mutex: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(handle as usize))
    }
    fn acquire(&self, timeout: u32) -> Result<Transaction<'_>, String> {
        let status = unsafe { WaitForSingleObject(self.0 as *mut c_void, timeout) };
        match status {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                if status == WAIT_ABANDONED {
                    log!("note: FFXIDB settings mutex recovered after another host exited during a callback");
                }
                Ok(Transaction {
                    mutex: self,
                    _same_thread: PhantomData,
                })
            }
            _ => Err(format!(
                "Cannot acquire FFXIDB settings mutex (wait status {status:#x}): {}",
                std::io::Error::last_os_error()
            )),
        }
    }
}
impl Drop for TransactionMutex {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0 as *mut c_void);
        }
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if unsafe { ReleaseMutex(self.mutex.0 as *mut c_void) } == 0 {
            log!("error: FFXIDB settings mutex release failed; terminating this host to prevent unlocked settings writes");
            std::process::abort();
        }
    }
}

// Preserve the old lock identity for already-running Crystal hosts. Hook,
// rather than the DLL's directory, chooses the actual XML settings location.
// This path alone cannot coordinate different plugin copies using one file.
fn legacy_settings_mutex_name(plugin: &Path) -> Result<String, String> {
    let canonical =
        std::fs::canonicalize(plugin).map_err(|e| format!("Cannot resolve FFXIDB path: {e}"))?;
    let directory = canonical.parent().ok_or("FFXIDB has no parent directory")?;
    let settings = directory.join("settings").join("FFXIDB.xml");
    // Canonicalize the existing DLL (the XML need not exist yet), resolving
    // junction/short-path aliases. Use a constant settings suffix and fold
    // case before hashing so spelling variations share the same mutex.
    let normalized = settings
        .as_os_str()
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
    let bytes: Vec<_> = std::ffi::OsStr::new(&normalized)
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    let digest = sha256(&bytes).ok_or("Cannot hash FFXIDB settings path")?;
    Ok(format!(
        "Local\\CrystalLauncher.FFXIDB.settings.v1.{digest}"
    ))
}

struct SettingsMutex {
    shared: TransactionMutex,
    legacy: TransactionMutex,
}

struct SettingsTransaction<'a> {
    // Drop in reverse acquisition order.
    _legacy: Transaction<'a>,
    _shared: Transaction<'a>,
}

impl SettingsMutex {
    fn open(plugin: &Path) -> Result<Self, String> {
        // Tests that drive `resolve` use an isolated name: never live games'.
        #[cfg(test)]
        if let Ok(name) = std::env::var("CRYSTAL_TEST_FFXIDB_SHARED_MUTEX") {
            return Self::with_shared_name(plugin, &name);
        }
        Self::with_shared_name(plugin, SHARED_SETTINGS_MUTEX)
    }

    fn with_shared_name(plugin: &Path, shared_name: &str) -> Result<Self, String> {
        Ok(Self {
            // Protected FFXIDB instances in this Windows session cooperate even
            // when different DLL paths resolve to the same settings file.
            shared: TransactionMutex::open(shared_name)?,
            legacy: TransactionMutex::open(&legacy_settings_mutex_name(plugin)?)?,
        })
    }

    fn acquire(&self, timeout: u32) -> Result<SettingsTransaction<'_>, String> {
        let started = std::time::Instant::now();
        let shared = self.shared.acquire(timeout)?;
        let remaining = if timeout == INFINITE {
            INFINITE
        } else {
            timeout.saturating_sub(started.elapsed().as_millis().min(u32::MAX as u128) as u32)
        };
        let legacy = self.legacy.acquire(remaining)?;
        Ok(SettingsTransaction {
            _legacy: legacy,
            _shared: shared,
        })
    }
}

fn as_farproc(factory: Factory) -> FARPROC {
    Some(unsafe { std::mem::transmute::<Factory, unsafe extern "system" fn() -> isize>(factory) })
}

unsafe extern "system" fn resolve(module: HMODULE, name: PCSTR) -> FARPROC {
    let bridge = BRIDGE.get().unwrap_or_else(|| std::process::abort());
    let original = (bridge.resolve)(module, name);
    if original.is_none()
        || name as usize <= 0xFFFF
        || CStr::from_ptr(name.cast()).to_bytes() != b"CreateInstance"
    {
        return original;
    }
    let Some(path) = module_path(module as usize).filter(|path| is_named(path, "FFXIDB.dll")) else {
        return original;
    };
    let _serial = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    let factory = original.map_or(0, |f| f as *const () as usize);
    match protect(module as usize, &path, factory) {
        Ok(factory) => factory,
        Err(reason) => {
            report_unprotected(&reason);
            original
        }
    }
}

/// The vtable at `vtable` already holds Crystal's wrappers: this mapping
/// built an instance before.
fn patched(vtable: usize) -> bool {
    read::<usize>(vtable + WRAPPED[0] * 4) == Some(wrappers()[0])
}

/// The factory Hook gets for the FFXIDB at `module`. Err: protection is off,
/// FFXIDB runs stock.
fn protect(module: usize, plugin: &Path, factory: usize) -> Result<FARPROC, String> {
    let game = GAME.load(Ordering::SeqCst);
    let mut mapping = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(last) = *mapping {
        if last.module != module && mapped_ffxidb(last.module) {
            log!("error: refusing a second FFXIDB image loaded beside the first");
            return Ok(as_farproc(disabled_factory));
        }
        if last.module == module && last.vtable != 0 && patched(last.vtable) {
            if last.game == game {
                // Hook asking again during the game that built it.
                return Ok(as_farproc(create_instance));
            }
            // Only reachable if something other than Crystal kept FFXIDB
            // mapped after Crystal released it: its scan caches hold the
            // previous game's addresses and its own code would crash this
            // game. A later game loads it again once that reference is gone.
            log!("note: FFXIDB not loaded for this game: the plugin stayed in memory after the previous game and still holds that game's addresses");
            return Ok(as_farproc(disabled_factory));
        }
    }
    route_file_opens(module)?;
    if settings().is_none_or(|settings| settings.plugin != plugin) {
        let settings = Box::new(Settings { plugin: plugin.to_owned(), mutex: SettingsMutex::open(plugin)? });
        SETTINGS.store(Box::leak(settings), Ordering::Release);
    }
    *mapping = Some(Mapping { module, factory, vtable: 0, game });
    log!("stage: FFXIDB at {module:#x}: its settings file opens are serialized across Crystal game hosts");
    Ok(as_farproc(create_instance))
}

/// Points FFXIDB's own KERNEL32 CreateFile imports at [`opening`]'s wrappers.
/// Err when it imports none of them or a slot cannot be changed.
fn route_file_opens(module: usize) -> Result<(), String> {
    let mut routed = 0;
    // SAFETY: Hook just loaded this module and holds it.
    for import in unsafe { crate::iat::import_slots(module, "KERNEL32.dll") } {
        let replacement = match import.name.as_deref() {
            Some("CreateFileW") => create_file_w as *const () as usize,
            Some("CreateFileA") => create_file_a as *const () as usize,
            Some("CreateFile2") => create_file_2 as *const () as usize,
            _ => continue,
        };
        let slot = import.slot as usize;
        let current = read::<usize>(slot).ok_or("FFXIDB's import table is unreadable")?;
        if current != replacement {
            replace_pointer(slot, current, replacement)?;
        }
        routed += 1;
    }
    if routed == 0 {
        return Err("this FFXIDB.dll imports no CreateFile function from KERNEL32, so Crystal cannot see it open its settings".into());
    }
    Ok(())
}

unsafe extern "C" fn disabled_factory() -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn create_instance() -> *mut c_void {
    let Some(mut mapping) = *MAPPING.lock().unwrap_or_else(|e| e.into_inner()) else {
        std::process::abort()
    };
    let factory = std::mem::transmute::<usize, Factory>(mapping.factory);
    let instance = serialized(|| factory());
    if instance.is_null() {
        return instance;
    }
    match wrap(instance as usize) {
        Ok(vtable) => {
            mapping.vtable = vtable;
            mapping.game = GAME.load(Ordering::SeqCst);
            *MAPPING.lock().unwrap_or_else(|e| e.into_inner()) = Some(mapping);
            hold(mapping.module);
            log!("stage: FFXIDB settings callbacks protected across Crystal game hosts (kept loaded until the next game starts)");
        }
        Err(reason) => report_unprotected(&reason),
    }
    instance
}

/// Replaces the [`WRAPPED`] slots of `instance`'s vtable, in place, keeping
/// the stock callbacks in [`ORIGINAL`]. A slot already wrapped (the same
/// mapping building another instance) is left as it is.
fn wrap(instance: usize) -> Result<usize, String> {
    let vtable = crate::mem::pointer(instance, 0x100).ok_or("FFXIDB's plugin object has no readable vtable")?;
    for (index, (slot, wrapper)) in WRAPPED.into_iter().zip(wrappers()).enumerate() {
        let at = vtable + slot * 4;
        let current = read::<usize>(at).ok_or("FFXIDB's vtable is unreadable")?;
        if current != wrapper {
            ORIGINAL[index].store(current, Ordering::Release);
            replace_pointer(at, current, wrapper)?;
        }
    }
    Ok(vtable)
}

/// The settings transaction of one thread's outermost callback.
struct Scope {
    depth: usize,
    open: Option<Open>,
}
/// FFXIDB.xml as it was when the scope first opened it, and the lock taken then.
struct Open {
    path: PathBuf,
    before: Option<String>,
    _guard: SettingsTransaction<'static>,
}
thread_local! {
    static SCOPE: RefCell<Scope> = const { RefCell::new(Scope { depth: 0, open: None }) };
}

/// Runs one FFXIDB callback as a settings scope: any FFXIDB.xml it opens is
/// read, changed and written under the settings mutex, which is released, after
/// the position repair, when the outermost scope returns.
fn serialized<R>(call: impl FnOnce() -> R) -> R {
    SCOPE.with(|scope| scope.borrow_mut().depth += 1);
    let result = call();
    let open = SCOPE.with(|scope| {
        let mut scope = scope.borrow_mut();
        scope.depth -= 1;
        if scope.depth == 0 { scope.open.take() } else { None }
    });
    if let Some(open) = open {
        open.finish();
    }
    result
}

/// FFXIDB is about to open `path`. The first open of its settings in a scope
/// takes the mutex and reads the file as it was.
fn opening(path: PathBuf) {
    if !is_named(&path, "FFXIDB.xml") {
        return;
    }
    // try_with: a thread that is exiting has no scope (nor a callback).
    let _ = SCOPE.try_with(|scope| {
        let mut scope = scope.borrow_mut();
        if scope.open.is_some() {
            return;
        }
        if scope.depth == 0 {
            static OUTSIDE: AtomicBool = AtomicBool::new(false);
            if !OUTSIDE.swap(true, Ordering::Relaxed) {
                report_unprotected("FFXIDB opened its settings outside the plugin callbacks Crystal serializes");
            }
            return;
        }
        let Some(settings) = settings() else { return };
        let guard = settings.mutex.acquire(INFINITE).unwrap_or_else(|error| {
            // A permanent valid handle cannot time out. Never let FFXIDB
            // read and write its settings unlocked.
            log!("error: {error}; terminating this host to prevent unlocked FFXIDB settings writes");
            std::process::abort()
        });
        let before = read_settings(&path);
        scope.open = Some(Open { path, before, _guard: guard });
    });
}

impl Open {
    /// After the callback, still under the mutex: restore the position fields
    /// a stock save dropped.
    fn finish(self) {
        static TRACES: AtomicUsize = AtomicUsize::new(0);
        let Some(before) = &self.before else { return };
        let Some(after) = read_settings(&self.path).filter(|after| after != before) else { return };
        let Some(repaired) = restore_positions(before, &after) else { return };
        let result = replace_settings(&self.path, &repaired);
        if TRACES.fetch_add(1, Ordering::Relaxed) < 64 {
            match result {
                Ok(()) => log!("stage: FFXIDB minimap position fields a stock save dropped were restored"),
                Err(error) => log!("note: FFXIDB minimap position repair failed ({:?})", error.kind()),
            }
        }
    }
}

unsafe fn wide_name(name: *const u16) -> Option<PathBuf> {
    if name.is_null() {
        return None;
    }
    let length = (0..32_768).find(|&at| *name.add(at) == 0)?;
    Some(PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(name, length))))
}

unsafe extern "system" fn create_file_w(
    name: PCWSTR, access: u32, share: u32, security: *const SECURITY_ATTRIBUTES,
    disposition: u32, flags: u32, template: HANDLE,
) -> HANDLE {
    if let Some(path) = wide_name(name) {
        opening(path);
    }
    CreateFileW(name, access, share, security, disposition, flags, template)
}

unsafe extern "system" fn create_file_2(
    name: PCWSTR, access: u32, share: u32, disposition: u32,
    parameters: *const CREATEFILE2_EXTENDED_PARAMETERS,
) -> HANDLE {
    if let Some(path) = wide_name(name) {
        opening(path);
    }
    CreateFile2(name, access, share, disposition, parameters)
}

unsafe extern "system" fn create_file_a(
    name: PCSTR, access: u32, share: u32, security: *const SECURITY_ATTRIBUTES,
    disposition: u32, flags: u32, template: HANDLE,
) -> HANDLE {
    if !name.is_null() && CStr::from_ptr(name.cast()).to_bytes().to_ascii_lowercase().ends_with(b"ffxidb.xml") {
        let mut wide = vec![0u16; 32_768];
        let length = MultiByteToWideChar(CP_ACP, 0, name, -1, wide.as_mut_ptr(), wide.len() as i32);
        if length > 0 {
            opening(PathBuf::from(OsString::from_wide(&wide[..length as usize - 1])));
        }
    }
    CreateFileA(name, access, share, security, disposition, flags, template)
}

unsafe extern "system" fn initialize(this: *mut c_void, api: *mut c_void) {
    let stock = std::mem::transmute::<usize, Initialize>(ORIGINAL[0].load(Ordering::Acquire));
    serialized(|| stock(this, api))
}
unsafe extern "system" fn command(this: *mut c_void, text: *const u8) {
    let stock = std::mem::transmute::<usize, Command>(ORIGINAL[1].load(Ordering::Acquire));
    serialized(|| stock(this, text))
}
unsafe extern "system" fn incoming(this: *mut c_void, previous: *const u8, data: *const u8, size: u32, blocked: u8) -> u8 {
    let stock = std::mem::transmute::<usize, Incoming>(ORIGINAL[2].load(Ordering::Acquire));
    serialized(|| stock(this, previous, data, size, blocked))
}
unsafe extern "system" fn mouse(this: *mut c_void, kind: u32, x: i32, y: i32, wheel: i32, blocked: u8) -> u8 {
    let stock = std::mem::transmute::<usize, Mouse>(ORIGINAL[3].load(Ordering::Acquire));
    serialized(|| stock(this, kind, x, y, wheel, blocked))
}

fn read_settings(path: &Path) -> Option<String> {
    let mut xml = String::new();
    File::open(path).ok()?.take(MAX_SETTINGS_BYTES + 1).read_to_string(&mut xml).ok()?;
    (xml.len() as u64 <= MAX_SETTINGS_BYTES).then_some(xml)
}

/// Writes `xml` beside `path` and renames it over `path`: a reader sees the
/// old file or the new one, never a partial write.
fn replace_settings(path: &Path, xml: &str) -> std::io::Result<()> {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let staging = path.with_file_name(format!(
        ".FFXIDB.crystal-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .and_then(|mut file| file.write_all(xml.as_bytes()).and_then(|()| file.sync_all()))
        .and_then(|()| std::fs::rename(&staging, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

const AXES: [&str; 2] = ["X", "Y"];

/// The settings XML `after` one callback, with the minimap position fields
/// the stock save dropped put back; None when nothing was dropped.
///
/// FFXIDB deletes a field whose value equals the one it inherits: a
/// character's X or Y equal to the global one, and a global X or Y equal to
/// itself (an axis the move left alone), removing a global section left
/// empty. So a field present `before` and missing `after` is restored with
/// the value it inherited: the global field after the callback, else the
/// global field before it. Every other byte is kept. A character section the
/// callback removed (the stock `save` command) stays removed.
fn restore_positions(before: &str, after: &str) -> Option<String> {
    use roxmltree::{Document, Node};
    fn child<'a, 'i>(parent: Option<Node<'a, 'i>>, name: &str) -> Option<Node<'a, 'i>> {
        parent?.children().find(|node| node.is_element() && node.has_tag_name(name))
    }
    fn number<'a, 'i>(node: Option<Node<'a, 'i>>) -> Option<Node<'a, 'i>> {
        node.filter(|node| node.text().and_then(|text| text.trim().parse::<f64>().ok()).is_some_and(f64::is_finite))
    }
    let (old, new) = (Document::parse(before).ok()?, Document::parse(after).ok()?);
    let (old_root, new_root) = (old.root_element(), new.root_element());
    if !old_root.has_tag_name("settings") || !new_root.has_tag_name("settings") {
        return None;
    }
    let (old_global, new_global) = (child(Some(old_root), "global"), child(Some(new_root), "global"));
    let inherited = AXES.map(|axis| {
        number(child(new_global, axis))
            .map(|node| &after[node.range()])
            .or_else(|| number(child(old_global, axis)).map(|node| &before[node.range()]))
    });
    let mut edits = Vec::new();
    for section in old_root.children().filter(Node::is_element) {
        let name = section.tag_name().name();
        // A repeated section: only the first counts, as when FFXIDB looks it up.
        if child(Some(old_root), name) != Some(section) {
            continue;
        }
        let current = child(Some(new_root), name);
        if current.is_none() && name != "global" {
            continue;
        }
        let missing: String = AXES
            .iter()
            .zip(inherited)
            .filter(|(axis, _)| child(Some(section), axis).is_some() && child(current, axis).is_none())
            .filter_map(|(_, value)| value)
            .collect();
        if !missing.is_empty() {
            edits.push(match current {
                Some(node) => append(after, node, &missing)?,
                None => append(after, new_root, &format!("<global>{missing}</global>"))?,
            });
        }
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(start, ..)| std::cmp::Reverse(*start));
    let mut repaired = after.to_owned();
    for (start, end, text) in edits {
        repaired.replace_range(start..end, &text);
    }
    Document::parse(&repaired).ok()?;
    Some(repaired)
}

/// The edit (start, end, replacement) of `xml` that adds `text` at the end of
/// `element`'s content.
fn append(xml: &str, element: roxmltree::Node<'_, '_>, text: &str) -> Option<(usize, usize, String)> {
    let range = element.range();
    let source = &xml[range.clone()];
    if source.ends_with("/>") {
        Some((range.end - 2, range.end, format!(">{text}</{}>", element.tag_name().name())))
    } else {
        let at = range.start + source.rfind("</")?;
        Some((at, at, text.to_owned()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        process::{Command as Process, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::WAIT_TIMEOUT,
        System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32},
    };

    fn unique() -> String {
        format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
    fn test_mutex() -> TransactionMutex {
        TransactionMutex::open(&format!("Local\\CrystalLauncher.FFXIDB.test.{}", unique())).unwrap()
    }

    #[repr(C)]
    struct Fixture {
        vptr: usize,
        calls: u32,
        api: usize,
        command: usize,
    }
    /// A plugin vtable in writable memory, as the loader leaves FFXIDB's.
    static FIXTURE_VTABLE: OnceLock<usize> = OnceLock::new();
    fn fixture_slot(slot: usize) -> usize {
        unsafe { *(*FIXTURE_VTABLE.get().unwrap() as *const usize).add(slot) }
    }
    unsafe extern "C" fn fixture_factory() -> *mut c_void {
        Box::into_raw(Box::new(Fixture {
            vptr: *FIXTURE_VTABLE.get().unwrap(),
            calls: 0,
            api: 0,
            command: 0,
        }))
        .cast()
    }
    unsafe extern "system" fn fixture_initialize(this: *mut c_void, api: *mut c_void) {
        let object = &mut *this.cast::<Fixture>();
        object.api = api as usize;
        object.calls |= 1;
        // Reenter a wrapped callback on the same thread inside Initialize's scope.
        let f: Command = std::mem::transmute(*(object.vptr as *const usize).add(7));
        f(this, c"nested".as_ptr().cast());
    }
    unsafe extern "system" fn fixture_shutdown(this: *mut c_void) {
        drop(Box::from_raw(this.cast::<Fixture>()));
    }
    unsafe extern "system" fn fixture_update(this: *mut c_void) {
        (*this.cast::<Fixture>()).calls |= 2;
    }
    unsafe extern "system" fn fixture_render(this: *mut c_void) {
        (*this.cast::<Fixture>()).calls |= 4;
    }
    unsafe extern "system" fn fixture_command(this: *mut c_void, text: *const u8) {
        (*this.cast::<Fixture>()).command = text as usize;
    }
    unsafe extern "system" fn fixture_incoming(
        this: *mut c_void,
        previous: *const u8,
        data: *const u8,
        size: u32,
        blocked: u8,
    ) -> u8 {
        assert!(!this.is_null());
        assert_eq!(
            (previous as usize, data as usize, size, blocked),
            (0xAABBCCDD, 0x12345678, 0x98765432, 0xA5)
        );
        0xC3
    }
    unsafe extern "system" fn fixture_mouse(
        this: *mut c_void,
        kind: u32,
        x: i32,
        y: i32,
        wheel: i32,
        blocked: u8,
    ) -> u8 {
        assert!(!this.is_null());
        assert_eq!(
            (kind, x, y, wheel, blocked),
            (0xFFFFFFFF, -500, 700, -120, 0xA5)
        );
        0xC7
    }

    /// The only test in this process that uses MAPPING, ORIGINAL and GAME.
    #[test]
    fn factory_and_wrapped_callbacks_preserve_arguments_results_reentry_and_relaunch() {
        let mut original = vec![0x1234usize; 34];
        for (slot, f) in [
            (2, fixture_initialize as *const () as usize),
            (3, fixture_shutdown as *const () as usize),
            (5, fixture_update as *const () as usize),
            (6, fixture_render as *const () as usize),
            (7, fixture_command as *const () as usize),
            (11, fixture_incoming as *const () as usize),
            (13, fixture_mouse as *const () as usize),
        ] {
            original[slot] = f;
        }
        let table = Box::leak(original.clone().into_boxed_slice()).as_ptr() as usize;
        assert!(FIXTURE_VTABLE.set(table).is_ok());
        static FIXTURE_IMAGE: [u8; 0x40] = [0; 0x40];
        let module = FIXTURE_IMAGE.as_ptr() as usize;
        *MAPPING.lock().unwrap() = Some(Mapping { module, factory: fixture_factory as *const () as usize, vtable: 0, game: GAME.load(Ordering::SeqCst) });
        for _ in 0..64 {
            unsafe {
                let object = create_instance();
                assert!(!object.is_null());
                assert_eq!((*object.cast::<Fixture>()).vptr, table, "changed in place, never replaced");
                for (slot, stock) in original.iter().enumerate() {
                    let expected = match WRAPPED.iter().position(|wrapped| *wrapped == slot) {
                        Some(index) => wrappers()[index],
                        None => *stock,
                    };
                    assert_eq!(fixture_slot(slot), expected, "slot {slot}");
                }
                let init: Initialize = std::mem::transmute(fixture_slot(2));
                init(object, 0xF1234567usize as *mut c_void);
                assert_eq!(
                    CStr::from_ptr((*object.cast::<Fixture>()).command as *const i8).to_bytes(),
                    b"nested"
                );
                let tick: Notify = std::mem::transmute(fixture_slot(5));
                tick(object);
                let draw: Notify = std::mem::transmute(fixture_slot(6));
                draw(object);
                let cmd: Command = std::mem::transmute(fixture_slot(7));
                let save = c"save".as_ptr().cast();
                cmd(object, save);
                let packet: Incoming = std::mem::transmute(fixture_slot(11));
                assert_eq!(
                    packet(object, 0xAABBCCDDusize as *const u8, 0x12345678usize as *const u8, 0x98765432, 0xA5),
                    0xC3
                );
                let input: Mouse = std::mem::transmute(fixture_slot(13));
                assert_eq!(input(object, 0xFFFFFFFF, -500, 700, -120, 0xA5), 0xC7);
                assert_eq!(((*object.cast::<Fixture>()).calls, (*object.cast::<Fixture>()).api), (7, 0xF1234567));
                assert_eq!((*object.cast::<Fixture>()).command, save as usize);
                let release: Notify = std::mem::transmute(fixture_slot(3));
                release(object);
            }
        }
        // The same mapping: served again during its own game, refused in a
        // later one (its scan caches would hold the earlier game's addresses).
        let plugin = Path::new("FFXIDB.dll");
        let factory = fixture_factory as *const () as usize;
        let served = |result: Result<FARPROC, String>| result.unwrap().map(|f| f as *const () as usize);
        assert_eq!(served(protect(module, plugin, factory)), Some(create_instance as *const () as usize));
        GAME.fetch_add(1, Ordering::SeqCst);
        assert_eq!(served(protect(module, plugin, factory)), Some(disabled_factory as *const () as usize));
    }

    /// The scope itself, without vendor code: the first settings open takes
    /// the lock and reads the file, a nested callback shares it, a stock save
    /// that drops X is repaired, and the lock is free once the scope returns.
    #[test]
    fn a_scope_locks_on_the_first_settings_open_and_repairs_a_dropped_field() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-scope-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let plugin = root.join("FFXIDB.dll");
        std::fs::write(&plugin, b"fixture").unwrap();
        let shared = format!("Local\\CrystalLauncher.FFXIDB.scope-test.{}", unique());
        let settings = Box::leak(Box::new(Settings { plugin: plugin.clone(), mutex: SettingsMutex::with_shared_name(&plugin, &shared).unwrap() }));
        SETTINGS.store(settings, Ordering::Release);
        let xml = root.join("FFXIDB.xml");
        std::fs::write(&xml, "<settings><global><X>100</X><Y>200</Y></global></settings>").unwrap();
        let other = TransactionMutex::open(&shared).unwrap();
        // Opening anything else is not a transaction.
        serialized(|| opening(root.join("map.dat")));
        assert!(other.acquire(0).is_ok());
        serialized(|| {
            serialized(|| opening(xml.clone()));
            let held = thread::scope(|s| s.spawn(|| TransactionMutex::open(&shared).unwrap().acquire(0).is_err()).join().unwrap());
            assert!(held, "the lock is held until the outermost scope returns");
            opening(xml.clone());
            // The stock position save: Y moved, X equal to itself dropped.
            std::fs::write(&xml, "<settings><global><Y>250</Y></global></settings>").unwrap();
        });
        assert!(thread::spawn(move || TransactionMutex::open(&shared).unwrap().acquire(0).is_ok()).join().unwrap());
        assert_eq!(std::fs::read_to_string(&xml).unwrap(), "<settings><global><Y>250</Y><X>100</X></global></settings>");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn another_thread_cannot_enter_until_all_recursive_guards_release() {
        let mutex = std::sync::Arc::new(test_mutex());
        let first = mutex.acquire(0).unwrap();
        let second = mutex.acquire(0).unwrap();
        for guard in [second, first] {
            let other = mutex.clone();
            assert_eq!(
                thread::spawn(move || unsafe { WaitForSingleObject(other.0 as *mut c_void, 0) })
                    .join()
                    .unwrap(),
                WAIT_TIMEOUT
            );
            drop(guard);
        }
        let other = mutex.clone();
        assert!(thread::spawn(move || {
            let _guard = other.acquire(1000).unwrap();
            true
        })
        .join()
        .unwrap());
    }

    // Reproduce a multibox host that makes CreateMutexW names process-local.
    // Run only in a child: the temporary IAT change cannot affect other tests.
    #[test]
    fn renamed_mutex_child() {
        let Ok(name) = std::env::var("CRYSTAL_FFXIDB_RENAMED_MUTEX") else { return };
        use windows_sys::Win32::{
            Foundation::HANDLE,
            Security::SECURITY_ATTRIBUTES,
            System::{LibraryLoader::GetModuleHandleW, Threading::{
                CreateMutexExW, CreateMutexW, CREATE_MUTEX_INITIAL_OWNER, MUTEX_ALL_ACCESS,
            }},
        };

        unsafe extern "system" fn isolated_name(
            attributes: *const SECURITY_ATTRIBUTES, owner: i32, name: *const u16,
        ) -> HANDLE {
            let mut length = 0;
            while length < 260 && *name.add(length) != 0 { length += 1; }
            assert!(length < 260);
            let mut isolated = std::slice::from_raw_parts(name, length).to_vec();
            isolated.extend(format!("-fixture-{}", std::process::id()).encode_utf16());
            isolated.push(0);
            CreateMutexExW(attributes, isolated.as_ptr(),
                if owner != 0 { CREATE_MUTEX_INITIAL_OWNER } else { 0 }, MUTEX_ALL_ACCESS)
        }

        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        let slot = unsafe { crate::iat::import_slots(base, "kernel32.dll") }
            .into_iter().find(|s| s.name.as_deref() == Some("CreateMutexW"))
            .expect("the probe below must retain the CreateMutexW import").slot as usize;
        let original = read::<usize>(slot).unwrap();
        let replacement = isolated_name as *const () as usize;
        replace_pointer(slot, original, replacement).unwrap();
        struct Restore { slot: usize, original: usize, replacement: usize }
        impl Drop for Restore {
            fn drop(&mut self) {
                replace_pointer(self.slot, self.replacement, self.original).unwrap();
            }
        }
        let _restore = Restore { slot, original, replacement };

        // Prove the fixture has separated the old API from the parent's mutex.
        let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        let isolated = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        assert!(!isolated.is_null());
        assert_eq!(unsafe { WaitForSingleObject(isolated, 0) }, WAIT_OBJECT_0);
        assert_ne!(unsafe { ReleaseMutex(isolated) }, 0);
        assert_ne!(unsafe { CloseHandle(isolated) }, 0);

        let mutex = TransactionMutex::open(&name).unwrap();
        assert!(mutex.acquire(0).is_err(),
            "the parent's real named mutex must block this client despite CreateMutexW name isolation");
    }

    #[test]
    fn mutex_name_is_shared_when_create_mutex_w_is_virtualized() {
        use windows_sys::Win32::System::Threading::{CreateMutexExW, MUTEX_ALL_ACCESS};
        let name = format!("Local\\CrystalLauncher.FFXIDB.rename-test.{}", unique());
        let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
        // An ordinary process outside the injected host opens the real name.
        let handle = unsafe { CreateMutexExW(std::ptr::null(), wide.as_ptr(), 0, MUTEX_ALL_ACCESS) };
        assert!(!handle.is_null());
        let mutex = TransactionMutex(handle as usize);
        let _guard = mutex.acquire(0).unwrap();
        let status = Process::new(std::env::current_exe().unwrap())
            .args(["--exact", "windower_plugin_settings::tests::renamed_mutex_child", "--nocapture"])
            .env("CRYSTAL_FFXIDB_RENAMED_MUTEX", &name)
            .status().unwrap();
        assert!(status.success(), "an injected client must honor the external settings mutex");
    }

    // Separate test-binary processes exercise the actual named kernel mutex,
    // without loading Windower/FFXIDB or executing any vendor code.
    #[test]
    fn settings_child_process() {
        let Some(root) = std::env::var_os("CRYSTAL_FFXIDB_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let role = std::env::var("CRYSTAL_FFXIDB_TEST_ROLE").unwrap();
        let plugin = PathBuf::from(std::env::var_os("CRYSTAL_FFXIDB_TEST_PLUGIN").unwrap());
        let mutex = SettingsMutex::with_shared_name(
            &plugin,
            &std::env::var("CRYSTAL_FFXIDB_TEST_MUTEX").unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(format!("{role}.ready")), b"ready").unwrap();
        for iteration in 0..12 {
            let _guard = mutex.acquire(30_000).unwrap();
            let _nested = mutex.acquire(0).unwrap();
            std::fs::write(root.join(format!("{role}.entered")), b"entered").unwrap();
            let path = root.join("FFXIDB.xml");
            // The real failure is a failed read becoming a fresh document,
            // followed by a non-atomic truncating save. Keep that behavior in
            // this fixture so only the transaction mutex prevents the loss.
            let mut xml = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| "<settings><global/></settings>".into());
            thread::sleep(Duration::from_millis(3));
            let field = format!(
                "<{role}{iteration}><X>{}</X><Y>55</Y></{role}{iteration}>",
                700 + iteration
            );
            xml = xml.replace("</settings>", &(field + "</settings>"));
            use std::{io::Write, os::windows::fs::OpenOptionsExt};
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .share_mode(0)
                .open(path)
                .unwrap();
            thread::sleep(Duration::from_millis(3));
            output.write_all(xml.as_bytes()).unwrap();
        }
    }

    #[test]
    fn six_plugin_paths_sharing_one_xml_preserve_all_coordinates() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let initial = "<settings><global><X>120</X><Y>240</Y></global><Existing><X>360</X><Y>480</Y></Existing></settings>";
        std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
        let name = format!("Local\\CrystalLauncher.FFXIDB.test.{}", unique());
        let mutex = TransactionMutex::open(&name).unwrap();
        let guard = mutex.acquire(0).unwrap();
        let roles = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta"];
        let mut legacy_names = std::collections::BTreeSet::new();
        let mut children: Vec<_> = roles
            .into_iter()
            .map(|role| {
                let plugin_folder = root.join(role);
                std::fs::create_dir(&plugin_folder).unwrap();
                let plugin = plugin_folder.join("FFXIDB.dll");
                std::fs::write(&plugin, b"fixture").unwrap();
                assert!(legacy_names.insert(legacy_settings_mutex_name(&plugin).unwrap()));
                Process::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windower_plugin_settings::tests::settings_child_process",
                        "--nocapture",
                    ])
                    .env("CRYSTAL_FFXIDB_TEST_ROOT", &root)
                    .env("CRYSTAL_FFXIDB_TEST_ROLE", role)
                    .env("CRYSTAL_FFXIDB_TEST_MUTEX", &name)
                    .env("CRYSTAL_FFXIDB_TEST_PLUGIN", plugin)
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let start = Instant::now();
        while !roles
            .iter()
            .all(|role| root.join(format!("{role}.ready")).exists())
        {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "all six child processes must reach the held shared mutex"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(legacy_names.len(), roles.len());
        for role in roles {
            assert!(!root.join(format!("{role}.entered")).exists());
        }
        assert_eq!(
            std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap(),
            initial
        );
        drop(guard);
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let xml = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
        assert!(xml.contains("<global><X>120</X><Y>240</Y></global>"));
        assert!(xml.contains("<Existing><X>360</X><Y>480</Y></Existing>"));
        for role in roles {
            for i in 0..12 {
                assert!(xml.contains(&format!(
                    "<{role}{i}><X>{}</X><Y>55</Y></{role}{i}>",
                    700 + i
                )));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_mutex_names_preserve_path_alias_compatibility() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-path-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let plugin = root.join("FFXIDB.dll");
        std::fs::write(&plugin, b"fixture").unwrap();
        let a = legacy_settings_mutex_name(&plugin).unwrap();
        let b = legacy_settings_mutex_name(&root.join(".").join("ffxidb.DLL")).unwrap();
        assert_eq!(a, b);
        let different = root.join("different");
        std::fs::create_dir(&different).unwrap();
        std::fs::write(different.join("FFXIDB.dll"), b"fixture").unwrap();
        assert_ne!(
            a,
            legacy_settings_mutex_name(&different.join("FFXIDB.dll")).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_settings_guard_cooperates_with_a_previous_release_guard() {
        let legacy_name = format!("Local\\CrystalLauncher.FFXIDB.test.{}", unique());
        let old_guard = TransactionMutex::open(&legacy_name).unwrap();
        let old_transaction = old_guard.acquire(0).unwrap();
        let current = std::sync::Arc::new(SettingsMutex {
            shared: test_mutex(),
            legacy: TransactionMutex::open(&legacy_name).unwrap(),
        });
        let other = current.clone();
        assert!(thread::spawn(move || other.acquire(0).is_err())
            .join()
            .unwrap());
        drop(old_transaction);
        assert!(thread::spawn(move || {
            let _guard = current.acquire(0).unwrap();
            let _nested = current.acquire(0).unwrap();
            true
        })
        .join()
        .unwrap());
    }

    #[test]
    fn restore_positions_puts_back_only_what_a_stock_save_dropped() {
        let before = "<settings><global><X>100</X><Y>200</Y></global><Alpha><X>310</X><Y>410</Y></Alpha><Beta><Zoom>2</Zoom></Beta></settings>";
        let restored = |after: &str| restore_positions(before, after);
        // Global `pos 100 250`: Y moved and is kept; X, equal to itself, comes back.
        assert_eq!(
            restored("<settings><global><Y>250</Y></global><Alpha><X>310</X><Y>410</Y></Alpha><Beta><Zoom>2</Zoom></Beta></settings>").as_deref(),
            Some("<settings><global><Y>250</Y><X>100</X></global><Alpha><X>310</X><Y>410</Y></Alpha><Beta><Zoom>2</Zoom></Beta></settings>")
        );
        // Global `pos 100 200`: both dropped and the emptied section removed.
        assert_eq!(
            restored("<settings><Alpha><X>310</X><Y>410</Y></Alpha><Beta><Zoom>2</Zoom></Beta></settings>").as_deref(),
            Some("<settings><Alpha><X>310</X><Y>410</Y></Alpha><Beta><Zoom>2</Zoom></Beta><global><X>100</X><Y>200</Y></global></settings>")
        );
        // Alpha `pos 100 250`: X moved to the global value and was dropped as
        // inherited; it is pinned at that value, not put back at 310.
        assert_eq!(
            restored("<settings><global><X>100</X><Y>200</Y></global><Alpha><Y>250</Y></Alpha><Beta><Zoom>2</Zoom></Beta></settings>").as_deref(),
            Some("<settings><global><X>100</X><Y>200</Y></global><Alpha><Y>250</Y><X>100</X></Alpha><Beta><Zoom>2</Zoom></Beta></settings>")
        );
        // A section left self-closing, and comments and other fields, are kept.
        assert_eq!(
            restored("<settings><!-- note --><global><X>100</X><Y>200</Y></global><Alpha/><Beta><Zoom>3</Zoom></Beta></settings>").as_deref(),
            Some("<settings><!-- note --><global><X>100</X><Y>200</Y></global><Alpha><X>100</X><Y>200</Y></Alpha><Beta><Zoom>3</Zoom></Beta></settings>")
        );
        // Intentional moves and the stock `save` (character sections removed) are left alone.
        assert_eq!(restored("<settings><global><X>1</X><Y>2</Y></global><Alpha><X>3</X><Y>4</Y></Alpha><Beta><Zoom>2</Zoom></Beta></settings>"), None);
        assert_eq!(restored("<settings><global><X>310</X><Y>410</Y></global></settings>"), None);
        // A field never present is not invented, and nothing is guessed from bad XML.
        assert_eq!(restore_positions("<settings><Beta/></settings>", "<settings><Beta/></settings>"), None);
        assert_eq!(restored("<settings><global>"), None);
        assert_eq!(restored("<other><global/></other>"), None);
    }

    // ---- The real plugin, isolated: copies of Windower's FFXIDB.dll builds
    // (CRYSTAL_TEST_FFXIDB_BUILDS_DIR, `FFXIDB-*.dll`, and/or the plugin under
    // CRYSTAL_WINDOWER_ROOT) run in child processes against temporary XML,
    // without Hook or a game.

    /// Test data only, never used by the protection: the two FFXIDB internals
    /// the children call to stand in for Initialize (no Hook API exists here),
    /// field registration and the character selector (its XML read), by link
    /// stamp. Both builds keep the settings (file name first) at object+B8
    /// and the map's drag flag at object+200.
    const BUILDS: [(u32, usize, usize); 2] = [
        (0x6A51_21AC, 0x1_73D0, 0x3_B550), // 1.2.3.14
        (0x6ADE_2305, 0x1_7390, 0x3_ADD0), // 1.2.3.17
    ];
    const SETTINGS_FIELD: usize = 0xB8;
    const DRAG_FLAG: usize = 0x200;

    fn link_stamp(file: &[u8]) -> u32 {
        let nt = u32::from_le_bytes(file[0x3C..0x40].try_into().unwrap()) as usize;
        u32::from_le_bytes(file[nt + 8..nt + 12].try_into().unwrap())
    }

    /// Every FFXIDB build available to these tests that the children know.
    fn fixture_builds() -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::var_os("CRYSTAL_TEST_FFXIDB_BUILDS_DIR") {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name.starts_with("ffxidb-") && name.ends_with(".dll") {
                    found.push(entry.path());
                }
            }
        }
        if let Some(root) = std::env::var_os("CRYSTAL_WINDOWER_ROOT") {
            found.push(PathBuf::from(root).join("plugins").join("FFXIDB.dll"));
        }
        found.retain(|path| std::fs::read(path).is_ok_and(|file| BUILDS.iter().any(|build| build.0 == link_stamp(&file))));
        found.sort();
        assert!(!found.is_empty(), "set CRYSTAL_TEST_FFXIDB_BUILDS_DIR or CRYSTAL_WINDOWER_ROOT to FFXIDB 1.2.3.14 / 1.2.3.17");
        found
    }

    /// Loads `path` and installs the host's resolver over the stock one, as
    /// `install` does for Hook.
    fn load_plugin(path: &Path) -> HMODULE {
        let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let module = unsafe {
            LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)
        };
        assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
        let _ = BRIDGE.set(Bridge { _hook: OwnedModule(0), resolve: GetProcAddress });
        module
    }

    fn factory_of(module: HMODULE) -> Option<usize> {
        unsafe { resolve(module, c"CreateInstance".as_ptr().cast()) }.map(|f| f as *const () as usize)
    }

    fn run_child(test: &str, root: &Path, env: &[(&str, &str)]) -> std::process::Child {
        let mut child = Process::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", &format!("windower_plugin_settings::tests::{test}"), "--ignored", "--nocapture"])
            .env("CRYSTAL_TEST_FFXIDB_ROOT", root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        for (key, value) in env {
            child.env(key, value);
        }
        child.spawn().unwrap()
    }

    fn wait_for(path: &Path, what: &str) {
        let started = Instant::now();
        while !path.exists() {
            assert!(started.elapsed() < Duration::from_secs(15), "{what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[repr(C)]
    struct VendorString {
        bytes: [u8; 16],
        length: u32,
        capacity: u32,
    }
    impl VendorString {
        fn new(value: &str) -> Self {
            assert!(value.len() < 16 && value.is_ascii());
            let mut result = Self { bytes: [0; 16], length: value.len() as u32, capacity: 15 };
            result.bytes[..value.len()].copy_from_slice(value.as_bytes());
            result
        }
    }

    #[test]
    #[ignore = "Child of the isolated exact-DLL regression; never run against live settings"]
    fn vendor_settings_child() {
        let Some(root) = std::env::var_os("CRYSTAL_TEST_FFXIDB_ROOT").map(PathBuf::from) else { return };
        let scenario = std::env::var("CRYSTAL_TEST_FFXIDB_SCENARIO").unwrap();
        let role = std::env::var("CRYSTAL_TEST_FFXIDB_ROLE").unwrap();
        let path = root.join("FFXIDB.dll");
        let xml = root.join("FFXIDB.xml").canonicalize().unwrap();
        let stamp = link_stamp(&std::fs::read(&path).unwrap());
        let &(_, register, select) = BUILDS.iter().find(|build| build.0 == stamp).unwrap();
        let module = load_plugin(&path);
        // Ready before anything can wait on the shared mutex the parent holds.
        std::fs::write(root.join(format!("{role}.ready")), b"ready").unwrap();
        assert_eq!(factory_of(module), Some(create_instance as *const () as usize));
        let base = module as usize;
        unsafe {
            let instance = create_instance();
            assert!(!instance.is_null());
            let vtable = *(instance as *const *const usize);
            assert_eq!(*vtable.add(WRAPPED[1]), wrappers()[1], "FFXIDB's own vtable now holds the wrappers");
            // The settings file name Initialize would take from Hook's API.
            let settings = (instance as *mut u8).add(SETTINGS_FIELD);
            let name = settings.cast::<u32>();
            assert_eq!((*name.add(4), *name.add(5)), (0, 7), "an empty std::wstring");
            let mut filename: Vec<_> = xml.as_os_str().encode_wide().chain(Some(0)).collect();
            *name = filename.as_mut_ptr() as u32;
            *name.add(4) = filename.len() as u32 - 1;
            *name.add(5) = filename.len() as u32 - 1;
            std::mem::forget(filename);
            let register: unsafe extern "thiscall" fn(*mut c_void, *const VendorString, i32, u32, u32) = std::mem::transmute(base + register);
            let select: unsafe extern "thiscall" fn(*mut c_void, *const VendorString) = std::mem::transmute(base + select);
            let character = if scenario.starts_with("named") { "Alpha" } else { "global" };
            // Initialize's settings work, in a scope as Initialize runs.
            serialized(|| {
                register(settings.cast(), &VendorString::new("X"), 0, 0, 0);
                register(settings.cast(), &VendorString::new("Y"), 0, 0, 0);
                select(settings.cast(), &VendorString::new(character));
            });
            if scenario == "mouse" {
                // A drag in progress from the map's corner, released at (100, 250).
                *(instance as *mut u8).add(DRAG_FLAG) = 1;
                *(instance as *mut u8).add(DRAG_FLAG - 8).cast::<f32>() = 0.0;
                *(instance as *mut u8).add(DRAG_FLAG - 4).cast::<f32>() = 0.0;
                let callback: Mouse = std::mem::transmute(*vtable.add(WRAPPED[3]));
                callback(instance, 2, 100, 250, 0, 0);
            } else {
                let callback: Command = std::mem::transmute(match scenario.as_str() {
                    "stock" => ORIGINAL[1].load(Ordering::Acquire),
                    _ => *vtable.add(WRAPPED[1]),
                });
                if scenario == "named-save" {
                    callback(instance, c"pos 100 250".as_ptr().cast());
                    callback(instance, c"save".as_ptr().cast());
                } else {
                    let (text, times) = if scenario == "six" { (c"pos 100 200", 12) } else { (c"pos 100 250", 1) };
                    for _ in 0..times {
                        callback(instance, text.as_ptr().cast());
                    }
                }
            }
        }
        std::fs::write(root.join(format!("{role}.done")), b"done").unwrap();
        // Vendor objects keep test-owned buffers; process exit reclaims them.
    }

    #[test]
    #[ignore = "Executes FFXIDB copies against isolated XML, without Hook or a game"]
    fn installed_plugin_position_callbacks_preserve_settings() {
        let initial = "<settings><global><X>100</X><Y>200</Y></global><Alpha><X>310</X><Y>410</Y></Alpha><Beta><X>510</X><Y>610</Y></Beta></settings>";
        let fields = |xml: &str, section: &str| {
            let document = roxmltree::Document::parse(xml).unwrap();
            let node = document.root_element().children().find(|node| node.has_tag_name(section));
            node.map(|node| AXES.map(|axis| node.children().find(|child| child.has_tag_name(axis)).and_then(|child| child.text()).map(str::to_owned)))
        };
        let pair = |x: Option<&str>, y: Option<&str>| Some([x.map(str::to_owned), y.map(str::to_owned)]);
        for source in fixture_builds() {
            let root = std::env::temp_dir().join(format!("crystal-ffxidb-vendor-{}", unique()));
            std::fs::create_dir(&root).unwrap();
            std::fs::copy(&source, root.join("FFXIDB.dll")).unwrap();
            let shared = format!("Local\\CrystalLauncher.FFXIDB.vendor-test.{}", unique());
            let spawn = |scenario: &str, role: &str| {
                run_child("vendor_settings_child", &root, &[
                    ("CRYSTAL_TEST_FFXIDB_SCENARIO", scenario),
                    ("CRYSTAL_TEST_FFXIDB_ROLE", role),
                    ("CRYSTAL_TEST_FFXIDB_SHARED_MUTEX", &shared),
                ])
            };
            for scenario in ["stock", "global", "mouse", "named", "named-save"] {
                std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
                assert!(spawn(scenario, scenario).wait().unwrap().success(), "{}: {scenario}", source.display());
                let xml = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
                let (global, alpha, beta) = (fields(&xml, "global"), fields(&xml, "Alpha"), fields(&xml, "Beta"));
                let (expected_global, expected_alpha, expected_beta) = match scenario {
                    // The stock callback, unwrapped, reproduces the loss.
                    "stock" => (pair(None, Some("250")), fields(initial, "Alpha"), fields(initial, "Beta")),
                    "global" | "mouse" => (pair(Some("100"), Some("250")), fields(initial, "Alpha"), fields(initial, "Beta")),
                    // X moved to the global value: pinned, not inherited or put back.
                    "named" => (fields(initial, "global"), pair(Some("100"), Some("250")), fields(initial, "Beta")),
                    // The stock `save`, which drops every character, is unchanged.
                    _ => (pair(Some("100"), Some("250")), None, None),
                };
                assert_eq!((global, alpha, beta), (expected_global, expected_alpha, expected_beta), "{}: {scenario}", source.display());
            }
            // Six games saving at once, held at the shared mutex until all have
            // started: every section survives 72 saves.
            std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
            let gate = TransactionMutex::open(&shared).unwrap();
            let guard = gate.acquire(0).unwrap();
            let mut children: Vec<_> = (0..6).map(|i| spawn("six", &format!("six{i}"))).collect();
            for i in 0..6 {
                wait_for(&root.join(format!("six{i}.ready")), "all six FFXIDB processes must start");
            }
            thread::sleep(Duration::from_millis(200));
            assert!(!(0..6).any(|i| root.join(format!("six{i}.done")).exists()), "the held mutex must stop every settings read");
            drop(guard);
            for child in &mut children {
                assert!(child.wait().unwrap().success());
            }
            let xml = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
            for section in ["global", "Alpha", "Beta"] {
                assert_eq!(fields(&xml, section), fields(initial, section), "{}: {section}", source.display());
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    fn loaded(base: usize) -> bool {
        let mut module = std::ptr::null_mut();
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                base as *const u16,
                &mut module,
            ) != 0
                && module as usize == base
        }
    }

    #[test]
    #[ignore = "Child of the isolated FFXIDB relaunch regression; never run against live settings"]
    fn relaunch_child() {
        let Some(root) = std::env::var_os("CRYSTAL_TEST_FFXIDB_ROOT").map(PathBuf::from) else { return };
        assert!(std::env::var_os("CRYSTAL_TEST_FFXIDB_SHARED_MUTEX").is_some());
        let path = root.join("FFXIDB.dll");
        let protected = Some(create_instance as *const () as usize);
        let disabled = Some(disabled_factory as *const () as usize);
        // Bounded: the parent also kills this child if any unload never returns.
        let starting = |game: usize| {
            let previous = HELD.load(Ordering::SeqCst);
            let started = Instant::now();
            game_starting();
            assert!(started.elapsed() < Duration::from_secs(10), "game {game}: releasing the previous FFXIDB copy took {:?}", started.elapsed());
            assert_eq!(HELD.load(Ordering::SeqCst), 0, "game {game}");
            previous
        };
        // Games 1 and 2 in one host, the way Hook runs them: load, resolve,
        // construct, then FreeLibrary at the end of the game. The instance is
        // never initialized (no Hook API), so it is leaked, not released.
        for game in 1..=2 {
            let previous = starting(game);
            if game > 1 {
                assert!(!loaded(previous), "game {game}: the previous game's FFXIDB must be unloaded before GameStart");
            }
            let module = load_plugin(&path);
            let base = module as usize;
            assert_eq!(factory_of(module), protected, "game {game} must get a fresh FFXIDB mapping");
            assert!(!unsafe { create_instance() }.is_null(), "game {game}");
            assert_eq!(HELD.load(Ordering::SeqCst), base, "game {game}");
            unsafe { FreeLibrary(module) };
            // The 2026-10-23 test build: Hook's FreeLibrary unmapped FFXIDB
            // inside the game's teardown, and clients hung at /logout.
            assert!(loaded(base), "game {game}: FFXIDB must stay mapped through the end of its game");
        }
        // Something else keeps FFXIDB mapped past its game: game 4's factory
        // must refuse that copy (its scan caches hold game 3's addresses).
        let previous = starting(3);
        assert!(!loaded(previous));
        let module = load_plugin(&path);
        let extra = load_plugin(&path);
        assert_eq!(module, extra);
        let base = module as usize;
        assert_eq!(factory_of(module), protected);
        assert!(!unsafe { create_instance() }.is_null());
        assert_eq!(factory_of(module), protected, "Hook asking again within game 3 is still served");
        unsafe { FreeLibrary(module) };
        assert_eq!(starting(4), base);
        assert!(loaded(base), "the extra reference keeps the old mapping");
        assert_eq!(factory_of(module), disabled, "game 4 must not reuse game 3's copy");
        unsafe { FreeLibrary(extra) };
        assert!(!loaded(base));
        std::fs::write(root.join("relaunch.done"), b"done").unwrap();
    }

    #[test]
    #[ignore = "Executes FFXIDB copies, without Hook or a game"]
    fn installed_plugin_unloads_between_games_in_one_host() {
        for source in fixture_builds() {
            let root = std::env::temp_dir().join(format!("crystal-ffxidb-relaunch-{}", unique()));
            std::fs::create_dir(&root).unwrap();
            std::fs::copy(&source, root.join("FFXIDB.dll")).unwrap();
            let shared = format!("Local\\CrystalLauncher.FFXIDB.relaunch-test.{}", unique());
            let mut child = run_child("relaunch_child", &root, &[("CRYSTAL_TEST_FFXIDB_SHARED_MUTEX", &shared)]);
            // Four games of load/unload take well under a second; a stuck
            // FreeLibrary (loader-lock wait in FFXIDB's detach) never returns.
            let deadline = Instant::now() + Duration::from_secs(60);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{}: FFXIDB relaunch child did not finish in 60 s: an unload or load hung", source.display());
                }
                thread::sleep(Duration::from_millis(50));
            };
            assert!(status.success(), "{}", source.display());
            assert!(root.join("relaunch.done").exists(), "child skipped the scenario");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    #[ignore = "Read-only installed PE verification; no vendor DLL is loaded or executed"]
    fn installed_hook_builds_match_the_plugin_loader_check() {
        let root = std::env::var_os("CRYSTAL_WINDOWER_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Games\Final Fantasy XI Programs\Windower 4"));
        let mut hooks = vec![root.join("Hook.dll")];
        // Optional local archive verifies every Hook build there, including
        // versions no longer installed. Never download or execute these DLLs.
        if let Some(archive) = std::env::var_os("CRYSTAL_TEST_HOOK_BUILDS_DIR") {
            for entry in std::fs::read_dir(archive).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
                if name.starts_with("hook-") && name.ends_with(".dll") {
                    hooks.push(path);
                }
            }
        }
        let known: Vec<Plugins> = crate::hook_layout::KNOWN.iter().filter_map(|k| k.plugins).collect();
        for path in hooks.into_iter().filter(|path| path.is_file()) {
            let file = std::fs::read(&path).unwrap();
            let mut image = crate::windower_bootstrap_contract::map_pe_relocated(&file);
            let base = image.as_ptr() as usize;
            let resolved = crate::hook_layout::tests::resolve_mapped(&image, base);
            let Ok(build) = resolved.plugins else {
                eprintln!("{}: plugin loader not resolved: {:?}", path.display(), resolved.plugins);
                assert!(resolved.timestamp < 0x6900_0000, "only the 2022 build may be refused");
                continue;
            };
            eprintln!("{}: {:08X} {build:x?}", path.display(), resolved.timestamp);
            assert!(hook_layout(base, &build));
            for other in known.iter().filter(|other| **other != build) {
                assert!(!hook_layout(base, other), "build offsets are not interchangeable");
            }
            for rva in [
                build.factory_lookup,
                build.factory_lookup + 8,
                build.factory_lookup + 0x17,
                build.factory_name,
                build.initialize_call,
            ] {
                image[rva] ^= 1;
                assert!(!hook_layout(base, &build), "changed loader at {rva:X}");
                image[rva] ^= 1;
            }
            assert!(hook_layout(base, &build));
        }
    }

    #[test]
    fn an_unprotected_ffxidb_is_reported_to_the_launcher_once_per_host() {
        let reason = "this FFXIDB.dll imports no CreateFile function from KERNEL32, so Crystal cannot see it open its settings";
        let sent = crate::events::capture(|| {
            report_unprotected(reason);
            report_unprotected(reason);
        });
        assert_eq!(sent, vec![crate::proto::notice_line(&format!("FFXIDB minimap settings protection is off: {reason}."))]);
    }
}
