//! Wildcard byte-pattern resolution inside a loaded game module.
//!
//! Read-only. [`Image::capture`] copies the module's header page, `.text`,
//! `.rdata` and initialized `.data` while this thread holds the loader lock, so
//! no DLL entry point (FFXiMain's unpacking stub runs as one) can be part-way
//! through writing them. Everything else works on that private copy and yields
//! RVAs; nothing here dereferences game memory in Rust, writes, hooks or calls
//! game code. Callers decide what a resolved value may be used for.

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

pub(crate) const SCN_EXECUTE: u32 = 0x2000_0000;
const HEADER_PAGE: usize = 0x1000;
/// Enough to tell "unique" from "ambiguous"; a pattern never needs more.
const MAX_HITS: usize = 4;

/// `"A1 ?? 85 C0"`: hexadecimal bytes; `??` matches any byte.
#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    bytes: Box<[u8]>,
    fixed: Box<[bool]>,
}

impl Pattern {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (mut bytes, mut fixed) = (Vec::new(), Vec::new());
        for token in text.split_ascii_whitespace() {
            if token == "??" {
                bytes.push(0);
                fixed.push(false);
            } else if token.len() == 2 {
                bytes.push(u8::from_str_radix(token, 16).ok()?);
                fixed.push(true);
            } else {
                return None;
            }
        }
        (!bytes.is_empty()).then(|| Self { bytes: bytes.into(), fixed: fixed.into() })
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn matches(&self, haystack: &[u8], at: usize) -> bool {
        let Some(window) = at.checked_add(self.len()).and_then(|end| haystack.get(at..end)) else {
            return false;
        };
        window.iter().zip(self.bytes.iter().zip(self.fixed.iter())).all(|(b, (p, f))| !*f || b == p)
    }

    fn pair(&self, at: usize) -> Option<usize> {
        (self.fixed[at] && self.fixed[at + 1]).then(|| self.bytes[at] as usize | (self.bytes[at + 1] as usize) << 8)
    }
}

/// Every match (at most [`MAX_HITS`]) of each pattern, as offsets into
/// `haystack`. One pass: each pattern is anchored at its rarest fixed byte pair.
/// A pattern without two adjacent fixed bytes never matches.
pub(crate) fn scan(haystack: &[u8], patterns: &[Pattern]) -> Vec<Vec<usize>> {
    let mut hits = vec![Vec::new(); patterns.len()];
    if haystack.len() < 2 {
        return hits;
    }
    // A sample is enough to rank pairs; the choice affects speed, never results.
    let mut frequency = vec![0u32; 1 << 16];
    for at in (0..haystack.len() - 1).step_by(7) {
        let key = haystack[at] as usize | (haystack[at + 1] as usize) << 8;
        frequency[key] = frequency[key].saturating_add(1);
    }
    // (pair, pattern, offset of the pair inside the pattern)
    let mut anchors: Vec<(u16, u32, u32)> = patterns
        .iter()
        .enumerate()
        .filter_map(|(id, pattern)| {
            (0..pattern.len().saturating_sub(1))
                .filter_map(|at| pattern.pair(at).map(|key| (key, at)))
                .min_by_key(|(key, _)| frequency[*key])
                .map(|(key, at)| (key as u16, id as u32, at as u32))
        })
        .collect();
    anchors.sort_unstable_by_key(|anchor| anchor.0);
    let mut start = vec![0u32; (1 << 16) + 1];
    // One bit per pair: an 8 KiB filter that stays in L1 for the whole pass.
    let mut anchored = Box::new([0u64; (1 << 16) / 64]);
    for anchor in &anchors {
        start[anchor.0 as usize + 1] += 1;
        anchored[usize::from(anchor.0 >> 6)] |= 1 << (anchor.0 & 63);
    }
    for key in 0..1 << 16 {
        start[key + 1] += start[key];
    }
    for (at, pair) in haystack.windows(2).enumerate() {
        let key = u16::from_le_bytes([pair[0], pair[1]]);
        if anchored[usize::from(key >> 6)] >> (key & 63) & 1 == 0 {
            continue;
        }
        let key = usize::from(key);
        let (low, high) = (start[key] as usize, start[key + 1] as usize);
        for &(_, id, offset) in &anchors[low..high] {
            let Some(begin) = at.checked_sub(offset as usize) else { continue };
            let found = &mut hits[id as usize];
            if found.len() < MAX_HITS && patterns[id as usize].matches(haystack, begin) {
                found.push(begin);
            }
        }
    }
    for found in &mut hits {
        found.sort_unstable();
    }
    hits
}

