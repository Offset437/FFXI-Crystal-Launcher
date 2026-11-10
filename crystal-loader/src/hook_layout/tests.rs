use super::*;
use crate::windower_bootstrap_contract as contract;

#[test]
fn every_pattern_and_shape_parses_and_names_are_unique() {
    for (name, text) in PATTERNS {
        assert!(crate::sigscan::Pattern::parse(text).is_some(), "{name}");
        assert_eq!(PATTERNS.iter().filter(|(n, _)| n == name).count(), 1, "{name}");
    }
    assert!(crate::sigscan::Pattern::parse(JOIN_BODY).is_some());
}

/// Resolves an image laid out at its RVAs and relocated for `base`.
pub(crate) fn resolve_mapped(image: &[u8], base: usize) -> Resolution {
    resolve(&Image::from_reader(base, |rva, out| image.get(rva..rva + out.len()).map(|b| out.copy_from_slice(b)).is_some()).unwrap())
}

const SYNTHETIC_SIZE: usize = 0x23_1000;
const IAT: usize = 0x14_1000;
const IMPORTS: [&str; 2] = ["SetEvent", "GetProcAddress"];
/// The plugin loader's factory lookup, wildcards as zero.
const FACTORY: [u8; 25] = [0x68, 0, 0, 0, 0, 0x57, 0xFF, 0x15, 0, 0, 0, 0, 0x85, 0xC0, 0x75, 0, 0x68, 0, 0, 0, 0, 0xEB, 0, 0xFF, 0xD0];

