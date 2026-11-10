//! Serialize FFXIDB's complete settings transactions between Crystal hosts.
//!
//! FFXIDB 1.2.3.14 ignores an unsuccessful XML read and then saves its fresh
//! document. Its CRT opens writers exclusively, so simultaneous startup or a
//! later minimap save can erase another character's settings. A file-open lock
//! would be too short: it must include read, modify, and write.
//!
//! This adapter is deliberately specific to the verified FFXIDB build below;
//! Hook's side is found by signature (crate::hook_layout) in any Hook build.
//! Only Hook's GetProcAddress import and the new FFXIDB object's vptr change.
//! No instructions, vendor files, other plugins, or process-wide APIs change.
//! The original factory, object, callbacks, calling conventions, and return
//! values remain in use. Crystal's tables live until process exit.
//!
//! FFXIDB caches its signature-scan results in a function-local static map
//! (RVA C77F0, filled at 3E358). Some entries are pointer values it read from
//! the game's heap at scan time, so they are only valid for the game that was
//! running then. A fresh mapping starts with that map unconstructed (zero
//! .bss); a mapping that outlived its game still holds the old pointers and
//! faults in the minimap target draw (6E3E4) once the next game is in a zone.
//!
//! So every game must get a fresh mapping, but FFXIDB must not be unmapped
//! inside the game's own teardown. Hook FreeLibrary's every plugin while the
//! game is closing, under its plugin-list lock, with game, Hook and renderer
//! threads still shutting down. The 2026-10-23 17:14 test build let that
//! FreeLibrary run FFXIDB's DLL_PROCESS_DETACH and unmap it there, and
//! clients then hung at /logout. Earlier builds kept a reference for the
//! whole process and never hung at logout, but reused the stale mapping in
//! the next game.
//! Crystal now holds one reference per mapping (HELD) through the end of its
//! game, exactly like those releases at logout, and drops it in
//! `game_starting`, on the host thread before the next GameStart, when no
//! game, Hook teardown or plugin is running. Hook then maps a fresh copy.
//! If anything else still keeps the old copy mapped, the factory refuses it
//! (logged) instead of letting FFXIDB use the previous game's addresses.

use crate::hook_layout::{OwnedModule, Plugins};
use std::{
    ffi::{c_void, CStr, OsString},
    fs::{File, OpenOptions},
    io::{Read, Write},
    marker::PhantomData,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicPtr, AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};
use windows_sys::{
    core::PCSTR,
    Win32::{
        Foundation::{CloseHandle, FreeLibrary, FARPROC, HMODULE, WAIT_ABANDONED, WAIT_OBJECT_0},
        Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE},
        Storage::FileSystem::{MoveFileExW, ReplaceFileW, FILE_SHARE_DELETE, FILE_SHARE_READ},
        System::{
            LibraryLoader::{
                GetModuleHandleExW, GetProcAddress,
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            },
            Threading::{
                CreateMutexExW, ReleaseMutex, WaitForSingleObject, INFINITE,
                MUTEX_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
            },
        },
    },
};

const FFXIDB_SHA256: &str = "a843f6eca81fdf6b3e60984e0dd0bd93f6b292b2ccfe8bf79cca0fe51dbd8a58";
const FFXIDB_ID: (u32, u32) = (0x6A51_21AC, 0xD_5000);
const FACTORY_RVA: usize = 0x6_9DC0;
const VTABLE_RVA: usize = 0xB_54C4;
const SHARED_SETTINGS_MUTEX: &str = "Local\\CrystalLauncher.FFXIDB.settings.shared.v1";
// FFXIDB's scan-result cache: a lazily constructed std::map {head, size} in
// zero-filled .data. `mov ecx, offset cache` at both its lookup (3DCB9) and
// its store (3E361) is checked live before the cache is trusted to be empty.
const SCAN_CACHE_RVA: usize = 0xC_77F0;
const SCAN_CACHE_REFERENCES: [usize; 2] = [0x3_DCB9, 0x3_E361];
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
static COMMAND_TRACES: AtomicUsize = AtomicUsize::new(0);
static POSITION_TRACES: AtomicUsize = AtomicUsize::new(0);

static REPAIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

// Hook's plugin loader (crate::hook_layout, any build): it pushes
// "CreateInstance" and the module, calls its KERNEL32!GetProcAddress import,
// then calls the returned cdecl factory at factory_lookup + 0x17. Initialize
// pushes (API, object) before public stdcall vtable+8. 4.7.9.3 and 4.7.9.6
// reproduce their proven tables; other builds use the scan alone.

// The 17 public stdcall callbacks are followed by a deleting destructor and
// 16 internal thiscall callbacks. Preserve every entry and the RTTI locator
// at vtable[-1], since the stock callbacks dispatch to the internal entries.
const VTABLE_RVAS: [usize; 34] = [
    0x78A50, 0x78A40, 0x78940, 0x78920, 0x78910, 0x78900, 0x788F0, 0x788D0, 0x788B0, 0x78890,
    0x78870, 0x78670, 0x78640, 0x78610, 0x785F0, 0x785D0, 0x785B0, 0x149D0, 0x6A670, 0x6A470,
    0x77810, 0x6A180, 0x6A170, 0x6A1A0, 0x77800, 0x777F0, 0x777E0, 0x6A4A0, 0x777D0, 0x6A120,
    0x777C0, 0x777B0, 0x777A0, 0x6A190,
];

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


struct Bridge {
    _hook: OwnedModule,
    resolve: Resolve,
}
/// One FFXIDB mapping. It records the base address only; the one module
/// reference Crystal keeps on that mapping lives in HELD and is dropped
/// before the next GameStart.
struct Adapter {
    module: usize,
    path: PathBuf,
    factory: Factory,
    original: [usize; 34],
    original_vtable: usize,
    // Box is stable when Adapter moves into its leaked allocation.
    shadow: Box<[usize; 35]>,
    transaction: &'static SettingsMutex,
    /// GAME value when an instance was last constructed; NONE before that.
    constructed_in: AtomicUsize,
}
const NONE: usize = usize::MAX;
static BRIDGE: OnceLock<Bridge> = OnceLock::new();
// The adapter for the FFXIDB mapping currently in use. Replaced when a game
// loads a new mapping; earlier adapters are leaked so no reference dangles.
static ADAPTER: AtomicPtr<Adapter> = AtomicPtr::new(std::ptr::null_mut());
static INSTALL: Mutex<()> = Mutex::new(());
static MAPPINGS: AtomicUsize = AtomicUsize::new(0);

fn current_adapter() -> Option<&'static Adapter> {
    let adapter = ADAPTER.load(Ordering::SeqCst);
    // SAFETY: only `publish` stores here, always a leaked, never-freed Box.
    (!adapter.is_null()).then(|| unsafe { &*adapter })
}

fn publish(adapter: Adapter) -> &'static Adapter {
    let adapter: &'static Adapter = Box::leak(Box::new(adapter));
    ADAPTER.store(adapter as *const Adapter as *mut Adapter, Ordering::SeqCst);
    adapter
}

// GameStart calls made by this host. FFXIDB scan results are valid only for
// the game that was running when they were taken.
static GAME: AtomicUsize = AtomicUsize::new(0);
// Base of the FFXIDB mapping Crystal holds one module reference on (0: none).
// Taken when a mapping is verified; dropped only by `release_held`.
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
        // Never more than one reference: the same mapping verified twice, or
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

/// The adapter in use, as `decide` sees it.
#[derive(Clone, Copy, Debug)]
struct Previous {
    module: usize,
    still_mapped: bool,
    /// The game during which an instance was last constructed from it.
    constructed_in: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Verify this mapping and protect its instance.
    Protect,
    /// The same mapping, asked again during the game that built it.
    SameGame,
    /// A second FFXIDB image is loaded at the same time.
    SecondImage,
    /// FFXIDB's scan cache holds game addresses from an earlier game: its
    /// stock code would dereference them. Never construct an instance.
    StaleScanCache,
}

fn decide(previous: Option<Previous>, module: usize, cache_fresh: bool, game: usize) -> Decision {
    if let Some(p) = previous {
        if p.module != module && p.still_mapped {
            return Decision::SecondImage;
        }
    }
    if cache_fresh {
        return Decision::Protect;
    }
    // A populated cache is expected only when this same mapping already
    // built an instance during the current game (Hook asking again).
    match previous {
        Some(p) if p.module == module && p.constructed_in == Some(game) => Decision::SameGame,
        _ => Decision::StaleScanCache,
    }
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
    ok != 0
        && module as usize == base
        && module_path(base).is_some_and(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("FFXIDB.dll"))
        })
}

/// FFXIDB's scan cache has never been constructed in this mapping, so every
/// game address it uses will be scanned against the running game.
fn scan_cache_fresh(base: usize) -> bool {
    read::<[u32; 2]>(base + SCAN_CACHE_RVA) == Some([0, 0])
}

use crate::mem::read;

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

fn file_matches(path: &Path, expected: &str) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| sha256(&bytes))
        .as_deref()
        == Some(expected)
}

/// The FFXIDB file at `path` is the one build this adapter supports (the
/// plugin side is still pinned; see the module notes). For the release survey.
#[cfg(test)]
pub(crate) fn ffxidb_file_supported(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("{} is not installed", path.display()));
    }
    file_matches(path, FFXIDB_SHA256).then_some(())
        .ok_or_else(|| "not the verified FFXIDB 1.2.3.14 build (the adapter is pinned to it)".into())
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

