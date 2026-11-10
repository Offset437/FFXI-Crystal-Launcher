//! Windower's existing startup data, carried unchanged from its own launcher.
//! This record has no account or authentication fields. Never print its contents:
//! user initialization commands and local paths are private configuration.

pub const SETTINGS_LEN: usize = 0xa4c4;
pub const MAP_NAME: &str = "WindowerMMFSettingsHandler";
pub const EVENT_NAME: &str = "windower.signal-injection";
/// The record's leading text fields Hook copies out first, as (byte offset,
/// UTF-16 units). Hook's own copy code must use exactly these.
const LEADING_FIELDS: [(usize, usize); 4] = [(0, 256), (512, 32), (576, 16), (608, 16)];

pub fn text(bytes: &[u8], offset: usize, units: usize) -> Option<String> {
    let field = bytes.get(offset..offset.checked_add(units.checked_mul(2)?)?)?;
    let chars: Vec<u16> = field
        .as_chunks::<2>().0.iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|&c| c != 0)
        .collect();
    if chars.len() == units {
        return None;
    }
    String::from_utf16(&chars).ok()
}

pub fn validate(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() != SETTINGS_LEN {
        return Err("Unexpected Windower startup record size.");
    }
    for (offset, units) in [
        (0, 256),
        (512, 32),
        (576, 16),
        (608, 16),
        (640, 4096),
        (8832, 4096),
        (17024, 4096),
        (25216, 16),
        (25284, 4096),
        (33476, 256),
        (33988, 4096),
    ] {
        text(bytes, offset, units).ok_or("Invalid Windower startup text field.")?;
    }
    if bytes[25248..25252] != 1u32.to_le_bytes() {
        return Err("This startup comparison requires the US region.");
    }
    for offset in [25252, 25256, 25260, 25264] {
        let value = i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        if !(1..=32768).contains(&value) {
            return Err("Invalid Windower startup dimensions.");
        }
    }
    if bytes[25276..25283].iter().any(|&b| b > 1) {
        return Err("Invalid Windower startup flags.");
    }
    // The version Windower wrote (4.7.9.3 today). The record layout is proven
    // from Hook's own code instead (`hook_startup`), so any Windower version
    // passes as long as this field holds one.
    if !text(bytes, 608, 16).is_some_and(|v| dotted_version(&v)) {
        return Err("Unsupported Windower startup version.");
    }
    Ok(())
}

fn dotted_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Where a Hook.dll file keeps its private-startup object names, found by
/// signature in the file so a Windower update that moves them does not switch
/// private startup off. Values are RVAs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookStartup {
    pub timestamp: u32,
    pub image_len: usize,
    pub map_name_rva: usize,
    pub event_name_rva: usize,
}

/// Builds whose values were proven by hand. The scan must reproduce them.
pub const KNOWN_HOOKS: [(&str, HookStartup); 2] = [
    ("4.7.9.3", HookStartup { timestamp: 0x6a5b_1a74, image_len: 0x23_0000, map_name_rva: 0x18_52d0, event_name_rva: 0x18_584c }),
    ("4.7.9.6", HookStartup { timestamp: 0x6ac4_f0dd, image_len: 0x23_0000, map_name_rva: 0x18_52e0, event_name_rva: 0x18_585c }),
];

