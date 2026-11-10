//! Where this process's Windower Hook.dll keeps what Crystal changes in it,
//! found by signature so a Windower update that moves code does not switch the
//! fixes off. Same machinery and rules as [`crate::game_layout`]:
//!
//! One read-only pass per process ([`hook`]): copy the loaded module (see
//! [`sigscan`]), find every locator, and resolve a group only when each of its
//! locators matches exactly once and every cross-check agrees (call targets,
//! operands that two sites share, import names from the import directory).
//! Known builds keep a table of proven values; the scan must reproduce it or
//! the group is refused. Unknown builds use the scan alone. Writers re-check
//! the exact bytes they depend on in live memory before changing anything,
//! and switch themselves off with the logged reason on any doubt.
//!
//! Hook's private-startup names are resolved earlier, from the file, before
//! Hook is loaded: see `windower_bootstrap_contract::hook_startup`.

use crate::{
    game_layout::{agree, expect, Ctx, Found},
    sigscan::{CaptureError, Image},
};
use std::{
    fmt::Debug,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Hook's renderer shutdown: its queue worker is a global the owner wakes
/// before its only join (every supported build). Resolved as a canary only:
/// nothing in Hook is changed for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Exit {
    pub join: usize,
    /// The owner destructor's join, right after its SetEvent.
    pub owner_join: usize,
    /// The worker's wait on the event.
    pub wait: usize,
    pub event: usize,
    pub stop: usize,
    pub thread: usize,
}

/// Hook's plugin loader: `GetProcAddress(plugin, "CreateInstance")` through
/// its IAT slot, the factory call, and the `Initialize` vtable call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plugins {
    pub get_proc_iat: usize,
    /// `push "CreateInstance"; push edi; call [GetProcAddress]`.
    pub factory_lookup: usize,
    pub factory_name: usize,
    /// `push edx; push ecx; mov eax,[ecx]; call [eax+8]`.
    pub initialize_call: usize,
}

pub(crate) struct Known {
    pub label: &'static str,
    pub timestamp: u32,
    pub size: usize,
    pub exit: Exit,
    pub plugins: Option<Plugins>,
}

/// Hook.dll 4.7.9.6 (linked 2026-10-06 13:00:13 UTC, 1,790,976 bytes).
pub(crate) const BUILD_6AC4F0DD: Known = Known {
    label: "4.7.9.6",
    timestamp: 0x6AC4_F0DD,
    size: 0x23_0000,
    exit: Exit {
        join: 0x3_5B00,
        owner_join: 0x6_EB5C,
        wait: 0x6_D00A,
        event: 0x1C_DDD0,
        stop: 0x1A_13C4,
        thread: 0x1C_DDC8,
    },
    plugins: Some(Plugins {
        get_proc_iat: 0x14_111C,
        factory_lookup: 0x6_F605,
        factory_name: 0x18_5BE0,
        initialize_call: 0x6_F9D0,
    }),
};

/// Hook.dll 4.7.9.3 (linked 2026-07-18): 4.7.9.6's code 0x20 earlier.
pub(crate) const BUILD_6A5B1A74: Known = Known {
    label: "4.7.9.3",
    timestamp: 0x6A5B_1A74,
    size: 0x23_0000,
    exit: Exit {
        join: 0x3_5B00,
        owner_join: 0x6_EB3C,
        wait: 0x6_CFEA,
        event: 0x1C_DDD0,
        stop: 0x1A_13C4,
        thread: 0x1C_DDC8,
    },
    plugins: Some(Plugins {
        get_proc_iat: 0x14_111C,
        factory_lookup: 0x6_F5E5,
        factory_name: 0x18_5BD0,
        initialize_call: 0x6_F9B0,
    }),
};

pub(crate) const KNOWN: [&Known; 2] = [&BUILD_6AC4F0DD, &BUILD_6A5B1A74];

const KERNEL32: &str = "KERNEL32.dll";

