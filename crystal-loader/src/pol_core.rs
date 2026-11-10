//! The PlayOnline core contract Crystal relies on, checked by structure on
//! whatever polcore build is installed. No build stamp and no address is
//! pinned: Square Enix moved every function in the 1.18.15f rebuild
//! (2026-11-09) while keeping the interface, and the interface is what the
//! unchanged FFXi.dll / FFXiMain and the Viewer's own app.dll call.
//!
//! What is checked, before any core code runs that Crystal did not already
//! rely on COM for:
//! - the IPOLCoreCom vtable lies in polcore's non-executable data and every method
//!   Crystal calls points into one of polcore's executable sections;
//! - the common function table that GetCommonFunctionTable returns lies in
//!   polcore's writable, non-executable data and has the known shape: 1,560
//!   entries, the same 899 code pointers and 661 null entries in both verified
//!   builds (1.18.15e and 1.18.15f), each code pointer inside executable code;
//! - every service Crystal calls (`SERVICES`) is present there. The argument
//!   count Crystal passes to each one is recorded here and enforced by
//!   `native_auth::Api` in debug builds, so the registry and the calls cannot
//!   drift apart. Both real builds were cross-checked offline (ignored tests).
//!
//! An unknown build is accepted when every check passes, and logged as such.
//! A failed check stops the native host before its first service call and
//! names the reason; nothing is patched first.

use crate::sigscan::{Headers, Section, SCN_EXECUTE};

/// Entries in polcore's common function table (both verified builds).
pub(crate) const TABLE_LEN: usize = 1560;

const SCN_WRITE: u32 = 0x8000_0000;

/// Bit `n` (LSB first): entry `n` is a code pointer in both verified builds.
const SHAPE: [u8; 195] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe0,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x3f, 0xf8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x0f, 0xbc, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x1f, 0xf8, 0xff, 0xf3, 0xff, 0x02, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0,
    0xff, 0xff, 0xff, 0x3f, 0x00, 0x00, 0xff, 0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff,
    0xff, 0x01, 0x00, 0x00, 0xf0, 0xf3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xff,
    0xf7, 0xfd, 0xff, 0xff, 0xff, 0x03, 0x00, 0x00, 0xe0, 0x07, 0xf0, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x0f, 0xff, 0xc3, 0x1f, 0xfc, 0xf5, 0xdf, 0xff, 0xff, 0xc7, 0x6f, 0xf3, 0xff, 0xf9, 0xf9, 0xf9,
    0xcf, 0xff, 0xff, 0x01, 0xa0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x07, 0x36, 0xde,
    0xfd, 0xff, 0xcf, 0xff, 0xff, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x07, 0x80, 0x7f, 0x78, 0x00, 0xff, 0xff, 0xe3, 0x7f, 0xfc, 0xff, 0xfd, 0x03, 0x80, 0x83,
    0x03, 0x80, 0x81,
];
/// Code pointers in [`SHAPE`].
pub(crate) const SHAPE_CODE: usize = 899;
/// Known code entries that may be absent or moved out of code before the table
/// stops being recognisable. Services Crystal calls never get this tolerance.
const SHAPE_TOLERANCE: usize = 9;

pub(crate) fn shape_has_code(slot: usize) -> bool {
    slot < TABLE_LEN && SHAPE[slot / 8] >> (slot % 8) & 1 != 0
}

/// Builds whose contract was verified by hand. A label for the log only; the
/// structural checks decide either way.
pub(crate) const KNOWN: [(u32, &str); 2] = [(0x4E4E_5917, "1.18.15e"), (0x6ABA_1641, "1.18.15f")];

pub(crate) fn known(timestamp: u32) -> Option<&'static str> {
    KNOWN.iter().find(|(stamp, _)| *stamp == timestamp).map(|(_, label)| *label)
}

