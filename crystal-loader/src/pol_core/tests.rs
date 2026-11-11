use super::*;

/// A synthetic polcore layout at `base`: `.text` 0x1000-0x40000 (code),
/// `.rdata` 0x40000-0x50000 (read-only), `.data` 0x50000-0x60000 (writable).
pub(crate) fn layout(base: usize) -> Layout {
    let section = |name: &str, rva: usize, size: usize, flags: u32| {
        let mut padded = [0u8; 8];
        padded[..name.len()].copy_from_slice(name.as_bytes());
        Section { name: padded, rva, size, raw: size, flags }
    };
    Layout { base, timestamp: 0x7000_0000, sections: vec![
        section(".text", 0x1000, 0x3f000, 0x6000_0020),
        section(".rdata", 0x40000, 0x10000, 0x4000_0040),
        section(".data", 0x50000, 0x10000, 0xC000_0040),
    ] }
}

const BASE: usize = 0x1000_0000;
const TABLE_AT: usize = BASE + 0x50100;

/// A table with the verified shape: every known code entry points into `.text`.
fn good_table() -> Vec<usize> {
    (0..TABLE_LEN).map(|slot| if shape_has_code(slot) { BASE + 0x1000 + slot * 16 } else { 0 }).collect()
}

#[test]
fn the_shape_and_registry_describe_the_verified_table() {
    assert_eq!((0..TABLE_LEN).filter(|&s| shape_has_code(s)).count(), SHAPE_CODE);
    assert!(!shape_has_code(TABLE_LEN));
    assert!(SERVICES.windows(2).all(|w| w[0].slot < w[1].slot), "sorted and unique (binary search)");
    for s in SERVICES {
        assert!(shape_has_code(usize::from(s.slot)), "service {} is a null entry in the verified builds", s.slot);
        assert!(s.args.is_none_or(|n| n <= 13));
        assert_eq!(service(usize::from(s.slot)).unwrap().slot, s.slot);
    }
    assert!(service(0).is_none() && service(usize::MAX).is_none());
    assert_eq!(known(0x6ABA_1641), Some("1.18.15f"));
    assert_eq!(known(0x4E4E_5917), Some("1.18.15e"));
    assert_eq!(known(1), None);
}

#[test]
fn a_table_with_the_verified_shape_passes_whatever_its_addresses() {
    for offset in [0, 0x2000, 0x8] {
        let table: Vec<usize> = good_table().iter().map(|&e| if e == 0 { 0 } else { e + offset }).collect();
        let checked = check_table(&layout(BASE), TABLE_AT, &table).unwrap();
        assert_eq!((checked.rva, checked.missing.len(), checked.added), (0x50100, 0, 0));
        assert!(checked.services > 100);
        assert!(checked.summary(&layout(BASE)).contains("unknown build, accepted by structure"));
    }
    // The same contract at another base (a rebased core).
    let moved: Vec<usize> = good_table().iter().map(|&e| if e == 0 { 0 } else { e + 0x0100_0000 }).collect();
    assert!(check_table(&layout(BASE + 0x0100_0000), TABLE_AT + 0x0100_0000, &moved).is_ok());
}

#[test]
fn every_used_service_must_be_present_and_in_code() {
    let l = layout(BASE);
    for s in SERVICES.iter().filter(|s| required(s.group)) {
        let slot = usize::from(s.slot);
        for (bad, why) in [(0, "is missing"), (BASE + 0x50000, "does not point into the core's code"),
            (BASE + 0x40010, "does not point into the core's code"), (0x2000_0000, "does not point into the core's code")] {
            let mut table = good_table();
            table[slot] = bad;
            let error = check_table(&l, TABLE_AT, &table).unwrap_err();
            assert!(error.contains(&format!("service {slot} ")) && error.contains(why), "{error}");
            assert!(error.contains(s.group.feature()), "{error}");
        }
    }
    // Offline-only entries are never required by a production host.
    let mut table = good_table();
    table[421] = 0;
    let checked = check_table(&l, TABLE_AT, &table).unwrap();
    assert_eq!(checked.missing, vec![421]);
}