/// Every scanned locator; each must match exactly once where it is required.
const PATTERNS: &[(&str, &str)] = &[
    // 4.7.9.x (6eb3d, 6d00a, 2edf0, 2ee10, 44923): the owner destructor pushes
    // [EVENT], clears [STOP], SetEvent, mov ecx,THREAD; call join. The worker
    // waits on [EVENT] and leaves when [STOP] is clear. The event is created
    // auto-reset, the thread constructed on its routine. The console command
    // thread is the only other joiner.
    ("exit.owner_dtor", "FF 35 ?? ?? ?? ?? C7 46 08 ?? ?? ?? ?? C6 05 ?? ?? ?? ?? 00 FF 15 ?? ?? ?? ?? B9 ?? ?? ?? ?? E8 ?? ?? ?? ??"),
    ("exit.global_wait", "6A FF FF 35 ?? ?? ?? ?? FF 15 ?? ?? ?? ?? 80 3D ?? ?? ?? ?? 00 74 ??"),
    ("exit.event_create", "6A 00 6A 00 6A 00 6A 00 FF 15 ?? ?? ?? ?? A3 ?? ?? ?? ?? C3"),
    ("exit.thread_ctor", "68 ?? ?? ?? ?? B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 59 C3"),
    ("exit.console_stop", "8B 47 04 80 78 04 00 74 ?? 8D 8F ?? ?? 00 00 C6 40 04 00 E8 ?? ?? ?? ??"),
    // Plugin loader (6f605, 6f9c4 in 4.7.9.6).
    ("plugin.factory", "68 ?? ?? ?? ?? 57 FF 15 ?? ?? ?? ?? 85 C0 75 ?? 68 ?? ?? ?? ?? EB ?? FF D0"),
    ("plugin.initialize", "8B 8D ?? ?? ?? ?? 8B 95 ?? ?? ?? ?? 52 51 8B 01 FF 50 08"),
];

/// MSVC `std::thread::join`, whole body: joinable? self-join? `_Thrd_join`,
/// clear, else throw. Checked where a locator's call lands.
const JOIN_BODY: &str = "55 8B EC 83 E4 F8 56 57 8B F9 83 7F 04 00 74 ?? 8B 77 04 E8 ?? ?? ?? ?? 3B F0 74 ?? \
    6A 00 FF 77 04 FF 37 E8 ?? ?? ?? ?? 83 C4 0C 85 C0 75 ?? 89 07 89 47 04 5F 5E 8B E5 5D C3 \
    6A 01 E8 ?? ?? ?? ?? 6A 05 E8 ?? ?? ?? ?? 6A 02 E8 ?? ?? ?? ??";
/// The worker routine's wait lies this close after its start.
const ROUTINE_SPAN: usize = 0x1000;
/// The Initialize call follows the factory lookup this closely (0x3cb today).
const LOADER_SPAN: usize = 0x1000;
/// See game_layout: a relocated module has hundreds of `call [IAT]` sites.
const MIN_IMPORT_CALLS: usize = 16;

fn imports(c: &Ctx, rva: usize, function: &str) -> Found<()> {
    c.image.imports(rva, KERNEL32, function).then_some(()).ok_or_else(|| format!("{rva:#x} does not call {function}"))
}

/// `rva` is `std::thread::join`: its body, its `_Thrd_id` thunk to
/// GetCurrentThreadId and its `_Thrd_join` waiting with WaitForSingleObjectEx.
fn is_join(c: &Ctx, rva: usize) -> Found<()> {
    c.shape(rva, "std::thread::join", JOIN_BODY)?;
    let id = c.call(rva + 0x13)?;
    expect("_Thrd_id thunk", c.word(id)?, 0x25FF)?;
    imports(c, id + 2, "GetCurrentThreadId")?;
    let thrd_join = c.call(rva + 0x23)?;
    let body = c.image.bytes(thrd_join, 0x20).ok_or("_Thrd_join is unreadable")?;
    let at = body.windows(2).position(|w| w == [0xFF, 0x15]).ok_or("_Thrd_join has no import call")?;
    imports(c, thrd_join + at + 2, "WaitForSingleObjectEx")
}