/// A relocated PE image with a KERNEL32 import directory and `int3` code
/// only: no locator matches unless a test writes one.
fn synthetic(base: usize) -> Vec<u8> {
    let mut image = vec![0u8; SYNTHETIC_SIZE];
    let put = |image: &mut [u8], at: usize, bytes: &[u8]| image[at..at + bytes.len()].copy_from_slice(bytes);
    let put32 = |image: &mut [u8], at: usize, value: usize| image[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
    put(&mut image, 0, b"MZ");
    put32(&mut image, 0x3C, 0x80);
    let nt = 0x80;
    put(&mut image, nt, b"PE\0\0");
    put(&mut image, nt + 4, &0x14Cu16.to_le_bytes());
    put(&mut image, nt + 6, &3u16.to_le_bytes());
    put32(&mut image, nt + 8, 0x7000_0000);
    put(&mut image, nt + 20, &0xE0u16.to_le_bytes());
    let opt = nt + 24;
    put(&mut image, opt, &0x10Bu16.to_le_bytes());
    put32(&mut image, opt + 28, 0x1000_0000);
    put32(&mut image, opt + 32, 0x1000);
    put32(&mut image, opt + 56, SYNTHETIC_SIZE);
    put32(&mut image, opt + 60, 0x400);
    put32(&mut image, opt + 92, 16);
    put32(&mut image, opt + 96 + 8, 0x15_0000);
    put32(&mut image, opt + 100 + 8, 40);
    put32(&mut image, opt + 96 + 12 * 8, IAT);
    put32(&mut image, opt + 100 + 12 * 8, 4 * (IMPORTS.len() + 1));
    for (index, (name, rva, size, flags)) in [(".text", 0x1000usize, 0x14_0000usize, 0x6000_0020usize),
        (".rdata", 0x14_1000, 0x5_3000, 0x4000_0040), (".data", 0x19_4000, 0x9_D000, 0xC000_0040)].into_iter().enumerate() {
        let at = opt + 0xE0 + index * 40;
        put(&mut image, at, name.as_bytes());
        for (field, value) in [(8, size), (12, rva), (16, size), (20, rva), (36, flags)] {
            put32(&mut image, at + field, value);
        }
    }
    image[0x1000..0x14_1000].fill(0xCC);
    // KERNEL32 imports: name table at 150100, names at 150200.., DLL name at 150400.
    for (index, function) in IMPORTS.iter().enumerate() {
        let name = 0x15_0200 + index * 0x30;
        put(&mut image, name + 2, function.as_bytes());
        put32(&mut image, 0x15_0100 + index * 4, name);
        put32(&mut image, IAT + index * 4, name);
    }
    put(&mut image, 0x15_0400, b"KERNEL32.dll");
    for (field, value) in [(0, 0x15_0100), (12, 0x15_0400), (16, IAT)] {
        put32(&mut image, 0x15_0000 + field, value);
    }
    // A relocated module calls through its IAT from many places.
    for index in 0..20 {
        let at = 0x10_0000 + index * 8;
        put(&mut image, at, &[0xFF, 0x15]);
        put32(&mut image, at + 2, base + IAT);
    }
    image
}

#[test]
fn an_image_read_unrelocated_for_its_load_address_resolves_nothing() {
    let resolved = resolve_mapped(&synthetic(0x1000_0000), 0x1000_0000);
    assert!(resolved.exit.unwrap_err().starts_with("exit.owner_dtor: no match"));
    let stale = resolve_mapped(&synthetic(0x1000_0000), 0x1800_0000);
    assert!(stale.exit.unwrap_err().contains("not fully relocated"));
    assert!(stale.hits.is_empty());
}
// --- the real Hook builds (read as data) ---------------------------------

fn builds_dir() -> std::path::PathBuf {
    std::env::var_os("CRYSTAL_HOOK_BUILDS")
        .map(Into::into)
        .unwrap_or_else(|| r"builds".into())
}

/// (Hook build, exit, plugins) the scan must find in each archived file:
/// tables for the known builds, the analysis' values for the older ones.
fn expected(timestamp: u32) -> Option<(Found<Exit>, Found<Plugins>)> {
    if let Some(known) = KNOWN.iter().find(|k| k.timestamp == timestamp) {
        return Some((Ok(known.exit), known.plugins.ok_or_else(String::new)));
    }
    match timestamp {
        // 4.7.9.1 (2026-05-23)
        0x6A12_2D04 => Some((
            Ok(Exit { join: 0x3_5B00, owner_join: 0x6_E4BC, wait: 0x6_BF7A, event: 0x1C_CD10, stop: 0x1A_03C4, thread: 0x1C_CD08 }),
            Ok(Plugins { get_proc_iat: 0x14_011C, factory_lookup: 0x6_EFB5, factory_name: 0x18_4CB0, initialize_call: 0x6_F380 }),
        )),
        // 4.7.8.1 (2026-04-02)
        0x69CE_EA53 => Some((
            Ok(Exit { join: 0x3_5480, owner_join: 0x6_921C, wait: 0x6_6D00, event: 0x1C_1F60, stop: 0x19_5394, thread: 0x1C_1F58 }),
            Ok(Plugins { get_proc_iat: 0x13_611C, factory_lookup: 0x6_9AC1, factory_name: 0x17_A660, initialize_call: 0x6_9DD0 }),
        )),
        _ => None,
    }
}

fn archived_hooks() -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(builds_dir()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("hook-") && name.ends_with(".dll") {
            out.push((entry.path(), std::fs::read(entry.path()).unwrap()));
        }
    }
    out.sort();
    out
}