#[test]
fn the_table_must_lie_whole_in_writable_data() {
    let l = layout(BASE);
    let table = good_table();
    for at in [BASE + 0x2000, BASE + 0x40000, BASE + 0x5F000, BASE + 0x70000, TABLE_AT + 2, 0] {
        assert!(check_table(&l, at, &table).unwrap_err().contains("writable data"), "{at:#x}");
    }
    assert!(check_table(&l, TABLE_AT, &table[..TABLE_LEN - 1]).is_err(), "short table");
}

#[test]
fn drift_beyond_the_tolerance_is_not_the_playonline_table() {
    let l = layout(BASE);
    let unused: Vec<usize> = (0..TABLE_LEN).filter(|&s| shape_has_code(s) && service(s).is_none_or(|s| !required(s.group))).collect();
    let mut table = good_table();
    for &slot in &unused[..SHAPE_TOLERANCE] { table[slot] = 0; }
    let checked = check_table(&l, TABLE_AT, &table).unwrap();
    assert_eq!(checked.missing.len(), SHAPE_TOLERANCE);
    assert!(checked.summary(&l).contains("absent"));
    table[unused[SHAPE_TOLERANCE]] = BASE + 0x50000; // data, not code
    assert!(check_table(&l, TABLE_AT, &table).unwrap_err().contains("does not have PlayOnline's shape"));
    // New entries in former null slots are a superset: accepted, counted.
    let mut grown = good_table();
    let nulls: Vec<usize> = (0..TABLE_LEN).filter(|&s| !shape_has_code(s)).take(40).collect();
    for &slot in &nulls { grown[slot] = BASE + 0x1000; }
    assert_eq!(check_table(&l, TABLE_AT, &grown).unwrap().added, 40);
    // A null slot holding a non-code value counts as a difference.
    let mut odd = good_table();
    for &slot in &nulls[..SHAPE_TOLERANCE + 1] { odd[slot] = 0x1234; }
    assert!(check_table(&l, TABLE_AT, &odd).is_err());
}

#[test]
fn the_com_vtable_must_be_data_and_its_methods_code() {
    let l = layout(BASE);
    let vtable = BASE + 0x40100;
    let methods: Vec<usize> = (0..COM_VTABLE_WORDS).map(|i| BASE + 0x2000 + i * 0x10).collect();
    assert!(check_com(&l, vtable, &methods).is_ok());
    assert!(check_com(&l, BASE + 0x2000, &methods).unwrap_err().contains("vtable"), "a vtable in code");
    assert!(check_com(&l, BASE + 0x80000, &methods).is_err(), "outside the image");
    assert!(check_com(&l, vtable + 1, &methods).is_err(), "misaligned");
    assert!(check_com(&l, vtable, &methods[..8]).is_err(), "short");
    for (index, name) in COM_METHODS {
        let mut bad = methods.clone();
        bad[index] = BASE + 0x50000;
        assert!(check_com(&l, vtable, &bad).unwrap_err().contains(name));
        bad[index] = 0;
        assert!(check_com(&l, vtable, &bad).is_err());
    }
    // Methods Crystal never calls are not checked.
    let mut unused = methods.clone();
    unused[3] = 0;
    assert!(check_com(&l, vtable, &unused).is_ok());
}

#[test]
fn this_executable_has_no_viewer_ticker_import() {
    let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    if cfg!(target_arch = "x86") {
        let imports = file_imports(&me).unwrap();
        assert!(imports.iter().any(|(dll, _)| dll.eq_ignore_ascii_case(b"kernel32.dll")));
        assert_eq!(viewer_ticker(&me), Ok(crate::native_jobs::LEGACY_STOCK_FRAME_US));
    } else {
        assert!(viewer_ticker(&me).is_err(), "only 32-bit PE files are read");
    }
    assert!(viewer_ticker(b"not a PE").is_err());
    assert!(viewer_ticker(&me[..0x200.min(me.len())]).is_err());
}

// --- the two real builds (read as files; nothing is loaded or run) ---------

use crate::windower_bootstrap_contract::map_pe;

fn path(var: &str, default: &str) -> std::path::PathBuf {
    std::env::var_os(var).map(Into::into).unwrap_or_else(|| default.into())
}
/// A path under the user's profile folder (the local reference copies' default home).
pub(crate) fn home(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(rel)
}
const REA: &str = r"pol-builds";
const VIEWER: &str = r"C:\Games\SquareEnix\PlayOnlineViewer";
const BACKUP: &str = r"C:\Games\SquareEnix\PlayOnlineViewer\.xi-updater\backups\1794255701306777500-27464";

