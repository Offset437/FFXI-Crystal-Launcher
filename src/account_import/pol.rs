//! Original reader derived from the installed Viewer's file layout and native
//! decoding/checksum routines. It does not load the Viewer or contact POL.
//!
//! app.dll 101ac489 decodes member records; 101ac87c verifies each record and
//! restores the separately protected password. 101acd05 sums the restored records.
//! The password key comes from the Viewer's checked SPS000 registry/file envelope
//! (101b0037 / 101b0771), or its older adapter-address fallback (101abf6d).
//! No Viewer code runs, no source is changed, and unsuccessful recovery leaves
//! the password missing in the review draft.

use super::{read_bounded, ImportBatch, ImportDraft};
use crate::i18n::t;
use std::path::Path;
use zeroize::Zeroizing;

const HEADER: usize = 0x74;
const RECORD: usize = 0x5370;
const CHECKSUM: usize = 0x52f4;
const PASSWORD_SUM: usize = 0x52f8;
const MAGIC: &[u8] = b"PlayOnline Login Setting File Ver 1.2\0";
const MAX_RECORDS: usize = 21;

pub(super) fn import(path: &Path, data: &[u8], region: u8) -> Result<ImportBatch, String> {
    let key_path = path.with_file_name("noramim.bin");
    let key = if key_path.exists() {
        Some(read_bounded(&key_path, 512)?)
    } else {
        None
    };
    parse_with_password_keys(data, key.as_deref().map(|k| k.as_slice()), region, || {
        machine_password_keys(region)
    })
}

fn invalid() -> String {
    t("The PlayOnline member file is damaged or uses an unsupported format. No accounts were imported.").into()
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated member record bounds"),
    )
}

fn byte_sum(bytes: &[u8]) -> u32 {
    bytes.iter().map(|b| u32::from(*b)).sum()
}

fn parse_with_password_keys(
    data: &[u8],
    key: Option<&[u8]>,
    region: u8,
    password_keys: impl FnOnce() -> Zeroizing<Vec<[u8; 8]>>,
) -> Result<ImportBatch, String> {
    if data.len() < HEADER
        || !data.starts_with(MAGIC)
        || word(data, 0x40) as usize != HEADER
        || word(data, 0x44) as usize != RECORD
        || word(data, 0x48) != !word(data, 0x4c)
    {
        return Err(invalid());
    }
    let capacity = word(data, 0x60) as usize;
    let count = word(data, 0x50) as usize;
    if capacity > MAX_RECORDS || count > capacity || data.len() != HEADER + capacity * RECORD {
        return Err(invalid());
    }
    let prepared_key = key.map(prepare_key).transpose()?;
    let mut records = Vec::with_capacity(capacity);
    let mut restored_sum = byte_sum(&data[..HEADER]);
    for encrypted in data[HEADER..].as_chunks::<RECORD>().0 {
        let mut record = Zeroizing::new(encrypted.to_vec());
        match &prepared_key {
            Some(key) => decode_record(&mut record, key),
            None => decode_legacy_record(&mut record),
        }
        let sum = byte_sum(&record);
        if sum - byte_sum(&record[CHECKSUM..CHECKSUM + 4]) != word(&record, CHECKSUM) {
            return Err(t("The PlayOnline member file could not be decoded. Keep the original noramim.bin beside login_w.bin and try again.").into());
        }
        let password_sum = word(&record, PASSWORD_SUM);
        if password_sum > 16 * 255 {
            return Err(invalid());
        }
        // The global checksum was computed after restoring these 16 password bytes.
        restored_sum += sum - byte_sum(&record[0x10..0x20]) + password_sum;
        records.push(record);
    }
    if restored_sum != word(data, 0x48) {
        return Err(invalid());
    }

    // Validate the entire active chain before returning any credentials. Slots are
    // reusable, so reading physical records in order would misidentify Forest slots.
    let mut chain = Vec::with_capacity(count);
    let mut visited = vec![false; capacity];
    let mut current = word(data, 0x54) as i32;
    let mut previous = -1;
    for _ in 0..count {
        if current < 0 || current as usize >= capacity || visited[current as usize] {
            return Err(invalid());
        }
        let index = current as usize;
        let record = &records[index];
        if word(record, 0) as i32 != previous {
            return Err(invalid());
        }
        visited[index] = true;
        chain.push(index);
        previous = current;
        current = word(record, 4) as i32;
    }
    if current != -1 || word(data, 0x58) as i32 != previous {
        return Err(invalid());
    }
    // Deleted members must form a disjoint, terminating free list covering all
    // remaining records. They contain old credentials and must never be imported.
    current = word(data, 0x5c) as i32;
    while current != -1 {
        if current < 0 || current as usize >= capacity || visited[current as usize] {
            return Err(invalid());
        }
        visited[current as usize] = true;
        current = word(&records[current as usize], 4) as i32;
    }
    if visited.iter().any(|seen| !seen) {
        return Err(invalid());
    }

    // Only consult this PC after all records, checksums and list links validate.
    let password_keys = password_keys();
    let mut batch = ImportBatch::default();
    for (position, index) in chain.into_iter().enumerate() {
        let record = &records[index];
        let id = decode_field(&record[8..16]);
        // The Viewer's permanent guest entry is list position zero. It has no ID.
        // Unfinished empty member entries likewise cannot form a saved retail login.
        if id.first() == Some(&0) {
            continue;
        }
        if position == 0 || !id.iter().all(u8::is_ascii_alphanumeric) {
            return Err(invalid());
        }
        let label_bytes = nul_field(&record[0x38..0x4d], record[0x4d] as usize)?;
        let se_bytes = nul_field(&record[0x5358..0x5369], record[0x5369] as usize)?;
        if !se_bytes.is_ascii() || se_bytes.iter().any(u8::is_ascii_control) || se_bytes.len() > 16
        {
            return Err(invalid());
        }
        let label = display_name(&label_bytes)?;
        let mut draft = ImportDraft::default();
        draft.label = if label.trim().is_empty() {
            t("Imported PlayOnline account").into()
        } else {
            label
        };
        draft.pol_id = std::str::from_utf8(&id).map_err(|_| invalid())?.to_owned();
        draft.se_id = std::str::from_utf8(&se_bytes)
            .map_err(|_| invalid())?
            .to_owned();
        draft.region = region;
        draft.pol_slot = Some(position as u8);
        if word(record, 0x24) != 1 {
            draft.notes.push(t("This PlayOnline member has no saved PlayOnline password. Square Enix passwords are not stored in this file.").into());
        } else if let Some(mut password) = recover_password(record, &password_keys) {
            draft.pol_password = std::mem::take(&mut *password);
            draft.notes.push(t("The saved PlayOnline password was imported. Square Enix passwords are not stored in this file.").into());
        } else {
            draft.notes.push(t("The saved PlayOnline password could not be verified on this PC. Enter it before saving.").into());
        }
        batch.drafts.push(draft);
    }
    if batch.drafts.is_empty() {
        batch
            .warnings
            .push(t("No saved retail accounts were found in this PlayOnline member file.").into());
    }
    Ok(batch)
}

