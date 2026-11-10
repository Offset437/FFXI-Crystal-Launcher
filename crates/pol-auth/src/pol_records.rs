//! The POL handle and character tables that polcore keeps after a sign-in,
//! rebuilt from the Get Handle List (0,9) and Get Chr List (1,3) catalogs.
//!
//! A POL-less loader answers FFXiMain's common-function-table slots 173 (handle
//! record, 0x28 bytes) and 177 (character record, 0x68 bytes) from these
//! tables. Without a matching slot-177 record, world entry fails (FFXiMain mode
//! 3 state 14, `gamecore-slots.presence.txt` section 1a).
//!
//! Evidence (installed polcore 73b1864b, preferred VAs, static reading only):
//! - Handle table `0x10405800`, 64 x 0x28, filled by the (0,9) poll at
//!   `1001e24c..1001e321` (`profile-wire-1001e050.txt`).
//! - Character table `0x10403080`, 64 x 0x68, filled by the (1,3) poll at
//!   `10022467..100226c7` (`signin-sequence-focused-disassembly.txt`), which
//!   also writes the handle records' eight character-slot bytes at +0x20
//!   (the `SelectionMap`).
//! - Slots 173/177 are plain copies of those records (`0x1001cc40`,
//!   `0x100227d0`).
//!
//! The field transforms below reproduce those stores exactly. Field meanings
//! beyond what FFXiMain reads (see the layout in [`PolRecords::to_blob`]) are not
//! inferred. Not live-verified: a recorded retail table has not been compared.

use crate::catalog::{SelectionMap, SelectionMapCatalog, ServiceCatalog};
use zeroize::{Zeroize, Zeroizing};

pub const SLOTS: usize = 64;
/// Slot 173 record size.
pub const HANDLE_RECORD_LEN: usize = 0x28;
/// Slot 177 record size.
pub const CHARACTER_RECORD_LEN: usize = 0x68;
/// `"XIPOLR"`, format version 1, NUL.
pub const MAGIC: [u8; 8] = *b"XIPOLR\x01\0";
pub const PRESENCE_OFFSET: usize = 8;
pub const PRESENCE_LEN: usize = 24;
pub const HANDLES_OFFSET: usize = PRESENCE_OFFSET + PRESENCE_LEN;
pub const CHARACTERS_OFFSET: usize = HANDLES_OFFSET + SLOTS * HANDLE_RECORD_LEN;
/// Total serialized length: 0x2420 (9248) bytes.
pub const BLOB_LEN: usize = CHARACTERS_OFFSET + SLOTS * CHARACTER_RECORD_LEN;

/// polcore's retained ChangeMyStatus (community 4,5) state: the values slots
/// 190/189/204/206/207 report and that slot 195's `-2` arguments keep.
/// Globals `1007541c`, `10075420`, `100ac54c`, `10075424`, `100aaac8`,
/// `10075428`; all updated together on a successful (4,5) (`1001de49..85`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresenceState {
    /// Slot 190 "chan": selected handle 0..63, or -1.
    pub handle: i32,
    /// Slot 189 "sub": selected character-table index 0..63, or -1.
    pub primary: i32,
    /// Slot 204 "flag": the community context boolean, 0 or 1.
    pub context: i32,
    /// Slot 206 "mode": selection type -1..4 (sign-in reset 4).
    pub selection_type: i32,
    /// Retained flag integer; the wire carries `flag != 0`.
    pub flag: i32,
    /// Slot 207 "word": content class 0..1023 (reset 1000, FFXI lobby 1).
    pub content_class: i32,
}

impl PresenceState {
    /// Native reset `1001c9d0` values (no sign-in).
    pub const RESET: Self = Self {
        handle: -1,
        primary: -1,
        context: 0,
        selection_type: 4,
        flag: 0,
        content_class: 1000,
    };