/// What a service is used for; a failed check names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    SignIn,
    Friends,
    Mail,
    Handles,
    Game,
    /// Pilot diagnostics builds only (native-backup-diagnostics).
    Backup,
    /// Ignored offline ABI tests only; never required in production.
    Offline,
}

impl Group {
    pub(crate) fn feature(self) -> &'static str {
        match self {
            Group::SignIn => "PlayOnline sign-in",
            Group::Friends => "Friends and status",
            Group::Mail => "PlayOnline mail",
            Group::Handles => "Handle setup",
            Group::Game => "Game exit result",
            Group::Backup => "Server settings trace",
            Group::Offline => "Offline ABI check",
        }
    }
}

/// One common-table service Crystal calls: its slot, the number of word
/// arguments Crystal passes (cdecl, the caller pops), and its use. `args` is
/// `None` for an entry Crystal hands back to the core as a callback value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Service {
    pub slot: u16,
    pub args: Option<u8>,
    pub group: Group,
}

const fn s(slot: u16, args: u8, group: Group) -> Service { Service { slot, args: Some(args), group } }
const fn value(slot: u16, group: Group) -> Service { Service { slot, args: None, group } }

use Group::*;
/// Every service Crystal calls, sorted by slot (binary-searched by `service`).
pub(crate) const SERVICES: &[Service] = &[
    s(125, 1, SignIn), s(126, 4, Mail),
    s(163, 0, Friends), s(164, 1, Friends), s(167, 2, Friends), s(169, 2, Friends),
    s(171, 0, Friends), s(172, 1, Friends), s(173, 2, Friends), s(174, 2, Handles), s(175, 0, Friends), s(176, 1, Friends),
    s(177, 2, Handles), s(183, 2, Handles), s(186, 1, Friends), s(188, 1, Friends), s(190, 0, SignIn),
    s(197, 3, Handles), s(206, 0, SignIn), s(207, 0, SignIn),
    s(270, 13, Mail), s(272, 5, Mail), s(284, 2, Mail), s(285, 2, Mail), s(286, 2, Mail), s(287, 2, Mail),
    s(288, 1, SignIn), s(291, 2, Mail), s(316, 3, Mail),
    s(322, 1, Mail), s(323, 1, Mail), s(324, 1, Mail), s(325, 1, Mail), s(326, 1, Mail), s(327, 1, Mail),
    s(328, 1, Mail), s(329, 2, Mail), s(330, 1, Mail), s(331, 1, Mail), s(333, 1, Mail), s(334, 1, SignIn),
    s(337, 1, SignIn), s(342, 0, Mail), s(367, 1, SignIn),
    s(374, 0, SignIn), s(375, 1, SignIn), s(376, 6, SignIn), s(377, 1, SignIn),
    s(383, 0, Friends), s(384, 1, Friends), s(385, 0, Friends), s(394, 2, Friends),
    value(401, SignIn), s(404, 1, Friends), s(405, 1, Friends),
    s(421, 0, Offline), s(434, 0, SignIn), s(463, 0, SignIn), s(464, 1, Backup), s(465, 1, Backup),
    s(807, 0, SignIn), s(809, 0, SignIn), s(814, 2, SignIn), s(815, 0, SignIn), s(816, 1, SignIn),
    s(817, 0, SignIn), s(818, 0, SignIn), s(819, 1, SignIn), s(823, 0, SignIn), s(824, 3, SignIn),
    s(825, 2, SignIn), s(826, 2, SignIn), s(828, 0, SignIn), s(835, 0, SignIn), s(838, 0, SignIn),
    s(845, 0, SignIn), s(846, 1, SignIn), s(849, 1, SignIn), s(851, 1, SignIn), s(852, 2, SignIn),
    s(853, 1, SignIn), s(854, 2, SignIn), s(856, 2, SignIn), s(858, 2, SignIn), s(868, 2, SignIn),
    s(870, 1, SignIn), s(871, 1, SignIn), s(872, 0, SignIn), s(884, 0, SignIn), s(886, 1, SignIn),
    s(891, 0, SignIn), s(892, 2, SignIn),
    s(914, 0, SignIn), s(915, 0, SignIn), s(920, 0, SignIn),
    s(921, 2, Offline), s(922, 1, Offline), s(923, 1, Offline), s(928, 3, Offline), s(929, 2, Offline),
    s(932, 0, SignIn), s(965, 1, SignIn), s(969, 1, SignIn), s(970, 0, Game),
    s(1027, 0, Game), s(1032, 1, SignIn), s(1034, 0, Friends), s(1035, 1, Friends),
    s(1036, 0, Handles), s(1037, 1, Handles), s(1038, 0, Handles), s(1039, 1, Handles),
    s(1072, 2, SignIn), s(1074, 0, SignIn), s(1075, 1, SignIn), s(1103, 0, SignIn), s(1107, 1, SignIn),
    s(1140, 1, SignIn), s(1149, 2, SignIn), s(1175, 1, SignIn), s(1302, 2, SignIn),
    s(1304, 0, Mail), s(1305, 1, Mail), s(1309, 1, SignIn), s(1502, 4, SignIn), s(1527, 1, SignIn),
    s(1528, 1, SignIn), s(1535, 4, Backup), s(1536, 3, Backup),
];