fn recover_password(record: &[u8], keys: &[[u8; 8]]) -> Option<Zeroizing<String>> {
    let len = record[0x20] as usize;
    if word(record, 0x24) != 1 || !(2..=16).contains(&len) {
        return None;
    }
    let mut recovered = None;
    for (index, key) in keys.iter().enumerate() {
        if keys[..index].contains(key) {
            continue;
        }
        let mut field = Zeroizing::new(<[u8; 16]>::try_from(&record[0x10..0x20]).ok()?);
        unmask(field.as_mut(), key);
        // Check every protected byte, including unused field bytes. A matching
        // sum alone is insufficient: also require its exact NUL-terminated field.
        if byte_sum(field.as_ref()) != word(record, PASSWORD_SUM) {
            continue;
        }
        let Ok(mut plain) = nul_field(field.as_ref(), len) else {
            continue;
        };
        if !plain.iter().all(|b| (32..=126).contains(b)) {
            continue;
        }
        if recovered.is_some() {
            return None;
        }
        recovered = Some(Zeroizing::new(
            String::from_utf8(std::mem::take(&mut *plain)).ok()?,
        ));
    }
    recovered
}

#[cfg(test)]
fn parse(data: &[u8], key: Option<&[u8]>, region: u8) -> Result<ImportBatch, String> {
    parse_with_password_keys(data, key, region, || Zeroizing::new(Vec::new()))
}

// Fixed format seed/alphabet from app.dll, not a machine key or credential.
const KEY_SEED: u32 = 0xef81_ed75;
const KEY_ALPHABET: &[u8; 32] = b"aiueohklmnpwAIUEOHKLMNPW01234567";
const KEY_PATH_BYTES: usize = 0x744;