    pub fn to_bytes(&self) -> [u8; PRESENCE_LEN] {
        let mut out = [0; PRESENCE_LEN];
        for (chunk, value) in out.chunks_exact_mut(4).zip([
            self.handle,
            self.primary,
            self.context,
            self.selection_type,
            self.flag,
            self.content_class,
        ]) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

/// The two native tables, zeroed on drop. No Debug: handle names and
/// character identifiers are account data.
pub struct PolRecords {
    handles: Zeroizing<[[u8; HANDLE_RECORD_LEN]; SLOTS]>,
    characters: Zeroizing<[[u8; CHARACTER_RECORD_LEN]; SLOTS]>,
}

impl PolRecords {
    /// Rebuild both tables as a fresh polcore would after fetching (0,9) then
    /// (1,3). `map` must be the `SelectionMap` refreshed from `characters`.
    pub fn build(
        handles: &ServiceCatalog,
        characters: &SelectionMapCatalog,
        map: &SelectionMap,
    ) -> Self {
        let mut out = Self {
            handles: Zeroizing::new([[0; HANDLE_RECORD_LEN]; SLOTS]),
            characters: Zeroizing::new([[0; CHARACTER_RECORD_LEN]; SLOTS]),
        };
        for record in &handles.records {
            let raw = record.raw();
            let slot = usize::from(record.slot());
            if slot >= SLOTS {
                continue; // The catalog reader already omits these.
            }
            let h = &mut out.handles[slot];
            // q[0]: bit0 valid; bits1..44 = wire flags44; bits45..50 = slot.
            let old = le64(&h[0..8]);
            let mut q = (old & (1 | (0xffff_e000u64 << 32))) | (record.flags() << 1) | 1;
            q = (q & !(0x3fu64 << 45)) | ((slot as u64 & 0x3f) << 45);
            h[0..8].copy_from_slice(&q.to_le_bytes());
            bounded_copy(&mut h[8..0x18], &raw[16..32], 15);
            // d[0x18]: bits0..1 = kind, bits2..7 = order; higher bits kept.
            let mut d18 = le32(&h[0x18..0x1c]);
            d18 = (d18 & !3) | u32::from(record.kind());
            d18 = (d18 & !0xfc) | (u32::from(record.order()) << 2);
            h[0x18..0x1c].copy_from_slice(&d18.to_le_bytes());
            h[0x1c..0x20].copy_from_slice(&record.field_04().to_le_bytes());
        }
        for (h, bytes) in out.handles.iter_mut().zip(map.bytes().iter()) {
            h[0x20..0x28].copy_from_slice(bytes);
        }
        for record in &characters.records {
            let raw = record.raw();
            let index = usize::from(raw[0]);
            if index >= SLOTS {
                continue;
            }
            let c = &mut out.characters[index];
            c[0] |= 1;
            c[2..4].copy_from_slice(&raw[8..10]);
            c[4..8].copy_from_slice(&raw[12..16]);
            c[8..12].copy_from_slice(&raw[16..20]);
            c[12..16].copy_from_slice(&raw[20..24]);
            bounded_copy(&mut c[0x18..0x28], &raw[24..40], 15);
            let mut q = le64(&c[0x10..0x18]);
            q = (q & !(0x3fu64 << 16)) | (u64::from(raw[5] & 0x3f) << 16);
            q = (q & !0x8000) | (u64::from(raw[4] & 1) << 15);
            q = (q & !(7u64 << 22)) | (u64::from(raw[5] & 7) << 22);
            if u16::from_le_bytes([raw[8], raw[9]]) >= 4 && raw[10] != 0 {
                q = (q & !(0xffu64 << 26)) | (u64::from(raw[10]) << 26);
                q = (q & !(0xffu64 << 34)) | (u64::from(raw[11]) << 34);
                let pair_is_0x30 = raw[10] == 0x30 && raw[11] == 0x30;
                q = (q & !(1u64 << 25)) | (u64::from(!pair_is_0x30) << 25);
            } else {
                q &= !((0xffu64 << 26) | (0xffu64 << 34) | (1u64 << 25));
            }
            q &= !(0x7fu64 << 8);
            q = (q & !0xff) | u64::from(raw[1]);
            c[0x10..0x18].copy_from_slice(&q.to_le_bytes());
            bounded_copy(&mut c[0x28..0x68], &raw[40..104], 63);
        }
        out
    }

    pub fn handle(&self, index: usize) -> Option<&[u8; HANDLE_RECORD_LEN]> {
        self.handles.get(index)
    }
    pub fn character(&self, index: usize) -> Option<&[u8; CHARACTER_RECORD_LEN]> {
        self.characters.get(index)
    }

    /// Content IDs (`d[8]`) of valid FFXI character records (`b[0]` bit0,
    /// `w[2] == 1`), in table order. These are the values slot 177 reports and
    /// FFXiMain matches against its lobby character list.
    pub fn content_ids(&self) -> Vec<u32> {
        self.characters
            .iter()
            .filter(|c| c[0] & 1 != 0 && u16::from_le_bytes([c[2], c[3]]) == 1)
            .map(|c| le32(&c[8..12]))
            .collect()
    }

    /// The serialized `pol_records` blob, exactly `BLOB_LEN` (0x2420) bytes, all
    /// integers little-endian:
    ///
    /// | Offset | Size | Content |
    /// |---|---|---|
    /// | 0x0000 | 8 | magic `"XIPOLR"`, version byte `0x01`, `0x00` |
    /// | 0x0008 | 24 | presence store, six `i32`: handle (slot 190), primary (189), context (204), type (206), flag, content class (207) |
    /// | 0x0020 | 64 x 0x28 | handle table; record `i` is what slot 173(`i`) copies out |
    /// | 0x0A20 | 64 x 0x68 | character table; record `i` is what slot 177(`i`) copies out |
    ///
    /// Handle record (0x28): `q[0]` bit0 valid, bits 1..44 server flags, bits
    /// 45..50 slot; `+0x08` name, 15 bytes + NUL; `d[0x18]` bits 0..1 status/kind,
    /// bits 2..7 order; `d[0x1c]` server word; `+0x20` eight character-slot bytes
    /// (bit0 used, bits 1..6 = character index).
    ///
    /// Character record (0x68): `b[0]` bit0 valid; `w[2]` type (1 = FFXI);
    /// `d[4]` packed character id `(id & 0xffff) | world << 16 | (id >> 16 & 0xff) << 24`;
    /// `d[8]` content id; `d[0xc]` 0 for FFXI; `q[0x10]` bits 0..7 server flags,
    /// bit 15 linked to a handle, bits 16..21 handle index; `+0x18` name
    /// (15 + NUL); `+0x28` text (63 + NUL). Unused records are all zero.
    pub fn to_blob(&self, presence: &PresenceState) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(BLOB_LEN));
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&presence.to_bytes());
        for h in self.handles.iter() {
            out.extend_from_slice(h);
        }
        for c in self.characters.iter() {
            out.extend_from_slice(c);
        }
        debug_assert_eq!(out.len(), BLOB_LEN);
        out
    }
}