#[test]
#[ignore = "reads Hook.dll copies in CRYSTAL_HOOK_BUILDS and the installed Hook.dll as data; nothing is loaded"]
fn every_archived_hook_build_resolves_by_signature() {
    let hooks = archived_hooks();
    assert!(hooks.len() >= 2, "no Hook builds in {}", builds_dir().display());
    let mut resolved_builds = 0;
    for (path, file) in &hooks {
        let pe = contract::map_pe(file).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        let at_preferred = resolve_mapped(&pe.image, pe.image_base);
        eprintln!("{name}: {:08X} known={:?}\n  exit {:x?}\n  plugins {:x?}", at_preferred.timestamp, at_preferred.known,
            at_preferred.exit, at_preferred.plugins);
        let startup = contract::hook_startup(file);
        eprintln!("  startup {startup:x?}");
        let Some((exit, plugins)) = expected(at_preferred.timestamp) else {
            // Only the 2022 (4.3.7.0) generation is outside every group.
            assert!(at_preferred.timestamp < 0x6900_0000, "{name}: no expectation");
            assert!(at_preferred.exit.is_err() && at_preferred.plugins.is_err() && startup.is_err(), "{name}");
            continue;
        };
        resolved_builds += 1;
        assert_eq!(at_preferred.exit, exit, "{name}");
        if plugins.is_ok() {
            assert_eq!(at_preferred.plugins, plugins, "{name}");
        }
        let startup = startup.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((startup.timestamp, startup.image_len), (at_preferred.timestamp, at_preferred.size), "{name}");
        // Loaded elsewhere: the same RVAs.
        let moved = contract::map_pe_relocated(file);
        let rebased = resolve_mapped(&moved, moved.as_ptr() as usize);
        assert_eq!((rebased.exit, rebased.plugins), (at_preferred.exit.clone(), at_preferred.plugins.clone()), "{name} rebased");
        // The next update with the same code: the scan alone gives the same values.
        let mut renamed = pe.image.clone();
        let nt = u32::from_le_bytes(renamed[0x3C..0x40].try_into().unwrap()) as usize;
        renamed[nt + 8..nt + 12].copy_from_slice(&0x7100_0000u32.to_le_bytes());
        let unknown = resolve_mapped(&renamed, pe.image_base);
        assert_eq!((unknown.known, unknown.exit, unknown.plugins), (None, at_preferred.exit.clone(), at_preferred.plugins.clone()), "{name} renamed");
        let mut renamed_file = file.clone();
        renamed_file[nt + 8..nt + 12].copy_from_slice(&0x7100_0000u32.to_le_bytes());
        let found = contract::hook_startup(&renamed_file).unwrap();
        assert_eq!((found.map_name_rva, found.event_name_rva), (startup.map_name_rva, startup.event_name_rva), "{name}");
        // A known identity whose scan finds other values is refused.
        if let Some((_, known)) = contract::KNOWN_HOOKS.iter().find(|(_, k)| k.timestamp == startup.timestamp) {
            assert_eq!(*known, startup);
            let other = contract::KNOWN_HOOKS.iter().find(|(_, k)| k.timestamp != startup.timestamp).unwrap().1;
            let mut claimed = file.clone();
            claimed[nt + 8..nt + 12].copy_from_slice(&other.timestamp.to_le_bytes());
            assert!(contract::hook_startup(&claimed).unwrap_err().contains("disagrees"), "{name}");
        }
    }
    assert!(resolved_builds >= 4, "4.7.8.1, 4.7.9.1, 4.7.9.3 and 4.7.9.6 are archived");
    // The installed Hook (read only) is one of the archived builds.
    let installed = std::path::Path::new(r"C:\Games\Final Fantasy XI Programs\Windower 4\Hook.dll");
    if let Ok(file) = std::fs::read(installed) {
        assert!(hooks.iter().any(|(_, archived)| *archived == file), "copy the installed Hook.dll to the archive");
        let startup = contract::hook_startup(&file).unwrap();
        eprintln!("installed: {startup:x?} {:?}", contract::known_hook(&startup));
    }
}