fn plugin_layout(base: usize) -> bool {
    read::<u32>(base + VTABLE_RVA - 4) == Some((base + 0xBCEE0) as u32)
        && VTABLE_RVAS.iter().enumerate().all(|(index, rva)| {
            read::<u32>(base + VTABLE_RVA + index * 4) == Some((base + rva) as u32)
        })
        && read::<[u8; 5]>(base + FACTORY_RVA) == Some([0x55, 0x8B, 0xEC, 0x6A, 0xFF])
        && read::<[u8; 5]>(base + 0x78940) == Some([0x55, 0x8B, 0xEC, 0x6A, 0xFF])
        && read::<[u8; 3]>(base + 0x78A35) == Some([0xC2, 0x08, 0x00])
        && read::<[u8; 3]>(base + 0x788EB) == Some([0xC2, 0x08, 0x00])
        && read::<[u8; 3]>(base + 0x78866) == Some([0xC2, 0x14, 0x00])
        && read::<[u8; 3]>(base + 0x78633) == Some([0xC2, 0x18, 0x00])
        && SCAN_CACHE_REFERENCES.iter().all(|rva| {
            read::<u8>(base + rva) == Some(0xB9)
                && read::<u32>(base + rva + 1) == Some((base + SCAN_CACHE_RVA) as u32)
        })
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
/// An unsupported Hook build is explicitly logged and left stock.
pub(crate) fn install() -> Result<bool, String> {
    let _serial = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(hook) = crate::hook_layout::retain_hook() else { return Ok(false) };
    let base = hook.0;
    let (build, version) = match verified_hook(base) {
        Ok(found) => found,
        Err(reason) => {
            log!(
                "note: FFXIDB shared-settings protection unavailable: Windower Hook.dll verification failed: {reason} (loaded PE identity {:X?})",
                pe_identity(base)
            );
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

fn shadow_table(original: &[usize; 34], rtti: usize) -> Box<[usize; 35]> {
    let mut shadow = Box::new([0; 35]);
    shadow[0] = rtti;
    shadow[1..].copy_from_slice(original);
    // XML entry points in this exact image:
    // - Initialize registers fields (173D0/17790/17B60/17F20) and loads the
    //   current character (3B550).
    // - Command -> 701F0 -> 238D0 (save all), 26170, or 6B3E0/6BD00/
    //   6C110/6C2D0 -> 25E70/25D60/25C40 -> 23DD0 (save one field).
    // - Mouse -> 72540 -> 6C110/6C2D0 -> 25C40 -> 23DD0.
    // - Incoming -> 78670 -> 3B550 loads character settings on 0x00A/0x00B.
    // Update/render/shutdown do not reach these XML helpers. Leave their
    // graphics/resource calls outside the mutex to avoid adding a lock order
    // between the settings lock and unrelated Hook/Direct3D callbacks.
    for (slot, wrapper) in [
        (2, initialize as *const () as usize),
        (7, command as *const () as usize),
        (11, incoming as *const () as usize),
        (13, mouse as *const () as usize),
    ] {
        shadow[slot + 1] = wrapper;
    }
    shadow
}

/// Verify one FFXIDB mapping. The module reference taken here only covers
/// preparation (Hook holds its own); it is released before returning.
fn prepare(
    module: usize,
    factory: usize,
    path: &Path,
    reuse: Option<&'static SettingsMutex>,
) -> Result<Option<Adapter>, String> {
    let _preparing =
        OwnedModule::retain_at(module).ok_or("FFXIDB module disappeared during adapter preparation")?;
    // The image identity, factory and layout were checked by the caller.
    if !file_matches(path, FFXIDB_SHA256) {
        return Ok(None);
    }
    let transaction = match reuse {
        Some(transaction) => transaction,
        None => &*Box::leak(Box::new(SettingsMutex::open(path)?)),
    };
    {
        let _probe = transaction.acquire(30_000)?;
    }
    let original = VTABLE_RVAS.map(|rva| module + rva);
    let shadow = shadow_table(&original, module + 0xBCEE0);
    Ok(Some(Adapter {
        module,
        path: path.to_owned(),
        factory: unsafe { std::mem::transmute::<usize, Factory>(factory) },
        original,
        original_vtable: module + VTABLE_RVA,
        shadow,
        transaction,
        constructed_in: AtomicUsize::new(NONE),
    }))
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
    let Some(path) = module_path(module as usize) else {
        return original;
    };
    if !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("FFXIDB.dll"))
    {
        return original;
    }
    let _serial = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
    resolve_ffxidb(module as usize, original, &path)
}

fn resolve_ffxidb(module: usize, original: FARPROC, path: &Path) -> FARPROC {
    let previous = current_adapter();
    let game = GAME.load(Ordering::SeqCst);
    let factory = original.unwrap() as *const () as usize;
    // The scan-cache offsets mean something only in the pinned image; any
    // other build is left stock.
    let pinned = pe_identity(module) == Some(FFXIDB_ID)
        && factory == module + FACTORY_RVA
        && plugin_layout(module);
    if !pinned {
        log!("note: FFXIDB shared-settings protection unavailable: unsupported plugin build (requires verified 1.2.3.14); leaving stock callbacks");
        return original;
    }
    let decision = decide(
        previous.map(|p| Previous {
            module: p.module,
            still_mapped: p.module != module && mapped_ffxidb(p.module),
            constructed_in: match p.constructed_in.load(Ordering::SeqCst) {
                NONE => None,
                game => Some(game),
            },
        }),
        module,
        scan_cache_fresh(module),
        game,
    );
    match decision {
        Decision::SecondImage => {
            log!("error: refusing a second FFXIDB image with a different settings location");
            return as_farproc(disabled_factory);
        }
        Decision::StaleScanCache => {
            // Only reachable if something other than Crystal kept FFXIDB
            // mapped after Crystal released it. Its own code would crash this
            // game. A later game loads it again once that reference is gone.
            log!("note: FFXIDB not loaded for this game: the plugin stayed in memory after the previous game and still holds that game's addresses");
            return as_farproc(disabled_factory);
        }
        Decision::SameGame => return as_farproc(create_instance),
        Decision::Protect => {}
    }
    let reuse = previous.filter(|p| p.path == path).map(|p| p.transaction);
    match prepare(module, factory, path, reuse) {
        Ok(Some(adapter)) => {
            publish(adapter);
            hold(module);
            let mapping = MAPPINGS.fetch_add(1, Ordering::Relaxed) + 1;
            log!("stage: verified FFXIDB 1.2.3.14 shared-settings transaction protection prepared (mapping {mapping} in this host, base {module:#x}, scan cache empty; kept loaded until the next game starts)");
            as_farproc(create_instance)
        }
        Ok(None) => {
            log!("note: FFXIDB shared-settings protection unavailable: unsupported plugin build (requires verified 1.2.3.14); leaving stock callbacks");
            original
        }
        Err(error) => {
            log!("error: FFXIDB disabled before construction because settings protection failed: {error}");
            as_farproc(disabled_factory)
        }
    }
}

unsafe extern "C" fn disabled_factory() -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn create_instance() -> *mut c_void {
    let adapter = adapter();
    let game = GAME.load(Ordering::SeqCst);
    // Re-check at construction: Hook calls the factory right after resolving
    // it, but never construct over a scan cache left by an earlier game.
    if !scan_cache_fresh(adapter.module)
        && adapter.constructed_in.load(Ordering::SeqCst) != game
    {
        log!("note: FFXIDB not constructed: its scan cache holds an earlier game's addresses");
        return std::ptr::null_mut();
    }
    let _guard = match adapter.transaction.acquire(30_000) {
        Ok(guard) => guard,
        Err(error) => {
            log!("error: FFXIDB not constructed: {error}");
            return std::ptr::null_mut();
        }
    };
    let instance = (adapter.factory)();
    if instance.is_null() {
        return instance;
    }
    if let Err(error) = replace_pointer(
        instance as usize,
        adapter.original_vtable,
        adapter.shadow[1..].as_ptr() as usize,
    ) {
        // Do not guess a destructor for an object whose vptr is unexpected.
        // Nothing has been initialized or published to Hook yet. Its normal
        // null-factory path rejects the plugin; the small allocation remains.
        log!("error: FFXIDB not initialized: {error}");
        return std::ptr::null_mut();
    }
    adapter.constructed_in.store(game, Ordering::SeqCst);
    log!("stage: FFXIDB settings callbacks protected across Crystal game hosts");
    instance
}

fn adapter() -> &'static Adapter {
    current_adapter().unwrap_or_else(|| std::process::abort())
}
fn callback_transaction(adapter: &Adapter) -> SettingsTransaction<'_> {
    adapter
        .transaction
        .acquire(INFINITE)
        .unwrap_or_else(|error| {
            // A permanent valid handle cannot time out. Never call stock code
            // unlocked, or return from void Initialize pretending it succeeded.
            log!(
                "error: {error}; terminating this host to prevent unlocked FFXIDB settings writes"
            );
            std::process::abort()
        })
}

#[derive(Debug, PartialEq)]
enum SettingsCommand {
    Position { x: Option<f32>, y: Option<f32> },
    Save,
    Reload,
}

fn settings_command(text: &str) -> Option<SettingsCommand> {
    // The verified callback treats ASCII verbs case-insensitively and removes
    // double quotes while grouping tokens. Classify the same numeric commands
    // without retaining or logging arbitrary command text.
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    for character in text.chars() {
        if character == '"' {
            quoted = !quoted;
        } else if character.is_ascii_whitespace() && !quoted {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(character.to_ascii_lowercase());
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    let mut words = tokens.iter().map(String::as_str);
    let mut verb = words.next()?;
    // The verified plugin accepts these optional map prefixes.
    if matches!(verb, "m" | "map") {
        verb = words.next()?;
    }
    let number = |word: Option<&str>| word?.parse::<f32>().ok().filter(|n| n.is_finite());
    match verb {
        "p" | "pos" | "position" => Some(SettingsCommand::Position {
            x: Some(number(words.next())?),
            y: Some(number(words.next())?),
        }),
        "px" | "posx" | "positionx" => Some(SettingsCommand::Position {
            x: Some(number(words.next())?),
            y: None,
        }),
        "py" | "posy" | "positiony" => Some(SettingsCommand::Position {
            x: None,
            y: Some(number(words.next())?),
        }),
        "save" => Some(SettingsCommand::Save),
        "r" | "reload" => Some(SettingsCommand::Reload),
        _ => None,
    }
}

fn bounded_command(text: *const u8) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let mut bytes = Vec::new();
    for offset in 0..256 {
        let byte = read::<u8>((text as usize).checked_add(offset)?)?;
        if byte == 0 {
            return String::from_utf8(bytes).ok();
        }
        bytes.push(byte);
    }
    None
}

fn global_settings_active(this: *mut c_void) -> bool {
    settings_scope(this).as_deref() == Some("global")
}

fn valid_settings_scope(scope: &str) -> bool {
    // Character scopes are XML element names in the verified settings format.
    // Accept the bounded ASCII representation; never interpolate arbitrary XML.
    !scope.is_empty()
        && scope.len() <= 128
        && scope.as_bytes()[0].is_ascii_alphabetic()
        && scope.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// The characters of an MSVC `basic_string<T>` {buffer-or-pointer[16], size,
/// capacity} at `address`, read in one span: inline below the small-string
/// capacity, else at the pointer. Refuses an empty, oversized or
/// inconsistent representation, and (`require_nul`) a missing terminator.
fn read_msvc_string<T: Copy + Default + PartialEq>(address: usize, max_len: usize, max_cap: Option<u32>, require_nul: bool) -> Option<Vec<T>> {
    let layout = read::<[u32; 6]>(address)?;
    let (length, capacity) = (layout[4] as usize, layout[5] as usize);
    if length == 0 || length > max_len || length > capacity || max_cap.is_some_and(|max| layout[5] > max) {
        return None;
    }
    let data = if capacity < 16 / std::mem::size_of::<T>() { address } else { layout[0] as usize };
    let count = length + usize::from(require_nul);
    let bytes = count.checked_mul(std::mem::size_of::<T>())?;
    data.checked_add(bytes)?;
    let mut out = vec![T::default(); count];
    if !crate::mem::read_into(data, &mut out) || (require_nul && out.pop() != Some(T::default())) {
        return None;
    }
    Some(out)
}

fn settings_scope(this: *mut c_void) -> Option<String> {
    // In this exact, hash-verified image, settings live at object+B8 and the
    // current character std::string at settings+18. An empty login name is
    // normalized to "global" by 3B550. Check the representation before reading.
    let bytes = read_msvc_string::<u8>((this as usize).checked_add(0xD0)?, 128, Some(32767), true)?;
    let scope = String::from_utf8(bytes).ok()?;
    valid_settings_scope(&scope).then_some(scope)
}

fn settings_filename(this: *mut c_void) -> Option<PathBuf> {
    // The filename already selected by FFXIDB is a std::wstring at object+B8.
    // Reading this avoids inferring Hook's settings root from the DLL location.
    let buffer = read_msvc_string::<u16>((this as usize).checked_add(0xB8)?, 32767, None, false)?;
    if buffer.contains(&0) {
        return None;
    }
    let path = PathBuf::from(OsString::from_wide(&buffer));
    (path.is_absolute()
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("FFXIDB.xml")))
    .then_some(path)
}

fn read_settings(file: &mut File) -> Result<String, String> {
    let mut xml = String::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_string(&mut xml)
        .map_err(|e| format!("settings read failed ({:?})", e.kind()))?;
    if xml.len() as u64 > MAX_SETTINGS_BYTES {
        return Err("settings XML exceeds repair limit".into());
    }
    Ok(xml)
}

fn settings_node<'a, 'input>(
    document: &'a roxmltree::Document<'input>,
    scope: &str,
) -> Result<Option<roxmltree::Node<'a, 'input>>, String> {
    let root = document.root_element();
    if root.tag_name().name() != "settings" || root.tag_name().namespace().is_some() {
        return Err("unexpected settings XML root".into());
    }
    let mut nodes = root
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == scope);
    let Some(node) = nodes.next() else {
        return Ok(None);
    };
    if nodes.next().is_some() {
        return Err("settings XML has duplicate target sections".into());
    }
    if node.tag_name().namespace().is_some() {
        return Err("unsupported settings section namespace".into());
    }
    Ok(Some(node))
}

#[derive(Debug)]
struct GlobalPosition {
    fields: [Option<String>; 2],
}

impl GlobalPosition {
    fn capture(xml: &str) -> Result<Self, String> {
        let document = roxmltree::Document::parse(xml).map_err(|_| "invalid settings XML")?;
        let global =
            settings_node(&document, "global")?.ok_or("settings XML has no global section")?;
        Ok(Self {
            fields: position_fields(xml, Some(global))?,
        })
    }

    fn restore_missing(&self, current: &str) -> Result<Option<String>, String> {
        let document =
            roxmltree::Document::parse(current).map_err(|_| "invalid current settings XML")?;
        let global = settings_node(&document, "global")?;
        let mut insertion = String::new();
        for (index, name) in ["X", "Y"].iter().enumerate() {
            let present = position_child(global, name, false)
                .map_err(|()| "ambiguous current global position field")?;
            if present.is_none() {
                if let Some(field) = &self.fields[index] {
                    insertion.push_str(field);
                }
            }
        }
        if insertion.is_empty() {
            return Ok(None);
        }
        // Splice only missing fields into the freshly read document. Preserve
        // every other byte, including newer edits, other characters and comments.
        // If X/Y were its only fields, the vendor removes the empty global
        // section too. Recreate only these coordinates, never its old document.
        append_position_fields(current, &document, global, "global", insertion).map(Some)
    }
}

/// The section's unique, un-namespaced `name` child, if any. Err on a
/// namespaced or second match, or (`leaf_only`) one with element children.
fn position_child<'a, 'i>(
    section: Option<roxmltree::Node<'a, 'i>>,
    name: &str,
    leaf_only: bool,
) -> Result<Option<roxmltree::Node<'a, 'i>>, ()> {
    let mut nodes = section
        .into_iter()
        .flat_map(|node| node.children())
        .filter(|node| node.is_element() && node.tag_name().name() == name);
    let Some(node) = nodes.next() else { return Ok(None) };
    if node.tag_name().namespace().is_some()
        || nodes.next().is_some()
        || (leaf_only && node.children().any(|child| child.is_element()))
    {
        return Err(());
    }
    Ok(Some(node))
}

fn position_fields(
    xml: &str,
    section: Option<roxmltree::Node<'_, '_>>,
) -> Result<[Option<String>; 2], String> {
    let mut fields = [None, None];
    for (index, name) in ["X", "Y"].iter().enumerate() {
        if let Some(node) = position_child(section, name, true).map_err(|()| "ambiguous position field")? {
            if node
                .text()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .is_some_and(f64::is_finite)
            {
                fields[index] = Some(xml[node.range()].to_owned());
            }
        }
    }
    Ok(fields)
}

fn append_position_fields(
    current: &str,
    document: &roxmltree::Document<'_>,
    section: Option<roxmltree::Node<'_, '_>>,
    scope: &str,
    mut insertion: String,
) -> Result<String, String> {
    let container = section.unwrap_or_else(|| document.root_element());
    if section.is_none() {
        insertion = format!("<{scope}>{insertion}</{scope}>");
    }
    let range = container.range();
    let container_xml = &current[range.clone()];
    let (start, end, replacement) = if container_xml.ends_with("/>") {
        (
            range.end - 2,
            range.end,
            format!(">{insertion}</{}>", container.tag_name().name()),
        )
    } else {
        let closing = container_xml
            .rfind("</")
            .ok_or("missing settings closing tag")?;
        (range.start + closing, range.start + closing, insertion)
    };
    let mut result = current.to_owned();
    result.replace_range(start..end, &replacement);
    roxmltree::Document::parse(&result).map_err(|_| "invalid repaired settings XML")?;
    Ok(result)
}

fn pin_character_position(
    current: &str,
    scope: &str,
    position: [Option<f32>; 2],
    retained: &[Option<String>; 2],
) -> Result<Option<String>, String> {
    if !valid_settings_scope(scope) || scope == "global" {
        return Err("cannot verify character settings scope".into());
    }
    let document = roxmltree::Document::parse(current).map_err(|_| "invalid settings XML")?;
    let section = settings_node(&document, scope)?;
    let mut insertion = String::new();
    for (index, name) in ["X", "Y"].into_iter().enumerate() {
        let field = match position[index] {
            Some(value) if value.is_finite() => format!("<{name}>{value}</{name}>"),
            Some(_) => return Err("non-finite character position".into()),
            None => match &retained[index] {
                Some(field) => field.clone(),
                None => continue,
            },
        };
        // An axis the callback already persisted keeps its value and
        // formatting; only compensate for its equality-based deletion.
        if position_child(section, name, true).map_err(|()| "ambiguous character position field")?.is_none() {
            insertion.push_str(&field);
        }
    }
    if insertion.is_empty() {
        Ok(None)
    } else {
        append_position_fields(current, &document, section, scope, insertion).map(Some)
    }
}

struct PositionRepair {
    path: PathBuf,
    before: GlobalPosition,
}

impl PositionRepair {
    fn capture(this: *mut c_void) -> Result<Option<Self>, String> {
        if !global_settings_active(this) {
            return Ok(None);
        }
        let path = settings_filename(this).ok_or("cannot verify FFXIDB settings filename")?;
        let mut file =
            File::open(&path).map_err(|e| format!("settings open failed ({:?})", e.kind()))?;
        let before = GlobalPosition::capture(&read_settings(&mut file)?)?;
        Ok(before
            .fields
            .iter()
            .any(Option::is_some)
            .then_some(Self { path, before }))
    }

    fn apply(&self) -> Result<bool, String> {
        edit_settings(&self.path, |current| self.before.restore_missing(current))
    }
}

fn edit_settings(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<Option<String>, String>,
) -> Result<bool, String> {
    // Deny writers while examining and staging the current file. The
    // callback's transaction mutex stays held through the replacement.
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
        .map_err(|e| format!("settings reopen failed ({:?})", e.kind()))?;
    let current = read_settings(&mut file)?;
    let Some(repaired) = edit(&current)? else {
        return Ok(false);
    };
    let parent = path.parent().ok_or("settings path has no parent")?;
    let temporary = parent.join(format!(
        ".FFXIDB.crystal-{}-{}.tmp",
        std::process::id(),
        REPAIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| format!("repair staging failed ({:?})", e.kind()))?;
    let staged = output
        .write_all(repaired.as_bytes())
        .and_then(|()| output.sync_all());
    drop(output);
    // ReplaceFile opens the destination for write access itself. Close our
    // read handle only after staging; all protected hosts still hold/wait on
    // the same transaction mutex. Preserve the existing file's metadata.
    drop(file);
    match staged {
        Ok(()) => {
            let backup = temporary.with_extension("backup");
            // Reserve this name with create_new: ReplaceFile may overwrite
            // its backup name, so it must never be someone else's file.
            let reservation = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&backup)
            {
                Ok(file) => file,
                Err(error) => {
                    let _ = std::fs::remove_file(&temporary);
                    return Err(format!(
                        "repair backup reservation failed ({:?})",
                        error.kind()
                    ));
                }
            };
            drop(reservation);
            let target: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let replacement: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let backup_w: Vec<_> = backup.as_os_str().encode_wide().chain(Some(0)).collect();
            if unsafe {
                ReplaceFileW(
                    target.as_ptr(),
                    replacement.as_ptr(),
                    backup_w.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } == 0
            {
                let error = std::io::Error::last_os_error().raw_os_error();
                recover_replacement_failure(path, &temporary, &backup, &current, error);
                Err(format!("repair replacement failed (Win32 {:?})", error))
            } else {
                let _ = std::fs::remove_file(backup);
                Ok(true)
            }
        }
        Err(e) => {
            // Replacement has not started, so only the incomplete staging
            // file belongs to this failed write; the current XML is intact.
            let _ = std::fs::remove_file(temporary);
            Err(format!("repair staging write failed ({:?})", e.kind()))
        }
    }
}

fn same_settings_file(path: &Path, expected: &str) -> bool {
    File::open(path)
        .ok()
        .and_then(|mut file| read_settings(&mut file).ok())
        .as_deref()
        == Some(expected)
}

fn recover_replacement_failure(
    target: &Path,
    staging: &Path,
    backup: &Path,
    current: &str,
    error: Option<i32>,
) {
    // ReplaceFile errors 1176/1177 can leave the target missing or renamed.
    // Recover only our complete backup, and never replace a newly created
    // target. A failed non-replacing rename leaves the backup untouched.
    if same_settings_file(backup, current) {
        let source: Vec<_> = backup.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(source.as_ptr(), destination.as_ptr(), 0);
        }
    }
    // Keep recovery copies after every partially completed replacement, even
    // when target recovery succeeded. On an ordinary failure, cleanup is safe
    // only after verifying the complete current file still exists at target.
    if !matches!(error, Some(1176 | 1177)) && same_settings_file(target, current) {
        let _ = std::fs::remove_file(staging);
        let _ = std::fs::remove_file(backup);
    }
}

enum PositionSnapshot {
    Global(PositionRepair),
    Character {
        path: PathBuf,
        scope: String,
        axes: [bool; 2],
        retained: [Option<String>; 2],
    },
}

impl PositionSnapshot {
    fn capture(this: *mut c_void, axes: [bool; 2]) -> Result<Option<Self>, String> {
        let scope = settings_scope(this).ok_or("cannot verify active settings scope")?;
        if scope == "global" {
            return PositionRepair::capture(this).map(|snapshot| snapshot.map(Self::Global));
        }
        let path = settings_filename(this).ok_or("cannot verify FFXIDB settings filename")?;
        let mut file = File::open(&path)
            .map_err(|error| format!("settings open failed ({:?})", error.kind()))?;
        let before = read_settings(&mut file)?;
        let document = roxmltree::Document::parse(&before).map_err(|_| "invalid settings XML")?;
        let before = position_fields(&before, settings_node(&document, &scope)?)?;
        // Stock px/py saves both axes. Keep an existing untouched override,
        // but never turn an inherited untouched axis into a new override.
        let retained = std::array::from_fn(|index| {
            if axes[index] {
                None
            } else {
                before[index].clone()
            }
        });
        Ok(Some(Self::Character {
            path,
            scope,
            axes,
            retained,
        }))
    }

    fn apply(&self, this: *mut c_void) -> Result<bool, String> {
        let (path, scope) = match self {
            Self::Global(repair) => (&repair.path, "global"),
            Self::Character { path, scope, .. } => (path, scope.as_str()),
        };
        if settings_scope(this).as_deref() != Some(scope)
            || settings_filename(this).as_ref() != Some(path)
        {
            return Err("active settings scope or filename changed during position save".into());
        }
        match self {
            Self::Global(repair) => repair.apply(),
            Self::Character { axes, retained, .. } => {
                // The verified position setters store accepted coordinates at
                // map+0/+4 (object+148) before saving fields. Read those values,
                // not raw command arguments or unrelated inherited defaults.
                let position = read::<[f32; 2]>(this as usize + 0x148)
                    .filter(|values| values.iter().all(|value| value.is_finite()))
                    .ok_or("cannot verify accepted character position")?;
                edit_settings(path, |current| {
                    pin_character_position(
                        current,
                        scope,
                        std::array::from_fn(|index| axes[index].then_some(position[index])),
                        retained,
                    )
                })
            }
        }
    }
}

fn position_snapshot(this: *mut c_void, axes: [bool; 2]) -> Option<PositionSnapshot> {
    match PositionSnapshot::capture(this, axes) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if POSITION_TRACES.fetch_add(1, Ordering::Relaxed) < 64 {
                log!("note: FFXIDB position preservation unavailable: {error}");
            }
            None
        }
    }
}

