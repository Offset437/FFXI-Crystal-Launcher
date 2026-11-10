//! Starting the real game through the installed polcore, FFXi.dll's
//! FFXiEntry, and `GameStart`. Everything here runs on
//! the game thread, which is a COM single-threaded apartment.

use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::Globalization::{MultiByteToWideChar, WideCharToMultiByte, CP_ACP};
use windows_sys::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
use windows_sys::Win32::System::Memory::{VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_READWRITE, PAGE_WRITECOPY};

use crate::log;
use crate::proto::{self, Region};
use crate::registry;

type HRESULT = i32;

use crate::pol_core::TABLE_LEN;

const CLSID_POLCORE: [GUID; 3] = [
    GUID::from_u128(0x07974581_0DF6_4EF0_BD05_604B3ADA9BE9), // JP
    GUID::from_u128(0x3501F5DD_7894_42DF_866A_A2B6527D8049), // US
    GUID::from_u128(0xE5966FB3_C97B_42EB_84BF_37F95EE54A9F), // EU
];
const IID_POLCORE: [GUID; 3] = [
    GUID::from_u128(0x9A30D565_A74C_4B56_B971_DCF02185B10D),
    GUID::from_u128(0xE0516654_EF77_435D_AA7D_50D2C069CE34),
    GUID::from_u128(0xDFEC2E93_4971_4A54_B8ED_63815C208C5A),
];
const CLSID_FFXIENTRY: GUID = GUID::from_u128(0x989D790D_6236_11D4_80E9_00105A81E890);
const IID_IFFXIENTRY: GUID = GUID::from_u128(0x989D790C_6236_11D4_80E9_00105A81E890);
const IID_ICLASSFACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_C000_000000000046);
const CLSID_GAME_MAIN: GUID = GUID::from_u128(0x1027DC46_750D_4B1F_8834_1D25B8BEBAB8);
const CLSID_FILE_MANAGER: GUID = GUID::from_u128(0x0DF0E951_D03C_4A94_90EF_40AE60668F5F);
static SELECTED_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

// IPOLCoreCom vtable indices.
const VT_RELEASE: usize = 2;
const VT_SET_PARAM_INIT: usize = 5;
const VT_GET_COMMON_FUNCTION_TABLE: usize = 7;
const VT_POL_VIEWER_EXEC: usize = 8;
const VT_SET_AREA_CODE: usize = 27;
// IFFXiEntry
const VT_GAME_START: usize = 3;
// IClassFactory
const VT_CREATE_INSTANCE: usize = 3;

const SLOT_EXIT_CODE: usize = 970;
const SLOT_EXIT_MESSAGE: usize = 1027;

/// app 102afa80: the game language the Viewer gives 1528 for its region:
/// Japanese 0, North America 1, Europe the Viewer's registry Language.
fn pol_language(area: i32, registry_language: i32) -> u32 {
    match area { 0 => 0, 1 => 1, _ => u32::try_from(registry_language).unwrap_or(1) }
}

/// The core's COM interface, checked by structure before any of its methods
/// runs (`pol_core::check_com`). Returns the core's layout for the table check.
unsafe fn check_core_interface(core: &Com, module: HMODULE) -> Result<crate::pol_core::Layout, String> {
    let layout = crate::pol_core::Layout::live(module as usize)?;
    let vtable = crate::mem::read::<usize>(core.0 as usize).ok_or("PlayOnline core check failed: its COM object is unreadable.")?;
    let mut methods = [0usize; crate::pol_core::COM_VTABLE_WORDS];
    if !crate::mem::read_into(vtable, &mut methods) {
        return Err("PlayOnline core check failed: its COM vtable is unreadable.".into());
    }
    crate::pol_core::check_com(&layout, vtable, &methods)?;
    Ok(layout)
}

/// The function table GetCommonFunctionTable returned, checked by structure
/// before any service is called (`pol_core::check_table`).
fn check_function_table(layout: &crate::pol_core::Layout, table: *mut usize) -> Result<crate::pol_core::Table, String> {
    let mut words = vec![0usize; TABLE_LEN];
    if !crate::mem::read_into(table as usize, &mut words) {
        return Err(format!("PlayOnline core check failed: the function table {:#x} is unreadable.", table as usize));
    }
    crate::pol_core::check_table(layout, table as usize, &words)
}

/// The installed Viewer's ticker sets the stock frame the Viewer's
/// frame-counted timers (auto-away, mail) run on.
fn adopt_viewer_frame(viewer: &Path, region: Region) {
    let folder = viewer.join("viewer").join("com");
    let file = if region == Region::Eu && folder.join("appEU.dll").is_file() { folder.join("appEU.dll") } else { folder.join("app.dll") };
    match std::fs::read(&file).map_err(|e| e.to_string()).and_then(|bytes| crate::pol_core::viewer_ticker(&bytes)) {
        Ok(us) => {
            crate::native_jobs::set_stock_frame_us(us);
            log!("stage: Viewer frame {:.2} ms from {} (3600 Viewer frames = {:.1} s)", us as f64 / 1000.0, file.display(), us as f64 * 3600.0 / 1e6);
        }
        Err(error) => log!("note: Viewer frame unknown ({error}); keeping {:.2} ms", crate::native_jobs::stock_frame_us() as f64 / 1000.0),
    }
}

pub struct GameOutcome {
    pub code: i32,
    pub message: String,
    /// Native GameStart returned S_OK with no message object or POL error text.
    /// This is evidence for the launcher, never a replacement for the exit code.
    pub game_returned: bool,
    /// FFXi.dll and FFXiMain.dll were unloaded after this game, so another
    /// game in this host would map fresh copies (game_modules).
    pub modules_released: bool,
}

pub struct GameConfig {
    pub region: Region,
    pub install_dir: Option<String>,
    pub pol_install_dir: Option<String>,
    pub native_auth: Option<proto::NativeCredentials>,
    pub start_game: bool,
    pub keep_session: bool,
    /// Session `viewer_gaps` (sign-in-only sessions; 07c §4).
    pub viewer_gaps: bool,
    pub auto_away_minutes: u32,
    pub auto_login: proto::AutoLogin,
}

/// # Safety-relevant
/// A COM interface pointer; only used on the thread that created it.
struct Com(*mut c_void);

impl Com {
    unsafe fn method<F: Copy>(&self, idx: usize) -> F {
        let vtbl = *(self.0 as *const *const usize);
        std::mem::transmute_copy(&*vtbl.add(idx))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a live COM object; Release is vtable slot 2.
            unsafe {
                let release: unsafe extern "system" fn(*mut c_void) -> u32 = self.method(VT_RELEASE);
                release(self.0);
            }
        }
    }
}

/// IPOLCoreCom::PolViewerExec (vtable 8) runs the Viewer modally inside the
/// game. FFXiMain calls it only when its "show Viewer" flag is set (the in-game
/// Viewer button); a normal game exit never does (pol-spec 03 §3.3). With no
/// advised sink the original returns an uninitialised HRESULT (polcore
/// 100070dc), so answer S_OK deterministically instead. Crystal has no viewer
/// to run there. The slot is in this process's verified polcore image.
struct ViewerReturn {
    slot: *mut usize,
    original: usize,
}

