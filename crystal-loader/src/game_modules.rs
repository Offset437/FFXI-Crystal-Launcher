//! FFXi.dll and FFXiMain.dll live for one game only, as in stock POL.
//!
//! pol.exe releases the FFXiEntry object after GameStart returns and then
//! calls CoFreeUnusedLibraries (pol.exe 414412-414456, pol-spec 03 §3.1/§3.4).
//! Both DLLs are single-threaded-apartment COM servers whose DllCanUnloadNow
//! only tests their object count (FFXiMain 1000f780, FFXi.dll 100033a0), so a
//! stock Viewer starts every game on freshly mapped game DLLs. Crystal creates
//! the game classes from the selected DLL with LoadLibraryEx, which COM can't
//! free, so it releases its own references here instead.
//!
//! A game must never run on an earlier game's memory. After a game,
//! [`release_after_game`] unloads both DLLs only when nothing can still use
//! them: no thread of this process started inside them, every object is gone
//! (DllCanUnloadNow), and no Crystal lease holds a game import. Then it checks
//! that both images are really unmapped. Anything else keeps them and reports
//! why, and the caller ends the host. [`refuse_reuse`] is the hard guard
//! before every GameStart: a game DLL that is still mapped stops the game.
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, FreeLibrary, HMODULE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Com::CoFreeUnusedLibraries;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetProcessIdOfThread, OpenThread, THREAD_QUERY_INFORMATION};

use crate::log;

/// FFXiMain first: FFXi.dll created it, so FFXi.dll is released after it.
pub(crate) const GAME_MODULES: [&str; 2] = ["FFXiMain.dll", "FFXi.dll"];

/// One entry per module base: the LoadLibrary references Crystal owns.
static HELD: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<(usize, usize)>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

use crate::win::{module_base as base, wide};

/// Is `file`'s name one of the game DLLs?
pub(crate) fn is_game_module(file: &std::path::Path) -> bool {
    file.file_name().and_then(|n| n.to_str())
        .is_some_and(|n| GAME_MODULES.iter().any(|m| m.eq_ignore_ascii_case(n)))
}

/// Records one successful LoadLibrary of a game DLL that Crystal owns until
/// [`release_after_game`].
pub(crate) fn note_reference(module: HMODULE) {
    let module = module as usize;
    if module == 0 { return; }
    let mut held = held();
    match held.iter_mut().find(|(base, _)| *base == module) {
        Some((_, count)) => *count += 1,
        None => held.push((module, 1)),
    }
}

/// The hard guard before every GameStart: no game DLL may still be mapped,
/// because a mapping that outlived its game still holds that game's state.
pub(crate) fn refuse_reuse() -> Result<(), String> {
    refuse(&GAME_MODULES)
}

