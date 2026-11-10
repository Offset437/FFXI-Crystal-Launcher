//! Crystal's packet encoding for PlayOnline's patch service.
//! Wire layout: size u32, truncated MD5, POLP, type u32, payload.
//! Multi-byte fields are little-endian; the digest covers magic/type/payload.

pub const MAGIC: [u8; 4] = *b"POLP";
pub const HEADER_LEN: usize = 16;
pub const CHUNK_LEN: u32 = 0x10000;
pub const VERSION_BLOCK_LEN: usize = 0x40;

pub mod kind {
    // Requests.
    pub const HELLO: u32 = 1;
    pub const FILE_REQUEST: u32 = 3;
    pub const VERSION: u32 = 7;
    // Replies.
    pub const MANIFEST_REPLY: u32 = 2;
    pub const FILE_REPLY: u32 = 4;
    pub const VERSION_REJECTED: u32 = 5;
    pub const VERSION_REPLY: u32 = 8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tags {
    pub platform: [u8; 4],
    pub title: [u8; 4],
}

/// A cursor over an untrusted packet. Every read advances only after its
/// entire field is available, including variable-length server fields.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let (field, remaining) = self.0.split_at_checked(count).ok_or_else(|| {
            format!(
                "packet field truncated: need {count}, have {}",
                self.0.len()
            )
        })?;
        self.0 = remaining;
        Ok(field)
    }

    fn integer(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

pub fn checksum(bytes: &[u8]) -> [u8; 4] {
    md5::compute(bytes).0[..4].try_into().unwrap()
}

pub fn declared_len(bytes: &[u8; 4]) -> usize {
    u32::from_le_bytes(*bytes) as usize
}

pub fn seal(message_type: u32, payload: &[u8]) -> Vec<u8> {
    let size = HEADER_LEN
        .checked_add(payload.len())
        .expect("packet size overflow");
    let wire_size = u32::try_from(size).expect("patch packet exceeds wire size");
    let mut packet = vec![0; size];
    packet[..4].copy_from_slice(&wire_size.to_le_bytes());
    packet[8..12].copy_from_slice(&MAGIC);
    packet[12..16].copy_from_slice(&message_type.to_le_bytes());
    packet[HEADER_LEN..].copy_from_slice(payload);
    let digest = checksum(&packet[8..]);
    packet[4..8].copy_from_slice(&digest);
    packet
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: u32,
    pub body: &'a [u8],
}

pub fn parse(packet: &[u8]) -> Result<Frame<'_>, String> {
    let mut fields = Fields(packet);
    if fields.integer()? as usize != packet.len() {
        return Err("patch frame length does not match received bytes".into());
    }
    let expected_digest = fields.take(4)?;
    if fields.take(4)? != MAGIC {
        return Err("patch frame magic mismatch".into());
    }
    let message_type = fields.integer()?;
    if expected_digest != checksum(&packet[8..]) {
        return Err("patch frame checksum mismatch".into());
    }
    Ok(Frame {
        kind: message_type,
        body: fields.0,
    })
}

fn tagged(tags: &Tags, tail: &[u8]) -> Vec<u8> {
    let mut payload = vec![0; 8 + tail.len()];
    payload[..4].copy_from_slice(&tags.platform);
    payload[4..8].copy_from_slice(&tags.title);
    payload[8..].copy_from_slice(tail);
    payload
}

pub fn hello(tags: &Tags) -> Vec<u8> {
    seal(kind::HELLO, &tagged(tags, &[]))
}

pub fn version(tags: &Tags, version: &[u8]) -> Vec<u8> {
    let mut padded = [0; VERSION_BLOCK_LEN];
    let count = version.len().min(VERSION_BLOCK_LEN - 1);
    padded[..count].copy_from_slice(&version[..count]);
    seal(kind::VERSION, &tagged(tags, &padded))
}