fn checked_viewer_slot(vtable: usize, base: usize, size: usize) -> Result<usize, String> {
    let pointer_size = std::mem::size_of::<usize>();
    let slot = vtable.checked_add(VT_POL_VIEWER_EXEC * pointer_size)
        .ok_or("PlayOnline viewer vtable address overflows")?;
    let end = slot.checked_add(pointer_size).ok_or("PlayOnline viewer slot address overflows")?;
    if vtable < base || !vtable.is_multiple_of(pointer_size) || end > base.checked_add(size).ok_or("PlayOnline core image address overflows")? {
        return Err("PlayOnline viewer vtable is outside the verified core image".into());
    }
    Ok(slot)
}

impl ViewerReturn {
    unsafe fn install(core: &Com, module: HMODULE) -> Result<Self, String> {
        let (base, size, _) = module_image(module);
        let vtable = *(core.0 as *const usize);
        let slot = checked_viewer_slot(vtable, base, size)? as *mut usize;
        Self::install_at(slot)
    }

    /// The caller owns a validated, aligned live COM vtable slot for this scope.
    unsafe fn install_at(slot: *mut usize) -> Result<Self, String> {
        let original = slot.read();
        if original == 0 { return Err("PlayOnline viewer method is null".into()); }
        let patch = Self { slot, original };
        patch.write(original, pol_viewer_exec as *const () as usize)?;
        log!("stage: in-game PlayOnline Viewer requests answered without a viewer");
        Ok(patch)
    }

    /// Swaps `from` for `to` in the slot. Windows gives this process a private
    /// writable image page; its protection is restored after the swap.
    fn write(&self, from: usize, to: usize) -> Result<(), String> {
        let done = crate::iat::swap(self.slot as usize, from, to)
            .map_err(|error| format!("Could not prepare PlayOnline viewer return: {error}"))?;
        if !done.changed {
            return Err("PlayOnline viewer method was changed by another module; left as is".into());
        }
        done.restored.map_err(|error| format!("Could not restore PlayOnline core page protection: {error}"))
    }
}

impl Drop for ViewerReturn {
    fn drop(&mut self) {
        if let Err(error) = self.write(pol_viewer_exec as *const () as usize, self.original) { log!("error: {error}"); }
    }
}

unsafe extern "system" fn pol_viewer_exec(_: *mut c_void, _: *mut u32) -> HRESULT {
    // stdcall(this, ULONG*) matches IPOLCoreCom vtable slot 8. The exit argument
    // belongs to FFXiEntry; returning success does not read or alter it.
    log!("stage: game requested the in-game PlayOnline Viewer; none to show, answered S_OK");
    0
}

use crate::win::wide;

/// ANSI (registry) bytes to a Rust string via the active code page.
pub(crate) fn ansi_to_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // SAFETY: sizes match the buffers.
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        let mut w = vec![0u16; n.max(0) as usize];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), bytes.len() as i32, w.as_mut_ptr(), n);
        String::from_utf16_lossy(&w)
    }
}

/// The module's full path; empty when it cannot be read whole (a truncated
/// path never matches the selected DLL).
fn module_path(m: HMODULE) -> String {
    crate::win::module_path(m).map(|path| path.to_string_lossy().into_owned()).unwrap_or_default()
}

/// (base, SizeOfImage, TimeDateStamp) of a loaded module; size and timestamp
/// are 0 when its headers can't be read (no build or bounds check passes).
fn module_image(m: HMODULE) -> (usize, usize, u32) {
    let base = m as usize;
    crate::pe::identity(base).map_or((base, 0, 0), |image| (base, image.size, image.timestamp))
}

/// POL uses its registered viewer, with a direct-DLL fallback for portable installs.
fn create(clsid: &GUID, iid: &GUID, dll: &Path, what: &str) -> Result<Com, String> {
    let mut p: *mut c_void = std::ptr::null_mut();
    #[cfg(feature = "native-login-diagnostics")]
    let started = (what == "polcore").then(crate::startup_timing::Stamp::capture);
    // SAFETY: valid GUIDs and out pointer.
    let hr = unsafe { CoCreateInstance(clsid, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, iid, &mut p) };
    #[cfg(feature = "native-login-diagnostics")]
    if let Some(started) = started {
        crate::startup_timing::report("polcore-registered-com", started, crate::startup_timing::Stamp::capture());
    }
    if hr >= 0 && !p.is_null() {
        log!("{what}: created through COM");
        return Ok(Com(p));
    }
    log!("{what}: CoCreateInstance failed (hr={hr:#010x}); trying {}", dll.display());
    create_from_dll(clsid, iid, dll, what)
}

/// Create from the selected DLL's class factory, bypassing global COM registration.
/// polcore stays resident for the whole host; the game DLLs are released after
/// each game (game_modules), like stock pol.exe's CoFreeUnusedLibraries.
fn create_from_dll(clsid: &GUID, iid: &GUID, dll: &Path, what: &str) -> Result<Com, String> {
    create_from_dll_with_outer(clsid, iid, std::ptr::null_mut(), dll, what)
}

fn create_from_dll_with_outer(clsid: &GUID, iid: &GUID, outer: *mut c_void, dll: &Path, what: &str) -> Result<Com, String> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let wpath = wide(&dll.to_string_lossy());
    let load_started = Instant::now();
    // SAFETY: NUL-terminated path.
    let m = unsafe { LoadLibraryExW(wpath.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
    if m.is_null() {
        return Err(format!("{what}: cannot load {} ({})", dll.display(), std::io::Error::last_os_error()));
    }
    log!("local launch timing: {what} DLL load {:.3}s", load_started.elapsed().as_secs_f64());
    let loaded = PathBuf::from(module_path(m));
    if !same_dll_path(&loaded, dll) {
        // SAFETY: balance only the reference we just acquired, before using any export.
        unsafe { FreeLibrary(m) };
        return Err(format!("{what}: loaded {} instead of the selected {}", loaded.display(), dll.display()));
    }
    // The game DLLs live for one game: game_modules releases this reference
    // after GameStart returns (stock pol.exe frees them there too).
    let counted = crate::game_modules::is_game_module(&loaded);
    if counted {
        crate::game_modules::note_reference(m);
    }
    // Patch imports before constructing GameMain: its constructor may itself create
    // other game components. LoadLibraryEx still passes through the injector's hooks.
    if what == "GameMain" && !crate::ffxi_settings::apply_to_ffximain() {
        return Err("GameMain: could not prepare the selected game DLL's imports".into());
    }
    // SAFETY: m is a loaded module; the export has the documented COM signature.
    unsafe {
        // A counted game-DLL reference stays, like every later failure:
        // game_modules releases it once after GameStart.
        let Some(get) = GetProcAddress(m, c"DllGetClassObject".as_ptr().cast()) else {
            if !counted { FreeLibrary(m); }
            return Err(format!("{what}: {} has no DllGetClassObject", dll.display()));
        };
        let get: unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(get);
        let mut factory: *mut c_void = std::ptr::null_mut();
        let factory_started = Instant::now();
        let hr = get(clsid, &IID_ICLASSFACTORY, &mut factory);
        log!("local launch timing: {what} class factory {:.3}s", factory_started.elapsed().as_secs_f64());
        if hr < 0 || factory.is_null() {
            return Err(format!("{what}: DllGetClassObject failed (hr={hr:#010x})"));
        }
        let factory = Com(factory);
        let create: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = factory.method(VT_CREATE_INSTANCE);
        let instance_started = Instant::now();
        let hr = create(factory.0, outer, iid, &mut p);
        log!("local launch timing: {what} instance creation {:.3}s", instance_started.elapsed().as_secs_f64());
        if hr < 0 || p.is_null() {
            return Err(format!("{what}: CreateInstance failed (hr={hr:#010x})"));
        }
    }
    log!("{what}: created from {}", dll.display());
    Ok(Com(p))
}

pub(crate) fn selected_game_directory() -> bool {
    SELECTED_DIRECTORY.get().is_some()
}

pub(crate) fn select_game_directory(directory: PathBuf) -> Result<(), &'static str> {
    SELECTED_DIRECTORY.set(directory).map_err(|_| "A game installation is already selected in this loader process.")
}