fn refuse(names: &[&str]) -> Result<(), String> {
    for name in names {
        let base = base(name);
        if base != 0 {
            log!("error: {name} is still loaded at {base:#x} from an earlier game in this host; GameStart refused");
            return Err(format!("{name} is still loaded from an earlier game in this game host, so another game was not started on its memory. Play again to start a new host."));
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Release {
    /// Neither game DLL is mapped any more; the next game maps fresh copies.
    Unloaded,
    /// A game DLL stays mapped, for this reason. This host may not run another game.
    Kept(String),
}

/// After GameStart returned and FFXiEntry was released: unload the game DLLs
/// like stock pol.exe, then forget every address Crystal kept in them.
pub(crate) fn release_after_game() -> Release {
    if crate::auto_login::keyboard_leased() {
        return Release::Kept("a game input hook is still leased".into());
    }
    let result = release(&GAME_MODULES);
    if result == Release::Unloaded {
        crate::ffxi_settings::module_unloaded();
        crate::game_layout::forget();
        crate::auto_login::forget_keyboard_import();
    }
    result
}

fn release(names: &[&str]) -> Release {
    let mut held = held();
    let mapped: Vec<(&str, usize)> = names.iter().map(|n| (*n, base(n))).filter(|(_, b)| *b != 0).collect();
    // Check everything before changing anything: a half-released pair is never left.
    for &(name, base) in &mapped {
        let Some(size) = image_size(base) else { return Release::Kept(format!("{name}'s image could not be read")) };
        match threads_started_in(base, size) {
            None => return Release::Kept(format!("the threads of this process could not be checked against {name}")),
            Some(0) => {}
            Some(n) => return Release::Kept(format!("{n} thread(s) started inside {name} are still running")),
        }
        match can_unload_now(base) {
            Some(true) => {}
            Some(false) => return Release::Kept(format!("{name} still has live objects (DllCanUnloadNow)")),
            None => return Release::Kept(format!("{name} has no DllCanUnloadNow")),
        }
    }
    for &(_, base) in &mapped {
        let count = held.iter().find(|(b, _)| *b == base).map_or(0, |(_, c)| *c);
        for _ in 0..count {
            // SAFETY: one LoadLibrary reference Crystal took and still owns.
            unsafe { FreeLibrary(base as HMODULE) };
        }
        held.retain(|(b, _)| *b != base);
    }
    // Stock's own call: frees any copy COM loaded on this apartment.
    // SAFETY: plain COM call on the apartment thread.
    unsafe { CoFreeUnusedLibraries() };
    for name in names {
        let base = base(name);
        if base != 0 {
            return Release::Kept(format!("{name} stayed loaded at {base:#x} after Crystal released its references; another module holds it"));
        }
    }
    Release::Unloaded
}

/// SizeOfImage from the mapped PE headers.
fn image_size(base: usize) -> Option<usize> {
    crate::pe::identity(base).map(|image| image.size)
}


/// DllCanUnloadNow() == S_OK. None: the module exports none.
fn can_unload_now(base: usize) -> Option<bool> {
    // SAFETY: a loaded module; the export has the documented COM signature.
    unsafe {
        let f = GetProcAddress(base as HMODULE, c"DllCanUnloadNow".as_ptr().cast())?;
        let f: unsafe extern "system" fn() -> i32 = std::mem::transmute(f);
        Some(f() == 0)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Owner { Ours, Foreign, Unknown }

/// Who owns an opened thread, from GetProcessIdOfThread (0: failed).
fn owner(thread_pid: u32, pid: u32) -> Owner {
    match thread_pid {
        0 => Owner::Unknown,
        p if p == pid => Owner::Ours,
        _ => Owner::Foreign,
    }
}

/// How many threads of this process (other than this one) have their start
/// address inside [base, base + size). None when that can't be established.
fn threads_started_in(base: usize, size: usize) -> Option<usize> {
    type Query = unsafe extern "system" fn(isize, i32, *mut usize, u32, *mut u32) -> i32;
    const THREAD_QUERY_SET_WIN32_START_ADDRESS: i32 = 9;
    let ntdll = wide("ntdll.dll");
    // SAFETY: ntdll is always loaded; NtQueryInformationThread has this signature.
    let query: Query = unsafe {
        let module = GetModuleHandleW(ntdll.as_ptr());
        std::mem::transmute(GetProcAddress(module, c"NtQueryInformationThread".as_ptr().cast())?)
    };
    let pid = unsafe { GetCurrentProcessId() };
    let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
    // SAFETY: snapshot handle checked and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() { return None; }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut count = 0;
    let mut complete = true;
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid && entry.th32ThreadID != me {
            // SAFETY: handle checked and closed; the query writes one pointer.
            let thread = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, entry.th32ThreadID) };
            if thread.is_null() {
                // ERROR_INVALID_PARAMETER: it exited after the snapshot and runs nothing.
                if unsafe { windows_sys::Win32::Foundation::GetLastError() } != 87 { complete = false; }
            } else {
                // The thread may have exited after the snapshot and its ID been
                // reused by another process (a sibling host maps FFXiMain at the
                // same base): only a thread still owned by this process counts.
                match owner(unsafe { GetProcessIdOfThread(thread) }, pid) {
                    Owner::Foreign => {}
                    Owner::Unknown => complete = false,
                    Owner::Ours => {
                        let mut start = 0usize;
                        let status = unsafe { query(thread as isize, THREAD_QUERY_SET_WIN32_START_ADDRESS, &mut start,
                            std::mem::size_of::<usize>() as u32, std::ptr::null_mut()) };
                        if status < 0 { complete = false; }
                        else if start >= base && start - base < size { count += 1; }
                    }
                }
                unsafe { CloseHandle(thread) };
            }
        }
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    complete.then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};

    static SERIAL: Mutex<()> = Mutex::new(());

    /// A private copy of a small 32/64-bit system COM server (matching this
    /// test binary) under a unique name, so it maps as its own module.
    fn test_module(tag: &str) -> (PathBuf, String) {
        let mut buffer = vec![0u16; 1024];
        let n = unsafe { windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        let system = PathBuf::from(String::from_utf16_lossy(&buffer[..n]));
        let name = format!("CrystalGameModule{tag}{}.dll", std::process::id());
        let dir = std::env::temp_dir().join(format!("crystal-game-modules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(&name);
        if !path.exists() { std::fs::copy(system.join("scrrun.dll"), &path).unwrap(); }
        (path, name)
    }

    fn load(path: &std::path::Path) -> HMODULE {
        let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
        module
    }

    /// The first byte after a writable section's used size: zero in a fresh
    /// mapping, and never used by the module's own code.
    fn spare_data_byte(base: usize) -> usize {
        let mut header = [0u8; 0x1000];
        assert!(crate::mem::read_into(base, &mut header));
        let nt = u32::from_le_bytes(header[0x3C..0x40].try_into().unwrap()) as usize;
        let sections = u16::from_le_bytes(header[nt + 6..nt + 8].try_into().unwrap()) as usize;
        let optional = u16::from_le_bytes(header[nt + 20..nt + 22].try_into().unwrap()) as usize;
        let first = nt + 24 + optional;
        for i in 0..sections {
            let s = &header[first + i * 40..first + (i + 1) * 40];
            let size = u32::from_le_bytes(s[8..12].try_into().unwrap()) as usize;
            let rva = u32::from_le_bytes(s[12..16].try_into().unwrap()) as usize;
            let flags = u32::from_le_bytes(s[36..40].try_into().unwrap());
            if flags & 0x8000_0000 != 0 && !size.is_multiple_of(0x1000) { return base + rva + size; }
        }
        panic!("no writable section with spare bytes");
    }

    #[test]
    fn a_released_game_module_maps_fresh_state_and_the_guard_refuses_a_live_one() {
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (path, name) = test_module("Fresh");
        let names = [name.as_str()];
        assert_eq!(refuse(&names), Ok(()));
        let first = load(&path);
        note_reference(first);
        note_reference(load(&path)); // a second Crystal reference, as from a second class
        let marker = spare_data_byte(first as usize);
        unsafe { *(marker as *mut u8) = 0xA5 }; // this game's state
        assert!(refuse(&names).unwrap_err().contains("earlier game"), "a live mapping is refused");
        assert_eq!(release(&names), Release::Unloaded);
        assert_eq!(base(&name), 0, "unmapped");
        assert_eq!(refuse(&names), Ok(()));
        let second = load(&path);
        note_reference(second);
        let marker = spare_data_byte(second as usize);
        assert_eq!(unsafe { *(marker as *const u8) }, 0, "the next game starts from a fresh image");
        assert_eq!(release(&names), Release::Unloaded);
    }

    #[test]
    fn a_module_someone_else_holds_is_kept_and_stays_refused() {
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (path, name) = test_module("Pinned");
        let names = [name.as_str()];
        note_reference(load(&path));
        let foreign = load(&path); // an injector's own reference
        match release(&names) {
            Release::Kept(reason) => assert!(reason.contains("another module holds it"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(refuse(&names).is_err());
        unsafe { FreeLibrary(foreign) };
        assert_eq!(base(&name), 0);
        assert_eq!(refuse(&names), Ok(()));
    }

    #[test]
    fn a_thread_started_inside_a_game_module_keeps_it_mapped() {
        use windows_sys::Win32::System::Threading::{CreateThread, TerminateThread, WaitForSingleObject, CREATE_SUSPENDED, INFINITE};
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (path, name) = test_module("Thread");
        let names = [name.as_str()];
        let module = load(&path);
        note_reference(module);
        let start = unsafe { GetProcAddress(module, c"DllCanUnloadNow".as_ptr().cast()) }.unwrap();
        let thread = unsafe { CreateThread(std::ptr::null(), 0, Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn(*mut std::ffi::c_void) -> u32>(start)), std::ptr::null(), CREATE_SUSPENDED, std::ptr::null_mut()) };
        assert!(!thread.is_null());
        match release(&names) {
            Release::Kept(reason) => assert!(reason.contains("thread"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert_ne!(base(&name), 0, "nothing was released");
        unsafe { TerminateThread(thread, 0); WaitForSingleObject(thread, INFINITE); CloseHandle(thread); }
        assert_eq!(release(&names), Release::Unloaded);
    }

    /// The real game DLLs, as copies under their own names: after their attach
    /// code ran (CRT and ATL init, as an image dumper does) and Crystal's
    /// references are released, both unmap, and a reload starts fresh.
    #[test]
    #[ignore = "loads copies of FFXiMain.dll/FFXi.dll from CRYSTAL_FFXIMAIN_BUILDS; never the installed game"]
    fn copies_of_the_game_dlls_unload_after_release_and_reload_fresh() {
        let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(builds) = std::env::var_os("CRYSTAL_FFXIMAIN_BUILDS").map(PathBuf::from) else { return; };
        let dir = std::env::temp_dir().join(format!("crystal-game-dlls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.to_string_lossy().to_ascii_lowercase();
        assert!(!text.contains("squareenix") && !text.contains("playonline"), "never load the installed game");
        for (from, to) in [("FFXiMain-6AC37846.dll", "FFXiMain.dll"), ("FFXi-6A995417.dll", "FFXi.dll")] {
            std::fs::copy(builds.join(from), dir.join(to)).unwrap();
        }
        assert_eq!(refuse_reuse(), Ok(()), "no game DLL in this test process yet");
        let mut markers = Vec::new();
        for name in GAME_MODULES {
            let module = load(&dir.join(name));
            note_reference(module);
            let marker = spare_data_byte(module as usize);
            unsafe { *(marker as *mut u8) = 0x5A };
            markers.push(name);
        }
        assert!(refuse_reuse().is_err());
        assert_eq!(release(&GAME_MODULES), Release::Unloaded);
        assert_eq!(refuse_reuse(), Ok(()));
        for name in markers {
            let module = load(&dir.join(name));
            note_reference(module);
            assert_eq!(unsafe { *(spare_data_byte(module as usize) as *const u8) }, 0, "{name} maps fresh");
        }
        assert_eq!(release(&GAME_MODULES), Release::Unloaded);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_reused_thread_id_in_another_process_is_not_counted() {
        assert_eq!(owner(42, 42), Owner::Ours);
        assert_eq!(owner(43, 42), Owner::Foreign, "the ID now names another process's thread");
        assert_eq!(owner(0, 42), Owner::Unknown);
        // This process's own threads are still found by the real walk.
        let main = base("ntdll.dll");
        assert!(threads_started_in(main, image_size(main).unwrap()).is_some());
    }

    #[test]
    fn only_the_game_dlls_are_counted() {
        assert!(is_game_module(std::path::Path::new(r"C:\x\FFXiMain.dll")));
        assert!(is_game_module(std::path::Path::new(r"C:\x\ffxi.dll")));
        assert!(!is_game_module(std::path::Path::new(r"C:\x\polcore.dll")));
    }
}