fn set_word(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn rotate_path_words(bytes: &mut [u8]) {
    let first = word(bytes, 0);
    for i in 0..256 {
        let next = if i == 255 {
            first
        } else {
            word(bytes, (i + 1) * 4)
        };
        let value = (word(bytes, i * 4) << 1) | (next >> 31);
        set_word(bytes, i * 4, value);
    }
}

/// Decode the Viewer's SPS000 registry value (app 101b0037). The embedded
/// permutation and final seeded checksum must both validate before any file opens.
fn decode_key_path(raw: &[u8]) -> Option<Zeroizing<String>> {
    if raw.len() != KEY_PATH_BYTES {
        return None;
    }
    let mut decoded = Zeroizing::new(raw.to_vec());
    let (mut seed, mut feedback) = (KEY_SEED, KEY_SEED);
    for offset in (0..KEY_PATH_BYTES).step_by(4) {
        let encrypted = word(&decoded, offset);
        set_word(&mut decoded, offset, encrypted ^ feedback);
        feedback ^= encrypted.wrapping_add(seed);
        seed = seed.rotate_right(1);
    }
    let mut permutation = Zeroizing::new([0usize; 512]);
    let mut seen = [false; 512];
    for i in 0..512 {
        let bit = i * 9;
        let offset = 0x104 + bit / 8;
        let pair = u16::from_be_bytes([decoded[offset], decoded[offset + 1]]);
        let index = ((pair >> (7 - bit % 8)) & 511) as usize;
        if seen[index] {
            return None;
        }
        seen[index] = true;
        permutation[i] = index;
    }
    let mut payload = Zeroizing::new(decoded[0x344..].to_vec());
    let original = Zeroizing::new(payload.to_vec());
    for (i, &index) in permutation.iter().enumerate() {
        payload[i * 2..i * 2 + 2].copy_from_slice(&original[index * 2..index * 2 + 2]);
    }
    rotate_path_words(&mut payload);
    for half in 0..2 {
        let original = Zeroizing::new(payload[half * 512..(half + 1) * 512].to_vec());
        for (i, &index) in permutation.iter().enumerate() {
            payload[half * 512 + i] = original[index];
        }
    }
    rotate_path_words(&mut payload);
    let mut feedback = Zeroizing::new([0u32; 64]);
    for i in 0..64 {
        feedback[i] = word(&decoded, 4 + i * 4);
    }
    let mut checksum = KEY_SEED;
    for i in 0..256 {
        let encrypted = word(&payload, i * 4);
        let value = encrypted ^ feedback[i % 64];
        feedback[(i + 1) % 64] ^= encrypted;
        let value =
            ((value >> 4) & 0x00f0_00f0) | ((value & 0x00f0_00f0) << 4) | (value & 0xf00f_f00f);
        set_word(&mut payload, i * 4, value);
        checksum = checksum.wrapping_add(value);
    }
    if checksum != word(&decoded, 0) {
        return None;
    }
    let wide = Zeroizing::new(
        payload
            .as_chunks::<2>().0.iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>(),
    );
    let end = wide.iter().position(|&unit| unit == 0)?;
    let path = Zeroizing::new(String::from_utf16(&wide[..end]).ok()?);
    let bytes = path.as_bytes();
    // This is a local machine key. Do not open UNC/network or device paths from
    // imported metadata, and do not expand environment variables or guess a path.
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
        || path.chars().any(char::is_control)
    {
        return None;
    }
    Some(path)
}

/// The eight-byte key file is a 24-character base32 envelope plus CRLF. Its
/// checksum and padding are bound to the exact UTF-16 path (app 101b02df).
fn decode_password_key(raw: &[u8], path: &str) -> Option<Zeroizing<[u8; 8]>> {
    if raw.len() != 26 || &raw[24..] != b"\r\n" {
        return None;
    }
    let mut decoded = Zeroizing::new([0u8; 15]);
    for (group, input) in raw[..24].as_chunks::<8>().0.iter().enumerate() {
        let mut bits = 0u64;
        for byte in input {
            bits = (bits << 5) | KEY_ALPHABET.iter().position(|v| v == byte)? as u64;
        }
        decoded[group * 5..group * 5 + 5].copy_from_slice(&bits.to_be_bytes()[3..]);
    }
    if u16::from_le_bytes([decoded[2], decoded[3]]) != 8 {
        return None;
    }
    let initial = path.encode_utf16().fold(KEY_SEED, |sum, unit| {
        sum.rotate_left(1).wrapping_add(u32::from(unit))
    });
    let mut seed = initial;
    let mut key = Zeroizing::new([0u8; 8]);
    for i in 0..8 {
        key[i] = decoded[i + 4] ^ seed as u8;
        seed = seed.rotate_left(1).wrapping_add(8);
    }
    let checksum = initial.wrapping_add(byte_sum(key.as_ref())) as u16;
    if checksum != u16::from_le_bytes([decoded[0], decoded[1]]) {
        return None;
    }
    for i in 12..15 {
        if decoded[i] != decoded[i - 1] ^ seed as u8 {
            return None;
        }
        seed = seed.rotate_left(1).wrapping_add(8);
    }
    Some(key)
}

fn password_key_registry_root(region: u8) -> Option<&'static str> {
    // Crystal's region order differs from the native Viewer's internal enum.
    match region {
        0 => Some(r"SOFTWARE\PlayOnlineUS\SquareEnix\PlayOnlineViewer"),
        1 => Some(r"SOFTWARE\PlayOnlineEU\SquareEnix\PlayOnlineViewer"),
        2 => Some(r"SOFTWARE\PlayOnline\SQUARE\PlayOnlineViewer"),
        _ => None,
    }
}

fn saved_key_path(region: u8) -> Option<Zeroizing<String>> {
    use crate::win::reg::{self, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RRF_SUBKEY_WOW6432KEY};
    let root = password_key_registry_root(region)?;
    // A larger value fails with ERROR_MORE_DATA, a shorter one reports its
    // size, and any other type fails the RRF_RT_REG_BINARY filter. The buffer
    // holds secret material (and is undefined after a failure): keep it zeroized.
    let mut blob = Zeroizing::new([0u8; KEY_PATH_BYTES]);
    let size = reg::get_bytes(HKEY_LOCAL_MACHINE, root, "SPS000", RRF_RT_REG_BINARY | RRF_SUBKEY_WOW6432KEY, &mut blob[..]).ok()?;
    if size != KEY_PATH_BYTES {
        return None;
    }
    decode_key_path(&blob[..])
}

