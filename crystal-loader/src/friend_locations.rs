//! Read the verified game's own friend display cache. No game calls, writes,
//! network requests or hooks. POL alone continues to own presence and auth.

use crate::{game_layout::{self, Friends, Group}, log, proto};
use std::collections::BTreeMap;
use windows_sys::Win32::{
    Foundation::FreeLibrary,
    System::LibraryLoader::GetModuleHandleExW,
};
use zeroize::Zeroizing;

pub use crate::wire::{LOCATIONS_LEN as BLOB_LEN, LOCATIONS_MAGIC as MAGIC, LOCATION_RECORD_LEN as RECORD_LEN};

// Every offset below is what this reader uses. game_layout extracts each from
// the game's own code (100e6320, 101eaec0, 100e7900, 100efce0, 10152f20,
// 102578b0, 101530c0, ...) and requires exactly these values; a build that
// changed one switches the feature off.
/// Lobby root ([LOBBY]): the friend rows and my own world.
pub(crate) const ROWS: usize = 0xa90;
pub(crate) const ROW_SIZE: usize = 0x100;
pub(crate) const OWN_WORLD: usize = 0x130;
/// Lobby root: the world table, {world id dword, name[16]} entries.
pub(crate) const WORLD_COUNT: usize = 0x14140;
pub(crate) const WORLD_ENTRIES: usize = 0x14144;
pub(crate) const WORLD_STRIDE: usize = 20;
pub(crate) const WORLD_NAME: usize = 4;
/// Friend row = the 0xb0-byte POL record, then the game's 0x48-byte detail.
pub(crate) const IN_USE: usize = 0x98;
pub(crate) const CHARACTERS: usize = 0x18;
pub(crate) const CHARACTER_WORLD: usize = 6;
pub(crate) const DETAIL: usize = 0xb0;
pub(crate) const DETAIL_LEN: usize = 0x48;
pub(crate) const NAME: usize = 0xb4;
pub(crate) const CHAR_ID: usize = 0xc8;
pub(crate) const ZONE: usize = 0xd8;
pub(crate) const PRIVACY: usize = 0xe0;
pub(crate) const HIDDEN_BIT: usize = 14;
pub(crate) const COMPLETED: usize = 0xfc;
/// [OWN_ZONE] + 0x3c350 is my own zone (101eaf3f/101eaf4d).
pub(crate) const OWN_ZONE_FIELD: usize = 0x3c350;
/// 101cbc10(zone, 0): group 4 (area names), else group 9 index 0x40.
pub(crate) const AREA_GROUP: usize = 4;
pub(crate) const FALLBACK_GROUP: usize = 9;
pub(crate) const FALLBACK_INDEX: usize = 0x40;
/// Managed resource objects: header, index and data pointers; header fields.
pub(crate) const RES_HEADER: usize = 4;
pub(crate) const RES_INDEX: usize = 8;
pub(crate) const RES_DATA: usize = 0xc;
pub(crate) const RES_COUNT: usize = 0x28;
pub(crate) const RES_KIND: usize = 8;
pub(crate) const RES_FIXED_KIND: usize = 0x10;
pub(crate) const RES_VARIANTS: usize = 0x3e;
pub(crate) const RES_INDEXED: usize = 0x1c;
pub(crate) const RES_STRIDE: usize = 0x20;
pub(crate) const FIXED_PRESENT: usize = 4;
pub(crate) const TEXT_RESERVED: usize = 8;
pub(crate) const TEXT_OFFSET: usize = 4;
pub(crate) const TEXT_MARKER: usize = 1;
pub(crate) const TEXT_STRING: usize = 0x1c;
/// Legacy resource tables, one slot per group.
pub(crate) const LEGACY_SLOTS: usize = 0x20;
pub(crate) const LEGACY_KIND: usize = 0xa;
pub(crate) const LEGACY_SMALL_BELOW: usize = 2;
pub(crate) const LEGACY_COUNT: usize = 0x24;
pub(crate) const LEGACY_ENTRIES: usize = 0x38;
const _: () = assert!(ROW_SIZE == 0x100 && DETAIL == proto::FRIEND_RECORD_LEN);

type Reader<'a> = dyn Fn(usize, &mut [u8]) -> bool + 'a;
fn read<const N: usize>(reader: &Reader<'_>, at: usize) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    at.checked_add(N)?;
    reader(at, &mut bytes).then_some(bytes)
}
fn word(reader: &Reader<'_>, at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(read(reader, at)?))
}
fn dword(reader: &Reader<'_>, at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(read(reader, at)?) as usize)
}
use crate::mem::read_into as process_read;