pub(crate) fn service(slot: usize) -> Option<&'static Service> {
    let slot = u16::try_from(slot).ok()?;
    SERVICES.binary_search_by_key(&slot, |s| s.slot).ok().map(|i| &SERVICES[i])
}

/// Services a production host requires. Pilot builds add their own.
fn required(group: Group) -> bool {
    match group {
        Offline => false,
        Backup => cfg!(feature = "native-backup-diagnostics"),
        _ => true,
    }
}

/// IPOLCoreCom methods Crystal calls (vtable index, name).
pub(crate) const COM_METHODS: [(usize, &str); 7] = [
    (0, "QueryInterface"), (1, "AddRef"), (2, "Release"), (5, "SetParamInit"),
    (7, "GetCommonFunctionTable"), (8, "PolViewerExec"), (27, "SetAreaCode"),
];
/// Words of the vtable that must be readable.
pub(crate) const COM_VTABLE_WORDS: usize = 28;

/// polcore's image layout: its base and section table.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub base: usize,
    pub timestamp: u32,
    pub sections: Vec<Section>,
}

impl Layout {
    pub(crate) fn new(base: usize, headers: Headers) -> Self {
        Self { base, timestamp: headers.timestamp, sections: headers.sections }
    }

    /// The image mapped at `base` in this process (checked reads).
    pub(crate) fn live(base: usize) -> Result<Self, String> {
        let mut page = vec![0u8; 0x1000];
        if base == 0 || !crate::mem::read_into(base, &mut page) {
            return Err("the PlayOnline core's headers cannot be read".into());
        }
        let headers = crate::sigscan::parse_headers(&page).map_err(|e| format!("the PlayOnline core's headers are invalid ({e})"))?;
        Ok(Self::new(base, headers))
    }

    fn section_of(&self, address: usize, len: usize) -> Option<&Section> {
        let rva = address.checked_sub(self.base)?;
        let end = rva.checked_add(len)?;
        self.sections.iter().find(|s| rva >= s.rva && end <= s.rva.saturating_add(s.size))
    }

    /// `address` is inside one of the image's executable sections.
    pub(crate) fn code(&self, address: usize) -> bool {
        self.section_of(address, 1).is_some_and(|s| s.flags & SCN_EXECUTE != 0)
    }

    /// `[address, address + len)` lies in one writable, non-executable section.
    pub(crate) fn writable_data(&self, address: usize, len: usize) -> bool {
        self.section_of(address, len).is_some_and(|s| s.flags & SCN_WRITE != 0 && s.flags & SCN_EXECUTE == 0)
    }