fn adapter_password_keys() -> Zeroizing<Vec<[u8; 8]>> {
    use windows_sys::Win32::{
        Foundation::ERROR_BUFFER_OVERFLOW,
        NetworkManagement::IpHelper::{GetAdaptersInfo, IP_ADAPTER_INFO},
    };
    let mut keys = Zeroizing::new(Vec::new());
    let mut size = 0u32;
    if unsafe { GetAdaptersInfo(std::ptr::null_mut(), &mut size) } != ERROR_BUFFER_OVERFLOW {
        return keys;
    }
    for _ in 0..3 {
        if size as usize > 1024 * 1024 || (size as usize) < std::mem::size_of::<IP_ADAPTER_INFO>() {
            return keys;
        }
        // Word allocation provides native pointer alignment and zeroizes the
        // returned adapter names/addresses as soon as extraction finishes.
        let mut buffer = Zeroizing::new(vec![
            0usize;
            (size as usize).div_ceil(std::mem::size_of::<usize>())
        ]);
        let begin = buffer.as_mut_ptr() as usize;
        let end = begin + buffer.len() * std::mem::size_of::<usize>();
        let result = unsafe { GetAdaptersInfo(begin as *mut IP_ADAPTER_INFO, &mut size) };
        if result == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if result != 0 {
            return keys;
        }
        let mut current = begin as *mut IP_ADAPTER_INFO;
        let mut visited = Vec::new();
        while !current.is_null() && visited.len() < 64 {
            let address = current as usize;
            if address < begin
                || address
                    .checked_add(std::mem::size_of::<IP_ADAPTER_INFO>())
                    .is_none_or(|last| last > end)
                || !address.is_multiple_of(std::mem::align_of::<IP_ADAPTER_INFO>())
                || visited.contains(&address)
            {
                break;
            }
            visited.push(address);
            let adapter = unsafe { &*current };
            if (1..=8).contains(&adapter.AddressLength)
                && adapter.Address.iter().any(|&byte| byte != 0)
                && !keys.contains(&adapter.Address)
            {
                keys.push(adapter.Address);
            }
            current = adapter.Next;
        }
        return keys;
    }
    keys
}

fn machine_password_keys(region: u8) -> Zeroizing<Vec<[u8; 8]>> {
    if password_key_registry_root(region).is_none() {
        return Zeroizing::new(Vec::new());
    }
    let Some(path) = saved_key_path(region) else {
        return adapter_password_keys();
    };
    let mut keys = Zeroizing::new(Vec::new());
    // Once the registry resolves, the Viewer does not fall back to adapters for
    // a missing or damaged key file. Preserve that boundary; do not try guesses.
    if let Ok(raw) = read_bounded(Path::new(path.as_str()), 64) {
        if let Some(key) = decode_password_key(&raw, &path) {
            keys.push(*key);
        }
    }
    keys
}

fn prepare_key(raw: &[u8]) -> Result<Zeroizing<[u8; 512]>, String> {
    let mut key = Zeroizing::new(<[u8; 512]>::try_from(raw).map_err(|_| invalid())?);
    for i in 0..256 {
        key[i] ^= [0xcd, 0xce, 0xd1, 0xc0][i % 4];
        key[i + 256] ^= [0xcc, 0xc8, 0xcc, 0xc8][i % 4];
    }
    let mut seen = [false; 256];
    for index in &key[256..] {
        if seen[*index as usize] {
            return Err(invalid());
        }
        seen[*index as usize] = true;
    }
    Ok(key)
}

fn decode_record(record: &mut [u8], key: &[u8; 512]) {
    // Three feedback/substitution rounds, with three overlapping block passes.
    for _ in 0..3 {
        let mut feedback = Zeroizing::new([0u8; 256]);
        feedback.copy_from_slice(&key[..256]);
        for (i, byte) in record.iter_mut().enumerate() {
            let encoded = *byte;
            *byte ^= feedback[i & 255];
            feedback[(i + 1) & 255] ^= encoded;
        }
        for first in [128, record.len() % 256, 0] {
            for start in (first..record.len().saturating_sub(255)).step_by(256) {
                let block = Zeroizing::new(
                    <[u8; 256]>::try_from(&record[start..start + 256])
                        .expect("complete member cipher block"),
                );
                for i in 0..256 {
                    record[start + i] = block[key[256 + i] as usize];
                }
            }
        }
    }
}

fn decode_legacy_record(record: &mut [u8]) {
    // Native fallback used only when there is no noramim.bin. This is a fixed file
    // format constant from the Viewer, not a user key or an account password.
    const KEY: [u8; 8] = 0x8324_a3d9_4ff1_377bu64.to_le_bytes();
    unmask(record, &KEY);
}

/// The member file's byte mask, shared by the legacy record and the saved password.
fn unmask(bytes: &mut [u8], key: &[u8; 8]) {
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = byte.wrapping_sub(key[i % 8] ^ i as u8) ^ key[i % 8];
    }
}

