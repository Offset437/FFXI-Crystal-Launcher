//! The built game host (`pol.exe`) as a file: FFXI runs inside it for every
//! route, so it must be large-address-aware (4 GB of address space on 64-bit
//! Windows) and must start without the VC++ redistributable (static CRT).

fn u16_at(file: &[u8], at: usize) -> u16 { u16::from_le_bytes(file[at..at + 2].try_into().unwrap()) }
fn u32_at(file: &[u8], at: usize) -> usize { u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize }

fn host() -> Vec<u8> {
    std::fs::read(env!("CARGO_BIN_EXE_pol")).expect("the built game host")
}

struct DllImports<'a> {
    name: &'a str,
    symbols: Vec<&'a str>,
}

/// DLLs and named imports from the 32-bit PE import table.
fn imports(file: &[u8]) -> Vec<DllImports<'_>> {
    let nt = u32_at(file, 0x3c);
    let opt = nt + 24;
    let sections: Vec<(usize, usize, usize)> = (0..u16_at(file, nt + 6) as usize).map(|i| {
        let at = opt + u16_at(file, nt + 20) as usize + i * 40;
        (u32_at(file, at + 12), u32_at(file, at + 8).max(u32_at(file, at + 16)), u32_at(file, at + 20))
    }).collect();
    let offset = |rva: usize| sections.iter().find(|(s, n, _)| rva >= *s && rva < s + n).map(|(s, _, raw)| rva - s + raw).unwrap();
    let name_at = |start: usize| {
        let end = start + file[start..].iter().position(|&b| b == 0).unwrap();
        std::str::from_utf8(&file[start..end]).unwrap()
    };
    let directory = u32_at(file, opt + 104);
    let mut out = Vec::new();
    for index in 0.. {
        let descriptor = offset(directory + index * 20);
        let name_rva = u32_at(file, descriptor + 12);
        if name_rva == 0 { break; }
        let lookup_rva = u32_at(file, descriptor);
        let thunk = offset(if lookup_rva == 0 { u32_at(file, descriptor + 16) } else { lookup_rva });
        let mut symbols = Vec::new();
        for index in 0.. {
            let entry = u32_at(file, thunk + index * 4);
            if entry == 0 { break; }
            if entry & 0x8000_0000 == 0 {
                // Skip the two-byte hint preceding the name.
                symbols.push(name_at(offset(entry) + 2));
            }
        }
        out.push(DllImports { name: name_at(offset(name_rva)), symbols });
    }
    out
}

#[test]
#[cfg(target_arch = "x86")]
fn the_game_host_is_large_address_aware() {
    let file = host();
    let nt = u32_at(&file, 0x3c);
    assert_eq!(&file[nt..nt + 4], b"PE\0\0");
    assert_eq!(u16_at(&file, nt + 4), 0x14c, "a 32-bit x86 host");
    assert_ne!(u16_at(&file, nt + 22) & 0x20, 0, "IMAGE_FILE_LARGE_ADDRESS_AWARE must be set (build.rs /LARGEADDRESSAWARE)");
}

#[test]
#[cfg(target_arch = "x86")]
fn the_game_host_needs_no_vc_redistributable() {
    let file = host();
    let dlls: Vec<_> = imports(&file).into_iter().map(|dll| dll.name).collect();
    assert!(dlls.iter().any(|d| d.eq_ignore_ascii_case("KERNEL32.dll")), "{dlls:?}");
    let dynamic_crt: Vec<_> = dlls.iter().filter(|d| {
        let d = d.to_ascii_lowercase();
        d.starts_with("vcruntime") || d.starts_with("msvcp") || d.starts_with("api-ms-win-crt") || d.starts_with("ucrtbase")
    }).collect();
    assert!(dynamic_crt.is_empty(), "static CRT expected (+crt-static), found {dynamic_crt:?}");
}

#[test]
#[cfg(target_arch = "x86")]
fn the_game_host_resolves_process_snapshotting_at_runtime() {
    let file = host();
    let imports = imports(&file);
    assert!(imports.iter().any(|dll| dll.name.eq_ignore_ascii_case("kernel32.dll")
        && dll.symbols.contains(&"CreateToolhelp32Snapshot")));
    let optional = [
        "PssCaptureSnapshot",
        "PssFreeSnapshot",
        "PssWalkMarkerCreate",
        "PssWalkMarkerFree",
        "PssWalkSnapshot",
    ];
    for dll in imports {
        for symbol in dll.symbols {
            assert!(!optional.contains(&symbol), "{} statically imports {symbol}", dll.name);
        }
    }
}