/// The live module still holds the scanned code at the resolved sites. The
/// byte checks first written for build 6A995428, now at the located addresses.
fn verified(base: usize, reader: &Reader<'_>, layout: &Friends) -> Option<()> {
    let nt = base.checked_add(dword(reader, base.checked_add(0x3c)?)?)?;
    if read::<4>(reader, nt)? != *b"PE\0\0" || word(reader, nt.checked_add(4)?)? != 0x14c {
        return None;
    }
    let at = |rva: usize| base.checked_add(rva);
    let relocated = |rva: usize| -> Option<[u8; 4]> { Some(u32::try_from(base.checked_add(rva)?).ok()?.to_le_bytes()) };
    // 100e6320: original cache getter, including relocated root.
    let mut expected = [
        0x66, 0x8b, 0x44, 0x24, 0x04, 0x66, 0x3d, 0x2c, 0x01, 0x7c, 0x03, 0x33, 0xc0, 0xc3, 0x8b,
        0x0d, 0, 0, 0, 0, 0x0f, 0xbf, 0xc0, 0xc1, 0xe0, 0x08, 0x8d, 0x84, 0x08, 0x90, 0x0a, 0, 0,
        0xc3,
    ];
    expected[16..20].copy_from_slice(&relocated(layout.lobby)?);
    let mut getter = read::<34>(reader, at(layout.cache_getter)?)?;
    // The row capacity (0x12c in the known builds) may grow in an update, as
    // the scan allows: it only has to hold the POL catalog.
    if (u16::from_le_bytes([getter[7], getter[8]]) as usize) < proto::FRIEND_SLOTS {
        return None;
    }
    getter[7..9].copy_from_slice(&expected[7..9]);
    // Branch distances are not checked, as in the scan: the `jl` here and
    // the name getter's `je`.
    getter[10] = expected[10];
    let mut name = read::<22>(reader, at(layout.name_getter)?)?;
    let name_expected = [
        0x8b, 0x44, 0x24, 0x04, 0xf6, 0x80, 0xb0, 0, 0, 0, 1, 0x74, 6, 5, 0xb4, 0, 0, 0,
        0xc3, 0x33, 0xc0, 0xc3,
    ];
    name[12] = name_expected[12];
    if getter != expected || name != name_expected {
        return None;
    }
    // 101eaf3f/101eaf4d: the own-zone object and field the display compares.
    let mut own = [0x8b, 0x0d, 0, 0, 0, 0];
    own[2..].copy_from_slice(&relocated(layout.own_zone)?);
    // 101cbc3a: the group 9, index 0x40 fallback.
    let fallback = [0xc7, 0x44, 0x24, 8, 0x40, 0, 0, 0, 0xc7, 0x44, 0x24, 4, 9, 0, 0, 0];
    if read::<6>(reader, at(layout.display_area + 0x1d)?)? != own
        || read::<6>(reader, at(layout.display_area + 0x2b)?)? != [0x8b, 0x91, 0x50, 0xc3, 3, 0]
        || read::<16>(reader, at(layout.area_fn + 0x2a)?)? != fallback
    {
        return None;
    }
    for group in [layout.area, layout.fallback] {
        let entry = at(layout.group_table + group.entry * 8)?;
        if dword(reader, entry)? != group.id || dword(reader, entry.checked_add(4)?)? != base.checked_add(group.object)? {
            return None;
        }
    }
    Some(())
}

fn bounded_text<const N: usize>(reader: &Reader<'_>, at: usize) -> Option<[u8; N]> {
    let mut text = read::<N>(reader, at)?;
    let end = text.iter().position(|b| *b == 0)?;
    if end == 0 || text[..end].iter().any(|b| *b < 0x20 || *b == 0x7f) {
        return None;
    }
    text[end..].fill(0);
    Some(text)
}

/// 101cbc10(zone, 0): the game's localized area string (group 4), else its
/// group 9 index 0x40 fallback text.
fn area_name(base: usize, layout: &Friends, zone: usize, reader: &Reader<'_>) -> Option<[u8; 64]> {
    resource_text(base, layout, layout.area, zone, reader)
        .or_else(|| resource_text(base, layout, layout.fallback, layout.fallback_index, reader))
}

