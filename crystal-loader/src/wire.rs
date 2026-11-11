//! Constants and encoders of the launcher <-> loader wire contract that both
//! sides must agree on byte for byte. The launcher (`src/main.rs`) and the
//! loader itself include this file via `#[path]`, so it depends on
//! `core`/`std` only and changes here change both ends together.

/// The one protocol major version (`hello`'s and the session lines' `v`).
pub const PROTOCOL: u32 = 1;
/// The longest line either side accepts, without its `\n`.
pub const MAX_LINE: usize = 1 << 20;

/// `native_friends` payload: polcore's two retained friend tables
/// (200 + 100 records of 0xb0 bytes) behind this magic.
pub const FRIENDS_MAGIC: &[u8; 8] = b"XIPOLF\x01\0";
pub const FRIEND_RECORD_LEN: usize = 0xb0;
pub const FRIEND_SLOTS: usize = 300;
pub const FRIENDS_LEN: usize = 8 + FRIEND_SLOTS * FRIEND_RECORD_LEN;

/// `native_friends` `locations`: one game-cache display record per friend slot.
pub const LOCATIONS_MAGIC: &[u8; 8] = b"XIPOLG\x01\0";
/// CP932: character[16], area[64], world[16].
pub const LOCATION_RECORD_LEN: usize = 96;
pub const LOCATIONS_LEN: usize = 8 + FRIEND_SLOTS * LOCATION_RECORD_LEN;

/// A native social mail header (polcore's 0x48-byte record header).
pub const SOCIAL_HEADER_LEN: usize = 0x48;

/// A PlayOnline portrait ID: 13 bits (0 is the default silhouette).
pub const PORTRAIT_MASK: u16 = 0x1fff;

/// Whether a live test has confirmed where the handle record keeps the
/// portrait. Until then Crystal only reads and shows it: neither the launcher
/// nor the loader ever writes a portrait to the server on a guess.
pub const PORTRAIT_FIELD_CONFIRMED: bool = false;

/// The `portrait` control result when Put Handle List succeeded but the handle
/// record read back afterwards (173) does not hold the chosen portrait.
pub const PORTRAIT_NOT_SAVED: i32 = -5121;

/// The handle-record (polcore 173/174/197, 0x28 bytes) field that holds the
/// portrait. UNVERIFIED: switching candidates is this one line.
const HANDLE_PORTRAIT: HandlePortrait = HandlePortrait::FlagsBits32;

#[derive(Clone, Copy)]
#[allow(dead_code)] // the candidate not chosen above
enum HandlePortrait {
    /// Bits 32..44 of the flags qword at +0 (173 reads, 174 copies it).
    FlagsBits32,
    /// The low 13 bits of the dword at +0x1c.
    Dword1c,
}

fn dword(record: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([record[at], record[at + 1], record[at + 2], record[at + 3]])
}

/// The portrait of a handle record. Every reader goes through here.
pub fn handle_portrait(record: &[u8]) -> u16 {
    let (flags, low) = handle_portrait_candidates(record);
    match HANDLE_PORTRAIT {
        HandlePortrait::FlagsBits32 => flags,
        HandlePortrait::Dword1c => low as u16 & PORTRAIT_MASK,
    }
}

/// `record` with only its portrait bits replaced by `portrait`.
pub fn set_handle_portrait(record: &mut [u8], portrait: u16) {
    let portrait = u32::from(portrait & PORTRAIT_MASK);
    let at = match HANDLE_PORTRAIT {
        HandlePortrait::FlagsBits32 => 4, // bits 32..44 of the qword at +0
        HandlePortrait::Dword1c => 0x1c,
    };
    let value = dword(record, at) & !u32::from(PORTRAIT_MASK) | portrait;
    record[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Both candidate fields of a handle record, for the sign-in probe: the
/// 13-bit field at bits 32..44 of +0, and the whole dword at +0x1c.
pub fn handle_portrait_candidates(record: &[u8]) -> (u16, u32) {
    (dword(record, 4) as u16 & PORTRAIT_MASK, dword(record, 0x1c))
}

/// The portrait of a friend record (0xb0 bytes): `+0x9c & 0x1fff`
/// (UNVERIFIED; bits 13..18 of the same dword are the target handle).
pub fn friend_portrait(record: &[u8]) -> u16 {
    dword(record, 0x9c) as u16 & PORTRAIT_MASK
}

/// Appends `s` as a JSON string literal (with quotes).
pub fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// JSON string literal (with quotes) for arbitrary text.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    push_json_string(&mut out, s);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_string_escapes_quotes_backslashes_and_controls() {
        assert_eq!(super::json_string("C:\\Games\\Square\"Enix\n\t\u{1}é"), r#""C:\\Games\\Square\"Enix\n\t\u0001é""#);
    }

    #[test]
    fn a_portrait_change_touches_only_the_portrait_bits() {
        let mut record = [0xffu8; 0x28];
        let before = record;
        super::set_handle_portrait(&mut record, 0x0123);
        assert_eq!(super::handle_portrait(&record), 0x0123);
        let changed: Vec<usize> = (0..0x28).filter(|&i| record[i] != before[i]).collect();
        assert!(!changed.is_empty() && changed.iter().all(|&i| (4..6).contains(&i) || (0x1c..0x1e).contains(&i)), "{changed:?}");
        super::set_handle_portrait(&mut record, 0xffff);
        assert_eq!(super::handle_portrait(&record), super::PORTRAIT_MASK, "13 bits only");
        assert_eq!(record, before);
        let mut friend = [0u8; 0xb0];
        friend[0x9c..0xa0].copy_from_slice(&((5u32 << 13) | 77).to_le_bytes());
        assert_eq!(super::friend_portrait(&friend), 77, "the target handle bits are not the portrait");
    }
}