impl Zeroize for PolRecords {
    fn zeroize(&mut self) {
        self.handles.zeroize();
        self.characters.zeroize();
    }
}

/// polcore `10012330`: copy until NUL or `limit` bytes, then write one NUL.
fn bounded_copy(dst: &mut [u8], src: &[u8], limit: usize) {
    let n = src
        .iter()
        .take(limit)
        .position(|b| *b == 0)
        .unwrap_or(limit.min(src.len()));
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}
fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::{
        self, SelectionMapRecord, ServiceRecord, SELECTION_MAP_RECORD_LEN, SERVICE_RECORD_LEN,
    };

    pub(crate) fn handle_wire(slot: u8, name: &[u8]) -> [u8; SERVICE_RECORD_LEN] {
        let mut raw = [0; SERVICE_RECORD_LEN];
        raw[0] = slot;
        raw[1] = 0xc5; // order 5, high bits ignored
        raw[2] = 0xfe; // kind 2
        raw[4..8].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        raw[8..16].copy_from_slice(&0xffff_f123_4567_89abu64.to_le_bytes());
        raw[16..16 + name.len()].copy_from_slice(name);
        raw
    }

    /// A (1,3) record for a linked FFXI character.
    pub(crate) fn character_wire(
        index: u8,
        handle: u8,
        subslot: u8,
        content_id: u32,
        packed: u32,
    ) -> [u8; SELECTION_MAP_RECORD_LEN] {
        let mut raw = [0; SELECTION_MAP_RECORD_LEN];
        raw[0] = index;
        raw[1] = 0x9a;
        raw[4] = 1;
        raw[5] = handle;
        raw[6] = subslot;
        raw[8..10].copy_from_slice(&1u16.to_le_bytes());
        raw[12..16].copy_from_slice(&packed.to_le_bytes());
        raw[16..20].copy_from_slice(&content_id.to_le_bytes());
        raw[24..30].copy_from_slice(b"Mogchr");
        raw[40..44].copy_from_slice(b"Text");
        raw
    }

    pub(crate) fn fixture() -> (ServiceCatalog, SelectionMapCatalog, SelectionMap) {
        let handles = ServiceCatalog {
            prefix: Zeroizing::new([1, 0, 0, 0, 0, 0, 0, 0]),
            records: vec![ServiceRecord::parse(&handle_wire(0, b"SixteenCharName!")).unwrap()],
            ignored_records: 0,
        };
        let characters = SelectionMapCatalog {
            prefix: Zeroizing::new([2, 0, 0, 0, 0, 0, 0, 0]),
            records: vec![
                SelectionMapRecord::parse(&character_wire(2, 0, 1, 0x0001_e240, 0x1c03_0201))
                    .unwrap(),
                // Unlinked, non-FFXI (type 2) record: present but not a content ID.
                SelectionMapRecord::parse(&{
                    let mut r = character_wire(5, 0, 0, 77, 0);
                    r[4] = 0;
                    r[8] = 2;
                    r
                })
                .unwrap(),
            ],
            ignored_records: 0,
        };
        let mut map = SelectionMap::default();
        map.refresh(&characters).unwrap();
        (handles, characters, map)
    }

    #[test]
    fn character_record_is_what_ffximain_mode3_matches() {
        let (h, c, m) = fixture();
        let records = PolRecords::build(&h, &c, &m);
        let r = records.character(2).unwrap();
        // 0x100efe00 match: (b[0]&1) && w[2]==1 && d[8]==contentId && d[0xc]==0 && d[4]==packed.
        assert_eq!(r[0] & 1, 1);
        assert_eq!(u16::from_le_bytes([r[2], r[3]]), 1);
        assert_eq!(le32(&r[4..8]), 0x1c03_0201);
        assert_eq!(le32(&r[8..12]), 0x0001_e240);
        assert_eq!(le32(&r[12..16]), 0);
        let q = le64(&r[0x10..0x18]);
        assert_eq!(q & 0x8000, 0x8000, "registered to a handle");
        assert_eq!((q >> 16) & 0x3f, 0, "handle index");
        assert_eq!(q & 0xff, 0x9a);
        assert_eq!(&r[0x18..0x1f], b"Mogchr\0");
        assert_eq!(&r[0x28..0x2d], b"Text\0");
        // Unlinked record: bit15 clear; unused slots stay all-zero.
        let unlinked = records.character(5).unwrap();
        assert_eq!(le64(&unlinked[0x10..0x18]) & 0x8000, 0);
        assert!(records.character(0).unwrap().iter().all(|b| *b == 0));
        assert_eq!(records.content_ids(), [0x0001_e240]);
    }

    #[test]
    fn handle_record_has_native_bitfields_name_and_character_slots() {
        let (h, c, m) = fixture();
        let records = PolRecords::build(&h, &c, &m);
        let r = records.handle(0).unwrap();
        let q = le64(&r[0..8]);
        assert_eq!(q & 1, 1);
        assert_eq!((q >> 1) & ((1 << 44) - 1), 0x0123_4567_89ab);
        assert_eq!((q >> 45) & 0x3f, 0);
        // 15-byte bounded copy plus NUL, as 10012330.
        assert_eq!(&r[8..0x18], b"SixteenCharName\0");
        assert_eq!(le32(&r[0x18..0x1c]), 2 | (5 << 2));
        assert_eq!(le32(&r[0x1c..0x20]), 0x1122_3344);
        // Subslot 1 of handle 0 links character 2: valid | (2 << 1).
        assert_eq!(r[0x21], 0x05);
        assert_eq!(r[0x20], 0);
        assert!(records.handle(1).unwrap().iter().all(|b| *b == 0));
    }

    #[test]
    fn extended_flag_pair_sets_bits_26_to_41_and_bit25() {
        let mut raw = character_wire(1, 3, 0, 9, 9);
        raw[8..10].copy_from_slice(&4u16.to_le_bytes());
        for (pair, bit25) in [((0x30, 0x30), 0), ((0x30, 0x31), 1), ((0xff, 0x01), 1)] {
            raw[10] = pair.0;
            raw[11] = pair.1;
            let catalog = SelectionMapCatalog {
                prefix: Zeroizing::new([1, 0, 0, 0, 0, 0, 0, 0]),
                records: vec![SelectionMapRecord::parse(&raw).unwrap()],
                ignored_records: 0,
            };
            let mut map = SelectionMap::default();
            map.refresh(&catalog).unwrap();
            let services = catalog::ServiceCatalog {
                prefix: Zeroizing::new([0; 8]),
                records: Vec::new(),
                ignored_records: 0,
            };
            let records = PolRecords::build(&services, &catalog, &map);
            let q = le64(&records.character(1).unwrap()[0x10..0x18]);
            assert_eq!((q >> 26) & 0xff, u64::from(pair.0));
            assert_eq!((q >> 34) & 0xff, u64::from(pair.1));
            assert_eq!((q >> 25) & 1, bit25);
            assert_eq!((q >> 16) & 0x3f, 3);
            assert_eq!((q >> 22) & 7, 3);
            // Type 4 is not FFXI: no content ID.
            assert!(records.content_ids().is_empty());
        }
    }

    #[test]
    fn blob_layout_offsets_and_zeroize() {
        let (h, c, m) = fixture();
        let mut records = PolRecords::build(&h, &c, &m);
        let presence = PresenceState {
            handle: 0,
            primary: -1,
            context: 0,
            selection_type: 0,
            flag: 1,
            content_class: 1,
        };
        let blob = records.to_blob(&presence);
        assert_eq!(blob.len(), 0x2420);
        assert_eq!(&blob[..8], b"XIPOLR\x01\0");
        assert_eq!(&blob[8..12], &0i32.to_le_bytes());
        assert_eq!(&blob[12..16], &(-1i32).to_le_bytes());
        assert_eq!(&blob[28..32], &1i32.to_le_bytes());
        assert_eq!(
            &blob[HANDLES_OFFSET..HANDLES_OFFSET + HANDLE_RECORD_LEN],
            records.handle(0).unwrap()
        );
        let at = CHARACTERS_OFFSET + 2 * CHARACTER_RECORD_LEN;
        assert_eq!(
            &blob[at..at + CHARACTER_RECORD_LEN],
            records.character(2).unwrap()
        );
        records.zeroize();
        assert!(records.character(2).unwrap().iter().all(|b| *b == 0));
        assert!(records.handle(0).unwrap().iter().all(|b| *b == 0));
    }
}