/// One string of a resource group. Mirror only its read-only lookup
/// (10152f20/102578b0), never its loading/decryption routines.
fn resource_text(base: usize, layout: &Friends, group: Group, zone: usize, reader: &Reader<'_>) -> Option<[u8; 64]> {
    let object = base.checked_add(group.object)?;
    let header = dword(reader, object.checked_add(RES_HEADER)?)?;
    if header == 0 {
        // 10152f20 falls back to the group's legacy table only when the
        // managed resource is absent. Its lookup is 101530c0/10153090.
        let pointer = base.checked_add(layout.legacy_table + 4 * group.id)?;
        let table = dword(reader, pointer)?;
        if table == 0 {
            return None;
        }
        let count = dword(reader, table.checked_add(LEGACY_COUNT)?)?;
        if count > 0x1000 || zone >= count {
            return None;
        }
        let kind = word(reader, table.checked_add(LEGACY_KIND)?)? as i16;
        let (stride, offset) = if kind < LEGACY_SMALL_BELOW as i16 {
            (
                4,
                word(reader, table.checked_add(LEGACY_ENTRIES + zone * 4)?)? as i16 as isize,
            )
        } else {
            (
                12,
                isize::try_from(dword(reader, table.checked_add(LEGACY_ENTRIES + zone * 12)?)?).ok()?,
            )
        };
        let at = table
            .checked_add(LEGACY_ENTRIES + count * stride)?
            .checked_add_signed(offset)?;
        let text = bounded_text(reader, at)?;
        return (dword(reader, pointer)? == table
            && dword(reader, table.checked_add(LEGACY_COUNT)?)? == count
            && word(reader, table.checked_add(LEGACY_KIND)?)? as i16 == kind
            && dword(reader, object.checked_add(RES_HEADER)?)? == 0)
            .then_some(text);
    }
    let count = dword(reader, header.checked_add(RES_COUNT)?)?;
    if count > 0x1000 || zone >= count {
        return None;
    }
    let data = dword(reader, object.checked_add(RES_DATA)?)?;
    if data == 0 {
        return None;
    }
    let at = if word(reader, header.checked_add(RES_KIND)?)? as usize == RES_FIXED_KIND {
        let variants = word(reader, header.checked_add(RES_VARIANTS)?)? as usize;
        if variants == 0 || variants > 16 {
            return None;
        }
        let entry = data.checked_add(zone.checked_mul(variants)?.checked_mul(8)?)?;
        if dword(reader, entry.checked_add(FIXED_PRESENT)?)? == 0 {
            return None;
        }
        data.checked_add(dword(reader, entry)?)?
    } else {
        let entry = if dword(reader, header.checked_add(RES_INDEXED)?)? != 0 {
            let index = dword(reader, object.checked_add(RES_INDEX)?)?;
            data.checked_add(dword(reader, index.checked_add(zone.checked_mul(8)?)?)?)?
        } else {
            let stride = dword(reader, header.checked_add(RES_STRIDE)?)?;
            if stride == 0 || stride > 0x10000 {
                return None;
            }
            data.checked_add(zone.checked_mul(stride)?)?
        };
        if word(reader, entry)? == 0 || dword(reader, entry.checked_add(TEXT_RESERVED)?)? != 0 {
            return None;
        }
        let text = entry.checked_add(dword(reader, entry.checked_add(TEXT_OFFSET)?)?)?;
        if word(reader, text)? as usize != TEXT_MARKER {
            return None;
        }
        text.checked_add(TEXT_STRING)?
    };
    let text = bounded_text(reader, at)?;
    // Resource identity must still match after copying its text.
    (dword(reader, object.checked_add(RES_HEADER)?)? == header
        && dword(reader, object.checked_add(RES_DATA)?)? == data)
        .then_some(text)
}

fn world_name(root: usize, world: u8, reader: &Reader<'_>) -> Option<[u8; 16]> {
    let count = dword(reader, root.checked_add(WORLD_COUNT)?)?;
    if count > 256 {
        return None;
    }
    for index in 0..count {
        let at = root.checked_add(WORLD_ENTRIES + index * WORLD_STRIDE)?;
        if dword(reader, at)? == world as usize {
            return bounded_text(reader, at.checked_add(WORLD_NAME)?);
        }
    }
    None
}

fn selected(record: &[u8]) -> Option<&[u8]> {
    let flags = u64::from_le_bytes(record[8..16].try_into().ok()?);
    if record[IN_USE] & 1 == 0
        || flags & (1 << 11) == 0
        || flags & (1 << 16) == 0
        || (flags >> 33) & 0x3ff != 1
        || record[0x10..0x18] == [0; 8]
    {
        return None;
    }
    let at = CHARACTERS + ((flags >> 17) & 7) as usize * 16;
    let character = &record[at..at + 16];
    (character[0] & 1 != 0 && character[2..4] == [1, 0]).then_some(character)
}

/// My own zone ([OWN_ZONE] + 0x3c350), or None when that object is absent.
fn own_zone(base: usize, layout: &Friends, reader: &Reader<'_>) -> Option<usize> {
    let object = dword(reader, base.checked_add(layout.own_zone)?)?;
    (object != 0).then_some(())?;
    dword(reader, object.checked_add(OWN_ZONE_FIELD)?)
}

/// 101eaec0, same-world branch. The row's +0xb0..+0xf8 is copied; its word
/// +0xd8 is the zone. The area (field 0x23) shows in both visibility branches
/// whenever my zone is known and differs (101eaf49/101eafc6); bit 14 of +0xe0
/// does not hide it. Visible (100e79a0: +0xb0 bit 6 and not +0xe0 bit 14)
/// always shows the name (field 0x25, +0xb4); hidden shows it only with +0xb0
/// bit 0 (100e7950).
fn shown_details(game: &[u8; 0x100], own_zone: Option<usize>) -> (bool, Option<usize>) {
    let visible = game[DETAIL] & 0x40 != 0
        && u32::from_le_bytes(game[PRIVACY..PRIVACY + 4].try_into().unwrap()) & (1 << HIDDEN_BIT) == 0;
    let zone = u16::from_le_bytes(game[ZONE..ZONE + 2].try_into().unwrap()) as usize;
    (visible || game[DETAIL] & 1 != 0, own_zone.filter(|own| *own != zone).map(|_| zone))
}