fn game_dll_for(directory: &Path, clsid: &GUID) -> Option<(PathBuf, &'static str)> {
    let matches = |known: &GUID| clsid.data1 == known.data1 && clsid.data2 == known.data2 && clsid.data3 == known.data3 && clsid.data4 == known.data4;
    if matches(&CLSID_GAME_MAIN) { Some((directory.join("FFXiMain.dll"), "GameMain")) }
    else if matches(&CLSID_FILE_MANAGER) { Some((directory.join("FFXi.dll"), "FxFileManager")) }
    else if matches(&CLSID_FFXIENTRY) { Some((directory.join("FFXi.dll"), "FFXiEntry")) }
    else { None }
}

/// Intercepts only FFXI's own COM classes; DirectShow, WMI and other classes retain
/// the original COM/injector route. The caller supplies normal COM API pointers.
pub(crate) unsafe fn create_selected_component(clsid: *const GUID, outer: *mut c_void, ctx: u32, iid: *const GUID, out: *mut *mut c_void) -> Option<HRESULT> {
    let directory = SELECTED_DIRECTORY.get()?;
    if clsid.is_null() { return None; }
    let (dll, what) = game_dll_for(directory, &*clsid)?;
    if iid.is_null() || out.is_null() { return Some(0x80004003u32 as HRESULT); } // E_POINTER
    *out = std::ptr::null_mut();
    if ctx & CLSCTX_INPROC_SERVER == 0 { return Some(0x80040111u32 as HRESULT); } // CLASS_E_CLASSNOTAVAILABLE
    match create_from_dll_with_outer(&*clsid, &*iid, outer, &dll, what) {
        Ok(com) => {
            let com = std::mem::ManuallyDrop::new(com);
            *out = com.0; // ownership passes to FFXI's COM caller
            Some(0)
        }
        Err(error) => {
            log!("error: selected game component: {error}");
            Some(0x80004005u32 as HRESULT) // E_FAIL; never substitute another installation
        }
    }
}

fn same_dll_path(a: &Path, b: &Path) -> bool {
    let normalize = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    normalize(a).to_string_lossy().eq_ignore_ascii_case(&normalize(b).to_string_lossy())
}

fn viewer_directory(selected: Option<&str>, registered: Option<&str>, region: Region) -> Result<PathBuf, String> {
    let directory = selected.or(registered).ok_or("PlayOnline Viewer folder not found; select an installed PlayOnline folder in Crystal.")?;
    let directory = PathBuf::from(directory);
    let core = if region == Region::Eu { "polcoreEU.dll" } else { "polcore.dll" };
    if !directory.is_absolute() || !directory.join("viewer").join("com").join(core).is_file() {
        return Err("The selected PlayOnline folder does not contain the installed PlayOnline core.".into());
    }
    Ok(directory)
}

fn ansi_path(path: &Path) -> Result<Vec<u8>, String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let len = i32::try_from(wide.len()).map_err(|_| "PlayOnline folder path is too long")?;
    let required = unsafe { WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), len,
        std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut()) };
    if required <= 0 { return Err("PlayOnline folder cannot be represented by the native core.".into()); }
    let mut bytes = vec![0u8; required as usize];
    let written = unsafe { WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), len,
        bytes.as_mut_ptr(), required, std::ptr::null(), std::ptr::null_mut()) };
    if written != required || ansi_to_string(&bytes) != path.to_string_lossy() || bytes.contains(&0) {
        return Err("PlayOnline folder uses characters unsupported by the native core's system code page.".into());
    }
    Ok(bytes)
}

fn game_directory(selected: Option<&str>, registered: Option<&str>) -> Result<PathBuf, String> {
    let directory = selected.or(registered).ok_or("FINAL FANTASY XI folder not found (session install_dir and InstallFolder\\0001 both missing)")?;
    let directory = PathBuf::from(directory);
    if !directory.is_absolute() {
        return Err("The FINAL FANTASY XI folder must be an absolute path.".into());
    }
    for name in ["FFXi.dll", "FFXiMain.dll"] {
        if !directory.join(name).is_file() {
            return Err(format!("{} has no {name}; the selected installation was not replaced by the registered one", directory.display()));
        }
    }
    Ok(directory)
}

/// Checks the table pointer lies inside polcore's image and is writable,
/// making it writable if it is not. Returns the table as a slice.
unsafe fn table_slice(tbl: *mut usize, polcore: HMODULE) -> Result<&'static mut [usize], String> {
    let (base, size, _) = module_image(polcore);
    let start = tbl as usize;
    let bytes = TABLE_LEN * std::mem::size_of::<usize>();
    let end = start.checked_add(bytes).ok_or("function-table address overflows")?;
    let image_end = base.checked_add(size).ok_or("polcore image address overflows")?;
    if start < base || end > image_end || !start.is_multiple_of(std::mem::size_of::<usize>()) {
        return Err(format!("function table {start:#x} is not inside polcore ({base:#x}+{size:#x})"));
    }
    // Every region the table spans must be writable (it is .data in the verified build).
    let mut at = start;
    let mut writable = true;
    while at < end {
        let mut mbi: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        if VirtualQuery(at as *const c_void, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) == 0 {
            return Err("cannot query the function table's memory".into());
        }
        writable &= matches!(mbi.Protect & 0xFF, PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY);
        at = mbi.BaseAddress as usize + mbi.RegionSize;
    }
    if !writable {
        let mut old = 0;
        if VirtualProtect(tbl as *const c_void, bytes, PAGE_READWRITE, &mut old) == 0 {
            return Err(format!("function table is not writable ({})", std::io::Error::last_os_error()));
        }
        log!("function table made writable (protect was {old:#x})");
    }
    Ok(std::slice::from_raw_parts_mut(tbl, TABLE_LEN))
}

/// The per-Play part of a parked host's session, from its `credentials` line
/// (pol-spec 07d §1.2): the fields a direct `session` line gives `GameConfig`.
pub struct Play {
    pub native_auth: proto::NativeCredentials,
    pub start_game: bool,
    pub keep_session: bool,
    pub viewer_gaps: bool,
    pub auto_away_minutes: u32,
}