fn exit(c: &Ctx) -> Found<Exit> {
    let owner = c.unique("exit.owner_dtor")?;
    let wait = c.unique("exit.global_wait")?;
    let create = c.unique("exit.event_create")?;
    let ctor = c.unique("exit.thread_ctor")?;
    let event = agree("EVENT", &[c.abs(owner + 2)?, c.abs(wait + 4)?, c.abs(create + 15)?])?;
    let stop = agree("STOP", &[c.abs(owner + 15)?, c.abs(wait + 16)?])?;
    let thread = agree("THREAD", &[c.abs(owner + 27)?, c.abs(ctor + 6)?])?;
    c.global("EVENT", event)?;
    c.global("THREAD", thread)?;
    c.image.inside(".data", stop, 1).then_some(()).ok_or("STOP is not a .data global")?;
    imports(c, owner + 22, "SetEvent")?;
    imports(c, wait + 10, "WaitForSingleObject")?;
    if !(c.image.imports(create + 10, KERNEL32, "CreateEventA") || c.image.imports(create + 10, KERNEL32, "CreateEventW")) {
        return Err("the worker event is not created by CreateEvent".into());
    }
    // The wait is inside the routine the thread is constructed on.
    let routine = c.abs(ctor + 1)?;
    let span = c.image.bytes(routine, wait.saturating_sub(routine)).filter(|_| routine < wait && wait < routine + ROUTINE_SPAN);
    if span.is_none_or(|code| code.windows(3).any(|w| w == [0xCC; 3])) {
        return Err("the worker wait is not inside the thread routine".into());
    }
    let owner_join = owner + 31;
    let join = c.call(owner_join)?;
    is_join(c, join)?;
    // Every direct join is the woken owner join or the console thread stop;
    // any other is a join this module cannot vouch for.
    let console: Vec<usize> = c.hits("exit.console_stop").iter().map(|at| at + 19).collect();
    if let Some(other) = c.image.callers(join).into_iter().find(|at| *at != owner_join && !console.contains(at)) {
        return Err(format!("unclassified std::thread::join call at {other:#x}"));
    }
    Ok(Exit { join, owner_join, wait, event, stop, thread })
}

fn plugins(c: &Ctx) -> Found<Plugins> {
    let lookup = c.unique("plugin.factory")?;
    c.string(lookup + 1, "CreateInstance")?;
    let name = c.abs(lookup + 1)?;
    imports(c, lookup + 8, "GetProcAddress")?;
    let get_proc_iat = c.abs(lookup + 8)?;
    let initialize_call = c.unique("plugin.initialize")? + 12;
    (lookup < initialize_call && initialize_call < lookup + LOADER_SPAN).then_some(())
        .ok_or("the plugin Initialize call is not in the plugin loader")?;
    Ok(Plugins { get_proc_iat, factory_lookup: lookup, factory_name: name, initialize_call })
}

#[derive(Debug)]
pub(crate) struct Resolution {
    pub base: usize,
    pub timestamp: u32,
    pub size: usize,
    pub known: Option<&'static str>,
    pub exit: Found<Exit>,
    pub plugins: Found<Plugins>,
    /// How often each locator matched (empty when nothing was scanned), so a
    /// refused group's log names the locator that did not match exactly once.
    pub hits: Vec<(&'static str, usize)>,
}

impl Resolution {
    fn unscanned(base: usize, timestamp: u32, size: usize, reason: String) -> Self {
        Self { base, timestamp, size, known: None, exit: Err(reason.clone()), plugins: Err(reason), hits: Vec::new() }
    }
}

fn checked<T: Copy + PartialEq + Debug>(what: &str, scanned: Found<T>, table: Option<&T>) -> Found<T> {
    match (scanned?, table) {
        (value, Some(known)) if value != *known => {
            Err(format!("{what}: scan {value:x?} disagrees with this build's known values {known:x?}"))
        }
        (value, _) => Ok(value),
    }
}

pub(crate) fn resolve(image: &Image) -> Resolution {
    let known = KNOWN.iter().copied().find(|k| k.timestamp == image.headers.timestamp && k.size == image.headers.size);
    let (relocated, stale) = image.import_calls();
    let (exit_found, plugins_found, hits) = if relocated < MIN_IMPORT_CALLS || stale > 0 {
        const REASON: &str = "the module's code is not fully relocated for its load address";
        (Err(REASON.to_string()), Err(REASON.to_string()), Vec::new())
    } else {
        let c = Ctx::new(image, PATTERNS);
        let hits = PATTERNS.iter().map(|(name, _)| (*name, c.hits(name).len())).collect();
        (exit(&c), plugins(&c), hits)
    };
    Resolution {
        base: image.base,
        timestamp: image.headers.timestamp,
        size: image.headers.size,
        known: known.map(|k| k.label),
        exit: checked("renderer shutdown", exit_found, known.map(|k| &k.exit)),
        plugins: checked("plugin loader", plugins_found, known.and_then(|k| k.plugins.as_ref())),
        hits,
    }
}

fn report(r: &Resolution, elapsed: Duration) {
    log!("hook layout: Hook.dll {:08X} size {:#x} at {:#x}, {}, resolved in {:.1} ms", r.timestamp, r.size, r.base,
        r.known.map_or("unknown build".to_string(), |label| format!("known build {label}")), elapsed.as_secs_f64() * 1000.0);
    let source = if r.known.is_some() { "signature scan, matches the known-build table" } else { "signature scan" };
    match &r.exit {
        Ok(_) => log!("hook layout: renderer shutdown wakes its own worker ({source})"),
        Err(reason) => log!("hook layout: renderer shutdown unavailable: {reason}"),
    }
    match &r.plugins {
        Ok(_) => log!("hook layout: plugin loader resolved ({source})"),
        Err(reason) => log!("hook layout: plugin loader unavailable: {reason}"),
    }
    // A refused group: every locator's match count, so the one that moved is
    // named even when the reason is a later cross-check.
    if (r.exit.is_err() || r.plugins.is_err()) && !r.hits.is_empty() {
        let hits: Vec<String> = r.hits.iter().map(|(name, count)| format!("{name}={count}")).collect();
        log!("hook layout: locator matches (1 = found): {}", hits.join(", "));
    }
}

/// Resolves the module loaded at `base` (no cache). `NotReady`: try later.
pub(crate) fn resolve_module(base: usize) -> Result<Resolution, CaptureError> {
    let started = Instant::now();
    match Image::capture(base) {
        Ok(image) => {
            let resolution = resolve(&image);
            report(&resolution, started.elapsed());
            Ok(resolution)
        }
        Err(CaptureError::NotReady) => Err(CaptureError::NotReady),
        Err(CaptureError::Invalid(reason)) => {
            log!("hook layout: Hook.dll at {base:#x} is unsupported: {reason}");
            Ok(Resolution::unscanned(base, 0, 0, format!("unsupported module: {reason}")))
        }
    }
}

/// One module reference, released on drop (0: none, for tests).
pub(crate) struct OwnedModule(pub(crate) usize);
impl OwnedModule {
    /// A reference on the module whose image starts exactly at `base`.
    pub(crate) fn retain_at(base: usize) -> Option<Self> {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS};
        let mut module = std::ptr::null_mut();
        // SAFETY: the address is only used to find the module containing it.
        let ok = unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, base as *const u16, &mut module) };
        (ok != 0).then(|| Self(module as usize)).filter(|owned| owned.0 == base)
    }
}
impl Drop for OwnedModule {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: one reference this value took.
            unsafe { windows_sys::Win32::Foundation::FreeLibrary(self.0 as _) };
        }
    }
}