pub fn file_request(tags: &Tags, path: &str, offset: u32, count: u32) -> Vec<u8> {
    let path_len = path
        .len()
        .checked_add(1)
        .and_then(|n| u32::try_from(n).ok())
        .expect("patch path exceeds wire size");
    let mut payload = vec![0; 20 + path_len as usize];
    for (slot, value) in [(0, offset), (4, count), (16, path_len)] {
        payload[slot..slot + 4].copy_from_slice(&value.to_le_bytes());
    }
    payload[8..12].copy_from_slice(&tags.platform);
    payload[12..16].copy_from_slice(&tags.title);
    payload[20..20 + path.len()].copy_from_slice(path.as_bytes());
    seal(kind::FILE_REQUEST, &payload)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReply {
    pub release_unix: u32,
    pub flags: u32,
    pub label: String,
    pub client_addr: String,
    pub version: String,
}

fn text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn parse_version_reply(payload: &[u8]) -> Result<VersionReply, String> {
    let mut fields = Fields(payload);
    let release_unix = fields.integer()?;
    let flags = fields.integer()?;
    let fixed = fields.take(VERSION_BLOCK_LEN)?;
    let label = text(fixed);
    let client_addr = fixed
        .iter()
        .position(|&byte| byte == 0)
        .map_or_else(String::new, |end| text(&fixed[end + 1..]));
    let length = fields.integer()? as usize;
    let version = text(fields.take(length)?);
    Ok(VersionReply {
        release_unix,
        flags,
        label,
        client_addr,
        version,
    })
}

pub fn file_chunk(payload: &[u8]) -> Result<(u32, &[u8]), String> {
    let mut fields = Fields(payload);
    let offset = fields.integer()?;
    let count = fields.integer()? as usize;
    let echoed_path = fields.integer()? as usize;
    fields.take(echoed_path)?;
    Ok((offset, fields.take(count)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fresh synthetic packets are generated independently with Python struct/hashlib.
    const VECTORS: &str = include_str!("fixtures/polp.tsv");

    fn rows(tag: &str) -> Vec<Vec<&'static str>> {
        VECTORS
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .filter(|row| row[0] == tag)
            .collect()
    }

    fn bytes(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .as_chunks::<2>().0.iter()
            .map(|pair| {
                let text = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }

    fn tags(row: &[&str]) -> Tags {
        Tags {
            platform: bytes(row[1]).try_into().unwrap(),
            title: bytes(row[2]).try_into().unwrap(),
        }
    }

    fn assert_packet(actual: Vec<u8>, expected: &str, message_type: u32) {
        assert_eq!(actual, bytes(expected));
        assert_eq!(declared_len(actual[..4].try_into().unwrap()), actual.len());
        let decoded = parse(&actual).unwrap();
        assert_eq!(decoded.kind, message_type);
        assert_eq!(decoded.body, &actual[16..]);
    }

    #[test]
    fn request_encoders_match_independently_generated_wire_bytes() {
        for row in rows("HELLO") {
            assert_packet(hello(&tags(&row)), row[3], 1);
        }
        for row in rows("VERSION") {
            assert_packet(version(&tags(&row), &bytes(row[3])), row[4], 7);
        }
        for row in rows("FILE") {
            let path = String::from_utf8(bytes(row[3])).unwrap();
            assert_packet(
                file_request(
                    &tags(&row),
                    &path,
                    row[4].parse().unwrap(),
                    row[5].parse().unwrap(),
                ),
                row[6],
                3,
            );
        }
        for row in rows("FRAME") {
            let kind = row[1].parse().unwrap();
            assert_packet(seal(kind, &bytes(row[2])), row[3], kind);
        }
    }

    #[test]
    fn every_request_prefix_and_single_bit_corruption_is_rejected() {
        for tag in ["HELLO", "VERSION", "FILE", "FRAME"] {
            for row in rows(tag) {
                let wire = bytes(row.last().unwrap());
                for end in 0..wire.len() {
                    assert!(parse(&wire[..end]).is_err(), "{tag} prefix {end}");
                }
                for position in 0..wire.len() {
                    for bit in 0..8 {
                        let mut damaged = wire.clone();
                        damaged[position] ^= 1 << bit;
                        assert!(parse(&damaged).is_err(), "{tag} byte {position} bit {bit}");
                    }
                }
                let mut appended = wire;
                appended.push(0);
                assert!(parse(&appended).is_err());
            }
        }
    }

    #[test]
    fn version_records_always_leave_a_terminator_at_the_fixed_width() {
        for row in rows("VERSION") {
            let input = bytes(row[3]);
            let packet = version(&tags(&row), &input);
            let record = &packet[24..];
            assert_eq!(record.len(), 64);
            assert_eq!(
                &record[..input.len().min(63)],
                &input[..input.len().min(63)]
            );
            assert!(record[input.len().min(63)..].iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn server_version_fields_and_hostile_lengths_use_the_entire_body() {
        for row in rows("REPLY") {
            let body = bytes(row[6]);
            let reply = parse_version_reply(&body).unwrap();
            assert_eq!(
                reply,
                VersionReply {
                    release_unix: row[1].parse().unwrap(),
                    flags: row[2].parse().unwrap(),
                    label: row[3].into(),
                    client_addr: row[4].into(),
                    version: row[5].into(),
                }
            );
            for end in 0..body.len() {
                assert!(parse_version_reply(&body[..end]).is_err(), "prefix {end}");
            }
            let mut oversized = body.clone();
            oversized[72..76].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(parse_version_reply(&oversized).is_err());
            let mut no_address = body;
            no_address[8..72].fill(b'Q');
            let reply = parse_version_reply(&no_address).unwrap();
            assert_eq!(reply.label, "Q".repeat(64));
            assert_eq!(reply.client_addr, "");
        }
    }

    #[test]
    fn chunk_parser_bounds_both_the_echoed_path_and_data_length() {
        for row in rows("CHUNK") {
            let body = bytes(row[4]);
            let (offset, data) = file_chunk(&body).unwrap();
            assert_eq!(offset, row[1].parse::<u32>().unwrap());
            assert_eq!(data, bytes(row[3]));
            for end in 0..body.len() {
                assert!(file_chunk(&body[..end]).is_err(), "prefix {end}");
            }
            for field in [4, 8] {
                let mut oversized = body.clone();
                oversized[field..field + 4].copy_from_slice(&u32::MAX.to_le_bytes());
                assert!(file_chunk(&oversized).is_err());
            }
        }
    }
}