// Hook's startup code (4.7.9.3: 10062c90, 10064dc4, 10064e03, 10067ae8).
// OpenFileMappingA(FILE_MAP_ALL_ACCESS, 0, MAP); MapViewOfFile; copy SETTINGS_LEN
// bytes to the stack, unmap, close, copy SETTINGS_LEN to the caller.
const STARTUP_MAP: &str = "68 ?? ?? ?? ?? 6A 00 68 1F 00 0F 00 FF 15 ?? ?? ?? ?? 8B F0 85 F6 74 ?? 6A 00 6A 00 6A 00 68 1F 00 0F 00 56 FF 15 ?? ?? ?? ?? 8B F8 85 FF 75 ?? 68 ?? ?? ?? ?? EB ?? 68 ?? ?? ?? ?? 8D 85 ?? ?? ?? ?? 57 50 E8 ?? ?? ?? ?? 83 C4 0C 57 FF 15 ?? ?? ?? ?? 56 FF 15 ?? ?? ?? ?? 8B 75 08 8D 85 ?? ?? ?? ?? 68 ?? ?? ?? ?? 50 56 E8";
/// That reader's prologue (stack probe for the record), ending at STARTUP_MAP.
const STARTUP_MAP_PROLOGUE: &str = "55 8B EC B8 ?? ?? 00 00 E8 ?? ?? ?? ?? 56 57";
// Its one caller: reader(&tmp, SETTINGS_LEN); copy to a second buffer; copy
// SETTINGS_LEN to the record Hook keeps.
const STARTUP_READ: &str = "8D 85 ?? ?? ?? ?? 68 ?? ?? 00 00 50 E8 ?? ?? ?? ?? 83 C4 04 50 8D 85 ?? ?? ?? ?? 50 E8 ?? ?? ?? ?? 68 ?? ?? 00 00 8D 85 ?? ?? ?? ?? 50 8D 85 ?? ?? ?? ?? 50 E8";
// Then the first four text fields are converted out of that record.
const STARTUP_FIELDS: &str = "68 00 01 00 00 8D 85 ?? ?? ?? ?? 83 C6 10 50 56 E8 ?? ?? ?? ?? 6A 20 8D 85 ?? ?? ?? ?? 50 8D 86 00 01 00 00 50 E8 ?? ?? ?? ?? 6A 10 8D 85 ?? ?? ?? ?? 50 8D 86 30 01 00 00 50 E8 ?? ?? ?? ?? 6A 10 8D 85 ?? ?? ?? ?? 50 8D 86 40 01 00 00 50 E8";
/// The record's frame displacements in STARTUP_FIELDS, in LEADING_FIELDS order.
const FIELD_DISPLACEMENTS: [usize; 4] = [7, 25, 46, 67];
// CreateEventA(0, FALSE, FALSE, EVENT): the completion signal.
const STARTUP_EVENT: &str = "68 ?? ?? ?? ?? 6A 00 6A 00 6A 00 FF 15 ?? ?? ?? ?? 85 C0 0F 84";

const MAX_HOOK_IMAGE: usize = 16 * 1024 * 1024;
const SCN_EXECUTE: u32 = 0x2000_0000;

/// A PE file laid out at its RVAs, unrelocated (operands hold `image_base + rva`).
pub struct MappedPe {
    pub image: Vec<u8>,
    pub image_base: usize,
    pub timestamp: u32,
    /// (name, rva, virtual size, flags)
    pub sections: Vec<([u8; 8], usize, usize, u32)>,
    pub imports: (usize, usize),
    pub iat: (usize, usize),
}

fn le16(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize)
}
fn le32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
}

/// Maps a 32-bit x86 PE file the way the loader lays out its sections.
pub fn map_pe(file: &[u8]) -> Result<MappedPe, &'static str> {
    const BAD: &str = "not a 32-bit x86 PE file";
    let nt = le32(file, 0x3c).ok_or(BAD)?;
    if file.get(..2) != Some(b"MZ") || file.get(nt..nt + 4) != Some(b"PE\0\0") || le16(file, nt + 4) != Some(0x14c) {
        return Err(BAD);
    }
    let opt = nt + 24;
    if le16(file, opt) != Some(0x10b) {
        return Err(BAD);
    }
    let count = le16(file, nt + 6).ok_or(BAD)?;
    let optional = le16(file, nt + 20).ok_or(BAD)?;
    let size = le32(file, opt + 56).ok_or(BAD)?;
    let headers = le32(file, opt + 60).ok_or(BAD)?;
    if size == 0 || size > MAX_HOOK_IMAGE || headers > size || headers > file.len() || !(1..=96).contains(&count) {
        return Err("implausible PE layout");
    }
    let directory = |index: usize| -> (usize, usize) {
        if index >= le32(file, opt + 92).unwrap_or(0) || 96 + index * 8 + 8 > optional {
            return (0, 0);
        }
        (le32(file, opt + 96 + index * 8).unwrap_or(0), le32(file, opt + 100 + index * 8).unwrap_or(0))
    };
    let mut image = vec![0u8; size];
    image[..headers].copy_from_slice(&file[..headers]);
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        let h = opt + optional + index * 40;
        let header = file.get(h..h + 40).ok_or(BAD)?;
        let (vsize, rva, raw_size, raw) = (le32(header, 8).unwrap(), le32(header, 12).unwrap(), le32(header, 16).unwrap(), le32(header, 20).unwrap());
        let flags = le32(header, 36).unwrap() as u32;
        let span = if vsize == 0 { raw_size } else { vsize };
        if rva.checked_add(span).is_none_or(|end| end > size) {
            return Err("a section lies outside SizeOfImage");
        }
        let copy = raw_size.min(span);
        let source = file.get(raw..raw.checked_add(copy).ok_or(BAD)?).ok_or("a section lies outside the file")?;
        image[rva..rva + copy].copy_from_slice(source);
        sections.push((header[..8].try_into().unwrap(), rva, span, flags));
    }
    Ok(MappedPe {
        image,
        image_base: le32(file, opt + 28).ok_or(BAD)?,
        timestamp: le32(file, nt + 8).ok_or(BAD)? as u32,
        sections,
        imports: directory(1),
        iat: directory(12),
    })
}