fn complete_position(snapshot: Option<PositionSnapshot>, this: *mut c_void) {
    if let Some(snapshot) = snapshot {
        match snapshot.apply(this) {
            Ok(true) => {
                if POSITION_TRACES.fetch_add(1, Ordering::Relaxed) < 64 {
                    match snapshot {
                        PositionSnapshot::Global(_) => log!("stage: FFXIDB preserved global coordinates removed by stock position save"),
                        PositionSnapshot::Character { .. } => log!("stage: FFXIDB pinned explicit character coordinates removed by stock position save"),
                    }
                }
            }
            Ok(false) => {}
            Err(error) => {
                if POSITION_TRACES.fetch_add(1, Ordering::Relaxed) < 64 {
                    log!("note: FFXIDB position preservation failed: {error}");
                }
            }
        }
    }
}

unsafe extern "system" fn initialize(this: *mut c_void, api: *mut c_void) {
    let a = adapter();
    let _guard = callback_transaction(a);
    let original: Initialize = std::mem::transmute(a.original[2]);
    original(this, api);
}
unsafe extern "system" fn command(this: *mut c_void, text: *const u8) {
    let a = adapter();
    let _guard = callback_transaction(a);
    let classified = bounded_command(text).and_then(|text| settings_command(&text));
    if let Some(command) = &classified {
        if COMMAND_TRACES.fetch_add(1, Ordering::Relaxed) < 128 {
            let scope = if global_settings_active(this) {
                "global"
            } else {
                "character-or-uninitialized"
            };
            // Only a fixed command class and numeric arguments; never raw text,
            // a character name, settings path or any other user-provided value.
            log!("stage: FFXIDB settings command {command:?}; scope={scope}");
        }
    }
    let snapshot = match classified {
        Some(SettingsCommand::Position { x, y }) => {
            position_snapshot(this, [x.is_some(), y.is_some()])
        }
        _ => None,
    };
    let original: Command = std::mem::transmute(a.original[7]);
    original(this, text);
    complete_position(snapshot, this);
}
/// Only zone packets 0x00A/0x00B reach FFXIDB's XML from Incoming (78670 ->
/// 3B550). Every other chunk runs without the session-wide settings lock, so
/// one client's file work never stalls another client's packet dispatch. An
/// unreadable header takes the lock.
fn incoming_reaches_settings(header: Option<u16>) -> bool {
    header.is_none_or(|header| matches!(header & 0x1FF, 0x0A | 0x0B))
}
/// Mouse kind 0 is an ordinary move (72540 never saves on it); every other
/// kind may reach 6C110/6C2D0 and takes the settings lock.
fn mouse_reaches_settings(kind: u32) -> bool {
    kind != 0
}
unsafe extern "system" fn incoming(
    this: *mut c_void,
    previous: *const u8,
    data: *const u8,
    size: u32,
    blocked: u8,
) -> u8 {
    let a = adapter();
    let original: Incoming = std::mem::transmute(a.original[11]);
    // Only the header is read, never the character-name payload.
    let header = if size >= 2 { read::<u16>(data as usize) } else { None };
    if !incoming_reaches_settings(header) {
        return original(this, previous, data, size, blocked);
    }
    let _guard = callback_transaction(a);
    original(this, previous, data, size, blocked)
}
unsafe extern "system" fn mouse(
    this: *mut c_void,
    kind: u32,
    x: i32,
    y: i32,
    wheel: i32,
    blocked: u8,
) -> u8 {
    let a = adapter();
    let original: Mouse = std::mem::transmute(a.original[13]);
    if !mouse_reaches_settings(kind) {
        return original(this, kind, x, y, wheel, blocked);
    }
    let _guard = callback_transaction(a);
    // Exact 72540 switch: kind 2 persists only when dragging (map+B8) and not
    // blocked. Do no file I/O for anything else.
    let saves_position = kind == 2
        && blocked == 0
        && read::<u8>(this as usize + 0x200).is_some_and(|dragging| dragging != 0);
    let snapshot = saves_position
        .then(|| position_snapshot(this, [true, true]))
        .flatten();
    let result = original(this, kind, x, y, wheel, blocked);
    complete_position(snapshot, this);
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        process::{Command as Process, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;

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

    #[test]
    fn only_callbacks_that_can_reach_the_xml_take_the_shared_lock() {
        assert!(incoming_reaches_settings(Some(0x000A)));
        assert!(incoming_reaches_settings(Some(0x040B)), "the size bits above 0x1FF are ignored");
        assert!(incoming_reaches_settings(None), "an unreadable header stays locked");
        for header in [0x0000, 0x000D, 0x0028, 0x0061, 0x020C] {
            assert!(!incoming_reaches_settings(Some(header)), "{header:#x}");
        }
        assert!(!mouse_reaches_settings(0), "a plain move never saves");
        for kind in [1, 2, 3, 4] {
            assert!(mouse_reaches_settings(kind), "{kind}");
        }
    }

    #[repr(C)]
    struct Fixture {
        vptr: usize,
        calls: u32,
        api: usize,
        command: usize,
    }
    static FIXTURE_VTABLE: OnceLock<[usize; 34]> = OnceLock::new();
    unsafe extern "C" fn fixture_factory() -> *mut c_void {
        Box::into_raw(Box::new(Fixture {
            vptr: FIXTURE_VTABLE.get().unwrap().as_ptr() as usize,
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
        // Reenter a wrapped callback on the same thread while Initialize owns
        // the mutex; this would deadlock with a nonreentrant Rust mutex.
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

    #[test]
    fn factory_and_stdcall_callbacks_preserve_arguments_results_reentry_and_reload() {
        let mut original = [0x1234usize; 34];
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
        assert!(FIXTURE_VTABLE.set(original).is_ok());
        let shadow = shadow_table(&original, 0x5678);
        let stable_table = shadow[1..].as_ptr() as usize;
        for slot in 0..34 {
            if ![2, 7, 11, 13].contains(&slot) {
                assert_eq!(shadow[slot + 1], original[slot]);
            }
        }
        // A zeroed stand-in image: its scan cache reads as never constructed.
        static FIXTURE_IMAGE: [u8; SCAN_CACHE_RVA + 8] = [0; SCAN_CACHE_RVA + 8];
        assert!(current_adapter().is_none());
        publish(Adapter {
            module: FIXTURE_IMAGE.as_ptr() as usize,
            path: PathBuf::from("FFXIDB.dll"),
            factory: fixture_factory,
            original,
            original_vtable: FIXTURE_VTABLE.get().unwrap().as_ptr() as usize,
            shadow,
            transaction: Box::leak(Box::new(SettingsMutex {
                shared: test_mutex(),
                legacy: test_mutex(),
            })),
            constructed_in: AtomicUsize::new(NONE),
        });
        for _ in 0..64 {
            unsafe {
                let object = create_instance();
                assert!(!object.is_null());
                let table = (*object.cast::<Fixture>()).vptr as *const usize;
                assert_eq!(table as usize, stable_table);
                assert_eq!(*table.sub(1), 0x5678);
                let init: Initialize = std::mem::transmute(*table.add(2));
                init(object, 0xF1234567usize as *mut c_void);
                assert_eq!(
                    CStr::from_ptr((*object.cast::<Fixture>()).command as *const i8).to_bytes(),
                    b"nested"
                );
                let tick: Notify = std::mem::transmute(*table.add(5));
                tick(object);
                let draw: Notify = std::mem::transmute(*table.add(6));
                draw(object);
                let cmd: Command = std::mem::transmute(*table.add(7));
                let save = c"save".as_ptr().cast();
                cmd(object, save);
                let packet: Incoming = std::mem::transmute(*table.add(11));
                assert_eq!(
                    packet(
                        object,
                        0xAABBCCDDusize as *const u8,
                        0x12345678usize as *const u8,
                        0x98765432,
                        0xA5
                    ),
                    0xC3
                );
                let input: Mouse = std::mem::transmute(*table.add(13));
                assert_eq!(input(object, 0xFFFFFFFF, -500, 700, -120, 0xA5), 0xC7);
                assert_eq!(
                    (
                        (*object.cast::<Fixture>()).calls,
                        (*object.cast::<Fixture>()).api
                    ),
                    (7, 0xF1234567)
                );
                assert_eq!((*object.cast::<Fixture>()).command, save as usize);
                let release: Notify = std::mem::transmute(*table.add(3));
                release(object);
            }
        }
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
    fn position_repair_keeps_current_edits_and_only_adds_missing_global_fields() {
        let before = GlobalPosition::capture(
            "<settings><global><X>100</X><Y>200</Y></global><Alpha><X>310</X></Alpha></settings>",
        )
        .unwrap();
        let current = "<?xml version=\"1.0\"?><settings><!-- new note --><global><Y>250</Y><Zoom>75</Zoom></global><Alpha><X>311</X></Alpha><NewCharacter><X>900</X></NewCharacter></settings>";
        let repaired = before.restore_missing(current).unwrap().unwrap();
        assert_eq!(
            repaired,
            current.replace("</global>", "<X>100</X></global>")
        );
        assert!(before.restore_missing(&repaired).unwrap().is_none());
        assert_eq!(
            before
                .restore_missing("<settings><global /></settings>")
                .unwrap()
                .unwrap(),
            "<settings><global ><X>100</X><Y>200</Y></global></settings>"
        );
        assert!(before
            .restore_missing("<settings><global><X>999</X><Y>250</Y></global></settings>")
            .unwrap()
            .is_none());
        let without_global =
            "<settings><!-- retained --><Alpha><X>311</X></Alpha><NewCharacter/></settings>";
        assert_eq!(
            before.restore_missing(without_global).unwrap().unwrap(),
            without_global.replace(
                "</settings>",
                "<global><X>100</X><Y>200</Y></global></settings>"
            )
        );
    }

    #[test]
    fn position_repair_rejects_partial_or_ambiguous_xml() {
        for xml in [
            "<settings><global><X>100",
            "<settings><global><X>1</X><X>2</X></global></settings>",
            "<settings><global/><global/></settings>",
            "<other><global><X>1</X></global></other>",
            "<settings xmlns:g=\"urn:test\"><g:global/></settings>",
            "<settings xmlns:g=\"urn:test\"><global><g:X>1</g:X></global></settings>",
        ] {
            assert!(GlobalPosition::capture(xml).is_err());
        }
        let before =
            GlobalPosition::capture("<settings><global><X>100</X></global></settings>").unwrap();
        assert!(before.restore_missing("<settings><global>").is_err());
        assert_eq!(
            before.restore_missing("<settings/>").unwrap().unwrap(),
            "<settings><global><X>100</X></global></settings>"
        );
        for xml in [
            "<settings xmlns:g=\"urn:test\"><g:global/></settings>",
            "<settings xmlns:g=\"urn:test\"><global><g:X>1</g:X></global></settings>",
            "<settings><global><X>1</X><X>2</X></global></settings>",
        ] {
            assert!(before.restore_missing(xml).is_err());
        }
        let namespaced_attribute = GlobalPosition::capture(
            "<settings xmlns:g=\"urn:test\"><global><X g:note=\"test\">100</X></global></settings>",
        )
        .unwrap();
        assert!(namespaced_attribute.restore_missing("<settings/>").is_err());
    }

    #[test]
    fn character_pinning_preserves_unedited_xml_and_inheritance() {
        let current = "<?xml version=\"1.0\"?><settings><!-- retained --><global><X>100</X><Y>200</Y></global><Alpha><Y>250</Y><Zoom>75</Zoom></Alpha><Beta><X>510</X><Y>610</Y></Beta></settings>";
        let pinned =
            pin_character_position(current, "Alpha", [Some(100.0), Some(250.0)], &[None, None])
                .unwrap()
                .unwrap();
        assert_eq!(pinned, current.replace("</Alpha>", "<X>100</X></Alpha>"));
        assert!(pin_character_position(
            &pinned,
            "Alpha",
            [Some(100.0), Some(250.0)],
            &[None, None]
        )
        .unwrap()
        .is_none());

        let inherited = "<settings><global><X>100</X><Y>200</Y></global><Alpha><Zoom>75</Zoom></Alpha><Beta/></settings>";
        assert_eq!(
            pin_character_position(inherited, "Alpha", [Some(100.0), None], &[None, None])
                .unwrap()
                .unwrap(),
            inherited.replace("</Alpha>", "<X>100</X></Alpha>")
        );
        // An untouched explicit Y is different from an inherited Y: retain
        // its original bytes when stock px also de-duplicates that field.
        assert_eq!(
            pin_character_position(
                inherited,
                "Alpha",
                [Some(100.0), None],
                &[None, Some("<Y note=\"kept\">200.0</Y>".into())]
            )
            .unwrap()
            .unwrap(),
            inherited.replace("</Alpha>", "<X>100</X><Y note=\"kept\">200.0</Y></Alpha>")
        );
        assert_eq!(
            pin_character_position(
                "<settings><Beta/></settings>",
                "Alpha",
                [Some(100.0), Some(200.0)],
                &[None, None]
            )
            .unwrap()
            .unwrap(),
            "<settings><Beta/><Alpha><X>100</X><Y>200</Y></Alpha></settings>"
        );
    }

    #[test]
    fn character_pinning_rejects_ambiguous_scope_fields_and_nonfinite_values() {
        for xml in [
            "<settings><Alpha>",
            "<other><Alpha/></other>",
            "<settings><Alpha/><Alpha/></settings>",
            "<settings><Alpha><X>1</X><X>2</X></Alpha></settings>",
            "<settings><Alpha><X><nested/></X></Alpha></settings>",
            "<settings xmlns:g=\"urn:test\"><g:Alpha/></settings>",
            "<settings xmlns:g=\"urn:test\"><Alpha><g:X>1</g:X></Alpha></settings>",
        ] {
            assert!(pin_character_position(
                xml,
                "Alpha",
                [Some(100.0), Some(200.0)],
                &[None, None]
            )
            .is_err());
        }
        for scope in ["", "global", "Alpha/><Beta", "Alpha:Beta", "9Alpha"] {
            assert!(pin_character_position(
                "<settings/>",
                scope,
                [Some(100.0), None],
                &[None, None]
            )
            .is_err());
        }
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(pin_character_position(
                "<settings/>",
                "Alpha",
                [Some(value), None],
                &[None, None]
            )
            .is_err());
        }
    }

    #[test]
    fn character_snapshot_refuses_changed_callback_context() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-context-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("FFXIDB.xml");
        let initial = "<settings><global><X>100</X><Y>200</Y></global><Alpha><Y>250</Y></Alpha><Beta/></settings>";
        std::fs::write(&path, initial).unwrap();
        // Test-owned object storage exercises validation only, never a vendor
        // call. A callback changing its selected character/file must not apply
        // the pre-callback snapshot to either scope or file.
        let mut object = [0u32; 0x150 / 4];
        let filename: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let set_filename = |object: &mut [u32], value: &[u16]| {
            object[0xB8 / 4..0xD0 / 4].copy_from_slice(&[
                value.as_ptr() as u32,
                0,
                0,
                0,
                value.len() as u32 - 1,
                value.len() as u32 - 1,
            ]);
        };
        let set_scope = |object: &mut [u32], name: &str| unsafe {
            object
                .as_mut_ptr()
                .add(0xD0 / 4)
                .cast::<VendorString>()
                .write(VendorString::new(name));
        };
        set_filename(&mut object, &filename);
        set_scope(&mut object, "Alpha");
        object[0x148 / 4] = 100.0f32.to_bits();
        object[0x14C / 4] = 250.0f32.to_bits();
        let this = object.as_mut_ptr().cast();
        let snapshot = PositionSnapshot::capture(this, [true, true])
            .unwrap()
            .unwrap();

        set_scope(&mut object, "Beta");
        assert!(snapshot.apply(this).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
        set_scope(&mut object, "Alpha");
        let changed_filename: Vec<_> = root
            .join("changed")
            .join("FFXIDB.xml")
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        set_filename(&mut object, &changed_filename);
        assert!(snapshot.apply(this).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
        set_filename(&mut object, &filename);
        object[0x148 / 4] = f32::NAN.to_bits();
        assert!(snapshot.apply(this).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
        set_scope(&mut object, "Alpha>");
        assert!(PositionSnapshot::capture(this, [true, true]).is_err());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn position_repair_replaces_atomically_and_leaves_failed_replacements_intact() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-repair-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("FFXIDB.xml");
        let before =
            GlobalPosition::capture("<settings><global><X>100</X><Y>200</Y></global></settings>")
                .unwrap();
        let repair = PositionRepair {
            path: path.clone(),
            before,
        };
        let current = "<settings><global><Y>250</Y></global><NewCharacter><X>900</X></NewCharacter></settings>";
        std::fs::write(&path, current).unwrap();
        assert!(repair.apply().unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            current.replace("</global>", "<X>100</X></global>")
        );

        std::fs::write(&path, current).unwrap();
        let denies_replace = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        assert!(repair.apply().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "failed replacement removes only its own staging file"
        );
        drop(denies_replace);

        let partial = "<settings><global><Y>";
        std::fs::write(&path, partial).unwrap();
        assert!(repair.apply().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), partial);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn settings_command_classification_retains_only_numeric_position_data() {
        assert_eq!(
            settings_command(" map pos -9999 250 "),
            Some(SettingsCommand::Position {
                x: Some(-9999.0),
                y: Some(250.0)
            })
        );
        assert_eq!(
            settings_command("px 100"),
            Some(SettingsCommand::Position {
                x: Some(100.0),
                y: None
            })
        );
        assert_eq!(
            settings_command("py 200"),
            Some(SettingsCommand::Position {
                x: None,
                y: Some(200.0)
            })
        );
        assert_eq!(settings_command("save"), Some(SettingsCommand::Save));
        assert_eq!(settings_command("m reload"), Some(SettingsCommand::Reload));
        for input in [
            "POS 100 250",
            "MAP POS 100 250",
            "pos \"100\" \"250\"",
            "\"Map\" \"Pos\" 100 250",
        ] {
            assert_eq!(
                settings_command(input),
                Some(SettingsCommand::Position {
                    x: Some(100.0),
                    y: Some(250.0)
                })
            );
        }
        for input in [
            "pos private 1",
            "pos NaN 1",
            "pos inf 1",
            "unknown private text",
        ] {
            assert_eq!(settings_command(input), None);
        }
    }

    #[test]
    fn partial_replacement_failure_retains_recovery_copies_and_never_overwrites_new_edits() {
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-recovery-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let target = root.join("FFXIDB.xml");
        let stage = root.join("repair.tmp");
        let backup = root.join("repair.backup");
        let current = "<settings><global><Y>250</Y></global></settings>";
        let repaired = "<settings><global><X>100</X><Y>250</Y></global></settings>";
        std::fs::write(&stage, repaired).unwrap();
        std::fs::write(&backup, current).unwrap();
        // Model ERROR_UNABLE_TO_MOVE_REPLACEMENT after the original was renamed.
        recover_replacement_failure(&target, &stage, &backup, current, Some(1176));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), current);
        assert_eq!(std::fs::read_to_string(&stage).unwrap(), repaired);
        assert!(!backup.exists());

        let newer = "<settings><global><X>900</X><Y>901</Y></global><NewCharacter/></settings>";
        std::fs::write(&target, newer).unwrap();
        std::fs::write(&backup, current).unwrap();
        // Model a partial replacement with another complete target already present.
        recover_replacement_failure(&target, &stage, &backup, current, Some(1177));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), newer);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), current);
        assert_eq!(std::fs::read_to_string(&stage).unwrap(), repaired);
        for path in [&target, &stage, &backup] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
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
            let mut result = Self {
                bytes: [0; 16],
                length: value.len() as u32,
                capacity: 15,
            };
            result.bytes[..value.len()].copy_from_slice(value.as_bytes());
            result
        }
    }

    unsafe fn vendor_cached_position(base: usize, settings: *mut c_void) -> [f32; 2] {
        let field: unsafe extern "thiscall" fn(*mut c_void, *const VendorString) -> usize =
            std::mem::transmute(base + 0x14600);
        ["X", "Y"].map(|name| {
            let field = field(settings, &VendorString::new(name));
            assert_ne!(field, 0);
            // Exact 2562B..25643 getter: flag +4 selects the std::string at
            // +20 (default) or +50 (current). Read its numeric input without
            // calling the internal float getter's nonstandard XMM0 return ABI.
            let current = read::<u8>(field + 4).unwrap() != 0;
            let address = field + if current { 0x50 } else { 0x20 };
            let bytes = read_msvc_string::<u8>(address, 32, None, true).unwrap();
            let value = std::str::from_utf8(&bytes).unwrap().parse::<f32>().unwrap();
            assert!(value.is_finite());
            value
        })
    }

    #[test]
    #[ignore = "Child of the isolated exact-DLL regression; never run against live settings"]
    fn vendor_settings_child() {
        let Some(root) = std::env::var_os("CRYSTAL_TEST_FFXIDB_VENDOR_ROOT").map(PathBuf::from)
        else {
            return;
        };
        let scenario = std::env::var("CRYSTAL_TEST_FFXIDB_VENDOR_SCENARIO").unwrap();
        let inherited_reader =
            scenario == "stock-inherited-reader" || scenario.starts_with("inherited-reader");
        let role = std::env::var("CRYSTAL_TEST_FFXIDB_VENDOR_ROLE").unwrap();
        let shared = std::env::var("CRYSTAL_TEST_FFXIDB_VENDOR_MUTEX").unwrap();
        let path = root.join("FFXIDB.dll");
        let xml = root.join("FFXIDB.xml").canonicalize().unwrap();
        assert!(file_matches(&path, FFXIDB_SHA256));
        let before_load = matches!(scenario.as_str(), "initialize" | "observer-initialize")
            .then(|| std::fs::read_to_string(&xml).unwrap());
        unsafe {
            use windows_sys::Win32::System::LibraryLoader::{
                LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            };
            let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let module = LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
            if let Some(before) = before_load {
                assert_eq!(std::fs::read_to_string(&xml).unwrap(), before);
            }
            let base = module as usize;
            assert_eq!(pe_identity(base), Some(FFXIDB_ID));
            assert!(plugin_layout(base));
            let original = VTABLE_RVAS.map(|rva| base + rva);
            // Use an isolated shared mutex, so this test cannot delay live games.
            assert!(scan_cache_fresh(base));
            assert!(current_adapter().is_none());
            publish(Adapter {
                module: base,
                path: path.clone(),
                factory: std::mem::transmute::<usize, Factory>(base + FACTORY_RVA),
                original,
                original_vtable: base + VTABLE_RVA,
                shadow: shadow_table(&original, base + 0xBCEE0),
                transaction: Box::leak(Box::new(
                    SettingsMutex::with_shared_name(&path, &shared).unwrap(),
                )),
                constructed_in: AtomicUsize::new(NONE),
            });
            std::fs::write(root.join(format!("{role}.ready")), b"ready").unwrap();
            let instance = create_instance();
            assert!(!instance.is_null());
            let settings = (instance as *mut u8).add(0xB8);
            let mut filename: Vec<_> = xml.as_os_str().encode_wide().chain(Some(0)).collect();
            assert!(filename.len() > 8);
            let layout = settings.cast::<u32>();
            assert_eq!(*layout.add(4), 0);
            assert_eq!(*layout.add(5), 7);
            // Borrow this test-owned filename for the process lifetime. Only
            // the uninitialized filename changes; no Hook, graphics or game API
            // is initialized. The DLL runs its own factory and XML algorithms.
            *layout = filename.as_mut_ptr() as u32;
            *layout.add(4) = filename.len() as u32 - 1;
            *layout.add(5) = filename.len() as u32 - 1;
            std::mem::forget(filename);
            let select: unsafe extern "thiscall" fn(*mut c_void, *const VendorString) =
                std::mem::transmute(base + 0x3B550);
            let before_initialize = std::fs::read_to_string(&xml).unwrap();
            let observer = (scenario == "observer-initialize").then(|| {
                OpenOptions::new()
                    .read(true)
                    .share_mode(
                        FILE_SHARE_READ
                            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                            | FILE_SHARE_DELETE,
                    )
                    .open(&xml)
                    .unwrap()
            });
            if observer.is_some() {
                // These are the exact bundled CRT entry points called by
                // TinyXML load (75240) and save (765E0), not another CRT's
                // approximation of its sharing flags.
                let fopen: unsafe extern "C" fn(*mut *mut c_void, *const u8, *const u8) -> i32 =
                    std::mem::transmute(base + 0x50944);
                let fclose: unsafe extern "C" fn(*mut c_void) -> i32 =
                    std::mem::transmute(base + 0x474E6);
                let name = std::ffi::CString::new(xml.to_str().unwrap()).unwrap();
                let mut stream = std::ptr::null_mut();
                assert_eq!(
                    fopen(&mut stream, name.as_ptr().cast(), c"rb".as_ptr().cast()),
                    0
                );
                assert!(!stream.is_null());
                assert_eq!(fclose(stream), 0);
                stream = std::ptr::null_mut();
                assert_ne!(fopen(&mut stream, name.as_ptr().cast(), c"w".as_ptr().cast()), 0);
                assert!(stream.is_null());
            }
            {
                let _guard = callback_transaction(adapter());
                let register: unsafe extern "thiscall" fn(
                    *mut c_void,
                    *const VendorString,
                    i32,
                    u32,
                    u32,
                ) = std::mem::transmute(base + 0x173D0);
                register(settings.cast(), &VendorString::new("X"), 0, 0, 0);
                register(settings.cast(), &VendorString::new("Y"), 0, 0, 0);
                let name = if matches!(scenario.as_str(), "named" | "save" | "named-save")
                    || inherited_reader
                {
                    "Alpha"
                } else {
                    "global"
                };
                select(settings.cast(), &VendorString::new(name));
                // Initialize normally copies these loaded X/Y fields into the
                // map. Seed only that fixture state: calling its graphics
                // initialization would require a live Hook API.
                *((instance as *mut u8).add(0x148).cast::<[f32; 2]>()) =
                    vendor_cached_position(base, settings.cast());
            }
            if scenario == "initialize" || scenario == "observer-initialize" {
                if observer.is_some() {
                    assert_eq!(std::fs::read_to_string(&xml).unwrap(), before_initialize);
                }
                assert_eq!(
                    vendor_cached_position(base, settings.cast()),
                    [2124.0, 1066.0]
                );
                std::fs::write(root.join(format!("{role}.done")), b"done").unwrap();
                return;
            }
            drop(observer);
            let vtable = *(instance.cast::<usize>()) as *const usize;
            if scenario == "mouse" || scenario == "inherited-reader-mouse" {
                *((instance as *mut u8).add(0x200)) = 1;
                *((instance as *mut u8).add(0x1F8).cast::<f32>()) = 0.0;
                *((instance as *mut u8).add(0x1FC).cast::<f32>()) = 0.0;
                let callback: Mouse = std::mem::transmute(*vtable.add(13));
                callback(instance, 2, 100, 250, 0, 0);
            } else {
                let callback: Command = std::mem::transmute(if scenario.starts_with("stock") {
                    original[7]
                } else {
                    *vtable.add(7)
                });
                let text: &[u8] = match scenario.as_str() {
                    "save" | "named-save" => b"save\0",
                    "inherited-writer" => b"pos 600 700\0",
                    "inherited-reader-same" => b"pos 100 200\0",
                    "inherited-reader-px" | "inherited-reader-px-explicit" => b"px 100\0",
                    "inherited-reader-py" | "inherited-reader-py-explicit" => b"py 200\0",
                    "same" | "six" => b"pos 100 200\0",
                    "upper" | "stock-upper" => b"POS 100 250\0",
                    "prefix" | "stock-prefix" => b"MAP POS 100 250\0",
                    "quoted" | "stock-quoted" => b"pos \"100\" \"250\"\0",
                    _ => b"pos 100 250\0",
                };
                if scenario == "named-save" {
                    callback(instance, c"pos 100 250".as_ptr().cast());
                    let positioned = std::fs::read_to_string(&xml).unwrap();
                    let document = roxmltree::Document::parse(&positioned).unwrap();
                    let character = settings_node(&document, "Alpha").unwrap().unwrap();
                    assert!(character.children().any(|node| node.has_tag_name("X")));
                }
                for _ in 0..if scenario == "six" { 12 } else { 1 } {
                    callback(instance, text.as_ptr());
                }
            }
            if inherited_reader {
                let expected_before = if matches!(
                    scenario.as_str(),
                    "inherited-reader-same"
                        | "inherited-reader-px"
                        | "inherited-reader-py"
                        | "inherited-reader-px-explicit"
                        | "inherited-reader-py-explicit"
                ) {
                    [100.0, 200.0]
                } else {
                    [100.0, 250.0]
                };
                assert_eq!(
                    vendor_cached_position(base, settings.cast()),
                    expected_before
                );
                assert_eq!(
                    read::<[f32; 2]>(instance as usize + 0x148),
                    Some(expected_before)
                );
                std::fs::write(root.join("inherited-reader.positioned"), b"ready").unwrap();
                let started = Instant::now();
                while !root.join("inherited-writer.done").exists() {
                    assert!(
                        started.elapsed() < Duration::from_secs(15),
                        "global writer did not finish"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                let _guard = callback_transaction(adapter());
                let before_xml = std::fs::read_to_string(&xml).unwrap();
                let before = vendor_cached_position(base, settings.cast());
                assert_eq!(
                    before, expected_before,
                    "the other process cannot update this cache"
                );
                // This is the real selector used by Initialize and incoming
                // 0x00A/0x00B. Invoke it directly; those public callbacks also
                // require a live Hook API/graphics context, which this fixture
                // deliberately does not fabricate.
                select(settings.cast(), &VendorString::new("Alpha"));
                let after = vendor_cached_position(base, settings.cast());
                let expected_after = match scenario.as_str() {
                    "stock-inherited-reader" => [600.0, 250.0],
                    "inherited-reader-px" => [100.0, 700.0],
                    "inherited-reader-py" => [600.0, 200.0],
                    _ => expected_before,
                };
                assert_eq!(
                    after, expected_after,
                    "only intentionally positioned axes should be pinned on settings reload"
                );
                assert_eq!(
                    std::fs::read_to_string(&xml).unwrap(),
                    before_xml,
                    "reload must not rewrite XML"
                );
                std::fs::write(
                    root.join("inherited-reader.observed"),
                    format!("{before:?}\n{after:?}\n"),
                )
                .unwrap();
            }
            std::fs::write(root.join(format!("{role}.done")), b"done").unwrap();
            // Vendor objects retain test-owned buffers; let process exit reclaim
            // everything instead of invoking graphics-related plugin teardown.
        }
    }

    #[test]
    fn relaunch_decisions_follow_the_scan_cache() {
        let at = |module, still_mapped, constructed_in| {
            Some(Previous { module, still_mapped, constructed_in })
        };
        // First game in a host.
        assert_eq!(decide(None, 0x5000_0000, true, 1), Decision::Protect);
        // Hook unloaded game 1's mapping; game 2 maps a fresh copy, at the
        // same base or elsewhere.
        assert_eq!(decide(at(0x5000_0000, false, Some(1)), 0x5000_0000, true, 2), Decision::Protect);
        assert_eq!(decide(at(0x5000_0000, false, Some(1)), 0x6000_0000, true, 2), Decision::Protect);
        // The 2026-10-23 crash: game 1's mapping survived into game 2 with
        // its scan cache (game 1 heap pointers) still populated.
        assert_eq!(
            decide(at(0x5000_0000, false, Some(1)), 0x5000_0000, false, 2),
            Decision::StaleScanCache
        );
        // A populated cache Crystal never saw constructed is not trusted.
        assert_eq!(decide(None, 0x5000_0000, false, 1), Decision::StaleScanCache);
        assert_eq!(decide(at(0x5000_0000, false, None), 0x5000_0000, false, 1), Decision::StaleScanCache);
        // Hook asking again for the mapping built during this same game.
        assert_eq!(decide(at(0x5000_0000, false, Some(2)), 0x5000_0000, false, 2), Decision::SameGame);
        // Two images at once stay refused.
        assert_eq!(decide(at(0x5000_0000, true, Some(2)), 0x6000_0000, true, 2), Decision::SecondImage);
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
        use windows_sys::Win32::System::LibraryLoader::{
            LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
        };
        let Some(root) = std::env::var_os("CRYSTAL_TEST_FFXIDB_RELAUNCH_ROOT").map(PathBuf::from)
        else {
            return;
        };
        assert!(std::env::var_os("CRYSTAL_TEST_FFXIDB_SHARED_MUTEX").is_some());
        let path = root.join("FFXIDB.dll");
        let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let load = || unsafe {
            let module = LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
            module
        };
        // Hook's resolver is the stock GetProcAddress, as in a live host.
        let stock: Resolve = GetProcAddress;
        assert!(BRIDGE.set(Bridge { _hook: OwnedModule(0), resolve: stock }).is_ok());
        let protected = create_instance as *const () as usize;
        let disabled = disabled_factory as *const () as usize;
        let resolve_factory = |module: HMODULE| unsafe {
            resolve(module, c"CreateInstance".as_ptr().cast()).map(|f| f as usize)
        };
        // Bounded: the parent also kills this child if any unload never returns.
        let starting = |game: usize| {
            let previous = HELD.load(Ordering::SeqCst);
            let started = std::time::Instant::now();
            game_starting();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "game {game}: releasing the previous FFXIDB copy took {:?}",
                started.elapsed()
            );
            assert_eq!(HELD.load(Ordering::SeqCst), 0, "game {game}");
            previous
        };
        // Games 1 and 2 in one host, the way Hook runs them: load, resolve,
        // construct, then FreeLibrary at the end of the game. The instance is
        // never initialized (no Hook API), so it is leaked, not released.
        for game in 1..=2 {
            let previous = starting(game);
            if game > 1 {
                // Earlier builds: the previous copy, and its scan cache, reached this game.
                assert!(!loaded(previous), "game {game}: the previous game's FFXIDB must be unloaded before GameStart");
            }
            let module = load();
            let base = module as usize;
            assert!(scan_cache_fresh(base), "game {game} must get a fresh FFXIDB mapping");
            assert_eq!(resolve_factory(module), Some(protected), "game {game}");
            let instance = unsafe { create_instance() };
            assert!(!instance.is_null(), "game {game}");
            assert_eq!(
                read::<usize>(instance as usize),
                Some(adapter().shadow[1..].as_ptr() as usize)
            );
            assert_eq!(adapter().module, base);
            assert_eq!(HELD.load(Ordering::SeqCst), base, "game {game}");
            unsafe { FreeLibrary(module) };
            // The 2026-10-23 test build: Hook's FreeLibrary unmapped FFXIDB
            // inside the game's teardown, and clients hung at /logout. It must
            // stay mapped until the host starts the next game.
            assert!(loaded(base), "game {game}: FFXIDB must stay mapped through the end of its game");
        }
        // Something else keeps FFXIDB mapped past its game: game 3's factory
        // must refuse the stale mapping rather than let FFXIDB crash.
        let previous = starting(3);
        assert!(!loaded(previous));
        let module = load();
        let extra = load();
        assert_eq!(module, extra);
        let base = module as usize;
        assert_eq!(resolve_factory(module), Some(protected));
        let instance = unsafe { create_instance() };
        assert!(!instance.is_null());
        // Stand in for game 3's scans: FFXIDB's std::map size becomes nonzero.
        let size = (base + SCAN_CACHE_RVA + 4) as *mut u32;
        unsafe { size.write_volatile(1) };
        // Hook asking again within game 3 is still served.
        assert_eq!(resolve_factory(module), Some(protected));
        unsafe { FreeLibrary(module) };
        assert!(loaded(base));
        assert_eq!(starting(4), base);
        assert!(loaded(base), "the extra reference keeps the old mapping");
        assert_eq!(resolve_factory(module), Some(disabled), "game 4 must not reuse game 3's cache");
        assert!(unsafe { create_instance() }.is_null());
        // Leave FFXIDB's .data as its unconstructed map expects at unload.
        unsafe { size.write_volatile(0) };
        unsafe { FreeLibrary(extra) };
        assert!(!loaded(base));
        std::fs::write(root.join("relaunch.done"), b"done").unwrap();
    }

    #[test]
    #[ignore = "Executes a verified local FFXIDB copy, without Hook or a game"]
    fn installed_plugin_unloads_between_games_in_one_host() {
        let windower = PathBuf::from(std::env::var_os("CRYSTAL_WINDOWER_ROOT")
            .expect("Set CRYSTAL_WINDOWER_ROOT to the installation containing the verified FFXIDB.dll; this test copies it to an isolated fixture"));
        let source = windower.join("plugins").join("FFXIDB.dll");
        assert!(file_matches(&source, FFXIDB_SHA256));
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-relaunch-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(source, root.join("FFXIDB.dll")).unwrap();
        let mut child = Process::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windower_plugin_settings::tests::relaunch_child",
                "--ignored",
                "--nocapture",
            ])
            .env("CRYSTAL_TEST_FFXIDB_RELAUNCH_ROOT", &root)
            .env(
                "CRYSTAL_TEST_FFXIDB_SHARED_MUTEX",
                format!("Local\\CrystalLauncher.FFXIDB.relaunch-test.{}", unique()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        // Four games of load/unload take well under a second; a stuck
        // FreeLibrary (loader-lock wait in FFXIDB's detach) never returns.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("FFXIDB relaunch child did not finish in 60 s: an unload or load hung");
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success());
        assert!(root.join("relaunch.done").exists(), "child skipped the scenario");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore = "Executes a verified local FFXIDB copy against isolated XML, without Hook or a game"]
    fn installed_plugin_position_callbacks_preserve_settings() {
        let windower = PathBuf::from(std::env::var_os("CRYSTAL_WINDOWER_ROOT")
            .expect("Set CRYSTAL_WINDOWER_ROOT to the installation containing the verified FFXIDB.dll; this test copies it to an isolated fixture"));
        let source = windower.join("plugins").join("FFXIDB.dll");
        assert!(file_matches(&source, FFXIDB_SHA256));
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-vendor-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(source, root.join("FFXIDB.dll")).unwrap();
        let shared = format!("Local\\CrystalLauncher.FFXIDB.vendor-test.{}", unique());
        let initial = "<settings><global><X>100</X><Y>200</Y></global><Alpha><X>310</X><Y>410</Y></Alpha><Beta><X>510</X><Y>610</Y></Beta></settings>";
        let spawn = |scenario: &str, role: &str| {
            Process::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windower_plugin_settings::tests::vendor_settings_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_ROOT", &root)
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_SCENARIO", scenario)
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_ROLE", role)
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_MUTEX", &shared)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap()
        };
        let fields = |xml: &str, section: &str| {
            let doc = roxmltree::Document::parse(xml).unwrap();
            doc.root_element()
                .children()
                .find(|node| node.has_tag_name(section))
                .map(|node| {
                    ["X", "Y"].map(|name| {
                        node.children()
                            .find(|child| child.has_tag_name(name))
                            .and_then(|child| child.text())
                            .map(str::to_owned)
                    })
                })
        };
        for scenario in [
            "stock",
            "stock-upper",
            "stock-prefix",
            "stock-quoted",
            "global",
            "upper",
            "prefix",
            "quoted",
            "same",
            "named",
            "mouse",
            "save",
            "named-save",
        ] {
            std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
            assert!(
                spawn(scenario, scenario).wait().unwrap().success(),
                "{scenario}"
            );
            let xml = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
            let global = fields(&xml, "global").unwrap();
            match scenario {
                "stock" | "stock-upper" | "stock-prefix" | "stock-quoted" => assert_eq!(
                    global,
                    [None, Some("250".into())],
                    "uncorrected real callback reproduces loss"
                ),
                "global" | "upper" | "prefix" | "quoted" | "mouse" => {
                    assert_eq!(global, [Some("100".into()), Some("250".into())])
                }
                "save" | "named-save" => {
                    let expected = if scenario == "save" {
                        ["310", "410"]
                    } else {
                        ["100", "250"]
                    };
                    assert_eq!(global, expected.map(|value| Some(value.into())));
                    assert!(
                        fields(&xml, "Alpha").is_none(),
                        "explicit stock save semantics are unchanged"
                    );
                    assert!(fields(&xml, "Beta").is_none());
                    continue;
                }
                _ => assert_eq!(global, [Some("100".into()), Some("200".into())]),
            }
            assert_eq!(
                fields(&xml, "Beta").unwrap(),
                [Some("510".into()), Some("610".into())]
            );
            if scenario == "named" {
                assert_eq!(
                    fields(&xml, "Alpha").unwrap(),
                    [Some("100".into()), Some("250".into())]
                );
            } else {
                assert_eq!(
                    fields(&xml, "Alpha").unwrap(),
                    [Some("310".into()), Some("410".into())]
                );
            }
        }
        std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
        let gate = TransactionMutex::open(&shared).unwrap();
        let guard = gate.acquire(0).unwrap();
        let mut children: Vec<_> = (0..6).map(|i| spawn("six", &format!("six{i}"))).collect();
        let started = Instant::now();
        while !(0..6).all(|i| root.join(format!("six{i}.ready")).exists()) {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "all six exact-DLL processes must reach the held guard"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!(0..6).any(|i| root.join(format!("six{i}.done")).exists()));
        drop(guard);
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let xml = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
        for section in ["global", "Alpha", "Beta"] {
            assert_eq!(fields(&xml, section), fields(initial, section));
        }
        assert!(root
            .canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Checks character axes using two verified FFXIDB copies and isolated XML"]
    fn installed_plugin_pins_explicit_character_axes() {
        let windower = PathBuf::from(std::env::var_os("CRYSTAL_WINDOWER_ROOT").expect(
            "Set CRYSTAL_WINDOWER_ROOT to the installation containing the verified FFXIDB.dll",
        ));
        let source = windower.join("plugins").join("FFXIDB.dll");
        assert!(file_matches(&source, FFXIDB_SHA256));
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-inherited-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        let initial = "<settings><global><X>100</X><Y>200</Y></global><Alpha><X>100</X><Y>200</Y></Alpha><Beta><X>510</X><Y>610</Y></Beta></settings>";
        let fields = |xml: &str, section: &str| {
            let doc = roxmltree::Document::parse(xml).unwrap();
            let section = doc
                .root_element()
                .children()
                .find(|node| node.has_tag_name(section))
                .unwrap();
            ["X", "Y"].map(|name| {
                section
                    .children()
                    .find(|node| node.has_tag_name(name))
                    .and_then(|node| node.text())
                    .map(str::to_owned)
            })
        };
        for (scenario, expected_fields, before, after) in [
            (
                "stock-inherited-reader",
                [None, Some("250")],
                [100.0, 250.0],
                [600.0, 250.0],
            ),
            (
                "inherited-reader",
                [Some("100"), Some("250")],
                [100.0, 250.0],
                [100.0, 250.0],
            ),
            (
                "inherited-reader-mouse",
                [Some("100"), Some("250")],
                [100.0, 250.0],
                [100.0, 250.0],
            ),
            (
                "inherited-reader-same",
                [Some("100"), Some("200")],
                [100.0, 200.0],
                [100.0, 200.0],
            ),
            (
                "inherited-reader-px",
                [Some("100"), None],
                [100.0, 200.0],
                [100.0, 700.0],
            ),
            (
                "inherited-reader-py",
                [None, Some("200")],
                [100.0, 200.0],
                [600.0, 200.0],
            ),
            (
                "inherited-reader-px-explicit",
                [Some("100"), Some("200")],
                [100.0, 200.0],
                [100.0, 200.0],
            ),
            (
                "inherited-reader-py-explicit",
                [Some("100"), Some("200")],
                [100.0, 200.0],
                [100.0, 200.0],
            ),
        ] {
            let case = root.join(scenario);
            std::fs::create_dir(&case).unwrap();
            std::fs::copy(&source, case.join("FFXIDB.dll")).unwrap();
            let shared = format!("Local\\CrystalLauncher.FFXIDB.inherited-test.{}", unique());
            let xml = case.join("FFXIDB.xml");
            let case_initial = if matches!(scenario, "inherited-reader-px" | "inherited-reader-py")
            {
                initial.replace(
                    "<Alpha><X>100</X><Y>200</Y></Alpha>",
                    "<Alpha><Zoom>75</Zoom></Alpha>",
                )
            } else {
                initial.to_owned()
            };
            std::fs::write(&xml, &case_initial).unwrap();
            let spawn = |child_scenario: &str| {
                Process::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windower_plugin_settings::tests::vendor_settings_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("CRYSTAL_TEST_FFXIDB_VENDOR_ROOT", &case)
                    .env("CRYSTAL_TEST_FFXIDB_VENDOR_SCENARIO", child_scenario)
                    .env("CRYSTAL_TEST_FFXIDB_VENDOR_ROLE", child_scenario)
                    .env("CRYSTAL_TEST_FFXIDB_VENDOR_MUTEX", &shared)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap()
            };
            let mut reader = spawn(scenario);
            let started = Instant::now();
            while !case.join("inherited-reader.positioned").exists() {
                assert!(
                    reader.try_wait().unwrap().is_none(),
                    "{scenario}: named client exited before positioning"
                );
                assert!(
                    started.elapsed() < Duration::from_secs(15),
                    "named client did not position"
                );
                thread::sleep(Duration::from_millis(10));
            }
            let expected_fields = expected_fields.map(|value| value.map(str::to_owned));
            let positioned = std::fs::read_to_string(&xml).unwrap();
            assert_eq!(
                fields(&positioned, "global"),
                [Some("100".into()), Some("200".into())]
            );
            assert_eq!(fields(&positioned, "Alpha"), expected_fields, "{scenario}");
            assert_eq!(fields(&positioned, "Beta"), fields(initial, "Beta"));
            assert!(spawn("inherited-writer").wait().unwrap().success());
            assert!(reader.wait().unwrap().success(), "{scenario}");
            let changed = std::fs::read_to_string(&xml).unwrap();
            assert_eq!(
                fields(&changed, "global"),
                [Some("600".into()), Some("700".into())]
            );
            assert_eq!(fields(&changed, "Alpha"), expected_fields);
            assert_eq!(fields(&changed, "Beta"), fields(initial, "Beta"));
            if matches!(scenario, "inherited-reader-px" | "inherited-reader-py") {
                let document = roxmltree::Document::parse(&changed).unwrap();
                let character = settings_node(&document, "Alpha").unwrap().unwrap();
                assert_eq!(
                    character
                        .children()
                        .find(|node| node.has_tag_name("Zoom"))
                        .unwrap()
                        .text(),
                    Some("75")
                );
            }
            assert_eq!(
                std::fs::read_to_string(case.join("inherited-reader.observed")).unwrap(),
                format!("{before:?}\n{after:?}\n")
            );
        }
        assert!(root
            .canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Executes real startup field registration with an isolated shared-read observer"]
    fn installed_plugin_registration_with_shared_reader() {
        let source = PathBuf::from(std::env::var_os("CRYSTAL_WINDOWER_ROOT").unwrap())
            .join("plugins")
            .join("FFXIDB.dll");
        assert!(file_matches(&source, FFXIDB_SHA256));
        let root = std::env::temp_dir().join(format!("crystal-ffxidb-initialize-{}", unique()));
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(&source, root.join("FFXIDB.dll")).unwrap();
        let initial = "<settings><global><X>2124</X><Y>1066</Y><Scale>1</Scale></global><Alpha><X>1985</X><Y>1035</Y></Alpha></settings>";
        for scenario in ["initialize", "observer-initialize"] {
            std::fs::write(root.join("FFXIDB.xml"), initial).unwrap();
            let result = Process::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windower_plugin_settings::tests::vendor_settings_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_ROOT", &root)
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_SCENARIO", scenario)
                .env("CRYSTAL_TEST_FFXIDB_VENDOR_ROLE", scenario)
                .env(
                    "CRYSTAL_TEST_FFXIDB_VENDOR_MUTEX",
                    format!("Local\\CrystalLauncher.FFXIDB.initialize-test.{}", unique()),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .status()
                .unwrap();
            assert!(result.success(), "{scenario}");
            let saved = std::fs::read_to_string(root.join("FFXIDB.xml")).unwrap();
            let document = roxmltree::Document::parse(&saved).unwrap();
            for (scope, expected) in [("global", ["2124", "1066"]), ("Alpha", ["1985", "1035"])] {
                let section = settings_node(&document, scope).unwrap().unwrap();
                for (axis, expected) in ["X", "Y"].into_iter().zip(expected) {
                    assert_eq!(
                        section
                            .children()
                            .find(|node| node.has_tag_name(axis))
                            .unwrap()
                            .text(),
                        Some(expected)
                    );
                }
            }
        }
        assert!(root
            .canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Read-only installed PE verification; neither vendor DLL is loaded or executed"]
    fn installed_builds_match_adapter_and_callback_abi() {
        let root = std::env::var_os("CRYSTAL_WINDOWER_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Games\Final Fantasy XI Programs\Windower 4"));
        let plugin = root.join("plugins").join("FFXIDB.dll");
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
        for path in hooks {
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

        let file = std::fs::read(plugin).unwrap();
        assert_eq!(sha256(&file).as_deref(), Some(FFXIDB_SHA256));
        let mut image = crate::windower_bootstrap_contract::map_pe_relocated(&file);
        let base = image.as_ptr() as usize;
        assert_eq!(pe_identity(base), Some(FFXIDB_ID));
        assert!(plugin_layout(base));
        assert!(scan_cache_fresh(base), "the scan cache lives in zero-filled .data");
        for rva in SCAN_CACHE_REFERENCES {
            image[rva + 1] ^= 1;
            assert!(!plugin_layout(base), "moved scan cache reference at {rva:X}");
            image[rva + 1] ^= 1;
        }
        assert!(plugin_layout(base));
        image[VTABLE_RVA + 2 * 4] ^= 1;
        assert!(!plugin_layout(base), "changed callback must be refused");
    }
}