    /// `[address, address + len)` lies in one non-executable section.
    pub(crate) fn data(&self, address: usize, len: usize) -> bool {
        self.section_of(address, len).is_some_and(|s| s.flags & SCN_EXECUTE == 0)
    }

    pub(crate) fn name(&self) -> String {
        match known(self.timestamp) {
            Some(label) => format!("{:08X} (verified {label})", self.timestamp),
            None => format!("{:08X} (unknown build, accepted by structure)", self.timestamp),
        }
    }
}

/// The COM interface: `vtable` is the object's vtable address and `methods`
/// its first [`COM_VTABLE_WORDS`] words.
pub(crate) fn check_com(layout: &Layout, vtable: usize, methods: &[usize]) -> Result<(), String> {
    let words = COM_VTABLE_WORDS * std::mem::size_of::<u32>();
    if !vtable.is_multiple_of(4) || !layout.data(vtable, words) || methods.len() < COM_VTABLE_WORDS {
        return Err(format!("PlayOnline core check failed: its COM vtable {vtable:#x} is not in the core's data."));
    }
    for (index, name) in COM_METHODS {
        if !layout.code(methods[index]) {
            return Err(format!("PlayOnline core check failed: COM method {name} (vtable {index}) does not point into the core's code."));
        }
    }
    Ok(())
}

/// The result of a passed table check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Table {
    /// Table RVA.
    pub rva: usize,
    /// Known code entries that are absent now (within tolerance; none Crystal calls).
    pub missing: Vec<usize>,
    /// Entries that are code now and null in the verified builds.
    pub added: usize,
    /// Required services that were checked.
    pub services: usize,
}

impl Table {
    pub(crate) fn summary(&self, layout: &Layout) -> String {
        let shape = if self.missing.is_empty() && self.added == 0 {
            format!("shape {SHAPE_CODE}/{SHAPE_CODE}")
        } else {
            format!("shape {}/{SHAPE_CODE}, {} new entries, absent {:?}", SHAPE_CODE - self.missing.len(), self.added, self.missing)
        };
        format!("POL check: polcore {}: COM ok, table {TABLE_LEN} entries at +{:#x} (writable data), {shape}, {} services present: OK",
            layout.name(), self.rva, self.services)
    }
}

/// A failed service: which one, and why.
fn service_error(service: &Service, why: &str) -> String {
    format!("PlayOnline core check failed: service {} ({}) {why}.", service.slot, service.group.feature())
}

/// The common function table at `address`, `table` being its [`TABLE_LEN`] words.
pub(crate) fn check_table(layout: &Layout, address: usize, table: &[usize]) -> Result<Table, String> {
    let bytes = TABLE_LEN * std::mem::size_of::<u32>();
    if table.len() != TABLE_LEN || !address.is_multiple_of(4) || !layout.writable_data(address, bytes) {
        return Err(format!("PlayOnline core check failed: the function table {address:#x} is not in the core's writable data."));
    }
    for service in SERVICES.iter().filter(|s| required(s.group)) {
        let entry = table[usize::from(service.slot)];
        if entry == 0 {
            return Err(service_error(service, "is missing from the function table"));
        }
        if !layout.code(entry) {
            return Err(service_error(service, "does not point into the core's code"));
        }
    }
    let mut missing = Vec::new();
    let mut added = 0;
    for (slot, &entry) in table.iter().enumerate() {
        let code = entry != 0 && layout.code(entry);
        match (shape_has_code(slot), code) {
            (true, false) => missing.push(slot),
            (false, true) => added += 1,
            (false, false) if entry != 0 => missing.push(slot),
            _ => {}
        }
    }
    if missing.len() > SHAPE_TOLERANCE {
        return Err(format!("PlayOnline core check failed: the function table does not have PlayOnline's shape ({} of {SHAPE_CODE} known entries differ).", missing.len()));
    }
    let services = SERVICES.iter().filter(|s| required(s.group)).count();
    Ok(Table { rva: address - layout.base, missing, added, services })
}

