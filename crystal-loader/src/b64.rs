//! Standard base64 (RFC 4648 alphabet). Decoding writes straight into a
//! caller-owned buffer so secrets never pass through an intermediate allocation.
//!
//! The one base64 codec of the launcher <-> loader wire contract: the
//! launcher (`src/main.rs`) and `crates/pol-auth` include this file via
//! `#[path]`, so it depends on `core`/`std` only.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Each byte's value in `ALPHABET`, 0xFF for bytes outside it (friend
/// payloads are ~70K characters).
const DECODE: [u8; 256] = {
    let mut table = [0xFF; 256];
    let mut i = 0;
    while i < 64 {
        table[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    table
};

/// Feeds the padded encoding of `data` to `push`, one ASCII byte at a time.
fn encode_with(data: &[u8], mut push: impl FnMut(u8)) {
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        push(ALPHABET[(n >> 18) as usize & 63]);
        push(ALPHABET[(n >> 12) as usize & 63]);
        push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] } else { b'=' });
        push(if chunk.len() > 2 { ALPHABET[n as usize & 63] } else { b'=' });
    }
}

/// Padded encoding of `data`.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(encoded_len(data.len()));
    push_str(&mut out, data);
    out
}

/// Length of the padded encoding of `n` bytes.
pub fn encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Appends the padded encoding in place (the caller reserves the space, so a
/// zeroizing buffer never reallocates).
pub fn encode_into(out: &mut Vec<u8>, data: &[u8]) {
    encode_with(data, |c| out.push(c));
}

/// [`encode_into`] for a `String` buffer.
pub fn push_str(out: &mut String, data: &[u8]) {
    encode_with(data, |c| out.push(char::from(c)));
}

fn value(c: u8) -> Option<u32> {
    match DECODE[usize::from(c)] {
        0xFF => None,
        v => Some(u32::from(v)),
    }
}

/// Number of bytes `src` decodes to, or `None` if it is not canonical base64
/// (padded or unpadded; no whitespace; zero trailing bits).
pub fn decoded_len(src: &[u8]) -> Option<usize> {
    let body = strip_padding(src)?;
    if body.len() % 4 == 1 {
        return None;
    }
    Some(body.len() / 4 * 3 + match body.len() % 4 {
        2 => 1,
        3 => 2,
        _ => 0,
    })
}

fn strip_padding(src: &[u8]) -> Option<&[u8]> {
    let pad = src.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 || (pad > 0 && !src.len().is_multiple_of(4)) {
        return None;
    }
    Some(&src[..src.len() - pad])
}

/// Decodes `src` into `out`, which must be exactly the decoded length.
pub fn decode_into(src: &[u8], out: &mut [u8]) -> Result<(), ()> {
    if decoded_len(src) != Some(out.len()) {
        return Err(());
    }
    let body = strip_padding(src).ok_or(())?;
    let mut o = 0;
    for chunk in body.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value(c).ok_or(())? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let take = chunk.len() - 1; // 4 -> 3, 3 -> 2, 2 -> 1
        // Non-canonical encodings (stray low bits) are refused.
        if take < 3 && bytes[take..].iter().any(|&b| b != 0) {
            return Err(());
        }
        out[o..o + take].copy_from_slice(&bytes[..take]);
        o += take;
    }
    Ok(())
}

/// Decodes canonical, padded base64 (length a multiple of 4) into a new
/// buffer of exactly the decoded length.
pub fn decode_padded(src: &[u8]) -> Option<Vec<u8>> {
    if !src.len().is_multiple_of(4) {
        return None;
    }
    let mut out = vec![0; decoded_len(src)?];
    decode_into(src, &mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for len in 0..70 {
            let data: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
            let enc = encode(&data);
            assert_eq!(enc.len(), encoded_len(data.len()));
            let mut out = vec![0u8; decoded_len(enc.as_bytes()).unwrap()];
            decode_into(enc.as_bytes(), &mut out).unwrap();
            assert_eq!(out, data);
            assert_eq!(decode_padded(enc.as_bytes()).unwrap(), data);
            let mut raw = Vec::new();
            encode_into(&mut raw, &data);
            assert_eq!(raw, enc.as_bytes());
            // unpadded is accepted too
            let unpadded = enc.trim_end_matches('=');
            let mut out2 = vec![0u8; decoded_len(unpadded.as_bytes()).unwrap()];
            decode_into(unpadded.as_bytes(), &mut out2).unwrap();
            assert_eq!(out2, data);
            if unpadded.len() != enc.len() {
                assert_eq!(decode_padded(unpadded.as_bytes()), None);
            }
        }
    }

    #[test]
    fn known_vectors() {
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (&[0xfb, 0xff, 0xbf], "+/+/"),
        ] {
            assert_eq!(encode(input), expected);
            assert_eq!(decode_padded(expected.as_bytes()).unwrap(), input);
        }
        let mut out = [0u8; 4];
        decode_into(b"Zm9vYg==", &mut out).unwrap();
        assert_eq!(&out, b"foob");
    }

    #[test]
    fn rejects_bad_input() {
        let mut out = [0u8; 3];
        assert!(decode_into(b"Zm9!", &mut out).is_err());
        assert!(decode_into(b"Zm9v ", &mut out).is_err());
        let mut out = [0u8; 2];
        assert!(decode_into(b"Zm8=", &mut out).is_ok());
        assert!(decode_into(b"Zm9=", &mut out).is_err()); // stray low bits
        assert!(decode_into(b"Zm8===", &mut out).is_err());
        assert!(decode_into(b"Zm+=", &mut out).is_err()); // stray low bits
        let mut wrong = [0u8; 5];
        assert!(decode_into(b"Zm9vYg==", &mut wrong).is_err());
        assert_eq!(decoded_len(b"A"), None);
        for bad in [&b"Zh=="[..], b"Zm9=", b"TW=u", b"TWF", b"====", b"Zg==AAAA", b"%%%%", b"Zg="] {
            assert_eq!(decode_padded(bad), None, "{:?}", std::str::from_utf8(bad));
        }
    }
}