pub(crate) struct Build {
    pub name: &'static str,
    pub core: std::path::PathBuf,
    pub app: std::path::PathBuf,
    /// IPOLCoreCom vtable RVAs (both interface variants), from the 2026-11-09 analysis.
    pub vtables: [usize; 2],
    /// Test cross-check only: the hand-verified table RVA and service RVAs.
    pub table_rva: usize,
    pub services: &'static [(usize, usize)],
    pub frame_us: u64,
}

pub(crate) fn builds() -> [Build; 2] {
    [
        Build { name: "1.18.15e", core: path("CRYSTAL_POL_LEGACY_CORE", &home(&format!(r"{REA}\old-polcore-unpacked-analysis.dll")).to_string_lossy()),
            app: path("CRYSTAL_POL_LEGACY_APP", &format!(r"{BACKUP}\13.bin")), vtables: [0x65874, 0x659dc], table_rva: 0x6fbe8,
            services: LEGACY, frame_us: crate::native_jobs::LEGACY_STOCK_FRAME_US },
        Build { name: "1.18.15f", core: path("CRYSTAL_POL_UPDATED_CORE", &format!(r"{VIEWER}\viewer\com\polcore.dll")),
            app: path("CRYSTAL_POL_UPDATED_APP", &format!(r"{VIEWER}\viewer\com\app.dll")), vtables: [0xac984, 0xaca80], table_rva: 0xc4180,
            services: UPDATED, frame_us: crate::native_jobs::FAST_STOCK_FRAME_US },
    ]
}