/// Edits of Hook's startup code that must each switch private startup off.
#[test]
#[ignore = "reads the archived Hook 4.7.9.3 as data"]
fn the_startup_contract_refuses_a_changed_record_or_name_use() {
    let path = builds_dir().join("Hook-6A5B1A74.dll");
    let file = std::fs::read(path).unwrap();
    let known = contract::hook_startup(&file).unwrap();
    assert_eq!(contract::known_hook(&known), Some("4.7.9.3"));
    let pe = contract::map_pe(&file).unwrap();
    // RVA -> file offset for .text and .rdata (raw offsets differ from RVAs).
    let file_at = |rva: usize| {
        let nt = u32::from_le_bytes(file[0x3C..0x40].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(file[nt + 6..nt + 8].try_into().unwrap()) as usize;
        let table = nt + 24 + u16::from_le_bytes(file[nt + 20..nt + 22].try_into().unwrap()) as usize;
        (0..count).find_map(|i| {
            let h = table + i * 40;
            let d = |o: usize| u32::from_le_bytes(file[h + o..h + o + 4].try_into().unwrap()) as usize;
            (rva >= d(12) && rva < d(12) + d(16)).then(|| rva - d(12) + d(20))
        }).unwrap()
    };
    let map = 0x6_2C9F; // push MAP_NAME in 4.7.9.3
    assert_eq!(pe.image[map], 0x68);
    let refused = |what: &str, edit: &dyn Fn(&mut Vec<u8>)| {
        let mut changed = file.clone();
        edit(&mut changed);
        let mut renamed = changed.clone();
        let nt = u32::from_le_bytes(renamed[0x3C..0x40].try_into().unwrap()) as usize;
        renamed[nt + 8] ^= 0x55; // as an unknown build, so only the scan decides
        let error = contract::hook_startup(&renamed).unwrap_err();
        assert!(error.contains(what), "{what}: {error}");
    };
    // A larger record: the first memcpy size.
    refused("another size", &|f| f[file_at(map + 57)] = 0xA5);
    // A field moved: the third field's frame displacement.
    refused("another field layout", &|f| f[file_at(0x6_4E03 + 46)] = f[file_at(0x6_4E03 + 46)].wrapping_add(2));
    // The name used a second time in code.
    refused("other uses", &|f| {
        let operand: [u8; 4] = f[file_at(map + 1)..file_at(map + 1) + 4].try_into().unwrap();
        let spare = file_at(0x1_0000);
        f[spare] = 0x68;
        f[spare + 1..spare + 5].copy_from_slice(&operand);
    });
    // The event opened another way.
    refused("expected imports", &|f| f[file_at(0x6_7AE8 + 14)] ^= 0x04);
    // A different name at the operand.
    refused("not where Hook's code reads it", &|f| f[file_at(known.map_name_rva)] = b'w');
}

#[test]
fn hook_without_a_base_is_refused_and_logged() {
    let lines = crate::log::capture(|| assert!(hook(0).is_none()));
    assert_eq!(lines, ["hook layout: no Hook.dll base to resolve"]);
}

#[test]
fn a_refused_group_logs_which_locator_failed() {
    let base = 0x1000_0000usize;
    // No locator matches: both groups are refused and the log names them.
    let resolved = resolve_mapped(&synthetic(base), base);
    let lines = crate::log::capture(|| report(&resolved, Duration::ZERO));
    let hits = lines.iter().find(|l| l.starts_with("hook layout: locator matches")).expect("a locator line");
    assert!(lines.iter().any(|l| l == "hook layout: plugin loader unavailable: plugin.factory: no match"), "{lines:#?}");
    assert!(hits.contains("plugin.factory=0") && hits.contains("exit.owner_dtor=0"), "{hits}");
    // A duplicated locator is reported with its count.
    let mut twice = synthetic(base);
    for at in [0x2_0000, 0x3_0000] {
        twice[at..at + FACTORY.len()].copy_from_slice(&FACTORY);
    }
    let resolved = resolve_mapped(&twice, base);
    let lines = crate::log::capture(|| report(&resolved, Duration::ZERO));
    assert!(lines.iter().any(|l| l.contains("plugin.factory: more than one match")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.starts_with("hook layout: locator matches") && l.contains("plugin.factory=2")), "{lines:#?}");
    // Everything resolved: no locator line.
    let mut ok = resolve_mapped(&synthetic(base), base);
    ok.exit = Ok(BUILD_6AC4F0DD.exit);
    ok.plugins = Ok(Plugins { get_proc_iat: 0, factory_lookup: 0, factory_name: 0, initialize_call: 0 });
    let lines = crate::log::capture(|| report(&ok, Duration::ZERO));
    assert!(!lines.iter().any(|l| l.contains("locator matches")), "{lines:#?}");
}
/// Mapping a DLL holds the loader lock, which makes a concurrent live capture
/// defer (NotReady): the tests that map copies take turns.
static MAPPING: Mutex<()> = Mutex::new(());

fn installed_hook() -> std::path::PathBuf {
    std::env::var_os("CRYSTAL_INSTALLED_HOOK")
        .map(Into::into)
        .unwrap_or_else(|| r"C:\Games\Final Fantasy XI Programs\Windower 4\Hook.dll".into())
}

/// The installed Hook.dll (or CRYSTAL_INSTALLED_HOOK), copied to a temporary
/// folder under its real name and mapped without running it, resolves through
/// [`hook`] with the base the callers get from GetModuleHandleExW. The live
/// failure (2026-10-23): hook() looked Hook up with GetModuleHandleW, which a
/// started Hook detours to hide itself, so it returned None and logged nothing.
#[test]
#[ignore = "copies the installed Hook.dll (or CRYSTAL_INSTALLED_HOOK) to %TEMP% and maps the copy; no code runs"]
fn the_installed_hook_resolves_through_the_callers_base() {
    use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::{
        GetModuleHandleExW, LoadLibraryExW, DONT_RESOLVE_DLL_REFERENCES, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT}};
    let _turn = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
    let source = installed_hook();
    let file = std::fs::read(&source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
    let dir = std::env::temp_dir().join(format!("crystal-hook-layout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("Hook.dll");
    std::fs::write(&copy, &file).unwrap();
    let wide: Vec<u16> = copy.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), DONT_RESOLVE_DLL_REFERENCES) };
    assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
    let name: Vec<u16> = "Hook.dll\0".encode_utf16().collect();
    let mut found = std::ptr::null_mut();
    assert_ne!(unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, name.as_ptr(), &mut found) }, 0);
    assert_eq!(found, module);
    let base = module as usize;
    let mut resolved = None;
    let lines = crate::log::capture(|| resolved = hook(base));
    eprintln!("{}: {lines:#?}", source.display());
    let resolved = resolved.expect("the installed Hook resolves");
    let pe = contract::map_pe(&file).unwrap();
    assert_eq!((resolved.base, resolved.timestamp), (base, resolve_mapped(&pe.image, pe.image_base).timestamp));
    assert!(resolved.exit.is_ok(), "{:?}", resolved.exit);
    assert!(resolved.plugins.is_ok(), "{:?}", resolved.plugins);
    if let Some(known) = KNOWN.iter().find(|k| k.timestamp == resolved.timestamp && k.size == resolved.size) {
        assert_eq!(resolved.known, Some(known.label));
        assert_eq!((resolved.exit.clone(), resolved.plugins.clone().ok()), (Ok(known.exit), known.plugins));
    }
    assert!(lines.iter().any(|l| l.starts_with("hook layout: Hook.dll ")), "{lines:#?}");
    // The cache answers the same base and refuses another one, logged.
    assert!(hook(base).is_some_and(|again| std::ptr::eq(again, resolved)));
    let other = crate::log::capture(|| assert!(hook(base + 0x1_0000).is_none()));
    assert!(other.iter().any(|l| l.contains("is not the one resolved")), "{other:#?}");
    unsafe { FreeLibrary(module) };
    let _ = std::fs::remove_dir_all(&dir);
}