// Anonymous counts only. Inspect the stages separately: treating every blank
// result as "no FFXI friends" concealed whether POL, the game cache or its
// localized resources lacked a field. Never retain or log record identities.
#[derive(Debug, Default, PartialEq, Eq)]
struct Diagnostics {
    catalog: usize,
    online: usize,
    unknown: usize,
    applications: BTreeMap<u16, usize>,
    modes: BTreeMap<u8, usize>,
    linked: usize,
    selected_classes: BTreeMap<u16, usize>,
    game_ready: bool,
    matched: usize,
    game_applications: BTreeMap<u16, usize>,
    same_world_links: usize,
    completed: usize,
    characters: usize,
    visible_areas: usize,
    localized_areas: usize,
}

fn diagnostics(base: usize, layout: &Friends, native: &[u8], reader: &Reader<'_>) -> Diagnostics {
    let mut out = Diagnostics::default();
    if native.len() != proto::FRIENDS_LEN {
        return out;
    }
    let root = base
        .checked_add(layout.lobby)
        .and_then(|at| dword(reader, at))
        .filter(|r| *r != 0);
    let own_world = root
        .and_then(|r| word(reader, r.checked_add(OWN_WORLD)?))
        .filter(|w| *w != 0xffff);
    out.game_ready = own_world.is_some();
    let own_zone = own_zone(base, layout, reader);
    for (index, r) in native[8..]
        .as_chunks::<{ proto::FRIEND_RECORD_LEN }>()
        .0
        .iter()
        .enumerate()
    {
        if r[IN_USE] & 1 == 0 {
            continue;
        }
        out.catalog += 1;
        if index >= 200 || r[0x10..0x18] == [0; 8] {
            out.unknown += 1;
            continue;
        }
        let flags = u64::from_le_bytes(r[8..16].try_into().unwrap());
        if flags & (1 << 11) == 0 {
            continue;
        }
        out.online += 1;
        *out.applications
            .entry(((flags >> 33) & 0x3ff) as u16)
            .or_default() += 1;
        *out.modes.entry(((flags >> 13) & 7) as u8).or_default() += 1;
        if flags & (1 << 16) != 0 {
            let at = CHARACTERS + ((flags >> 17) & 7) as usize * 16;
            let character = &r[at..at + 16];
            if character[0] & 1 != 0 {
                out.linked += 1;
                *out.selected_classes
                    .entry(u16::from_le_bytes(character[2..4].try_into().unwrap()))
                    .or_default() += 1;
                out.same_world_links += usize::from(own_world == Some(character[CHARACTER_WORLD] as u16));
            }
        }
        let Some(root) = root else {
            continue;
        };
        let Some(at) = root.checked_add(ROWS + index * ROW_SIZE) else {
            continue;
        };
        let Some(game) = read::<0x100>(reader, at) else {
            continue;
        };
        if game[..8] != r[..8] || game[IN_USE..IN_USE + 8] != r[IN_USE..IN_USE + 8] {
            continue;
        }
        if read::<0x100>(reader, at).as_ref() != Some(&game) {
            continue;
        }
        out.matched += 1;
        let game_flags = u64::from_le_bytes(game[8..16].try_into().unwrap());
        *out.game_applications
            .entry(((game_flags >> 33) & 0x3ff) as u16)
            .or_default() += 1;
        if game[COMPLETED] & 1 == 0 {
            continue;
        }
        out.completed += 1;
        let (named, area) = shown_details(&game, own_zone);
        out.characters += usize::from(named);
        if let Some(zone) = area {
            out.visible_areas += 1;
            out.localized_areas += usize::from(area_name(base, layout, zone, reader).is_some());
        }
    }
    out
}

fn copy_locations(base: usize, layout: &Friends, native: &[u8], reader: &Reader<'_>) -> Option<Zeroizing<Vec<u8>>> {
    if native.len() != proto::FRIENDS_LEN {
        return None;
    }
    let root_at = base.checked_add(layout.lobby)?;
    let root = dword(reader, root_at)?;
    if root == 0 {
        return None;
    }
    let own_world = word(reader, root.checked_add(OWN_WORLD)?)?;
    if own_world == 0xffff {
        return None;
    }
    let own_zone = own_zone(base, layout, reader);
    let mut out = Zeroizing::new(vec![0; BLOB_LEN]);
    out[..8].copy_from_slice(MAGIC);
    for (index, record) in native[8..]
        .as_chunks::<{ proto::FRIEND_RECORD_LEN }>()
        .0
        .iter()
        .enumerate()
    {
        let Some(character) = selected(record) else {
            continue;
        };
        let detail = &mut out[8 + index * RECORD_LEN..][..RECORD_LEN];
        let world = character[CHARACTER_WORLD];
        // 101eaee6/100e7900: another world, or a row the game has not
        // completed (+0xfc bit 0), shows only the world label (100db7b0).
        let world_label = |detail: &mut [u8]| {
            if let Some(name) = world_name(root, world, reader) {
                detail[80..96].copy_from_slice(&name);
            }
        };
        if world as u16 != own_world {
            world_label(detail);
            continue;
        }
        let at = root.checked_add(ROWS + index * ROW_SIZE)?;
        let Some(game) = read::<0x100>(reader, at) else {
            continue;
        };
        // Account, handle and selected character must match. A slot can be
        // reused while POL and the game are updating their independent caches.
        let Some(game_character) = selected(&game) else {
            continue;
        };
        if game[..8] != record[..8]
            || game[IN_USE..IN_USE + 8] != record[IN_USE..IN_USE + 8]
            || game_character != character
        {
            continue;
        }
        if game[COMPLETED] & 1 == 0 {
            world_label(detail);
        } else {
            let packed = u32::from_le_bytes(character[4..8].try_into().ok()?);
            let id = (packed & 0xffff) | ((packed >> 8) & 0xf0000);
            if u32::from_le_bytes(game[CHAR_ID..CHAR_ID + 4].try_into().ok()?) != id {
                continue;
            }
            let (named, area) = shown_details(&game, own_zone);
            if named {
                if let Some(name) = bounded_text::<16>(reader, at.checked_add(NAME)?) {
                    detail[..16].copy_from_slice(&name);
                }
            }
            if let Some(name) = area.and_then(|zone| area_name(base, layout, zone, reader)) {
                detail[16..80].copy_from_slice(&name);
            }
        }
        // Reject a torn read instead of publishing mixed character/area data.
        if read::<0x100>(reader, at).as_ref() != Some(&game) {
            detail.fill(0);
        }
    }
    (dword(reader, root_at)? == root
        && word(reader, root.checked_add(OWN_WORLD)?)? == own_world
        && own_zone == self::own_zone(base, layout, reader))
        .then_some(out)
}