/// Runs the production checks over a polcore file mapped as data: the COM
/// vtables, then the table GetCommonFunctionTable names (its `mov [eax], imm32`).
pub(crate) fn check_file(core: &[u8], vtables: &[usize]) -> Result<(Layout, Table), String> {
    let pe = map_pe(core).map_err(str::to_string)?;
    let headers = crate::sigscan::parse_headers(&pe.image[..0x1000]).map_err(str::to_string)?;
    let layout = Layout::new(pe.image_base, headers);
    let word = |rva: usize| pe.image.get(rva..rva + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let mut table_at = None;
    for &vtable in vtables {
        let methods: Vec<usize> = (0..COM_VTABLE_WORDS).map(|i| word(vtable + 4 * i).unwrap_or(0)).collect();
        check_com(&layout, pe.image_base + vtable, &methods)?;
        let getter = methods[7] - pe.image_base;
        let pattern = crate::sigscan::Pattern::parse("C7 00 ?? ?? ?? ??").unwrap();
        let hits = crate::sigscan::scan(&pe.image[getter..getter + 24], &[pattern]);
        let at = *hits[0].first().ok_or("GetCommonFunctionTable stores no table")?;
        let found = word(getter + at + 2).unwrap();
        assert!(table_at.is_none_or(|t| t == found), "both interfaces name one table");
        table_at = Some(found);
    }
    let table_at = table_at.ok_or("no vtable")?;
    let words: Vec<usize> = (0..TABLE_LEN).map(|i| word(table_at - pe.image_base + 4 * i).unwrap_or(0)).collect();
    let table = check_table(&layout, table_at, &words)?;
    Ok((layout, table))
}

#[test]
#[ignore = "reads the two real polcore/app.dll builds as files (CRYSTAL_POL_* or the default paths); nothing is loaded or run"]
fn both_real_builds_pass_the_production_checks_and_agree_with_the_hand_verified_maps() {
    for build in builds() {
        let file = std::fs::read(&build.core).unwrap_or_else(|e| panic!("{}: {e}", build.core.display()));
        let (layout, table) = check_file(&file, &build.vtables).unwrap_or_else(|e| panic!("{}: {e}", build.name));
        eprintln!("{}: {}", build.name, table.summary(&layout));
        assert_eq!(known(layout.timestamp), Some(build.name));
        assert_eq!(table.rva, build.table_rva, "{}: the getter names the verified table", build.name);
        assert!(table.missing.is_empty() && table.added == 0, "{}: exact verified shape", build.name);
        // Cross-check: every registry slot resolves to the hand-verified address.
        let pe = map_pe(&file).unwrap();
        for &(slot, rva) in build.services {
            let at = build.table_rva + 4 * slot;
            let entry = u32::from_le_bytes(pe.image[at..at + 4].try_into().unwrap()) as usize;
            assert_eq!(entry - pe.image_base, rva, "{} slot {slot}", build.name);
        }
        for s in SERVICES {
            assert!(build.services.iter().any(|&(slot, _)| slot == usize::from(s.slot)), "{}: service {} not cross-checked", build.name, s.slot);
        }
        // The Viewer's frame from its app.dll.
        let app = std::fs::read(&build.app).unwrap_or_else(|e| panic!("{}: {e}", build.app.display()));
        assert_eq!(viewer_ticker(&app), Ok(build.frame_us), "{} app.dll", build.name);
    }
}

#[test]
#[ignore = "reads the real polcore builds as files; mutates copies in memory only"]
fn mutated_real_tables_fail_closed_with_the_slot_named() {
    let build = &builds()[1];
    let file = std::fs::read(&build.core).unwrap();
    let pe = map_pe(&file).unwrap();
    let headers = crate::sigscan::parse_headers(&pe.image[..0x1000]).unwrap();
    let layout = Layout::new(pe.image_base, headers);
    let words: Vec<usize> = (0..TABLE_LEN).map(|i| u32::from_le_bytes(pe.image[build.table_rva + 4 * i..][..4].try_into().unwrap()) as usize).collect();
    let at = pe.image_base + build.table_rva;
    assert!(check_table(&layout, at, &words).is_ok());
    for slot in [815, 1528, 376, 970] {
        let mut bad = words.clone();
        bad[slot] = 0;
        assert!(check_table(&layout, at, &bad).unwrap_err().contains(&format!("service {slot} ")));
    }
    // The table one entry off (the wrong table start) is not PlayOnline's.
    assert!(check_table(&layout, at + 4, &(0..TABLE_LEN).map(|i| words.get(i + 1).copied().unwrap_or(0)).collect::<Vec<_>>()).is_err());
}

const UPDATED: &[(usize, usize)] = &[(125, 0x43400), (126, 0x435a0), (163, 0x1ce80), (164, 0x1cea0), (167, 0x1b660), (169, 0x1b850), (171, 0x1a8a0), (172, 0x1a8c0), (173, 0x18ff0), (175, 0x1e520), (176, 0x1e540), (177, 0x1d8b0), (183, 0x1dee0), (186, 0x1c690), (188, 0x1c650), (190, 0x18f60), (197, 0x19670), (199, 0x1cb40), (206, 0x18ec0), (207, 0x18ef0), (270, 0x150e0), (272, 0x15260), (284, 0x15640), (285, 0x15610), (286, 0x156a0), (287, 0x15670), (288, 0x15e90), (291, 0x160a0), (316, 0x14920), (322, 0x16ab0), (323, 0x16ac0), (324, 0x168b0), (325, 0x168d0), (326, 0x16a20), (327, 0x16af0), (328, 0x16b20), (329, 0x16930), (330, 0x16960), (331, 0x15fe0), (333, 0x16b50), (334, 0x160c0), (337, 0x184e0), (342, 0x187a0), (367, 0x45070), (374, 0x197b0), (375, 0x19840), (376, 0x19f40), (377, 0x19f70), (383, 0x1ed80), (384, 0x1edd0), (385, 0x1ea90), (394, 0x1ece0), (401, 0x219c0), (404, 0x20a40), (405, 0x20b30), (421, 0x12960), (434, 0x211b0), (463, 0x44700), (464, 0x44920), (465, 0x44980), (807, 0x414b0), (809, 0x3f7c0), (814, 0x3f940), (815, 0x3faf0), (816, 0x40390), (817, 0x403f0), (818, 0x40520), (819, 0x409d0), (823, 0x40a90), (824, 0x40ac0), (825, 0x40af0), (826, 0x40b70), (828, 0x40c30), (835, 0x41230), (838, 0x410b0), (845, 0x41880), (846, 0x41b30), (849, 0x41d70), (851, 0x41e50), (852, 0x41eb0), (853, 0x41f10), (854, 0x41f30), (856, 0x41f70), (858, 0x421a0), (868, 0x42520), (870, 0x425c0), (871, 0x425e0), (872, 0x42760), (884, 0x427f0), (886, 0x41ef0), (891, 0x42930), (892, 0x42570), (914, 0x2bdc0), (915, 0x2bde0), (920, 0x29920), (921, 0x29c40), (922, 0x29930), (923, 0x29ef0), (928, 0x29ea0), (929, 0x29e70), (932, 0x29860), (965, 0x12910), (969, 0x45780), (970, 0x457d0), (1027, 0x457e0), (1032, 0x15e80), (1034, 0x1cec0), (1035, 0x1cf20), (1036, 0x1a380), (1037, 0x1a430), (1038, 0x1e430), (1039, 0x1e490), (1072, 0x15730), (1074, 0x13d80), (1075, 0x13d70), (1103, 0x409c0), (1107, 0x13d40), (1140, 0x415a0), (1149, 0x417d0), (1175, 0x45850), (1302, 0x15a90), (1304, 0x16ed0), (1305, 0x16f00), (1309, 0x140d0), (1502, 0x1a8e0), (1527, 0x48840), (1528, 0x12930), (1535, 0x44740), (1536, 0x448b0)];
const LEGACY: &[(usize, usize)] = &[(125, 0x48a40), (126, 0x48be0), (163, 0x23440), (164, 0x255c0), (167, 0x23da0), (169, 0x23f90), (171, 0x1e470), (172, 0x1e490), (173, 0x1cc40), (175, 0x23440), (176, 0x23460), (177, 0x227d0), (183, 0x22e00), (186, 0x24dd0), (188, 0x24d90), (190, 0x1cbb0), (197, 0x1d2c0), (199, 0x25280), (206, 0x1cb20), (207, 0x1cb50), (270, 0x1a760), (272, 0x1a8e0), (284, 0x1acb0), (285, 0x1ac80), (286, 0x1ad10), (287, 0x1ace0), (288, 0x1b500), (291, 0x1b6d0), (316, 0x1a050), (322, 0x1c0e0), (323, 0x1c0f0), (324, 0x1bee0), (325, 0x1bf00), (326, 0x1c050), (327, 0x1c120), (328, 0x1c150), (329, 0x1bf60), (330, 0x1bf90), (331, 0x1b610), (333, 0x1c180), (334, 0x1b6f0), (337, 0x1fc30), (342, 0x1fef0), (367, 0x49d40), (374, 0x1d400), (375, 0x1d490), (376, 0x1db60), (377, 0x1db90), (383, 0x28330), (384, 0x28380), (385, 0x28040), (394, 0x28290), (401, 0x2af70), (404, 0x29ff0), (405, 0x2a0e0), (421, 0x18200), (434, 0x2a760), (463, 0x48020), (464, 0x48240), (465, 0x482a0), (807, 0x46410), (809, 0x44720), (814, 0x448a0), (815, 0x44a50), (816, 0x452f0), (817, 0x45350), (818, 0x45480), (819, 0x45930), (823, 0x459f0), (824, 0x45a20), (825, 0x45a50), (826, 0x45ad0), (828, 0x45b90), (835, 0x46190), (838, 0x46010), (845, 0x467c0), (846, 0x46a70), (849, 0x46cb0), (851, 0x46d90), (852, 0x46df0), (853, 0x46e50), (854, 0x46e70), (856, 0x46eb0), (858, 0x47340), (868, 0x47390), (870, 0x473c0), (871, 0x473e0), (872, 0x47550), (884, 0x475e0), (886, 0x46e30), (891, 0x47720), (892, 0x47390), (914, 0x30d70), (915, 0x30d90), (920, 0x2e910), (921, 0x2ec30), (922, 0x2e920), (923, 0x2eeb0), (928, 0x2ee60), (929, 0x2edc0), (932, 0x2e850), (965, 0x181b0), (969, 0x4a440), (970, 0x4a490), (1027, 0x4a4a0), (1032, 0x1b4f0), (1034, 0x255e0), (1035, 0x25640), (1036, 0x1dfa0), (1037, 0x1e000), (1038, 0x23350), (1039, 0x233b0), (1072, 0x1ada0), (1074, 0x19560), (1075, 0x19550), (1103, 0x45920), (1107, 0x19520), (1140, 0x464f0), (1149, 0x46720), (1175, 0x4a510), (1302, 0x1b100), (1304, 0x1c500), (1305, 0x1c530), (1309, 0x19880), (1502, 0x1e4b0), (1527, 0x4db30), (1528, 0x181d0), (1535, 0x48060), (1536, 0x481d0)];