/// Runs the game to completion. Errors are plain stage descriptions (no secrets).
pub fn run(mut cfg: GameConfig) -> Result<GameOutcome, String> {
    run_with(&mut cfg, None)
}

/// A parked host (07d §1.3): the cold path's pre-credential part, then
/// `ready(polcore creation time)` and window messages only until the Play.
/// `cfg` carries the host fields; `play` gives the rest. A main thread that
/// gives up (pipe closed, credentials rejected) ends it with EXIT_PIPE; nothing
/// was networked and no polcore entry was called.
pub fn run_parked(mut cfg: GameConfig, ready: std::sync::mpsc::Sender<std::time::Duration>, play: std::sync::mpsc::Receiver<Play>) -> Result<GameOutcome, String> {
    let mut park = move |cfg: &mut GameConfig, polcore: std::time::Duration| {
        let _ = ready.send(polcore);
        let Some(play) = crate::park::wait_for_play(&play, crate::park::PUMP, &mut crate::native_auth::pump_idle_messages) else { return false };
        cfg.native_auth = Some(play.native_auth);
        (cfg.start_game, cfg.keep_session, cfg.viewer_gaps, cfg.auto_away_minutes) = (play.start_game, play.keep_session, play.viewer_gaps, play.auto_away_minutes);
        true
    };
    run_with(&mut cfg, Some(&mut park))
}

/// Called once at the park end point; false when no Play will come.
type ParkHook<'a> = &'a mut dyn FnMut(&mut GameConfig, std::time::Duration) -> bool;

fn run_with(cfg: &mut GameConfig, park: Option<ParkHook<'_>>) -> Result<GameOutcome, String> {
    #[cfg(feature = "native-login-diagnostics")]
    let started = crate::startup_timing::Stamp::capture();
    // SAFETY: plain COM init on this thread.
    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    #[cfg(feature = "native-login-diagnostics")]
    crate::startup_timing::report("com-apartment", started, crate::startup_timing::Stamp::capture());
    if hr < 0 {
        return Err(format!("CoInitializeEx failed (hr={hr:#010x})"));
    }
    log!("stage: COM initialised (STA)");
    let result = run_in_apartment(cfg, park);
    log!("stage: closing COM apartment");
    // SAFETY: balances CoInitializeEx above.
    unsafe { CoUninitialize() };
    log!("stage: COM apartment closed");
    result
}