#[derive(Default)]
pub(crate) struct GameFriends {
    module: usize,
    layout: Option<Friends>,
    unavailable: bool,
    diagnostics: Option<Diagnostics>,
    inputs: u64,
    quiet_ticks: u32,
}
/// Snapshots between unconditional diagnostic re-walks (the observer ticks
/// every 500 ms, so about 10 s).
const RECOUNT_TICKS: u32 = 20;
impl GameFriends {
    pub(crate) fn snapshot(&mut self, native: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        if self.unavailable {
            return None;
        }
        if self.module == 0 {
            let name: Vec<u16> = "FFXiMain.dll\0".encode_utf16().collect();
            let mut module = std::ptr::null_mut();
            if unsafe { GetModuleHandleExW(0, name.as_ptr(), &mut module) } == 0 {
                return None;
            }
            self.module = module as usize;
        }
        if self.layout.is_none() {
            // None while FFXiMain is still initializing: try again next time.
            let resolution = game_layout::ffximain()?;
            let module = self.module;
            let layout = if resolution.base != module {
                Err("the resolved module is not this one".to_string())
            } else {
                resolution.friends.clone().and_then(|layout| {
                    verified(module, &process_read, &layout)
                        .map(|_| layout)
                        .ok_or_else(|| "live friend display code differs from the scanned image".to_string())
                })
            };
            match layout {
                Ok(layout) => {
                    self.layout = Some(layout);
                    log!("native friend locations: verified read-only game cache enabled");
                }
                Err(reason) => {
                    self.unavailable = true;
                    log!("native friend locations unavailable: {reason}");
                    return None;
                }
            }
        }
        let layout = self.layout?;
        let locations = copy_locations(self.module, &layout, native, &process_read);
        // The stage counts are re-walked when the POL list or the published
        // result changed, and every RECOUNT_TICKS otherwise: several counts
        // read game rows that can change while both inputs stay the same.
        let inputs = fingerprint(native, locations.as_deref().map(Vec::as_slice));
        self.quiet_ticks = self.quiet_ticks.saturating_add(1);
        if self.diagnostics.is_none() || self.inputs != inputs || self.quiet_ticks >= RECOUNT_TICKS {
            self.inputs = inputs;
            self.quiet_ticks = 0;
            let counts = diagnostics(self.module, &layout, native, &process_read);
            if self.diagnostics.as_ref() != Some(&counts) {
                log!("native friend display inputs: {counts:?}");
                self.diagnostics = Some(counts);
            }
        }
        locations
    }
}