/// `bytes[at..at + 4]` is the absolute address of RVA `rva` in a module
/// loaded at `base` (a relocated operand, checked live).
pub(crate) fn operand_at(base: usize, bytes: &[u8], at: usize, rva: usize) -> bool {
    base.checked_add(rva).and_then(|v| u32::try_from(v).ok())
        .is_some_and(|v| at.checked_add(4).and_then(|end| bytes.get(at..end)) == Some(&v.to_le_bytes()[..]))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Section {
    pub name: [u8; 8],
    pub rva: usize,
    /// VirtualSize.
    pub size: usize,
    /// SizeOfRawData: the initialized part, the rest is zero-filled at load.
    pub raw: usize,
    pub flags: u32,
}

impl Section {
    pub(crate) fn named(&self, name: &str) -> bool {
        let mut padded = [0u8; 8];
        let bytes = name.as_bytes();
        bytes.len() <= 8 && {
            padded[..bytes.len()].copy_from_slice(bytes);
            padded == self.name
        }
    }

    fn contains(&self, rva: usize, len: usize) -> bool {
        rva >= self.rva && rva.checked_add(len).is_some_and(|end| end <= self.rva + self.size)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Headers {
    pub timestamp: u32,
    /// SizeOfImage: the bound for every RVA and every operand.
    pub size: usize,
    /// The preferred base. Windows leaves it in the mapped header after a
    /// rebase, so code still relocated for it can be told apart.
    pub image_base: usize,
    pub sections: Vec<Section>,
    /// IAT directory (rva, size); a relocated import call lands inside it.
    pub iat: (usize, usize),
    /// Debug directory (rva, size); its CodeView entry repeats the link stamp.
    pub debug: (usize, usize),
    /// Import directory (rva, size): names for IAT slots, see [`Image::import_name`].
    pub imports: (usize, usize),
    /// Index of `.text` in `sections`.
    pub text: usize,
}

fn le16(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize)
}
fn le32(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// A 32-bit x86 PE header page. SizeOfImage must be where the last section
/// ends, and `.text` must be an executable section inside the image.
pub(crate) fn parse_headers(page: &[u8]) -> Result<Headers, &'static str> {
    let nt = le32(page, 0x3C).ok_or("truncated DOS header")?;
    if page.get(..2) != Some(b"MZ") || page.get(nt..nt + 4) != Some(b"PE\0\0") {
        return Err("not a PE image");
    }
    if le16(page, nt + 4) != Some(0x14C) || le16(page, nt + 24) != Some(0x10B) {
        return Err("not a 32-bit x86 image");
    }
    let count = le16(page, nt + 6).ok_or("truncated file header")?;
    let optional = le16(page, nt + 20).ok_or("truncated file header")?;
    let timestamp = le32(page, nt + 8).ok_or("truncated file header")? as u32;
    let opt = nt + 24;
    let alignment = le32(page, opt + 32).ok_or("truncated optional header")?;
    let size = le32(page, opt + 56).ok_or("truncated optional header")?;
    let image_base = le32(page, opt + 28).ok_or("truncated optional header")?;
    let directories = le32(page, opt + 92).ok_or("truncated optional header")?;
    if !(1..=96).contains(&count) || !alignment.is_power_of_two() || size == 0 {
        return Err("implausible optional header");
    }
    let directory = |index: usize| -> (usize, usize) {
        if index >= directories || 96 + index * 8 + 8 > optional {
            return (0, 0);
        }
        (le32(page, opt + 96 + index * 8).unwrap_or(0), le32(page, opt + 100 + index * 8).unwrap_or(0))
    };
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        let at = opt + optional + index * 40;
        let header = page.get(at..at + 40).ok_or("section table outside the header page")?;
        sections.push(Section {
            name: header[..8].try_into().unwrap(),
            size: le32(header, 8).unwrap(),
            rva: le32(header, 12).unwrap(),
            raw: le32(header, 16).unwrap(),
            flags: le32(header, 36).unwrap() as u32,
        });
    }
    let last = sections.last().unwrap();
    let end = last.rva.checked_add(last.size.max(1)).and_then(|end| end.checked_add(alignment - 1)).ok_or("section overflows")?
        & !(alignment - 1);
    if end != size {
        return Err("SizeOfImage does not end at the last section");
    }
    if sections.iter().any(|s| s.rva.checked_add(s.size).is_none_or(|end| end > size)) {
        return Err("a section lies outside SizeOfImage");
    }
    let text = sections.iter().position(|s| s.named(".text") && s.flags & SCN_EXECUTE != 0 && s.size > 0)
        .ok_or("no executable .text section")?;
    Ok(Headers { timestamp, size, image_base, sections, iat: directory(12), debug: directory(6), imports: directory(1), text })
}

/// The copied parts of one loaded module; byte `n` of a region is RVA
/// `region.0 + n`. Region 0 is `.text`.
pub(crate) struct Image {
    pub base: usize,
    pub headers: Headers,
    regions: Vec<(usize, Vec<u8>)>,
}

/// Why a capture produced no image.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CaptureError {
    /// Try again later: the module is still initializing, or another thread
    /// is inside the loader right now.
    NotReady,
    /// The module can never be resolved in this process.
    Invalid(&'static str),
}

use crate::mem::read_into as read_live;

/// Holding the loader lock excludes every DLL entry point, including the stub
/// that unpacks and relocates FFXiMain's `.text` on attach. Try-only: this
/// never waits for, or deadlocks with, a thread that is loading a DLL.
struct LoaderLock {
    unlock: unsafe extern "system" fn(u32, usize) -> i32,
    cookie: usize,
}

impl LoaderLock {
    fn try_acquire() -> Option<Self> {
        type Lock = unsafe extern "system" fn(u32, *mut u32, *mut usize) -> i32;
        type Unlock = unsafe extern "system" fn(u32, usize) -> i32;
        const TRY_ONLY: u32 = 2;
        const ACQUIRED: u32 = 1;
        let name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        // SAFETY: ntdll is always loaded; both exports have had these
        // signatures since Windows XP.
        unsafe {
            let ntdll = GetModuleHandleW(name.as_ptr());
            if ntdll.is_null() {
                return None;
            }
            let lock: Lock = std::mem::transmute(GetProcAddress(ntdll, c"LdrLockLoaderLock".as_ptr().cast())?);
            let unlock: Unlock = std::mem::transmute(GetProcAddress(ntdll, c"LdrUnlockLoaderLock".as_ptr().cast())?);
            let (mut disposition, mut cookie) = (0u32, 0usize);
            (lock(TRY_ONLY, &mut disposition, &mut cookie) >= 0 && disposition == ACQUIRED)
                .then_some(Self { unlock, cookie })
        }
    }
}

impl Drop for LoaderLock {
    fn drop(&mut self) {
        // SAFETY: releases exactly the acquisition this value represents.
        unsafe { (self.unlock)(0, self.cookie) };
    }
}

impl Image {
    /// The parts the resolver reads: the header page, all of `.text`, `.rdata`
    /// and the initialized part of `.data`. Region 0 is `.text`.
    fn wanted(headers: &Headers) -> Vec<(usize, usize)> {
        let text = &headers.sections[headers.text];
        let mut wanted = vec![(text.rva, text.size), (0, HEADER_PAGE.min(headers.size))];
        for section in &headers.sections {
            if section.named(".rdata") || section.named(".data") {
                wanted.push((section.rva, section.size.min(section.raw)));
            }
        }
        wanted.retain(|(_, len)| *len > 0);
        wanted
    }

    /// Builds an image from any reader of the module's RVAs (tests read files).
    #[cfg(test)]
    pub(crate) fn from_reader(base: usize, read: impl Fn(usize, &mut [u8]) -> bool) -> Result<Self, &'static str> {
        let mut page = vec![0; HEADER_PAGE];
        if !read(0, &mut page) {
            return Err("unreadable header page");
        }
        let headers = parse_headers(&page)?;
        let mut regions = Vec::new();
        for (rva, len) in Self::wanted(&headers) {
            let mut bytes = vec![0; len];
            if !read(rva, &mut bytes) {
                return Err("unreadable section");
            }
            regions.push((rva, bytes));
        }
        Ok(Self { base, headers, regions })
    }

    /// Copies the module loaded at `base` in this process.
    pub(crate) fn capture(base: usize) -> Result<Self, CaptureError> {
        let mut page = vec![0; HEADER_PAGE];
        if base == 0 || !read_live(base, &mut page) {
            return Err(CaptureError::Invalid("unreadable header page"));
        }
        let headers = parse_headers(&page).map_err(CaptureError::Invalid)?;
        let text = &headers.sections[headers.text];
        // Cheap test before any copy: a packed .text is zero until its stub runs.
        let mut probe = vec![0; text.size.min(HEADER_PAGE)];
        if !read_live(base + text.rva, &mut probe) || probe.iter().all(|b| *b == 0) {
            return Err(CaptureError::NotReady);
        }
        let wanted = Self::wanted(&headers);
        let mut regions: Vec<(usize, Vec<u8>)> = wanted.iter().map(|&(rva, len)| (rva, vec![0; len])).collect();
        {
            let _lock = LoaderLock::try_acquire().ok_or(CaptureError::NotReady)?;
            for (rva, bytes) in &mut regions {
                if !read_live(base + *rva, bytes) {
                    return Err(CaptureError::NotReady);
                }
            }
        }
        let text = &regions[0].1;
        if text.iter().filter(|b| **b != 0).count() < text.len() / 2 {
            return Err(CaptureError::NotReady);
        }
        Ok(Self { base, headers, regions })
    }

    pub(crate) fn text(&self) -> (usize, &[u8]) {
        (self.regions[0].0, &self.regions[0].1)
    }

    pub(crate) fn bytes(&self, rva: usize, len: usize) -> Option<&[u8]> {
        let end = rva.checked_add(len)?;
        self.regions.iter().find_map(|(start, bytes)| {
            (rva >= *start && end <= start + bytes.len()).then(|| &bytes[rva - start..end - start])
        })
    }

    pub(crate) fn u8(&self, rva: usize) -> Option<usize> {
        self.bytes(rva, 1).map(|b| b[0] as usize)
    }

    pub(crate) fn u32(&self, rva: usize) -> Option<usize> {
        self.bytes(rva, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    }

    /// A relocated absolute operand at `rva`, as an RVA inside this image.
    pub(crate) fn abs(&self, rva: usize) -> Option<usize> {
        self.u32(rva)?.checked_sub(self.base).filter(|target| *target < self.headers.size)
    }

    fn relative(&self, rva: usize, opcode: u8) -> Option<usize> {
        if self.u8(rva)? != opcode as usize {
            return None;
        }
        let displacement = self.u32(rva + 1)? as u32 as i32 as isize;
        (rva + 5).checked_add_signed(displacement).filter(|target| *target < self.headers.size)
    }

    /// Target of `call rel32` at `rva`.
    pub(crate) fn call(&self, rva: usize) -> Option<usize> {
        self.relative(rva, 0xE8)
    }

    /// Every direct `call rel32` in `.text` to `target`.
    pub(crate) fn callers(&self, target: usize) -> Vec<usize> {
        let (start, text) = self.text();
        (0..text.len().saturating_sub(4))
            .filter(|&at| text[at] == 0xE8 && self.call(start + at) == Some(target))
            .map(|at| start + at)
            .collect()
    }

    /// Target of `jmp rel32` at `rva`.
    pub(crate) fn jump(&self, rva: usize) -> Option<usize> {
        self.relative(rva, 0xE9)
    }

    /// Target of a short conditional jump (`7x rel8`) at `rva`.
    pub(crate) fn short_branch(&self, rva: usize) -> Option<usize> {
        let opcode = self.u8(rva)?;
        if !(0x70..=0x7F).contains(&opcode) {
            return None;
        }
        (rva + 2).checked_add_signed(self.u8(rva + 1)? as u8 as i8 as isize)
    }

    /// Follows one `jmp rel32` thunk, if `rva` is one.
    pub(crate) fn through_thunk(&self, rva: usize) -> usize {
        self.jump(rva).unwrap_or(rva)
    }

    pub(crate) fn matches_at(&self, rva: usize, pattern: &Pattern) -> bool {
        let (start, text) = self.text();
        rva.checked_sub(start).is_some_and(|at| pattern.matches(text, at))
    }

    pub(crate) fn section(&self, name: &str) -> Option<&Section> {
        self.headers.sections.iter().find(|s| s.named(name))
    }

    /// `[rva, rva + len)` lies inside the named section.
    pub(crate) fn inside(&self, name: &str, rva: usize, len: usize) -> bool {
        self.section(name).is_some_and(|s| s.contains(rva, len))
    }

    /// The NUL-terminated string at `rva` is exactly `text`.
    pub(crate) fn c_string(&self, rva: usize, text: &[u8]) -> bool {
        self.bytes(rva, text.len() + 1).is_some_and(|b| &b[..text.len()] == text && b[text.len()] == 0)
    }

    /// The CodeView debug entry's link stamp, which normally repeats the PE
    /// header's. Diagnostic only: a later linker may add or reorder entries.
    pub(crate) fn codeview_timestamp(&self) -> Option<u32> {
        let (rva, size) = self.headers.debug;
        (0..size / 28).find_map(|index| {
            let entry = self.bytes(rva + index * 28, 28)?;
            (le32(entry, 12)? == 2).then(|| le32(entry, 4).map(|stamp| stamp as u32)).flatten()
        })
    }

    /// `call/jmp [abs32]` sites into the IAT, counted two ways: relocated for
    /// this base, and still at the preferred base. A rebase moves operands by a
    /// multiple of 64 KiB, far more than the IAT's length, so a rebased `.text`
    /// read before (or during) its relocation shows sites of the second kind.
    pub(crate) fn import_calls(&self) -> (usize, usize) {
        let (iat, len) = self.headers.iat;
        let preferred = self.headers.image_base;
        let in_iat = |va: usize, base: usize| va.checked_sub(base).is_some_and(|rva| rva >= iat && rva + 4 <= iat + len);
        let (_, text) = self.text();
        let (mut relocated, mut stale) = (0, 0);
        for at in 0..text.len().saturating_sub(6) {
            if text[at] == 0xFF && matches!(text[at + 1], 0x15 | 0x25) {
                let va = u32::from_le_bytes(text[at + 2..at + 6].try_into().unwrap()) as usize;
                relocated += usize::from(in_iat(va, self.base));
                stale += usize::from(preferred != self.base && in_iat(va, preferred));
            }
        }
        (relocated, stale)
    }

    /// The NUL-terminated string at `rva`, at most `max` bytes before the NUL.
    fn c_str(&self, rva: usize, max: usize) -> Option<&[u8]> {
        (1..=max + 1).find_map(|len| self.bytes(rva, len).filter(|b| b[len - 1] == 0).map(|b| &b[..len - 1]))
    }

    /// The (DLL, function) an IAT slot at `slot` imports by name, read from
    /// the import directory's name table, which the loader never rewrites.
    /// `None` for ordinal imports, slots outside every descriptor, or a
    /// directory outside the copied sections.
    pub(crate) fn import_name(&self, slot: usize) -> Option<(&[u8], &[u8])> {
        let (directory, size) = self.headers.imports;
        for index in 0..size / 20 {
            let at = directory + index * 20;
            let (names, dll, first) = (self.u32(at)?, self.u32(at + 12)?, self.u32(at + 16)?);
            if dll == 0 || first == 0 {
                break;
            }
            if names == 0 || slot < first || !(slot - first).is_multiple_of(4) {
                continue;
            }
            let entry_index = (slot - first) / 4;
            // The slot must lie before this descriptor's terminating entry.
            if (0..=entry_index).any(|i| self.u32(names + i * 4) == Some(0)) {
                continue;
            }
            let entry = self.u32(names + entry_index * 4)?;
            if entry & 0x8000_0000 != 0 {
                return None;
            }
            return Some((self.c_str(dll, 64)?, self.c_str(entry + 2, 128)?));
        }
        None
    }

    /// The absolute operand at `rva` is the IAT slot of `dll!function`.
    pub(crate) fn imports(&self, rva: usize, dll: &str, function: &str) -> bool {
        let Some(slot) = self.abs(rva) else { return false };
        let (iat, len) = self.headers.iat;
        slot >= iat && slot + 4 <= iat + len
            && self.import_name(slot).is_some_and(|(d, f)| d.eq_ignore_ascii_case(dll.as_bytes()) && f == function.as_bytes())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn pattern(text: &str) -> Pattern {
        Pattern::parse(text).unwrap()
    }

    #[test]
    fn patterns_parse_hex_and_wildcards_only() {
        assert_eq!(pattern("A1 ?? 85").len(), 3);
        assert!(Pattern::parse("").is_none());
        assert!(Pattern::parse("A1 ? 85").is_none());
        assert!(Pattern::parse("A1 GG").is_none());
        assert!(Pattern::parse("A10").is_none());
        let p = pattern("A1 ?? 85 C0");
        assert!(p.matches(&[0xA1, 0x77, 0x85, 0xC0], 0));
        assert!(!p.matches(&[0xA1, 0x77, 0x85, 0xC1], 0));
        assert!(!p.matches(&[0xA1, 0x77, 0x85], 0), "a match never runs past the end");
        assert!(!p.matches(&[0xA1, 0x77, 0x85, 0xC0], usize::MAX));
    }

    #[test]
    fn scan_reports_zero_one_and_several_matches() {
        let mut text = vec![0x90u8; 64];
        text[10..14].copy_from_slice(&[0xA1, 0x55, 0x85, 0xC0]);
        text[40..44].copy_from_slice(&[0xA1, 0x66, 0x85, 0xC0]);
        text[60..63].copy_from_slice(&[0x12, 0x34, 0x56]);
        let patterns = [pattern("A1 ?? 85 C0"), pattern("A1 55 85 C0"), pattern("12 34 56"), pattern("34 56 78"),
            pattern("?? 12 ?? 56"), pattern("90 90")];
        let hits = scan(&text, &patterns);
        assert_eq!(hits[0], vec![10, 40]);
        assert_eq!(hits[1], vec![10]);
        assert_eq!(hits[2], vec![60]);
        assert!(hits[3].is_empty(), "a match never runs past the end");
        assert!(hits[4].is_empty(), "no two adjacent fixed bytes: never matches");
        assert_eq!(hits[5].len(), MAX_HITS, "hit lists are capped");
        assert!(scan(&[0xA1], &patterns).iter().all(Vec::is_empty));
    }

    /// A minimal PE image: header page, `.text` at 0x1000 and `.data` at 0x2000.
    pub(crate) fn tiny_image(text: &[u8], data: &[u8]) -> Vec<u8> {
        let mut image = vec![0u8; 0x3000];
        image[..2].copy_from_slice(b"MZ");
        image[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let nt = 0x80;
        image[nt..nt + 4].copy_from_slice(b"PE\0\0");
        image[nt + 4..nt + 6].copy_from_slice(&0x14Cu16.to_le_bytes());
        image[nt + 6..nt + 8].copy_from_slice(&2u16.to_le_bytes());
        image[nt + 8..nt + 12].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        image[nt + 20..nt + 22].copy_from_slice(&0xE0u16.to_le_bytes());
        let opt = nt + 24;
        image[opt..opt + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
        image[opt + 28..opt + 32].copy_from_slice(&0x1000_0000u32.to_le_bytes());
        image[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        image[opt + 56..opt + 60].copy_from_slice(&0x3000u32.to_le_bytes());
        image[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes());
        // IAT directory: 8 bytes at 0x2000.
        image[opt + 96 + 12 * 8..opt + 104 + 12 * 8].copy_from_slice(&[0, 0x20, 0, 0, 8, 0, 0, 0]);
        let table = opt + 0xE0;
        for (index, (name, rva, flags)) in [(".text", 0x1000u32, 0xE000_0020u32), (".data", 0x2000, 0xC000_0040)].iter().enumerate() {
            let at = table + index * 40;
            image[at..at + name.len()].copy_from_slice(name.as_bytes());
            image[at + 8..at + 12].copy_from_slice(&0x1000u32.to_le_bytes());
            image[at + 12..at + 16].copy_from_slice(&rva.to_le_bytes());
            image[at + 16..at + 20].copy_from_slice(&0x1000u32.to_le_bytes());
            image[at + 36..at + 40].copy_from_slice(&flags.to_le_bytes());
        }
        image[0x1000..0x1000 + text.len()].copy_from_slice(text);
        image[0x2000..0x2000 + data.len()].copy_from_slice(data);
        image
    }

    pub(crate) fn load(file: &[u8], base: usize) -> Image {
        Image::from_reader(base, |rva, out| {
            file.get(rva..rva + out.len()).map(|b| out.copy_from_slice(b)).is_some()
        })
        .unwrap()
    }

    #[test]
    fn headers_bound_the_image_and_find_text() {
        let file = tiny_image(&[], &[]);
        let image = load(&file, 0x1000_0000);
        assert_eq!(image.headers.timestamp, 0x1234_5678);
        assert_eq!(image.headers.size, 0x3000);
        assert_eq!(image.text().0, 0x1000);
        assert!(image.inside(".data", 0x2000, 0x1000));
        assert!(!image.inside(".data", 0x2FFF, 2), "ranges are bounded by the section");
        assert!(!image.inside(".data", 0x1FFF, 1));
        assert_eq!(image.bytes(0x2FFF, 2), None, "reads stop at the image's end");
        let mut bad = file.clone();
        bad[0x80 + 24 + 56..0x80 + 24 + 60].copy_from_slice(&0x4000u32.to_le_bytes());
        assert_eq!(parse_headers(&bad[..0x1000]).unwrap_err(), "SizeOfImage does not end at the last section");
        let mut bad = file.clone();
        bad[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        assert!(parse_headers(&bad[..0x1000]).is_err());
        let mut bad = file;
        bad[0x80 + 24 + 0xE0 + 36 + 3] = 0x40; // .text loses its execute flag
        assert_eq!(parse_headers(&bad[..0x1000]).unwrap_err(), "no executable .text section");
    }

    #[test]
    fn operands_and_branches_are_relative_to_the_runtime_base() {
        for base in [0x1000_0000usize, 0x1800_0000] {
            let mut text = vec![0x90u8; 0x40];
            text[0] = 0xA1;
            text[1..5].copy_from_slice(&((base + 0x2010) as u32).to_le_bytes());
            text[5] = 0xE8; // call 0x1020
            text[6..10].copy_from_slice(&(0x1020i32 - 0x100A).to_le_bytes());
            text[0x20] = 0xE9; // thunk: jmp 0x1008
            text[0x21..0x25].copy_from_slice(&(0x1008i32 - 0x1025).to_le_bytes());
            text[0x30] = 0x74; // je back to 0x1028
            text[0x31] = (0x28i8 - 0x32) as u8;
            text[0x34] = 0xA1;
            text[0x35..0x39].copy_from_slice(&((base - 0x10) as u32).to_le_bytes());
            let file = tiny_image(&text, &[]);
            let image = load(&file, base);
            assert_eq!(image.abs(0x1001), Some(0x2010));
            assert_eq!(image.abs(0x1035), None, "operands outside the image are rejected");
            assert_eq!(image.call(0x1005), Some(0x1020));
            assert_eq!(image.call(0x1000), None, "the opcode is checked");
            assert_eq!(image.through_thunk(0x1020), 0x1008);
            assert_eq!(image.through_thunk(0x1005), 0x1005);
            assert_eq!(image.short_branch(0x1030), Some(0x1028));
            assert!(image.matches_at(0x1005, &pattern("E8 ?? ?? ?? ??")));
            assert!(!image.matches_at(0x0FFF, &pattern("90")), "shapes are checked in .text only");
        }
    }

    #[test]
    fn import_calls_tell_relocated_from_stale_operands() {
        let base = 0x1000_0000usize;
        let mut text = vec![0x90u8; 0x20];
        text[0..2].copy_from_slice(&[0xFF, 0x15]);
        text[2..6].copy_from_slice(&((base + 0x2004) as u32).to_le_bytes());
        text[8..10].copy_from_slice(&[0xFF, 0x25]);
        text[10..14].copy_from_slice(&((base + 0x2100) as u32).to_le_bytes());
        text[16..18].copy_from_slice(&[0xFF, 0x15]);
        text[18..22].copy_from_slice(&((base + 0x2000) as u32).to_le_bytes());
        let file = tiny_image(&text, &[]);
        assert_eq!(load(&file, base).import_calls(), (2, 0));
        assert_eq!(load(&file, base + 0x10000).import_calls(), (0, 2), "a rebased image read before its relocation");
        // Relocate one of the two sites only: relocation was still running.
        let mut half = text.clone();
        half[18..22].copy_from_slice(&((base + 0x10000 + 0x2000) as u32).to_le_bytes());
        assert_eq!(load(&tiny_image(&half, &[]), base + 0x10000).import_calls(), (1, 1));
    }

    #[test]
    fn strings_must_end_where_expected() {
        let mut data = vec![0u8; 0x1000];
        data[..17].copy_from_slice(b"menu    dbnamese\0");
        data[0xFFC..].copy_from_slice(b"menu");
        let file = tiny_image(&[], &data);
        let image = load(&file, 0x1000_0000);
        assert!(image.c_string(0x2000, b"menu    dbnamese"));
        assert!(!image.c_string(0x2000, b"menu    dbname"), "the whole string must match");
        assert!(!image.c_string(0x2FFC, b"menu"), "no terminator inside the image");
    }

    #[test]
    fn this_process_is_never_captured_mid_load() {
        // The test executable is fully initialized; capture never deadlocks.
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        match Image::capture(base) {
            Ok(image) => assert_eq!(image.base, base),
            Err(error) => assert_eq!(error, CaptureError::NotReady, "only a busy loader may defer"),
        }
        assert_eq!(Image::capture(0).err(), Some(CaptureError::Invalid("unreadable header page")));
    }
}