/// Slot 463 (backup cancel) ends `push <backup context>; call; pop ecx; ret`
/// in both verified builds; the pushed address is the core's backup context,
/// whose first word is its state. Found by that shape and checked to lie in
/// writable data. `code` is the first bytes of slot 463.
#[cfg(any(test, feature = "native-backup-diagnostics"))]
pub(crate) fn backup_state(layout: &Layout, code: &[u8]) -> Result<usize, String> {
    let pattern = crate::sigscan::Pattern::parse("68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 59 C3").expect("valid pattern");
    let hits = crate::sigscan::scan(code, std::slice::from_ref(&pattern));
    let [at] = hits[0][..] else {
        return Err(format!("backup context: {} matches in service 463", hits[0].len()));
    };
    let address = u32::from_le_bytes(code[at + 1..at + 5].try_into().unwrap()) as usize;
    if !layout.writable_data(address, 4) {
        return Err("backup context is not in the core's writable data".into());
    }
    Ok(address)
}

/// The installed Viewer's frame period (µs), read from its app.dll file. The
/// 1.18.15f ticker raises the timer resolution with winmm's timeBeginPeriod,
/// which the 1.18.15e app.dll never imports (its import table stays readable
/// even where the code is packed). Err when the file is not a readable PE.
pub(crate) fn viewer_ticker(app_dll: &[u8]) -> Result<u64, String> {
    let imports = file_imports(app_dll).ok_or("app.dll is not a readable 32-bit PE file")?;
    let fast = imports.iter().any(|(dll, name)| dll.eq_ignore_ascii_case(b"winmm.dll") && name == b"timeBeginPeriod");
    Ok(if fast { crate::native_jobs::FAST_STOCK_FRAME_US } else { crate::native_jobs::LEGACY_STOCK_FRAME_US })
}

/// Every by-name import (dll, function) of a 32-bit PE file, read from the file.
fn file_imports(file: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(file.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize);
    let u32_at = |at: usize| Some(u32::from_le_bytes(file.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize);
    let nt = u32_at(0x3c)?;
    if file.get(..2)? != b"MZ" || file.get(nt..nt + 4)? != b"PE\0\0" || u16_at(nt + 4)? != 0x14c || u16_at(nt + 24)? != 0x10b {
        return None;
    }
    let count = u16_at(nt + 6)?;
    let table = nt + 24 + u16_at(nt + 20)?;
    let sections: Vec<(usize, usize, usize)> = (0..count).map(|i| {
        let at = table + i * 40;
        Some((u32_at(at + 12)?, u32_at(at + 8)?.max(u32_at(at + 16)?), u32_at(at + 20)?))
    }).collect::<Option<_>>()?;
    let offset = |rva: usize| sections.iter().find(|(start, size, _)| rva >= *start && rva < start + size).map(|(start, _, raw)| rva - start + raw);
    let c_str = |rva: usize| -> Option<Vec<u8>> {
        let at = offset(rva)?;
        let bytes = file.get(at..)?;
        Some(bytes[..bytes.iter().take(256).position(|&b| b == 0)?].to_vec())
    };
    let directory = u32_at(nt + 24 + 104)?;
    let mut out = Vec::new();
    for index in 0..4096 {
        let at = offset(directory + index * 20)?;
        let (names, dll) = (u32_at(at)?, u32_at(at + 12)?);
        let first = u32_at(at + 16)?;
        if dll == 0 && first == 0 { break; }
        let dll = c_str(dll)?;
        let thunks = if names != 0 { names } else { first };
        for entry in 0..65536 {
            let value = u32_at(offset(thunks + entry * 4)?)?;
            if value == 0 { break; }
            if value & 0x8000_0000 == 0 {
                out.push((dll.clone(), c_str(value + 2)?));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests;