/// A change detector for the native list and the published blob; neither is
/// kept.
fn fingerprint(native: &[u8], locations: Option<&[u8]>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    (native, locations).hash(&mut hasher);
    hasher.finish()
}
impl Drop for GameFriends {
    fn drop(&mut self) {
        if self.module != 0 {
            unsafe {
                FreeLibrary(self.module as _);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::BTreeMap};
    const L: Friends = game_layout::BUILD_6A995428.friends;
    const BASE: usize = 0x10000000;
    const ROOT: usize = 0x20000000;
    const HEADER: usize = 0x21000000;
    const DATA: usize = 0x22000000;
    const ZONE: usize = 0x23000000;
    const FALLBACK_HEADER: usize = 0x24000000;
    const FALLBACK_DATA: usize = 0x25000000;
    struct Fixture {
        memory: BTreeMap<usize, Vec<u8>>,
        native: Vec<u8>,
    }
    impl Fixture {
        fn new() -> Self {
            let mut f = Self {
                memory: BTreeMap::new(),
                native: vec![0; proto::FRIENDS_LEN],
            };
            f.native[..8].copy_from_slice(b"XIPOLF\x01\0");
            let rec = &mut f.native[8..8 + proto::FRIEND_RECORD_LEN];
            rec[..8].copy_from_slice(&123u64.to_le_bytes());
            rec[8..16].copy_from_slice(&((1u64 << 11) | (1u64 << 16) | (1u64 << 33)).to_le_bytes());
            rec[0x10..0x18].copy_from_slice(&321u64.to_le_bytes());
            rec[0x18] = 1;
            rec[0x1a] = 1;
            rec[0x1c..0x20].copy_from_slice(&((28u32 << 16) | 42).to_le_bytes());
            rec[0x98] = 1;
            f.memory.insert(
                BASE + L.lobby,
                (ROOT as u32).to_le_bytes().to_vec(),
            );
            f.memory.insert(ROOT, vec![0; 0x14300]);
            f.put(ROOT + 0x130, &28u16.to_le_bytes());
            let rec = f.native[8..8 + proto::FRIEND_RECORD_LEN].to_vec();
            f.put(ROOT + 0xa90, &rec);
            f.put(ROOT + 0xa90 + 0xfc, &[1]);
            f.put(ROOT + 0xa90 + 0xb0, &[0x41]);
            f.put(ROOT + 0xa90 + 0xb4, b"Alice\0");
            f.put(ROOT + 0xa90 + 0xc8, &42u32.to_le_bytes());
            f.put(ROOT + 0xa90 + 0xd8, &1u16.to_le_bytes());
            f.put(ROOT + 0xa90 + 0xe0, &0u16.to_le_bytes());
            f.memory.insert(BASE + 0x5785f8, vec![0; 16]);
            f.put(BASE + 0x5785fc, &(HEADER as u32).to_le_bytes());
            f.put(BASE + 0x578604, &(DATA as u32).to_le_bytes());
            f.memory.insert(HEADER, vec![0; 64]);
            f.put(HEADER + 8, &0x10u16.to_le_bytes());
            f.put(HEADER + 0x28, &2u32.to_le_bytes());
            f.put(HEADER + 0x3e, &1u16.to_le_bytes());
            f.memory.insert(DATA, vec![0; 160]);
            f.put(DATA + 8, &32u32.to_le_bytes());
            f.put(DATA + 12, &1u32.to_le_bytes());
            f.put(DATA + 32, b"Bastok Markets\0");
            // I am in zone 5; the friend's row says zone 1.
            f.memory.insert(BASE + L.own_zone, (ZONE as u32).to_le_bytes().to_vec());
            f.memory.insert(ZONE + OWN_ZONE_FIELD, 5u32.to_le_bytes().to_vec());
            // Group 9 (managed), index 0x40.
            f.memory.insert(BASE + L.fallback.object, vec![0; 16]);
            f.put(BASE + L.fallback.object + 4, &(FALLBACK_HEADER as u32).to_le_bytes());
            f.put(BASE + L.fallback.object + 12, &(FALLBACK_DATA as u32).to_le_bytes());
            f.memory.insert(FALLBACK_HEADER, vec![0; 64]);
            f.put(FALLBACK_HEADER + 8, &0x10u16.to_le_bytes());
            f.put(FALLBACK_HEADER + 0x28, &0x41u32.to_le_bytes());
            f.put(FALLBACK_HEADER + 0x3e, &1u16.to_le_bytes());
            f.memory.insert(FALLBACK_DATA, vec![0; 0x300]);
            f.put(FALLBACK_DATA + 0x200, &0x280u32.to_le_bytes());
            f.put(FALLBACK_DATA + 0x204, &1u32.to_le_bytes());
            f.put(FALLBACK_DATA + 0x280, b"Somewhere\0");
            // World table entry for my world 28.
            f.put(ROOT + 0x14140, &1u32.to_le_bytes());
            f.put(ROOT + 0x14144, &28u32.to_le_bytes());
            f.put(ROOT + 0x14148, b"Bahamut\0");
            f
        }
        fn put(&mut self, at: usize, bytes: &[u8]) {
            let (&start, block) = self.memory.range_mut(..=at).next_back().unwrap();
            block[at - start..at - start + bytes.len()].copy_from_slice(bytes);
        }
        fn read(&self, at: usize, out: &mut [u8]) -> bool {
            let Some((&start, block)) = self.memory.range(..=at).next_back() else {
                return false;
            };
            let Some(bytes) = block.get(at - start..at - start + out.len()) else {
                return false;
            };
            out.copy_from_slice(bytes);
            true
        }
        fn locations(&self) -> Zeroizing<Vec<u8>> {
            copy_locations(BASE, &L, &self.native, &|a, b| self.read(a, b)).unwrap()
        }
    }
    #[test]
    fn same_world_reads_character_and_localized_area_without_world_label() {
        let f = Fixture::new();
        let loc = f.locations();
        assert_eq!(&loc[..8], MAGIC);
        assert_eq!(&loc[8..14], b"Alice\0");
        assert_eq!(&loc[24..39], b"Bastok Markets\0");
        assert!(loc[88..104].iter().all(|b| *b == 0));
    }
    #[test]
    fn diagnostics_distinguish_missing_application_from_available_game_fields() {
        let mut f = Fixture::new();
        // Keep the selected character and completed game lookup; remove only
        // the native record's reported application. The aggregate must reveal
        // this difference without logging a name, account or character ID.
        let flags = (1u64 << 11) | (1u64 << 16);
        f.native[16..24].copy_from_slice(&flags.to_le_bytes());
        let counts = diagnostics(BASE, &L, &f.native, &|a, b| f.read(a, b));
        assert_eq!((counts.catalog, counts.online, counts.linked), (1, 1, 1));
        assert_eq!(counts.applications.get(&0), Some(&1));
        assert_eq!(counts.selected_classes.get(&1), Some(&1));
        assert_eq!(counts.game_applications.get(&1), Some(&1));
        assert_eq!(
            (
                counts.completed,
                counts.characters,
                counts.visible_areas,
                counts.localized_areas
            ),
            (1, 1, 1, 1)
        );
        let text = format!("{counts:?}");
        assert!(!text.contains("Alice") && !text.contains("Bastok") && !text.contains("123"));
        assert_eq!(
            diagnostics(BASE, &L, &f.native[..1], &|_, _| false),
            Diagnostics::default()
        );
    }
    #[test]
    fn privacy_hides_only_the_name_and_an_incomplete_row_shows_the_world() {
        let mut f = Fixture::new();
        // Hidden (+0xe0 bit 14) with +0xb0 bit 0: name and area (101eaf49).
        f.put(ROOT + 0xa90 + 0xe0, &0x4001u16.to_le_bytes());
        assert_eq!(&f.locations()[8..14], b"Alice\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        // Hidden without bit 0: the area only (100e7950).
        f.put(ROOT + 0xa90 + 0xb0, &[0x40]);
        assert!(f.locations()[8..24].iter().all(|b| *b == 0));
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        // Visible: the name even without bit 0 (101eaff6).
        f.put(ROOT + 0xa90 + 0xe0, &0u16.to_le_bytes());
        assert_eq!(&f.locations()[8..14], b"Alice\0");
        // Not completed by the game (+0xfc bit 0): the world label only.
        f.put(ROOT + 0xa90 + 0xfc, &[0]);
        let loc = f.locations();
        assert!(loc[8..88].iter().all(|b| *b == 0));
        assert_eq!(&loc[88..96], b"Bahamut\0");
    }
    #[test]
    fn the_area_is_hidden_only_in_my_own_zone_or_without_my_zone() {
        let mut f = Fixture::new();
        f.put(ZONE + OWN_ZONE_FIELD, &1u32.to_le_bytes());
        assert!(f.locations()[24..88].iter().all(|b| *b == 0));
        assert_eq!(&f.locations()[8..14], b"Alice\0");
        f.put(ZONE + OWN_ZONE_FIELD, &5u32.to_le_bytes());
        f.put(BASE + L.own_zone, &0u32.to_le_bytes());
        assert!(f.locations()[24..88].iter().all(|b| *b == 0));
    }
    #[test]
    fn a_missing_area_name_uses_the_group_9_fallback() {
        let mut f = Fixture::new();
        f.put(ROOT + 0xa90 + 0xd8, &7u16.to_le_bytes());
        assert_eq!(&f.locations()[24..34], b"Somewhere\0");
    }
    #[test]
    fn zone_id_is_separate_from_status_bits_and_changes_with_the_game_cache() {
        let mut f = Fixture::new();
        // Two real table entries make a mistaken privacy-as-zone lookup return
        // plausible text, rather than just failing on an out-of-range index.
        f.put(DATA, &64u32.to_le_bytes());
        f.put(DATA + 4, &1u32.to_le_bytes());
        f.put(DATA + 64, b"Windurst Waters\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        f.put(ROOT + 0xa90 + 0xe0, &0x3000u16.to_le_bytes());
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        f.put(ROOT + 0xa90 + 0xd8, &0u16.to_le_bytes());
        assert_eq!(&f.locations()[24..40], b"Windurst Waters\0");
        // Bit 14 hides the name, never the area (101eaf49).
        f.put(ROOT + 0xa90 + 0xe0, &0x4000u16.to_le_bytes());
        assert_eq!(&f.locations()[24..40], b"Windurst Waters\0");
        assert_eq!(&f.locations()[8..14], b"Alice\0");
    }
    #[test]
    fn legacy_area_fallback_matches_both_native_table_formats() {
        let mut f = Fixture::new();
        f.put(BASE + 0x5785fc, &0u32.to_le_bytes());
        f.memory
            .insert(BASE + 0x578558, (HEADER as u32).to_le_bytes().to_vec());
        f.memory.insert(HEADER, vec![0; 160]);
        f.put(HEADER + 0xa, &1u16.to_le_bytes());
        f.put(HEADER + 0x24, &2u32.to_le_bytes());
        f.put(HEADER + 0x3c, &8u16.to_le_bytes());
        f.put(HEADER + 0x48, b"Bastok Markets\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        f.put(HEADER + 0xa, &2u16.to_le_bytes());
        f.put(HEADER + 0x44, &8u32.to_le_bytes());
        f.put(HEADER + 0x58, b"Bastok Markets\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        assert!(resource_text(BASE, &L, L.area, 2, &|a, b| f.read(a, b)).is_none());
        let reads = Cell::new(0);
        assert!(resource_text(BASE, &L, L.area, 1, &|at, out| {
            let ok = f.read(at, out);
            if at == BASE + 0x578558 {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    out.fill(0);
                }
            }
            ok
        })
        .is_none());
    }
    #[test]
    fn another_world_uses_native_world_name_and_does_not_reuse_same_world_details() {
        let mut f = Fixture::new();
        f.native[8 + 0x1e] = 16;
        f.put(ROOT + 0x14140, &1u32.to_le_bytes());
        f.put(ROOT + 0x14144, &16u32.to_le_bytes());
        f.put(ROOT + 0x14148, b"Phoenix\0");
        let loc = f.locations();
        assert!(loc[8..88].iter().all(|b| *b == 0));
        assert_eq!(&loc[88..96], b"Phoenix\0");
    }
    #[test]
    fn stale_character_account_or_unknown_presence_cannot_supply_details() {
        let mut f = Fixture::new();
        f.put(ROOT + 0xa90 + 0xc8, &43u32.to_le_bytes());
        assert!(f.locations()[8..104].iter().all(|b| *b == 0));
        f.put(ROOT + 0xa90 + 0xc8, &42u32.to_le_bytes());
        f.put(ROOT + 0xa90, &124u64.to_le_bytes());
        assert!(f.locations()[8..104].iter().all(|b| *b == 0));
        f.put(ROOT + 0xa90, &123u64.to_le_bytes());
        f.native[8 + 0x10..8 + 0x18].fill(0);
        assert!(f.locations()[8..104].iter().all(|b| *b == 0));
    }
    #[test]
    fn concurrent_row_update_is_rejected_and_unmapped_root_is_safe() {
        let f = Fixture::new();
        let reads = Cell::new(0);
        let loc = copy_locations(BASE, &L, &f.native, &|at, out| {
            let ok = f.read(at, out);
            if at == ROOT + 0xa90 && out.len() == 0x100 {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    out[0xe0] ^= 1;
                }
            }
            ok
        })
        .unwrap();
        assert!(loc[8..104].iter().all(|b| *b == 0));
        assert!(copy_locations(BASE, &L, &f.native, &|_, _| false).is_none());
    }
    #[test]
    fn supported_layout_checks_relocation_machine_and_getter_code_at_resolved_sites() {
        let mut f = Fixture::new();
        f.memory.insert(BASE, vec![0; 512]);
        f.put(BASE + 0x3c, &0x80u32.to_le_bytes());
        f.put(BASE + 0x80, b"PE\0\0");
        f.put(BASE + 0x84, &0x14cu16.to_le_bytes());
        let mut getter = vec![
            0x66, 0x8b, 0x44, 0x24, 4, 0x66, 0x3d, 0x2c, 1, 0x7c, 3, 0x33, 0xc0, 0xc3, 0x8b, 0x0d,
        ];
        getter.extend_from_slice(&((BASE + L.lobby) as u32).to_le_bytes());
        getter.extend_from_slice(&[
            0x0f, 0xbf, 0xc0, 0xc1, 0xe0, 8, 0x8d, 0x84, 8, 0x90, 0x0a, 0, 0, 0xc3,
        ]);
        f.memory.insert(BASE + L.cache_getter, getter);
        f.memory.insert(
            BASE + L.name_getter,
            vec![
                0x8b, 0x44, 0x24, 4, 0xf6, 0x80, 0xb0, 0, 0, 0, 1, 0x74, 6, 5, 0xb4, 0, 0, 0, 0xc3,
                0x33, 0xc0, 0xc3,
            ],
        );
        let mut own = vec![0x8b, 0x0d];
        own.extend_from_slice(&((BASE + L.own_zone) as u32).to_le_bytes());
        f.memory.insert(BASE + L.display_area + 0x1d, own);
        f.memory.insert(BASE + L.display_area + 0x2b, vec![0x8b, 0x91, 0x50, 0xc3, 3, 0]);
        f.memory.insert(BASE + L.area_fn + 0x2a, vec![0xc7, 0x44, 0x24, 8, 0x40, 0, 0, 0, 0xc7, 0x44, 0x24, 4, 9, 0, 0, 0]);
        f.memory.insert(BASE + L.group_table, vec![0; 0xe0]);
        for group in [L.area, L.fallback] {
            f.put(BASE + L.group_table + group.entry * 8, &(group.id as u32).to_le_bytes());
            f.put(BASE + L.group_table + group.entry * 8 + 4, &((BASE + group.object) as u32).to_le_bytes());
        }
        assert!(verified(BASE, &|a, b| f.read(a, b), &L).is_some());
        // The same code elsewhere is not this build's resolved site.
        let moved = Friends { cache_getter: L.cache_getter + 0x20, ..L };
        assert!(verified(BASE, &|a, b| f.read(a, b), &moved).is_none());
        f.put(BASE + L.group_table + 3 * 8, &10u32.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b), &L).is_none(), "group table must map 9");
        f.put(BASE + L.group_table + 3 * 8, &9u32.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b), &L).is_some());
        f.put(BASE + 0x84, &0x8664u16.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b), &L).is_none());
        f.put(BASE + 0x84, &0x14cu16.to_le_bytes());
        f.put(BASE + L.cache_getter + 16, &0u32.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b), &L).is_none());
    }
}
