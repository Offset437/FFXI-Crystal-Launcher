//! Credential-free process probe. Never creates a POL core or starts FFXI.
//!
//! `windower-bootstrap-probe <Hook.dll> <startup record> <24-hex nonce> [trace]`
//! initializes Windower's Hook privately, exactly as a game host does, and
//! prints the time. With `trace`, it also times Hook's own calls to the
//! system functions that can explain its readiness wait (thread and module
//! snapshots, thread suspension, sleeps and waits). Point the record's
//! Windower folder at a copy of Windower, never at the installed one.
use std::{
    ffi::c_void,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError},
    System::Memory::{VirtualProtect, PAGE_READWRITE},
};

fn main() {
    crystal_loader::log::init();
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 && !(args.len() == 5 && args[4] == "trace") {
        std::process::exit(2);
    }
    let hook = PathBuf::from(&args[1]);
    let bytes = std::fs::read(&args[2]).expect("probe settings fixture");
    let trace = args.len() == 5;
    let started = Instant::now();
    let _ = START.set(started);
    let result = crystal_loader::windower_parallel::initialize_with(&hook, &bytes, &args[3], |base| {
        if trace { unsafe { install(base) }; }
    });
    match &result {
        Ok(time) => println!("PRIVATE_STARTUP_OK elapsed_ms={}", time.as_millis()),
        Err(e) => println!("PRIVATE_STARTUP_FAILED {e}"),
    }
    if trace { report(); }
    let _ = std::io::stdout().flush();
    // End this disposable probe without asking vendor background workers to
    // tear down a game that the probe deliberately never created.
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            if result.is_ok() { 0 } else { 1 },
        );
    }
}

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
const SLOTS: usize = 34;
static ORIGINAL: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static CALLS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static MICROS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static FIRST_MS: [AtomicU64; SLOTS] = [const { AtomicU64::new(u64::MAX) }; SLOTS];
static LAST_MS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static SNAPSHOT_FLAGS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static SNAPSHOT_MICROS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static SLEEP_MS: AtomicU64 = AtomicU64::new(0);

fn since_start() -> u64 {
    START.get().map_or(0, |s| s.elapsed().as_millis() as u64)
}
fn record(index: usize, began: Instant) -> u64 {
    let micros = began.elapsed().as_micros() as u64;
    CALLS[index].fetch_add(1, Ordering::Relaxed);
    MICROS[index].fetch_add(micros, Ordering::Relaxed);
    let at = since_start();
    FIRST_MS[index].fetch_min(at, Ordering::Relaxed);
    LAST_MS[index].fetch_max(at, Ordering::Relaxed);
    micros
}

macro_rules! traced {
    ($( $index:literal $dll:literal $name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty ; )*) => {
        mod wrappers { use super::*;
        $(
            #[allow(non_snake_case)]
            pub(super) unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
                let original: unsafe extern "system" fn($($ty),*) -> $ret =
                    unsafe { std::mem::transmute(ORIGINAL[$index].load(Ordering::Acquire)) };
                let saved = unsafe { GetLastError() };
                let began = Instant::now();
                unsafe { SetLastError(saved) };
                let result = unsafe { original($($arg),*) };
                let outgoing = unsafe { GetLastError() };
                let micros = record($index, began);
                extra($index, &[$($arg as usize),*], micros);
                unsafe { SetLastError(outgoing) };
                result
            }
        )*
        }
        fn traced() -> Vec<(usize, &'static str, &'static str, usize)> {
            vec![$(($index, $dll, stringify!($name), wrappers::$name as *const () as usize)),*]
        }
    };
}