/// [`map_pe`], then relocated for where its buffer lies (tests that run or
/// re-scan the image at a non-preferred base). Only HIGHLOW fixups exist.
#[cfg(test)]
pub(crate) fn map_pe_relocated(file: &[u8]) -> Vec<u8> {
    let pe = map_pe(file).unwrap();
    let mut image = pe.image;
    let nt = le32(&image, 0x3C).unwrap();
    let delta = (image.as_ptr() as u32).wrapping_sub(pe.image_base as u32);
    let mut at = le32(&image, nt + 24 + 96 + 5 * 8).unwrap();
    let end = at + le32(&image, nt + 24 + 100 + 5 * 8).unwrap();
    while at < end {
        let (page, size) = (le32(&image, at).unwrap(), le32(&image, at + 4).unwrap());
        assert!(size >= 8);
        for offset in (at + 8..at + size).step_by(2) {
            let entry = le16(&image, offset).unwrap();
            if entry >> 12 == 3 {
                let target = page + (entry & 0xFFF);
                let value = (le32(&image, target).unwrap() as u32).wrapping_add(delta);
                image[target..target + 4].copy_from_slice(&value.to_le_bytes());
            } else {
                assert_eq!(entry >> 12, 0);
            }
        }
        at += size;
    }
    // Moving the Vec keeps its heap buffer, so the fixups stay valid.
    image
}

impl MappedPe {
    fn section(&self, name: &str) -> Option<&([u8; 8], usize, usize, u32)> {
        self.sections.iter().find(|s| s.0.split(|b| *b == 0).next() == Some(name.as_bytes()))
    }

    /// `.text`, as (rva, bytes).
    fn text(&self) -> Result<(usize, &[u8]), &'static str> {
        let &(_, rva, len, _) = self.section(".text").filter(|s| s.3 & SCN_EXECUTE != 0).ok_or("no executable .text")?;
        Ok((rva, self.image.get(rva..rva + len).ok_or("no executable .text")?))
    }

    fn u32(&self, rva: usize) -> Option<usize> {
        le32(&self.image, rva)
    }

    /// The absolute operand at `rva`, as an RVA inside the image.
    fn abs(&self, rva: usize) -> Option<usize> {
        self.u32(rva)?.checked_sub(self.image_base).filter(|t| *t < self.image.len())
    }

    fn c_str(&self, rva: usize, max: usize) -> Option<&[u8]> {
        let bytes = self.image.get(rva..)?;
        let len = bytes.iter().take(max + 1).position(|b| *b == 0)?;
        Some(&bytes[..len])
    }

    /// The operand at `rva` is the IAT slot importing `dll!function` by name.
    fn imports(&self, rva: usize, dll: &str, function: &str) -> bool {
        let Some(slot) = self.abs(rva) else { return false };
        let (iat, iat_len) = self.iat;
        if slot < iat || slot + 4 > iat + iat_len {
            return false;
        }
        let (directory, size) = self.imports;
        for at in (directory..directory + size).step_by(20) {
            let (Some(names), Some(name), Some(first)) = (self.u32(at), self.u32(at + 12), self.u32(at + 16)) else { return false };
            if name == 0 || first == 0 {
                return false;
            }
            if names == 0 || slot < first || (slot - first) % 4 != 0 {
                continue;
            }
            let index = (slot - first) / 4;
            if (0..=index).any(|i| self.u32(names + i * 4).is_none_or(|e| e == 0)) {
                continue;
            }
            let entry = self.u32(names + index * 4).unwrap();
            return entry & 0x8000_0000 == 0
                && self.c_str(name, 64).is_some_and(|d| d.eq_ignore_ascii_case(dll.as_bytes()))
                && self.c_str(entry + 2, 128) == Some(function.as_bytes());
        }
        false
    }
}

fn pattern(text: &str) -> Vec<Option<u8>> {
    text.split_ascii_whitespace().map(|t| if t == "??" { None } else { u8::from_str_radix(t, 16).ok() }).collect()
}

fn pattern_at(hay: &[u8], at: usize, pattern: &[Option<u8>]) -> bool {
    hay.get(at..at + pattern.len()).is_some_and(|w| w.iter().zip(pattern).all(|(b, p)| p.is_none_or(|p| p == *b)))
}