fn run_in_apartment(cfg: &mut GameConfig, park: Option<ParkHook<'_>>) -> Result<GameOutcome, String> {
    crate::controller_catalog::prefetch();
    let region = cfg.region;
    let area = region.area_code();
    if region != Region::Us {
        log!("warning: region {} is not verified; only US is", region.name());
    }

    // --- selected/discovered paths, then read-only registry fallback ---
    let registered_viewer = registry::install_folder(region, "1000").map(|bytes| ansi_to_string(&bytes));
    let viewer = viewer_directory(cfg.pol_install_dir.as_deref(), registered_viewer.as_deref(), region)?;
    let viewer_ansi = ansi_path(&viewer)?;
    let reg_ffxi = registry::install_folder(region, "0001").map(|b| ansi_to_string(&b));
    let ffxi_dir = game_directory(cfg.install_dir.as_deref(), reg_ffxi.as_deref())?;
    select_game_directory(ffxi_dir.clone())?;
    if let (Some(d), Some(r)) = (&cfg.install_dir, &reg_ffxi) {
        if !d.eq_ignore_ascii_case(r) {
            log!("note: session install_dir differs from InstallFolder\\0001 ({r}); using {}", ffxi_dir.display());
        }
    }
    let lang = registry::viewer_language(region);
    log!("stage: paths: viewer={} ffxi={} area={area} lang={lang}", viewer.display(), ffxi_dir.display());

    // --- polcore ---
    let polcore_name = if region == Region::Eu { "polcoreEU.dll" } else { "polcore.dll" };
    let polcore_path = viewer.join("viewer").join("com").join(polcore_name);
    let polcore_started = Instant::now();
    let polcore = if cfg.pol_install_dir.is_some() {
        create_from_dll(&CLSID_POLCORE[area as usize], &IID_POLCORE[area as usize], &polcore_path, "polcore")?
    } else {
        create(&CLSID_POLCORE[area as usize], &IID_POLCORE[area as usize], &polcore_path, "polcore")?
    };
    // Includes polcore's second-boundary spin (0.1-0.7 s; 08 §2), which a park absorbs.
    let polcore_time = polcore_started.elapsed();
    log!("local launch timing: polcore created in {:.3}s", polcore_time.as_secs_f64());
    let wname = wide(polcore_name);
    // SAFETY: NUL-terminated name.
    let pmod = unsafe { GetModuleHandleW(wname.as_ptr()) };
    if pmod.is_null() {
        return Err(format!("{polcore_name} is not loaded after creating polcore"));
    }
    #[cfg_attr(not(feature = "native-login-diagnostics"), allow(unused_variables))]
    let (pbase, _psize, _) = module_image(pmod);
    // SAFETY: a live core interface; only its vtable and methods are read.
    let layout = match unsafe { check_core_interface(&polcore, pmod) } {
        Ok(layout) => layout,
        Err(error) => {
            crate::feature_check::report_pol(crate::pol_core::Layout::live(pmod as usize).ok().as_ref(), Err(&error));
            return Err(error);
        }
    };
    log!("stage: polcore loaded from {} (build {})", module_path(pmod), layout.name());
    adopt_viewer_frame(&viewer, region);
    // SAFETY: a live core interface whose vtable passed the structural check;
    // its slot is bounds-checked again before the change. Keep the patch
    // through FFXi release.
    let viewer_return = unsafe { ViewerReturn::install(&polcore, pmod)? };

    // The strings polcore keeps pointers to must outlive it: leak them.
    let cmd: &'static mut [u8] = Box::leak(b"\0".to_vec().into_boxed_slice());
    let regkey: &'static CString = Box::leak(Box::new(CString::new(registry::playonline_key(region)).expect("no NUL")));
    // ansi_path refuses a path with a NUL byte.
    let viewer_c: &'static CString = Box::leak(Box::new(CString::new(viewer_ansi).expect("no NUL")));

    let tbl: *mut usize = unsafe {
        // SAFETY: live IPOLCoreCom object; method indices and signatures match its layout.
        let set_area: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT = polcore.method(VT_SET_AREA_CODE);
        let set_param: unsafe extern "system" fn(*mut c_void, HMODULE, *mut u8) -> HRESULT = polcore.method(VT_SET_PARAM_INIT);
        let get_tbl: unsafe extern "system" fn(*mut c_void, *mut *mut usize) -> HRESULT = polcore.method(VT_GET_COMMON_FUNCTION_TABLE);
        set_area(polcore.0, area);
        let hr = set_param(polcore.0, GetModuleHandleW(std::ptr::null()), cmd.as_mut_ptr());
        if hr < 0 {
            return Err(format!("polcore SetParamInit failed (hr={hr:#010x})"));
        }
        let mut t: *mut usize = std::ptr::null_mut();
        let hr = get_tbl(polcore.0, &mut t);
        if hr < 0 || t.is_null() {
            return Err(format!("polcore GetCommonFunctionTable failed (hr={hr:#010x})"));
        }
        t
    };
    let checked = check_function_table(&layout, tbl);
    crate::feature_check::report_pol(Some(&layout), checked.as_ref().map_err(|e| e.as_str()));
    let checked = checked?;
    log!("{}", checked.summary(&layout));
    // SAFETY: the table passed the structural check (inside polcore's data).
    let table = unsafe { table_slice(tbl, pmod)? };
    log!("stage: polcore SetAreaCode/SetParamInit done; function table at {:#x}", tbl as usize);

    // A pilot records socket metadata only during native sign-in/idle. The
    // forwarding imports stay valid for the process lifetime, including error
    // teardown and native worker calls; dropping this guard only stops logging.
    #[cfg(feature = "native-login-diagnostics")]
    let socket_trace = match unsafe { crate::native_socket_trace::Recording::install(pbase, _psize) } {
        Ok(trace) => Some(trace),
        Err(error) => { log!("native POL socket trace unavailable: {error}"); None }
    };

    // --- park end point (07d §1.3 step 3): polcore and its validated table
    // only. No restore_viewer_policy, no bootstrap, no member select, no Open
    // and no entry call of any kind until the credentials arrive.
    if let Some(park) = park {
        if !park(cfg, polcore_time) {
            log!("stage: parked host released without a Play; nothing was networked");
            return Ok(GameOutcome { code: crate::EXIT_PIPE, message: String::new(), game_returned: false, modules_released: false });
        }
    }
    let Some(credentials) = cfg.native_auth.take() else { return Err("The game host received no credentials.".into()) };

    // --- native: the Viewer's own startup and sign-in (pol-spec 01 §8) ---
    #[cfg(feature = "native-login-diagnostics")]
    let bootstrap_started = crate::startup_timing::Stamp::capture();
    // SAFETY: the supported core owns this table and outlives the session's
    // scope; the leaked strings outlive the core.
    let mut session = unsafe { crate::native_auth::NativeSession::bootstrap(table, &checked, &viewer, viewer_c, regkey, area, pol_language(area, lang), cfg.auto_away_minutes)? };
    #[cfg(feature = "native-login-diagnostics")]
    crate::startup_timing::report("native-bootstrap-complete", bootstrap_started, crate::startup_timing::Stamp::capture());
    session.set_viewer_gaps(cfg.viewer_gaps);
    let signed_in = session.sign_in(credentials);
    #[cfg(feature = "native-login-diagnostics")]
    if let Some(trace) = &socket_trace { trace.stop(); }
    if let Err(error) = signed_in {
        // The launcher's startup sign-out is the user's choice, not a
        // failure: end cleanly (exit 0) after the session's sign-out.
        if crate::native_auth::sign_in_cancelled() {
            log!("stage: native sign-in cancelled by the launcher ({error})");
            return Ok(GameOutcome { code: 0, message: String::new(), game_returned: false, modules_released: false });
        }
        return Err(error);
    }
    log!("stage: original POL service table retained (0 overrides)");
    #[cfg(feature = "native-backup-diagnostics")]
    // Pilot-only transparent forwarding; native arguments/results and all
    // other entries stay unchanged. Restore after every game caller exits.
    let backup_trace = match unsafe { crate::native_backup::BackupTrace::install(table, &layout) } {
        Ok(trace) => Some(trace),
        Err(error) => { log!("note: {error}"); None }
    };

    #[cfg(target_arch = "x86")]
    // A protection that cannot be installed is switched off with its reason;
    // it never stops the game from starting.
    match crate::windower_plugin_settings::install() {
        Ok(true) => log!("stage: Windower plugin settings coordinator prepared"),
        Ok(false) => {}
        Err(reason) => log!("note: FFXIDB shared-settings protection unavailable: {reason}"),
    }
    let mut play = cfg.start_game;
    let outcome = loop {
        if !play && !session.wait_for_game()? {
            break GameOutcome { code: 0, message: String::new(), game_returned: false, modules_released: false };
        }
        #[cfg(feature = "native-login-diagnostics")]
        if let Some(trace) = &socket_trace { trace.stop(); }
        // A batch must release every signed-in owner before any takes this turn.
        // Windower saves on the first main window, so protect game DLL loading and
        // COM construction too, rather than waiting until the GameStart call.
        // A failed batch loaded no game DLL: this Play is refused and the
        // signed-in owner returns to the idle Viewer.
        if let Err(reason) = crate::native_auth::wait_batch_game() {
            log!("stage: batch game start refused ({reason}); staying signed in");
            events_game_ended(&GameOutcome { code: crate::EXIT_GAME_START, message: reason, game_returned: false, modules_released: false });
            play = false;
            continue;
        }
        let outcome = run_game(&polcore, &ffxi_dir, table, &mut session, cfg.auto_login)?;
        events_game_ended(&outcome);
        if !retain_native_session(cfg.keep_session, &outcome) {
            if cfg.keep_session && outcome.game_returned && !outcome.modules_released {
                log!("stage: this host ends after its game because the game DLLs stayed loaded; the next game signs in a new host");
            }
            break outcome;
        }
        play = false;
    };
    #[cfg(feature = "native-backup-diagnostics")]
    drop(backup_trace);
    log!("stage: native POL session cleanup starting");
    drop(session); // Viewer sign-out and close teardown, file worker last
    log!("stage: native POL session cleanup finished");
    drop(viewer_return);
    log!("stage: releasing PlayOnline core interface");
    drop(polcore);
    log!("stage: PlayOnline core interface released");
    Ok(outcome)
}

fn events_game_ended(outcome: &GameOutcome) {
    crate::events::send(format!("{{\"type\":\"native_game_ended\",\"code\":{},\"game_returned\":{},\"game_id\":{}}}\n", outcome.code, outcome.game_returned, crate::game_shutdown::last_game()));
}

fn successful_native_return(hr: HRESULT, has_message_object: bool, message: &str) -> bool {
    hr == 0 && !has_message_object && message.is_empty()
}

/// Another game may follow in this host only on fresh game DLLs.
fn retain_native_session(enabled: bool, outcome: &GameOutcome) -> bool {
    enabled && outcome.game_returned && outcome.modules_released && matches!(outcome.code, 0 | -14)
}