/// `GetModuleHandleExW("Hook.dll")`. Never look Hook up with
/// `GetModuleHandleW`: once Hook has started, it detours GetModuleHandleA/W
/// (and Module32First/Next) to hide itself, so that lookup returns NULL in a
/// live Windower process.
fn hook_module(flags: u32) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleExW;
    let name: Vec<u16> = "Hook.dll\0".encode_utf16().collect();
    let mut module = std::ptr::null_mut();
    // SAFETY: NUL-terminated name.
    let ok = unsafe { GetModuleHandleExW(flags, name.as_ptr(), &mut module) };
    (ok != 0 && !module.is_null()).then_some(module as usize)
}

/// This process's Hook.dll base, without a reference (see [`hook_module`]).
pub(crate) fn hook_base() -> Option<usize> {
    hook_module(windows_sys::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT)
}

/// This process's Hook.dll, kept loaded while the value lives.
pub(crate) fn retain_hook() -> Option<OwnedModule> {
    hook_module(0).map(OwnedModule)
}

static RESOLVED: OnceLock<Resolution> = OnceLock::new();
static RESOLVING: Mutex<()> = Mutex::new(());

/// The layout of the Hook.dll loaded at `base`, resolved once and cached.
///
/// `base` comes from [`hook_base`] or [`retain_hook`] (the latter also keeps
/// Hook loaded while the caller holds it). Hook is loaded and initialized before its callers
/// run, so a busy loader (another thread loading a DLL) is waited out briefly.
/// `None`, always logged: no base, another Hook was resolved earlier, or the
/// loader stayed busy; callers then leave Hook unchanged.
pub(crate) fn hook(base: usize) -> Option<&'static Resolution> {
    if base == 0 {
        log!("hook layout: no Hook.dll base to resolve");
        return None;
    }
    let current = |r: &'static Resolution| {
        if r.base != base {
            log!("hook layout: Hook.dll at {base:#x} is not the one resolved at {:#x}; its fixes are off", r.base);
        }
        (r.base == base).then_some(r)
    };
    let _one = RESOLVING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = RESOLVED.get() {
        return current(r);
    }
    for _ in 0..100 {
        match resolve_module(base) {
            Ok(resolution) => return current(RESOLVED.get_or_init(|| resolution)),
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    log!("hook layout: Hook.dll stayed busy in the loader; its fixes are off for this launch");
    None
}

#[cfg(test)]
pub(crate) mod tests;