/// Up to three match offsets of `text` in `hay` (enough to tell one from many).
fn find(hay: &[u8], text: &str) -> Vec<usize> {
    let p = pattern(text);
    let (lead, first) = p.iter().enumerate().find_map(|(i, b)| b.map(|b| (i, b))).expect("a fixed byte");
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(next) = hay.get(from + lead..).and_then(|h| h.iter().position(|b| *b == first)) {
        let at = from + next;
        if pattern_at(hay, at, &p) {
            hits.push(at);
            if hits.len() == 3 {
                break;
            }
        }
        from = at + 1;
    }
    hits
}

/// Finds Hook's private-startup contract in a Hook.dll file: the two object
/// names (each referenced exactly once, by the code that opens that object,
/// in a non-executable section), and proof from Hook's own code that it reads
/// a SETTINGS_LEN record whose leading fields sit where `validate` expects.
/// A known build must also reproduce its proven values. `Err`: unsupported.
pub fn hook_startup(file: &[u8]) -> Result<HookStartup, &'static str> {
    let pe = map_pe(file)?;
    let (text_rva, text) = pe.text()?;
    let unique = |name: &'static str, text_pattern: &str| -> Result<usize, &'static str> {
        match find(text, text_pattern)[..] {
            [only] => Ok(text_rva + only),
            _ => Err(name),
        }
    };
    let map = unique("Hook's startup-record reader was not found exactly once", STARTUP_MAP)?;
    let read = unique("Hook's startup-record copy was not found exactly once", STARTUP_READ)?;
    let fields = unique("Hook's startup-field copy was not found exactly once", STARTUP_FIELDS)?;
    let event = unique("Hook's startup completion event was not found exactly once", STARTUP_EVENT)?;
    let kernel = |at: usize, function: &str| pe.imports(at, "KERNEL32.dll", function);
    if !(kernel(map + 14, "OpenFileMappingA") && kernel(map + 38, "MapViewOfFile") && kernel(map + 79, "UnmapViewOfFile")
        && kernel(map + 86, "CloseHandle") && kernel(event + 13, "CreateEventA"))
    {
        return Err("Hook's startup objects are not opened through the expected imports");
    }
    let sizes = [pe.u32(map + 56), pe.u32(map + 100), pe.u32(read + 7), pe.u32(read + 34)];
    if sizes.iter().any(|s| *s != Some(SETTINGS_LEN)) {
        return Err("Hook reads a startup record of another size");
    }
    let reader = (read + 17).checked_add_signed(pe.u32(read + 13).ok_or("unreadable")? as u32 as i32 as isize);
    if reader.and_then(|r| r.checked_add(15)) != Some(map) || !pattern_at(&pe.image, map - 15, &pattern(STARTUP_MAP_PROLOGUE)) {
        return Err("Hook's startup-record copy does not call its reader");
    }
    let record = pe.u32(read + 47).ok_or("unreadable")?;
    let offsets = FIELD_DISPLACEMENTS.map(|at| pe.u32(fields + at).map(|d| d.wrapping_sub(record) as u32 as usize));
    if !(read < fields && fields < read + 0x100) || offsets.iter().zip(LEADING_FIELDS).any(|(got, (want, _))| *got != Some(want)) {
        return Err("Hook's startup record has another field layout");
    }
    let mut names = [0usize; 2];
    for (slot, (site, name)) in names.iter_mut().zip([(map, MAP_NAME), (event, EVENT_NAME)]) {
        let rva = pe.abs(site + 1).ok_or("a startup name is outside the image")?;
        if pe.c_str(rva, name.len()) != Some(name.as_bytes()) {
            return Err("a startup name is not where Hook's code reads it");
        }
        let in_data = pe.sections.iter().any(|&(_, start, len, flags)| {
            flags & SCN_EXECUTE == 0 && rva >= start && rva + name.len() < start + len
        });
        if !in_data {
            return Err("a startup name is not in a data section");
        }
        // Replacing the name must change only this one use of it.
        let va = ((pe.image_base + rva) as u32).to_le_bytes();
        if text.windows(4).filter(|w| *w == va).count() != 1 {
            return Err("a startup name has other uses in Hook's code");
        }
        *slot = rva;
    }
    let found = HookStartup { timestamp: pe.timestamp, image_len: pe.image.len(), map_name_rva: names[0], event_name_rva: names[1] };
    match KNOWN_HOOKS.iter().find(|(_, k)| (k.timestamp, k.image_len) == (found.timestamp, found.image_len)) {
        Some((_, known)) if *known != found => Err("Hook's startup scan disagrees with this build's proven values"),
        _ => Ok(found),
    }
}