fn run_game(polcore: &Com, ffxi_dir: &Path, table: &[usize], session: &mut crate::native_auth::NativeSession, auto_login: proto::AutoLogin) -> Result<GameOutcome, String> {
    #[cfg(target_arch = "x86")]
    crate::windower_start::coordinate(&mut || session.startup_frame())?;
    // A game never starts on an earlier game's memory: both game DLLs must be
    // absent here, so this GameStart maps fresh copies.
    crate::game_modules::refuse_reuse()?;
    // --- FFXi.dll ---
    let _boot_timing = crate::boot_timing::Run::begin();
    if let Err(e) = std::env::set_current_dir(ffxi_dir) {
        log!("note: cannot set the working directory to {}: {e}", ffxi_dir.display());
    }
    let ffxi = create_from_dll(&CLSID_FFXIENTRY, &IID_IFFXIENTRY, &ffxi_dir.join("FFXi.dll"), "FFXiEntry")?;

    // Per-process settings: FFXi.dll's GameMain creation loads FFXiMain, which reads its
    // settings later, in FFXiGameMain.
    {
        let name = wide("FFXi.dll");
        // SAFETY: NUL-terminated name.
        let m = unsafe { GetModuleHandleW(name.as_ptr()) };
        if !crate::ffxi_settings::hook_ffxi_dll(m as usize) {
            return Err("FFXi.dll's game-component loader could not be prepared; the selected installation was kept.".into());
        }
        crate::ffxi_settings::apply_to_ffximain();
    }

    // Read-only during play: no request is issued while FFXI runs (B1).
    let native_friends = session.observe_friends()?;
    let lobby_observer = crate::lobby::Observer::start();
    // Keep Viewer push ownership until all local waits have finished. Pumping
    // while waiting for Windower must happen before the game takes that over.
    let handoff_started = Instant::now();
    session.attach_game();
    log!("local launch timing: native POL handoff {:.3}s", handoff_started.elapsed().as_secs_f64());
    crate::events::send(proto::started_line(std::process::id()));
    let auto_login = crate::auto_login::Worker::start(auto_login);
    let live_controller = crate::controller_settings::Worker::start();
    let shutdown = crate::game_shutdown::Worker::start();
    #[cfg(target_arch = "x86")]
    crate::windower_plugin_settings::game_starting();
    log!("stage: calling FFXiEntry::GameStart");
    let mut message: *mut c_void = std::ptr::null_mut();
    // SAFETY: IFFXiEntry::GameStart(this, IUnknown* pol, IUnknown** msg); blocks for the session.
    let hr = unsafe {
        let start: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT = ffxi.method(VT_GAME_START);
        start(ffxi.0, polcore.0, &mut message)
    };
    log!("stage: GameStart returned hr={hr:#010x}");
    // Stop all input before game callbacks/interfaces can be detached or freed.
    log!("stage: stopping background shutdown worker");
    drop(shutdown);
    log!("stage: stopping live controller worker");
    drop(live_controller);
    log!("stage: stopping auto-login worker");
    drop(auto_login);
    log!("stage: stopping game observers");
    if let Some(observer) = lobby_observer { observer.finish(); }
    drop(native_friends);
    session.detach_game();
    log!("stage: game observers stopped and native game callbacks detached");
    crate::ffxi_settings::report();
    let has_message_object = !message.is_null();
    drop(Com(message));

    // Exit code and message FFXi.dll/FFXiMain left in polcore (slots 970/1027, real polcore).
    // SAFETY: real polcore entries; 1027 returns a NUL-terminated buffer of <= 0x400 bytes.
    let (code, msg) = unsafe {
        let code = std::mem::transmute::<usize, extern "C" fn() -> i32>(table[SLOT_EXIT_CODE])();
        let p = std::mem::transmute::<usize, extern "C" fn() -> *const u8>(table[SLOT_EXIT_MESSAGE])();
        let msg = if p.is_null() {
            String::new()
        } else {
            let bytes = std::slice::from_raw_parts(p, 0x400);
            let n = bytes.iter().position(|&b| b == 0).unwrap_or(0x400);
            ansi_to_string(&bytes[..n])
        };
        (code, msg)
    };
    log!("stage: releasing FFXI game interface");
    drop(ffxi);
    log!("stage: FFXI game interface released");
    // Stock pol.exe's CoFreeUnusedLibraries point (414456).
    let unload_started = Instant::now();
    let modules_released = match crate::game_modules::release_after_game() {
        crate::game_modules::Release::Unloaded => {
            log!("stage: FFXi.dll and FFXiMain.dll unloaded after the game in {:.3}s; a next game maps fresh copies", unload_started.elapsed().as_secs_f64());
            true
        }
        crate::game_modules::Release::Kept(reason) => {
            log!("stage: game DLLs kept loaded ({reason}); this host runs no further game");
            false
        }
    };
    if hr < 0 {
        return Err(format!("GameStart failed (hr={hr:#010x}){}", if msg.is_empty() { String::new() } else { format!(": {msg}") }));
    }
    // The installed FFXI /shutdown route sets the shared return flag +0x1d;
    // FFXi.dll translates it to POL -14 after orderly GameStart cleanup. Keep
    // the numeric result and independently report the successful native return.
    // The launcher additionally requires GameStarted and native sign-out before
    // it can present -14 as a normal close. Other errors retain their meaning.
    let game_returned = successful_native_return(hr, has_message_object, &msg);
    Ok(GameOutcome { code, message: msg, game_returned, modules_released })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_return_slot_rejects_foreign_or_overflowing_vtables() {
        assert_eq!(checked_viewer_slot(0x1000, 0x1000, 0x1000).unwrap(), 0x1000 + 8 * std::mem::size_of::<usize>());
        assert!(checked_viewer_slot(0x0ffc, 0x1000, 0x1000).is_err());
        assert!(checked_viewer_slot(0x1001, 0x1000, 0x1000).is_err());
        assert!(checked_viewer_slot(0x1000, 0x1000, 8 * std::mem::size_of::<usize>()).is_err());
        assert!(checked_viewer_slot(usize::MAX - 3, 0, usize::MAX).is_err());
    }

    #[test]
    fn game_exit_skips_the_absent_viewer_and_restores_the_com_method() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static VIEWER_CALLS: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn original_viewer(_: *mut c_void, _: *mut u32) -> HRESULT {
            VIEWER_CALLS.fetch_add(1, Ordering::Relaxed);
            7
        }
        // Exercise the actual COM vtable dispatch and 32-bit stdcall ABI. No
        // game DLL, account, network connection or viewer is involved.
        let original = original_viewer as *const () as usize;
        let mut table = [original; VT_POL_VIEWER_EXEC + 1];
        let before = table;
        let mut vtable = table.as_mut_ptr();
        let core = Com((&mut vtable as *mut *mut usize).cast());
        let mut exit = 123u32;
        unsafe {
            let patch = ViewerReturn::install_at(table.as_mut_ptr().add(VT_POL_VIEWER_EXEC)).unwrap();
            let invoke: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT = core.method(VT_POL_VIEWER_EXEC);
            assert_eq!(invoke(core.0, &mut exit), 0);
            assert_eq!(VIEWER_CALLS.load(Ordering::Relaxed), 0);
            assert_eq!(exit, 123);
            assert_eq!(&table[..VT_POL_VIEWER_EXEC], &before[..VT_POL_VIEWER_EXEC]);
            drop(patch);
            assert_eq!(table, before);
            let invoke: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT = core.method(VT_POL_VIEWER_EXEC);
            assert_eq!(invoke(core.0, &mut exit), 7);
            assert_eq!(VIEWER_CALLS.load(Ordering::Relaxed), 1);
        }
        std::mem::forget(core); // The fixture has no real IUnknown::Release.
    }

    #[test]
    #[ignore = "Requires an installed US PlayOnline core; no game or sign-in is started"]
    fn installed_polcore_supports_the_viewer_exit_override() {
        unsafe {
            assert!(CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0);
            let viewer = registry::install_folder(Region::Us, "1000").expect("installed US viewer");
            let dll = PathBuf::from(ansi_to_string(&viewer)).join("viewer/com/polcore.dll");
            let core = create(&CLSID_POLCORE[1], &IID_POLCORE[1], &dll, "polcore exit check").unwrap();
            let module = GetModuleHandleW(wide("polcore.dll").as_ptr());
            assert!(!module.is_null());
            check_core_interface(&core, module).unwrap();
            let area: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT = core.method(VT_SET_AREA_CODE);
            assert!(area(core.0, 1) >= 0);
            let original: usize = core.method(VT_POL_VIEWER_EXEC);
            let patch = ViewerReturn::install(&core, module).unwrap();
            // FFXiEntry asks for the regional interface again during exit.
            let query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = core.method(0);
            let mut out = std::ptr::null_mut();
            assert!(query(core.0, &IID_POLCORE[1], &mut out) >= 0);
            assert!(!out.is_null());
            let queried = Com(out);
            let invoke: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT = queried.method(VT_POL_VIEWER_EXEC);
            let mut exit = 0;
            assert_eq!(invoke(queried.0, &mut exit), 0);
            drop(queried);
            drop(patch);
            assert_eq!(core.method::<usize>(VT_POL_VIEWER_EXEC), original);
            drop(core);
            CoUninitialize();
        }
    }

    // Run each route in a fresh test process so DLL/COM caches cannot turn this
    // into a warm-load comparison. These tests never initialize a member,
    // invoke network functions, patch the core, or load any game component.
    fn benchmark_installed_polcore_activation(registered: bool) {
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::{GetCurrentProcess, GetProcessTimes}};
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) { unsafe { CoUninitialize() }; }
        }
        unsafe {
            let cpu_ms = || {
                let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
                let pointer = times.as_mut_ptr();
                assert_ne!(GetProcessTimes(GetCurrentProcess(), pointer, pointer.add(1), pointer.add(2), pointer.add(3)), 0);
                let value = |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
                (value(times[2]) + value(times[3])) as f64 / 10_000.0
            };
            assert!(GetModuleHandleW(wide("polcore.dll").as_ptr()).is_null(), "Run only this test in a fresh process");
            let apartment_started = Instant::now();
            assert!(CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0);
            let _apartment = Apartment;
            let apartment_ms = apartment_started.elapsed().as_secs_f64() * 1000.0;
            let viewer = registry::install_folder(Region::Us, "1000").expect("installed US viewer");
            let dll = PathBuf::from(ansi_to_string(&viewer)).join("viewer/com/polcore.dll");
            let cpu_before = cpu_ms();
            let phase_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_millis();
            let started = Instant::now();
            let core = if registered {
                let mut pointer = std::ptr::null_mut();
                let hr = CoCreateInstance(&CLSID_POLCORE[1], std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_POLCORE[1], &mut pointer);
                assert!(hr >= 0 && !pointer.is_null(), "Strict COM activation failed: {hr:#010x}");
                Com(pointer)
            } else {
                create_from_dll(&CLSID_POLCORE[1], &IID_POLCORE[1], &dll, "offline core activation benchmark").unwrap()
            };
            let activation_ms = started.elapsed().as_secs_f64() * 1000.0;
            let activation_cpu_ms = cpu_ms() - cpu_before;
            let module = GetModuleHandleW(wide("polcore.dll").as_ptr());
            assert!(!module.is_null());
            assert!(same_dll_path(&PathBuf::from(module_path(module)), &dll), "Activation must use the selected installed core");
            let (base, size, timestamp) = module_image(module);
            check_core_interface(&core, module).unwrap();
            let vtable = *(core.0 as *const *const usize) as usize;
            assert!(vtable >= base && vtable.checked_add((VT_SET_AREA_CODE + 1) * std::mem::size_of::<usize>()).is_some_and(|end| end <= base + size));
            let indices = [0, VT_RELEASE, VT_SET_PARAM_INIT, VT_GET_COMMON_FUNCTION_TABLE, VT_SET_AREA_CODE];
            let methods = indices.map(|index| core.method::<usize>(index));
            assert!(methods.iter().all(|&method| method >= base && method < base + size));
            let query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = core.method(0);
            let mut pointer = std::ptr::null_mut();
            assert!(query(core.0, &IID_POLCORE[1], &mut pointer) >= 0 && !pointer.is_null());
            let queried = Com(pointer);
            assert_eq!(indices.map(|index| queried.method::<usize>(index)), methods);
            let method_offsets = methods.map(|method| method - base);
            let route = if registered { "com" } else { "direct" };
            eprintln!("CORE_ACTIVATION route={route} apartment_ms={apartment_ms:.3} activation_ms={activation_ms:.3} cpu_ms={activation_cpu_ms:.3} start_second_phase_ms={phase_ms} timestamp={timestamp:08x} vtable_offset={:x} method_offsets={method_offsets:x?}", vtable - base);
            drop(queried);
            drop(core);
        }
    }

    #[test]
    #[ignore = "Fresh-process installed US core activation benchmark; no game or sign-in"]
    fn installed_polcore_activation_through_com() {
        benchmark_installed_polcore_activation(true);
    }

    #[test]
    #[ignore = "Fresh-process installed US core activation benchmark; no game or sign-in"]
    fn installed_polcore_activation_through_dll() {
        // The production helper already separates DLL load/factory/instance
        // timings. Keep those in this offline test's log, away from live hosts.
        assert!(crate::log::init().is_some());
        benchmark_installed_polcore_activation(false);
    }

    #[test]
    #[ignore = "Installed-core ABI check with synthetic credentials; never drives connect or loads the game"]
    fn installed_polcore_accepts_native_credential_bootstrap_offline() {
        unsafe {
            assert!(CoInitializeEx(std::ptr::null(),COINIT_APARTMENTTHREADED as u32)>=0);
            let viewer=registry::install_folder(Region::Us,"1000").expect("installed US viewer");
            let viewer=PathBuf::from(ansi_to_string(&viewer));
            let core=create(&CLSID_POLCORE[1],&IID_POLCORE[1],&viewer.join("viewer/com/polcore.dll"),"native POL ABI check").unwrap();
            let module=GetModuleHandleW(wide("polcore.dll").as_ptr());
            let (base,_,_)=module_image(module);let layout=check_core_interface(&core,module).unwrap();
            let area:unsafe extern "system" fn(*mut c_void,i32)->HRESULT=core.method(VT_SET_AREA_CODE);
            assert!(area(core.0,1)>=0);
            let param:unsafe extern "system" fn(*mut c_void,HMODULE,*mut u8)->HRESULT=core.method(VT_SET_PARAM_INIT);
            let mut command=[0u8];assert!(param(core.0,GetModuleHandleW(std::ptr::null()),command.as_mut_ptr())>=0);
            let getter:unsafe extern "system" fn(*mut c_void,*mut *mut usize)->HRESULT=core.method(VT_GET_COMMON_FUNCTION_TABLE);
            let mut pointer=std::ptr::null_mut();assert!(getter(core.0,&mut pointer)>=0);
            check_function_table(&layout,pointer).unwrap();let table=table_slice(pointer,module).unwrap();let before=table.to_vec();
            crate::native_auth::smoke_prepare(table,base,&viewer);
            crate::native_auth::smoke_message_files(table);
            crate::native_social::smoke_builders(table);
            // Never 814/815/816/818: no connect, pump or close step runs here.
            crate::native_backup::smoke_entrypoints(table,&layout);
            assert_eq!(table,before,"native login must not patch any POL callback");
            drop(core);CoUninitialize();
        }
    }

    #[test]
    #[ignore = "Installed-core handle ABI check; synthetic local cache only, no sign-in or game"]
    fn installed_polcore_accepts_handle_setup_offline() {
        unsafe {
            assert!(GetModuleHandleW(wide("polcore.dll").as_ptr()).is_null(), "run this test alone in a fresh process");
            assert!(CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0);
            let viewer = registry::install_folder(Region::Us, "1000").expect("installed US viewer");
            let dll = PathBuf::from(ansi_to_string(&viewer)).join("viewer/com/polcore.dll");
            let core = create_from_dll(&CLSID_POLCORE[1], &IID_POLCORE[1], &dll, "offline handle ABI check").unwrap();
            let module = GetModuleHandleW(wide("polcore.dll").as_ptr());
            let (base, _, _) = module_image(module);
            let layout = check_core_interface(&core, module).unwrap();
            let area: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT = core.method(VT_SET_AREA_CODE);
            assert!(area(core.0, 1) >= 0);
            let param: unsafe extern "system" fn(*mut c_void, HMODULE, *mut u8) -> HRESULT = core.method(VT_SET_PARAM_INIT);
            let mut command = [0u8];
            assert!(param(core.0, GetModuleHandleW(std::ptr::null()), command.as_mut_ptr()) >= 0);
            let getter: unsafe extern "system" fn(*mut c_void, *mut *mut usize) -> HRESULT = core.method(VT_GET_COMMON_FUNCTION_TABLE);
            let mut pointer = std::ptr::null_mut();
            assert!(getter(core.0, &mut pointer) >= 0);
            check_function_table(&layout, pointer).unwrap();
            let table = table_slice(pointer, module).unwrap();
            let before = table.to_vec();
            crate::native_handles::smoke_local(table, base);
            assert_eq!(table, before);
            drop(core);
            CoUninitialize();
        }
    }

    #[test]
    fn game_language_follows_the_viewer_region() {
        assert_eq!(pol_language(0, 1), 0, "Japanese Viewer: Japanese");
        assert_eq!(pol_language(1, 0), 1, "North American Viewer: English, whatever the registry says");
        assert_eq!(pol_language(2, 3), 3, "European Viewer: its registry language");
        assert_eq!(pol_language(2, -1), 1);
    }

    #[test]
    fn the_vtable_indices_called_here_are_the_ones_the_core_check_covers() {
        let checked: Vec<usize> = crate::pol_core::COM_METHODS.iter().map(|m| m.0).collect();
        for index in [0, VT_RELEASE, VT_SET_PARAM_INIT, VT_GET_COMMON_FUNCTION_TABLE, VT_POL_VIEWER_EXEC, VT_SET_AREA_CODE] {
            assert!(checked.contains(&index), "vtable {index} is called but not checked");
        }
        assert!(VT_SET_AREA_CODE < crate::pol_core::COM_VTABLE_WORDS);
        assert!(crate::pol_core::service(SLOT_EXIT_CODE).is_some() && crate::pol_core::service(SLOT_EXIT_MESSAGE).is_some());
    }

    #[test]
    fn native_return_evidence_requires_success_and_no_error_message() {
        assert!(successful_native_return(0, false, ""));
        assert!(!successful_native_return(1, false, ""));
        assert!(!successful_native_return(-1, false, ""));
        assert!(!successful_native_return(0, true, ""));
        assert!(!successful_native_return(0, false, "native session failed"));
    }

    #[test]
    fn only_clean_native_game_returns_can_reuse_the_signed_in_owner() {
        for code in [0, -14, -1, 19] {
            for returned in [false, true] {
                for released in [false, true] {
                    let outcome = GameOutcome { code, message: String::new(), game_returned: returned, modules_released: released };
                    // A game whose DLLs stayed loaded ends its host: no game runs on its memory.
                    assert_eq!(retain_native_session(true, &outcome), returned && released && matches!(code, 0 | -14));
                    assert!(!retain_native_session(false, &outcome));
                }
            }
        }
    }

    #[test]
    fn explicit_pol_installation_requires_its_core_and_does_not_fall_back() {
        let root = std::env::temp_dir().join(format!("crystal-pol-path-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let selected = root.join("selected");
        let registered = root.join("registered");
        for path in [&selected, &registered] {
            std::fs::create_dir_all(path.join("viewer/com")).unwrap();
            std::fs::write(path.join("viewer/com/polcore.dll"), b"fixture").unwrap();
        }
        let chosen = selected.to_str().unwrap();
        let global = registered.to_str().unwrap();
        assert_eq!(viewer_directory(Some(chosen), Some(global), Region::Us).unwrap(), selected);
        assert_eq!(viewer_directory(None, Some(global), Region::Us).unwrap(), registered);
        assert!(viewer_directory(Some("relative"), Some(global), Region::Us).is_err());
        assert!(viewer_directory(None, None, Region::Us).is_err());
        assert!(viewer_directory(Some(chosen), Some(global), Region::Eu).is_err());
        assert_eq!(ansi_to_string(&ansi_path(&selected).unwrap()), chosen);
        std::fs::remove_file(selected.join("viewer/com/polcore.dll")).unwrap();
        assert!(viewer_directory(Some(chosen), Some(global), Region::Us).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_installation_never_falls_back_to_the_registered_game() {
        let root = std::env::temp_dir().join(format!("crystal-game-path-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let selected = root.join("selected");
        let registered = root.join("registered");
        for path in [&selected, &registered] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("FFXi.dll"), b"fixture").unwrap();
            std::fs::write(path.join("FFXiMain.dll"), b"fixture").unwrap();
        }
        let chosen = selected.to_str().unwrap();
        let global = registered.to_str().unwrap();
        assert_eq!(game_directory(Some(chosen), Some(global)).unwrap(), selected);
        assert_eq!(game_directory(None, Some(global)).unwrap(), registered);
        assert!(!same_dll_path(&selected.join("FFXi.dll"), &registered.join("FFXi.dll")));
        assert!(same_dll_path(&selected.join("FFXi.dll"), &selected.join(".\\FFXi.dll")));
        std::fs::remove_file(selected.join("FFXi.dll")).unwrap();
        assert!(game_directory(Some(chosen), Some(global)).is_err());
        assert!(game_directory(Some("relative"), Some(global)).is_err());
        assert!(game_directory(None, None).is_err());
        assert_eq!(game_dll_for(&registered, &CLSID_GAME_MAIN).unwrap().0, registered.join("FFXiMain.dll"));
        assert_eq!(game_dll_for(&registered, &CLSID_FILE_MANAGER).unwrap().0, registered.join("FFXi.dll"));
        assert!(game_dll_for(&registered, &IID_ICLASSFACTORY).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