traced! {
    0 "KERNEL32.dll" CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
    1 "KERNEL32.dll" Thread32First(snapshot: isize, entry: *mut c_void) -> i32;
    2 "KERNEL32.dll" Thread32Next(snapshot: isize, entry: *mut c_void) -> i32;
    3 "KERNEL32.dll" Module32First(snapshot: isize, entry: *mut c_void) -> i32;
    4 "KERNEL32.dll" Module32Next(snapshot: isize, entry: *mut c_void) -> i32;
    5 "KERNEL32.dll" OpenThread(access: u32, inherit: i32, id: u32) -> isize;
    6 "KERNEL32.dll" SuspendThread(thread: isize) -> u32;
    7 "KERNEL32.dll" ResumeThread(thread: isize) -> u32;
    8 "KERNEL32.dll" GetThreadContext(thread: isize, context: *mut c_void) -> i32;
    9 "KERNEL32.dll" Sleep(ms: u32) -> ();
    10 "KERNEL32.dll" WaitForSingleObject(handle: isize, ms: u32) -> u32;
    11 "KERNEL32.dll" WaitForSingleObjectEx(handle: isize, ms: u32, alertable: i32) -> u32;
    12 "KERNEL32.dll" SetEvent(handle: isize) -> i32;
    13 "KERNEL32.dll" CreateThread(attributes: *const c_void, stack: usize, start: *const c_void, parameter: *const c_void, flags: u32, id: *mut u32) -> isize;
    14 "KERNEL32.dll" VirtualProtect(address: *const c_void, size: usize, protection: u32, old: *mut u32) -> i32;
    15 "KERNEL32.dll" LoadLibraryW(name: *const u16) -> isize;
    16 "KERNEL32.dll" LoadLibraryA(name: *const u8) -> isize;
    17 "KERNEL32.dll" LoadLibraryExW(name: *const u16, file: isize, flags: u32) -> isize;
    18 "KERNEL32.dll" SleepConditionVariableSRW(variable: *mut c_void, lock: *mut c_void, ms: u32, flags: u32) -> i32;
    19 "KERNEL32.dll" CreateFileW(name: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: isize) -> isize;
    20 "KERNEL32.dll" CreateFileA(name: *const u8, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: isize) -> isize;
    21 "KERNEL32.dll" GetFileVersionInfoW(name: *const u16, handle: u32, length: u32, data: *mut c_void) -> i32;
    22 "USER32.dll" FindWindowA(class: *const u8, title: *const u8) -> isize;
    23 "USER32.dll" FindWindowW(class: *const u16, title: *const u16) -> isize;
    24 "gdiplus.dll" GdiplusStartup(token: *mut usize, input: *const c_void, output: *mut c_void) -> i32;
    25 "KERNEL32.dll" WriteFile(file: isize, data: *const c_void, length: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    26 "KERNEL32.dll" FlushFileBuffers(file: isize) -> i32;
    27 "ADVAPI32.dll" RegOpenKeyExW(key: isize, name: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
    28 "KERNEL32.dll" GetProcAddress(module: isize, name: *const u8) -> isize;
    29 "KERNEL32.dll" CreateFileMappingA(file: isize, security: *const c_void, protection: u32, high: u32, low: u32, name: *const u8) -> isize;
    30 "KERNEL32.dll" OpenFileMappingA(access: u32, inherit: i32, name: *const u8) -> isize;
    31 "KERNEL32.dll" MapViewOfFile(mapping: isize, access: u32, high: u32, low: u32, size: usize) -> *mut c_void;
    32 "KERNEL32.dll" CloseHandle(handle: isize) -> i32;
    33 "KERNEL32.dll" GetCurrentThreadId() -> u32;
}

static ORDER: [AtomicUsize; 160] = [const { AtomicUsize::new(usize::MAX) }; 160];
static ORDER_AT: AtomicUsize = AtomicUsize::new(0);
fn extra(index: usize, args: &[usize], micros: u64) {
    if !matches!(index, 2 | 33 | 28 | 15) {
        let at = ORDER_AT.fetch_add(1, Ordering::Relaxed);
        if at < ORDER.len() { ORDER[at].store(index | (unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as usize) << 8, Ordering::Relaxed); }
    }
    match index {
        0 => {
            let kind = match args[0] { 4 => 0, 8 => 1, 0x18 => 2, _ => 3 };
            SNAPSHOT_FLAGS[kind].fetch_add(1, Ordering::Relaxed);
            SNAPSHOT_MICROS[kind].fetch_add(micros, Ordering::Relaxed);
        }
        9 => { SLEEP_MS.fetch_add(args[0] as u64, Ordering::Relaxed); }
        12 => println!("TRACE SetEvent at +{} ms", since_start()),
        _ => {}
    }
}

unsafe fn read<T: Copy>(address: usize) -> T {
    unsafe { std::ptr::read_unaligned(address as *const T) }
}

/// Redirect Hook's own import slots (only those) to the timing wrappers.
unsafe fn install(base: usize) {
    let pe = base + unsafe { read::<u32>(base + 0x3c) } as usize;
    let imports = unsafe { read::<u32>(pe + 24 + 96 + 8) } as usize;
    let mut descriptor = base + imports;
    let mut patched = 0;
    loop {
        let names = unsafe { read::<u32>(descriptor) } as usize;
        let dll_rva = unsafe { read::<u32>(descriptor + 12) } as usize;
        let table = unsafe { read::<u32>(descriptor + 16) } as usize;
        if dll_rva == 0 { break; }
        let dll = unsafe { std::ffi::CStr::from_ptr((base + dll_rva) as *const i8) }.to_string_lossy().into_owned();
        let mut index = 0;
        loop {
            let thunk = unsafe { read::<u32>(base + names + index * 4) } as usize;
            if thunk == 0 { break; }
            if thunk & 0x8000_0000 == 0 {
                let name = unsafe { std::ffi::CStr::from_ptr((base + thunk + 2) as *const i8) }.to_string_lossy().into_owned();
                if let Some(&(slot, _, _, wrapper)) = traced().iter().find(|(_, d, n, _)| d.eq_ignore_ascii_case(&dll) && *n == name) {
                    let address = base + table + index * 4;
                    ORIGINAL[slot].store(unsafe { read::<usize>(address) }, Ordering::Release);
                    let mut old = 0;
                    if unsafe { VirtualProtect(address as *const c_void, 4, PAGE_READWRITE, &mut old) } != 0 {
                        unsafe { std::ptr::write_volatile(address as *mut usize, wrapper) };
                        unsafe { VirtualProtect(address as *const c_void, 4, old, &mut old) };
                        patched += 1;
                    }
                }
            }
            index += 1;
        }
        descriptor += 20;
    }
    println!("TRACE installed {patched} wrappers at +{} ms", since_start());
}

fn report() {
    for (index, dll, name, _) in traced() {
        let calls = CALLS[index].load(Ordering::Relaxed);
        if calls == 0 { continue; }
        println!("TRACE {dll}!{name}: calls={calls} total_ms={:.1} first=+{}ms last=+{}ms",
            MICROS[index].load(Ordering::Relaxed) as f64 / 1000.0, FIRST_MS[index].load(Ordering::Relaxed), LAST_MS[index].load(Ordering::Relaxed));
    }
    for (kind, label) in ["threads", "modules", "modules+32", "other"].iter().enumerate() {
        let calls = SNAPSHOT_FLAGS[kind].load(Ordering::Relaxed);
        if calls > 0 {
            println!("TRACE snapshot {label}: calls={calls} total_ms={:.1}", SNAPSHOT_MICROS[kind].load(Ordering::Relaxed) as f64 / 1000.0);
        }
    }
    println!("TRACE Sleep requested_ms={}", SLEEP_MS.load(Ordering::Relaxed));
    let names = traced();
    let order: Vec<String> = ORDER.iter().map(|o| o.load(Ordering::Relaxed)).take_while(|o| *o != usize::MAX)
        .map(|o| format!("{}@{}", names.iter().find(|n| n.0 == o & 0xff).map_or("?", |n| n.2), o >> 8)).collect();
    println!("TRACE order: {}", order.join(" "));
}