/// The proven version label of a known build, for logs.
pub fn known_hook(startup: &HookStartup) -> Option<&'static str> {
    KNOWN_HOOKS.iter().find(|(_, k)| k == startup).map(|(label, _)| *label)
}

pub fn private_names(nonce: &str) -> Option<(String, String)> {
    if nonce.len() != 24 || !nonce.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((format!("m{nonce}"), format!("e{nonce}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn synthetic(name: &str, width: i32) -> Vec<u8> {
        let mut b = vec![0; SETTINGS_LEN];
        let mut set = |offset, value: &str| {
            for (i, c) in value.encode_utf16().enumerate() {
                b[offset + i * 2..offset + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
            }
        };
        set(0, name);
        set(512, "stable");
        set(576, "4.7.9.3");
        set(608, "4.7.9.3");
        set(640, r"C:\Viewer\");
        set(8832, r"C:\Game\");
        set(17024, r"C:\Windower\");
        set(25216, "Insert");
        b[25248..25252].copy_from_slice(&1u32.to_le_bytes());
        for (offset, n) in [(25252, width), (25256, 720), (25260, width), (25264, 720)] {
            b[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
        }
        b
    }
    #[test]
    fn validates_the_existing_layout_without_changing_it() {
        let a = synthetic("First", 1280);
        let b = synthetic("Second", 1920);
        assert!(validate(&a).is_ok() && validate(&b).is_ok());
        assert_eq!(text(&a, 0, 256).as_deref(), Some("First"));
        assert_ne!(a, b);
        let mut invalid = a;
        invalid[25276] = 2;
        assert!(validate(&invalid).is_err());
        assert!(validate(&invalid[..100]).is_err());
    }
    #[test]
    fn any_windower_version_passes_but_the_field_must_hold_one() {
        for version in ["4.7.9.3", "4.7.9.6", "4.8.0.0", "5.0"] {
            let mut b = synthetic("Main", 1280);
            b[608..640].fill(0);
            for (i, c) in version.encode_utf16().enumerate() {
                b[608 + i * 2..610 + i * 2].copy_from_slice(&c.to_le_bytes());
            }
            assert!(validate(&b).is_ok(), "{version}");
        }
        for bad in ["", "4", "4.7.9.x", "4..9", "stable"] {
            let mut b = synthetic("Main", 1280);
            b[608..640].fill(0);
            for (i, c) in bad.encode_utf16().enumerate() {
                b[608 + i * 2..610 + i * 2].copy_from_slice(&c.to_le_bytes());
            }
            assert!(validate(&b).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn startup_patterns_parse_and_a_non_hook_file_is_refused() {
        for text in [STARTUP_MAP, STARTUP_MAP_PROLOGUE, STARTUP_READ, STARTUP_FIELDS, STARTUP_EVENT] {
            assert!(pattern(text).iter().filter(|b| b.is_none()).count() < pattern(text).len());
            assert!(!text.split_ascii_whitespace().any(|t| t != "??" && u8::from_str_radix(t, 16).is_err()), "{text}");
        }
        assert!(pattern_at(&[0x68, 1, 2], 0, &pattern("68 ?? 02")));
        assert!(!pattern_at(&[0x68, 1], 0, &pattern("68 ?? 02")), "never past the end");
        assert_eq!(find(&[0, 0x68, 9, 2, 0x68, 8, 2, 0x68], "68 ?? 02"), vec![1, 4]);
        assert!(hook_startup(b"MZ not a PE").is_err());
        assert!(hook_startup(&[]).is_err());
        // This test executable is a valid x86 PE, but not Hook.
        let own = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        if map_pe(&own).is_ok() {
            assert!(hook_startup(&own).is_err());
        }
    }

    #[test]
    fn known_hook_values_fit_the_private_names() {
        for (label, known) in KNOWN_HOOKS {
            assert_eq!(known_hook(&known), Some(label));
            assert_eq!(known.image_len, 0x23_0000);
            assert_eq!(known.event_name_rva - known.map_name_rva, 0x57c, "{label}");
        }
        assert_eq!(known_hook(&HookStartup { map_name_rva: 0, ..KNOWN_HOOKS[0].1 }), None);
    }

    #[test]
    fn private_names_fit_both_existing_slots_and_are_distinct() {
        let (map, event) = private_names("0123456789abcdef01234567").unwrap();
        assert!(map.len() <= MAP_NAME.len() && event.len() <= EVENT_NAME.len());
        assert_ne!(map, event);
        assert_ne!(private_names("1123456789abcdef01234567").unwrap().0, map);
        assert!(private_names("bad").is_none());
    }
}
