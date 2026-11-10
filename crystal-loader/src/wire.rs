//! Constants and encoders of the launcher <-> loader wire contract that both
//! sides must agree on byte for byte. The launcher (`src/main.rs`) and
//! `crates/pol-auth` include this file via `#[path]`, so it depends on
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
}