/// Loads each archived build into this test process without running it
/// (DONT_RESOLVE_DLL_REFERENCES: mapped and relocated, no DllMain, no imports
/// bound) and resolves it through the same live capture the host uses.
#[test]
#[ignore = "maps copies of Hook.dll from CRYSTAL_HOOK_BUILDS (no code runs); never the installed Windower"]
fn mapped_copies_resolve_through_the_live_capture() {
    use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::{LoadLibraryExW, DONT_RESOLVE_DLL_REFERENCES}};
    let _turn = MAPPING.lock().unwrap_or_else(|e| e.into_inner());
    let mut loaded = Vec::new();
    for (path, _) in archived_hooks() {
        let text = path.to_string_lossy().to_ascii_lowercase();
        assert!(!text.contains(r"\games\"), "never map the installed Windower");
        let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), DONT_RESOLVE_DLL_REFERENCES) };
        assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
        loaded.push(module);
        let base = module as usize;
        let resolution = resolve_module(base).expect("a mapped module is never deferred");
        eprintln!("{} at {base:#x}: {:x?} {:x?}", path.display(), resolution.exit, resolution.plugins);
        if let Some((exit, plugins)) = expected(resolution.timestamp) {
            assert_eq!(resolution.exit, exit, "{}", path.display());
            if plugins.is_ok() {
                assert_eq!(resolution.plugins, plugins, "{}", path.display());
            }
        }
    }
    assert!(loaded.iter().filter(|m| **m as usize != 0x1000_0000).count() >= 1, "at least one copy is relocated");
    for module in loaded.into_iter().rev() {
        unsafe { FreeLibrary(module) };
    }
}