fn decode_field(encoded: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut bytes = Zeroizing::new(vec![0; encoded.len()]);
    for (i, byte) in encoded.iter().enumerate() {
        bytes[encoded.len() - 1 - i] =
            byte.rotate_right((i % 8) as u32) ^ if i % 2 == 1 { 255 } else { 0 };
    }
    bytes
}

fn nul_field(block: &[u8], len: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    if len == 0 || len > block.len() {
        return Err(invalid());
    }
    let mut bytes = decode_field(&block[..len]);
    if bytes.last() != Some(&0) || bytes[..len - 1].contains(&0) {
        return Err(invalid());
    }
    bytes.pop();
    Ok(bytes)
}

fn display_name(bytes: &[u8]) -> Result<String, String> {
    if bytes.iter().any(u8::is_ascii_control) {
        return Err(invalid());
    }
    if bytes.is_ascii() {
        return Ok(std::str::from_utf8(bytes)
            .map_err(|_| invalid())?
            .to_owned());
    }
    {
        use windows_sys::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};
        let mut wide = Zeroizing::new([0u16; 64]);
        let written = unsafe {
            MultiByteToWideChar(
                932,
                MB_ERR_INVALID_CHARS,
                bytes.as_ptr(),
                bytes.len() as i32,
                wide.as_mut_ptr(),
                wide.len() as i32,
            )
        };
        if written <= 0 {
            return Err(invalid());
        }
        let label = String::from_utf16(&wide[..written as usize]).map_err(|_| invalid())?;
        if label.chars().any(char::is_control) {
            return Err(invalid());
        }
        Ok(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroize;

    fn put(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn encode_field(plain: &[u8]) -> Vec<u8> {
        plain
            .iter()
            .rev()
            .enumerate()
            .map(|(i, b)| (b ^ if i % 2 == 1 { 255 } else { 0 }).rotate_left((i % 8) as u32))
            .collect()
    }
    fn member(id: &[u8; 8], label: &[u8], se: &[u8], prev: i32, next: i32) -> Vec<u8> {
        let mut record = vec![0; RECORD];
        put(&mut record, 0, prev as u32);
        put(&mut record, 4, next as u32);
        record[8..16].copy_from_slice(&encode_field(id));
        let name = encode_field(label);
        record[0x38..0x38 + name.len()].copy_from_slice(&name);
        record[0x4d] = name.len() as u8;
        let se_id = encode_field(se);
        record[0x5358..0x5358 + se_id.len()].copy_from_slice(&se_id);
        record[0x5369] = se_id.len() as u8;
        // A separately protected synthetic password whose original byte sum is 42.
        record[0x10..0x20].fill(19);
        put(&mut record, PASSWORD_SUM, 42);
        let sum = byte_sum(&record);
        put(&mut record, CHECKSUM, sum);
        record
    }
    fn file(records: Vec<Vec<u8>>, active: usize, tail: usize, free: i32) -> Vec<u8> {
        let mut bytes = vec![0; HEADER];
        bytes[..MAGIC.len()].copy_from_slice(MAGIC);
        put(&mut bytes, 0x40, HEADER as u32);
        put(&mut bytes, 0x44, RECORD as u32);
        put(&mut bytes, 0x50, active as u32);
        put(&mut bytes, 0x54, 0);
        put(&mut bytes, 0x58, tail as u32);
        put(&mut bytes, 0x5c, free as u32);
        put(&mut bytes, 0x60, records.len() as u32);
        put(&mut bytes, 0x4c, u32::MAX);
        let mut sum = byte_sum(&bytes);
        for mut record in records {
            sum += byte_sum(&record) - byte_sum(&record[0x10..0x20]) + word(&record, PASSWORD_SUM);
            const KEY: [u8; 8] = 0x8324_a3d9_4ff1_377bu64.to_le_bytes();
            for (i, byte) in record.iter_mut().enumerate() {
                *byte = (*byte ^ KEY[i % 8]).wrapping_add(KEY[i % 8] ^ i as u8);
            }
            bytes.extend(record);
        }
        put(&mut bytes, 0x48, sum);
        put(&mut bytes, 0x4c, !sum);
        bytes
    }
    fn sample() -> Vec<u8> {
        file(
            vec![
                member(&[0; 8], b"Guest\0", b"\0", -1, 2),
                member(b"TEST0002", b"Second\0", b"sample2\0", 2, -1),
                member(b"TEST0001", b"First\0", b"sample1\0", 0, 1),
            ],
            3,
            1,
            -1,
        )
    }

    // Fixture credentials and keys are independently authored synthetic data.
    const TEST_KEY: [u8; 8] = [0x12, 0x25, 0x37, 0x46, 0x53, 0x61, 0x75, 0x09];

    fn saved_password(mut record: Vec<u8>, password: &[u8], key: &[u8; 8]) -> Vec<u8> {
        let mut terminated = password.to_vec();
        terminated.push(0);
        let encoded = encode_field(&terminated);
        record[0x10..0x20].fill(0x31); // Unused bytes can survive a shorter saved password.
        record[0x10..0x10 + encoded.len()].copy_from_slice(&encoded);
        record[0x20] = encoded.len() as u8;
        put(&mut record, 0x24, 1);
        let sum = byte_sum(&record[0x10..0x20]);
        put(&mut record, PASSWORD_SUM, sum);
        for (i, byte) in record[0x10..0x20].iter_mut().enumerate() {
            *byte = (*byte ^ key[i % 8]).wrapping_add(key[i % 8] ^ i as u8);
        }
        put(&mut record, CHECKSUM, 0);
        let sum = byte_sum(&record);
        put(&mut record, CHECKSUM, sum);
        record
    }

    fn password_sample(password: &[u8]) -> Vec<u8> {
        file(
            vec![
                member(&[0; 8], b"Guest\0", b"\0", -1, 1),
                saved_password(
                    member(b"TEST0001", b"Saved\0", b"sample\0", 0, -1),
                    password,
                    &TEST_KEY,
                ),
            ],
            2,
            1,
            -1,
        )
    }

    fn with_keys(data: &[u8], keys: &[[u8; 8]]) -> ImportBatch {
        parse_with_password_keys(data, None, 1, || Zeroizing::new(keys.to_vec())).unwrap()
    }

    fn key_file(key: &[u8; 8], path: &str) -> Vec<u8> {
        let mut seed = path.encode_utf16().fold(KEY_SEED, |sum, unit| {
            sum.rotate_left(1).wrapping_add(unit as u32)
        });
        let mut envelope = [0u8; 15];
        envelope[..2].copy_from_slice(&(seed.wrapping_add(byte_sum(key)) as u16).to_le_bytes());
        envelope[2..4].copy_from_slice(&8u16.to_le_bytes());
        for i in 0..8 {
            envelope[4 + i] = key[i] ^ seed as u8;
            seed = seed.rotate_left(1).wrapping_add(8);
        }
        for i in 12..15 {
            envelope[i] = envelope[i - 1] ^ seed as u8;
            seed = seed.rotate_left(1).wrapping_add(8);
        }
        let mut result = Vec::new();
        for block in envelope.as_chunks::<5>().0 {
            let bits = block
                .iter()
                .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
            for shift in (0..8).rev() {
                result.push(KEY_ALPHABET[((bits >> (shift * 5)) & 31) as usize]);
            }
        }
        result.extend(b"\r\n");
        result
    }

    fn key_path_blob(path: &str) -> Vec<u8> {
        let mut payload: Vec<u8> = (0..1024).map(|i| (i as u8).wrapping_mul(13)).collect();
        for (i, unit) in path.encode_utf16().chain(Some(0)).enumerate() {
            payload[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let checksum = (0..256).fold(KEY_SEED, |sum, i| sum.wrapping_add(word(&payload, i * 4)));
        for i in 0..256 {
            let value = word(&payload, i * 4);
            set_word(
                &mut payload,
                i * 4,
                ((value >> 4) & 0x00f0_00f0) | ((value & 0x00f0_00f0) << 4) | (value & 0xf00f_f00f),
            );
        }
        let initial: Vec<u32> = (0..64)
            .map(|i| 0x1020_3040u32.wrapping_add(i * 977))
            .collect();
        let mut feedback = initial.clone();
        for i in 0..256 {
            let encoded = word(&payload, i * 4) ^ feedback[i % 64];
            set_word(&mut payload, i * 4, encoded);
            feedback[(i + 1) % 64] ^= encoded;
        }
        let rotate_back = |payload: &mut [u8]| {
            let words: Vec<u32> = (0..256).map(|i| word(payload, i * 4)).collect();
            for i in 0..256 {
                set_word(
                    payload,
                    i * 4,
                    (words[i] >> 1) | (words[(i + 255) % 256] << 31),
                );
            }
        };
        let permutation: Vec<usize> = (0..512).map(|i| (i * 73 + 41) % 512).collect();
        rotate_back(&mut payload);
        for half in 0..2 {
            let original = payload[half * 512..(half + 1) * 512].to_vec();
            for (i, &index) in permutation.iter().enumerate() {
                payload[half * 512 + index] = original[i];
            }
        }
        rotate_back(&mut payload);
        let original = payload.clone();
        for (i, &index) in permutation.iter().enumerate() {
            payload[index * 2..index * 2 + 2].copy_from_slice(&original[i * 2..i * 2 + 2]);
        }
        let mut blob = vec![0; KEY_PATH_BYTES];
        put(&mut blob, 0, checksum);
        for (i, value) in initial.iter().enumerate() {
            put(&mut blob, 4 + i * 4, *value);
        }
        for (i, index) in permutation.iter().enumerate() {
            for bit in 0..9 {
                blob[0x104 + (i * 9 + bit) / 8] |=
                    (((index >> (8 - bit)) & 1) as u8) << (7 - (i * 9 + bit) % 8);
            }
        }
        blob[0x344..].copy_from_slice(&payload);
        let (mut seed, mut feedback) = (KEY_SEED, KEY_SEED);
        for offset in (0..blob.len()).step_by(4) {
            let encoded = word(&blob, offset) ^ feedback;
            put(&mut blob, offset, encoded);
            feedback ^= encoded.wrapping_add(seed);
            seed = seed.rotate_right(1);
        }
        blob
    }

    #[test]
    fn saved_pol_passwords_recover_only_with_a_verified_unambiguous_key() {
        let data = password_sample(b"Synthetic Pass!");
        let recovered = with_keys(&data, &[TEST_KEY]);
        assert_eq!(recovered.drafts[0].pol_password, "Synthetic Pass!");
        assert_eq!(
            recovered.drafts[0].missing_fields(),
            ["Square Enix password"]
        );
        for keys in [
            Vec::new(),
            vec![[0x17; 8]],
            vec![TEST_KEY, TEST_KEY.map(|b| b ^ 0x80)],
        ] {
            let batch = with_keys(&data, &keys);
            assert_eq!(batch.drafts.len(), 1);
            assert!(batch.drafts[0].pol_password.is_empty());
        }
        // Identical adapter keys are not distinct recovery candidates.
        assert_eq!(
            with_keys(&data, &[TEST_KEY, TEST_KEY]).drafts[0].pol_password,
            "Synthetic Pass!"
        );
    }

    #[test]
    fn password_sum_covers_all_bytes_and_bad_lengths_content_or_unsaved_flags_never_recover() {
        let record = saved_password(
            member(b"TEST0001", b"Saved\0", b"sample\0", 0, -1),
            b"fixture",
            &TEST_KEY,
        );
        assert!(recover_password(&record, &[TEST_KEY]).is_some());
        for length in [0, 1, 17, 255] {
            let mut bad = record.clone();
            bad[0x20] = length;
            assert!(recover_password(&bad, &[TEST_KEY]).is_none());
        }
        let mut bad = record.clone();
        bad[0x1f] ^= 1;
        assert!(
            recover_password(&bad, &[TEST_KEY]).is_none(),
            "unused protected bytes must also match"
        );
        for flag in [0, 2, u32::MAX] {
            let mut bad = record.clone();
            put(&mut bad, 0x24, flag);
            assert!(recover_password(&bad, &[TEST_KEY]).is_none());
        }
        for password in [&b"a\0b"[..], &b"line\nbreak"[..], &b"nonascii\xff"[..]] {
            let bad = saved_password(
                member(b"TEST0001", b"Saved\0", b"sample\0", 0, -1),
                password,
                &TEST_KEY,
            );
            assert!(recover_password(&bad, &[TEST_KEY]).is_none());
        }
        // Native stores a byte length, with unrelated bytes immediately after it.
        let mut same = record.clone();
        same[0x21..0x24].fill(0xff);
        assert!(recover_password(&same, &[TEST_KEY]).is_some());
        // Shortening a reversed field can leave a valid suffix because trailing
        // bytes survive edits. The record checksum must reject altered lengths.
        let mut bad = password_sample(b"fixture");
        bad[HEADER + RECORD + 0x20] ^= 1;
        assert!(parse_with_password_keys(&bad, None, 0, || panic!(
            "damaged lengths must fail before recovery"
        ))
        .is_err());
    }

    #[test]
    fn guest_and_deleted_passwords_stay_excluded_and_validation_precedes_key_access() {
        let data = file(
            vec![
                saved_password(
                    member(&[0; 8], b"Guest\0", b"\0", -1, 2),
                    b"guest",
                    &TEST_KEY,
                ),
                saved_password(
                    member(b"OLD00001", b"Deleted\0", b"old\0", -1, -1),
                    b"deleted",
                    &TEST_KEY,
                ),
                saved_password(
                    member(b"TEST0001", b"Active\0", b"sample\0", 0, -1),
                    b"active",
                    &TEST_KEY,
                ),
            ],
            2,
            2,
            1,
        );
        let batch = with_keys(&data, &[TEST_KEY]);
        assert_eq!(batch.drafts.len(), 1);
        assert_eq!(batch.drafts[0].pol_password, "active");
        let mut bad = data;
        bad[HEADER + 0x100] ^= 1;
        assert!(parse_with_password_keys(&bad, None, 1, || panic!(
            "invalid records must not access machine keys"
        ))
        .is_err());
    }

    #[test]
    fn password_key_regions_follow_crystal_us_eu_jp_order() {
        assert_eq!(
            password_key_registry_root(0),
            Some(r"SOFTWARE\PlayOnlineUS\SquareEnix\PlayOnlineViewer")
        );
        assert_eq!(
            password_key_registry_root(1),
            Some(r"SOFTWARE\PlayOnlineEU\SquareEnix\PlayOnlineViewer")
        );
        assert_eq!(
            password_key_registry_root(2),
            Some(r"SOFTWARE\PlayOnline\SQUARE\PlayOnlineViewer")
        );
        assert!(password_key_registry_root(3).is_none());
        assert!(password_key_registry_root(u8::MAX).is_none());
    }

    #[test]
    fn persisted_key_envelopes_validate_path_checksum_permutation_file_length_and_padding() {
        let path = r"C:\Synthetic fixture\key.txt";
        let blob = key_path_blob(path);
        let decoded = decode_key_path(&blob).unwrap();
        assert_eq!(decoded.as_str(), path);
        let file = key_file(&TEST_KEY, &decoded);
        assert_eq!(*decode_password_key(&file, &decoded).unwrap(), TEST_KEY);
        for offset in [0, 4, 0x100, 0x104, 0x300, 0x344] {
            let mut bad = blob.clone();
            bad[offset] ^= 1;
            assert!(
                decode_key_path(&bad).is_none(),
                "damaged synthetic path byte {offset}"
            );
        }
        assert!(decode_key_path(&blob[..blob.len() - 1]).is_none());
        for path in [
            r"\\server\share\key.txt",
            r"relative\key.txt",
            r"\\.\device",
            "C:\\bad\npath",
        ] {
            assert!(decode_key_path(&key_path_blob(path)).is_none());
        }
        assert!(decode_password_key(&file, r"D:\different.txt").is_none());
        for offset in [0, 7, 17, 22, 25] {
            let mut bad = file.clone();
            bad[offset] = if bad[offset] == b'a' { b'i' } else { b'a' };
            assert!(decode_password_key(&bad, path).is_none());
        }
        assert!(decode_password_key(&file[..25], path).is_none());
    }

    #[test]
    fn pol_import_follows_member_list_order_and_leaves_passwords_missing() {
        let batch = parse(&sample(), None, 2).unwrap();
        assert_eq!(batch.drafts.len(), 2);
        assert_eq!(batch.drafts[0].label, "First");
        assert_eq!(batch.drafts[0].pol_id, "TEST0001");
        assert_eq!(batch.drafts[0].se_id, "sample1");
        assert_eq!(batch.drafts[0].pol_slot, Some(1));
        assert_eq!(batch.drafts[1].pol_slot, Some(2));
        assert_eq!(
            batch.drafts[0].missing_fields(),
            ["PlayOnline password", "Square Enix password"]
        );
    }

    #[test]
    fn deleted_records_are_never_imported() {
        let data = file(
            vec![
                member(&[0; 8], b"Guest\0", b"\0", -1, 2),
                member(b"OLD00001", b"Deleted\0", b"old\0", -1, -1),
                member(b"TEST0001", b"Active\0", b"sample\0", 0, -1),
            ],
            2,
            2,
            1,
        );
        let batch = parse(&data, None, 0).unwrap();
        assert_eq!(batch.drafts.len(), 1);
        assert_eq!(batch.drafts[0].label, "Active");
    }

    #[test]
    fn corruption_truncation_and_wrong_keys_return_no_drafts() {
        let mut data = sample();
        data[HEADER + 0x100] ^= 1;
        assert!(parse(&data, None, 0).is_err());
        let good = sample();
        for end in [0, 64, HEADER - 1, HEADER, good.len() - 1] {
            assert!(parse(&good[..end], None, 0).is_err());
        }
        assert!(parse(&good, Some(&[0; 511]), 0).is_err());
        assert!(parse(&good, Some(&[0; 512]), 0).is_err());
        let cycle = file(
            vec![
                member(&[0; 8], b"Guest\0", b"\0", -1, 1),
                member(b"TEST0001", b"Cycle\0", b"sample\0", 0, 1),
            ],
            2,
            1,
            -1,
        );
        assert!(parse(&cycle, None, 0).is_err());
    }

    #[test]
    fn malformed_encoded_fields_are_rejected() {
        assert!(nul_field(&[0; 17], 18).is_err());
        assert!(nul_field(&[0; 17], 0).is_err());
        assert!(nul_field(&encode_field(b"no nul"), 6).is_err());
        assert!(nul_field(&encode_field(b"a\0b\0"), 4).is_err());
    }

    #[test]
    fn permutation_cipher_decodes_an_independent_synthetic_pattern() {
        // Encrypt an authored byte pattern by inverting the documented file stages.
        // No user key or member bytes are embedded in the test.
        let mut key = [0u8; 512];
        for i in 0..256 {
            key[i] = (i as u8).wrapping_mul(19);
            key[i + 256] = (i as u8).wrapping_mul(73).wrapping_add(11);
        }
        let plain: Vec<u8> = (0..RECORD)
            .map(|i| (i as u8).wrapping_mul(5).wrapping_add(3))
            .collect();
        let mut data = plain.clone();
        for _ in 0..3 {
            for first in [0, RECORD % 256, 128] {
                let starts: Vec<_> = (first..RECORD - 255).step_by(256).collect();
                for start in starts.into_iter().rev() {
                    let block = data[start..start + 256].to_vec();
                    for i in 0..256 {
                        data[start + key[i + 256] as usize] = block[i];
                    }
                }
            }
            let mut feedback = key[..256].to_vec();
            for (i, byte) in data.iter_mut().enumerate() {
                *byte ^= feedback[i % 256];
                feedback[(i + 1) % 256] ^= *byte;
            }
        }
        decode_record(&mut data, &key);
        assert_eq!(data, plain);
        key.zeroize();
    }
}
