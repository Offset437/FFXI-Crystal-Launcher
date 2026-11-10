//! The machine-wide PlayOnline connection profile list (`usr\all\inet_w.bin`)
//! and the Viewer's member/connection selection, driven through the installed
//! core exactly as app.dll does (pol-spec/02-member-settings.md Â§2.1, Â§5).
//! The file is read only; the Viewer's default-save (app 101a09e6) is omitted.

use crate::native_auth::Api;
use std::io::Read;
use std::path::Path;

pub(crate) const HEADER_LEN: usize = 0xe4;
pub(crate) const RECORD_LEN: usize = 0x9ac;
/// Encoded 0x102-byte string block plus its length byte (858/886, record+8).
pub(crate) const NAME_LEN: usize = 0x103;
const MAGIC: &[u8] = b"PlayOnline NetWork Setting File Ver 1.00";
/// Stock files hold one record; refuse implausible capacities outright.
const MAX_RECORDS: usize = 64;
/// core 891 (10047720): the encoded name was not found in the list.
pub(crate) const PROFILE_NOT_FOUND: i32 = -11267;

/// Bounded, read-only copy of the Viewer's profile file; `None` when absent.
/// Holds no credentials for a LAN profile (02 Â§2.1).
pub(crate) fn read_file(viewer: &Path) -> Option<Vec<u8>> {
    let path = viewer.join("usr").join("all").join("inet_w.bin");
    let limit = HEADER_LEN + MAX_RECORDS * RECORD_LEN;
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > limit {
        return Some(Vec::new()); // present but invalid: the Viewer's defaults
    }
    Some(bytes)
}

fn u32_at(p: usize, offset: usize) -> u32 {
    unsafe { ((p + offset) as *const u32).read_unaligned() }
}

/// app 101a0f48: 825 â†’ header, 826 â†’ records, validated by 884, magic and sizes
/// (101a100d..101a1077); anything else gets the built-in default (101a0edf).
/// Returns the core-owned header and whether the file (not the default) was
/// used. Requires 824 to have been registered.
pub(crate) fn load(api: &Api, file: Option<&[u8]>) -> Result<(usize, bool), String> {
    let (mut header, mut size) = (0usize, 0u32);
    let code = api.call(825, &[&mut header as *mut usize as usize, &mut size as *mut u32 as usize]);
    if code != 0 || header == 0 || size as usize != HEADER_LEN {
        return Err(format!("PlayOnline could not allocate its connection settings ({code})."));
    }
    let valid = file.is_some_and(|bytes| unsafe { copy_in(api, header, bytes) });
    if !valid {
        install_default(api);
        crate::log!("native POL connection settings: built-in WindowsNetWork default ({})", if file.is_some() { "file invalid" } else { "no file" });
    } else {
        crate::log!("native POL connection settings: usr\\all\\inet_w.bin");
    }
    Ok((header, valid))
}

/// The file branch of 101a0f48. False means "use the default", as the Viewer
/// does for every validation failure (101a09d1 records 1..4).
unsafe fn copy_in(api: &Api, header: usize, bytes: &[u8]) -> bool {
    if bytes.len() < HEADER_LEN {
        return false;
    }
    let capacity = u32::from_le_bytes(bytes[0x60..0x64].try_into().unwrap()) as usize;
    if capacity > MAX_RECORDS || bytes.len() != HEADER_LEN + capacity * RECORD_LEN {
        return false;
    }
    let field = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    if !bytes[..0x40].starts_with(MAGIC) || bytes[MAGIC.len()] != 0
        || field(0x40) != HEADER_LEN || field(0x44) != RECORD_LEN {
        return false;
    }
    // The core's 809 returns count directly; 849 walks from the first record
    // through unchecked next indices. A valid checksum does not bound that walk.
    let count = field(0x50);
    if count > capacity { return false; }
    let mut index = field(0x54);
    let mut visited = [false; MAX_RECORDS];
    for number in 0..count {
        if index >= capacity || visited[index] { return false; }
        visited[index] = true;
        if number + 1 < count { index = field(HEADER_LEN + index * RECORD_LEN + 4); }
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), header as *mut u8, HEADER_LEN);
    let (mut records, mut size) = (0usize, 0u32);
    let code = api.call(826, &[&mut records as *mut usize as usize, &mut size as *mut u32 as usize]);
    if size as usize != capacity * RECORD_LEN || capacity > 0 && (code != 1 || records == 0) {
        return false;
    }
    if capacity > 0 {
        std::ptr::copy_nonoverlapping(bytes[HEADER_LEN..].as_ptr(), records as *mut u8, capacity * RECORD_LEN);
    }
    // 884 (10047600) checksums [hdr+0x40] header bytes and capacity*[hdr+0x44]
    // record bytes: check the file's own sizes before letting it read with them.
    let magic = std::slice::from_raw_parts(header as *const u8, 0x40);
    magic.starts_with(MAGIC) && magic[MAGIC.len()] == 0
        && u32_at(header, 0x40) == HEADER_LEN as u32
        && u32_at(header, 0x44) == RECORD_LEN as u32
        && api.zero(884) as u32 == u32_at(header, 0x48)
}

/// app 101a0edf without its file save: 845 resets the header, 101a0ad0 builds
/// "WindowsNetWork" (LAN, no proxy, "46.49"), 846 appends it. Byte-identical to
/// the stock file (pol-spec/work/settings/s10_rebuild_inet.py).
fn install_default(api: &Api) {
    api.zero(845);
    let mut record = vec![0u8; RECORD_LEN];
    let base = record.as_mut_ptr() as usize;
    api.call(858, &[base + 8, c"WindowsNetWork".as_ptr() as usize]);
    record[0x10c..0x110].copy_from_slice(&1u32.to_le_bytes()); // kind: LAN
    record[0x558..0x55c].copy_from_slice(&1u32.to_le_bytes()); // proxy: none
    api.call(892, &[base + 0x55c, c"".as_ptr() as usize]);
    api.call(892, &[base + 0x5a4, c"".as_ptr() as usize]);
    record[0x5ec..0x5f2].copy_from_slice(b"46.49\0");
    api.call(846, &[base]);
}

/// The connect screen's selection (app 1019c5b0): header+0x64 when it names a
/// listed profile, else the first. Returns that record's encoded name block,
/// which is what 858 re-encodes from the displayed name (10199d02).
pub(crate) fn selected_name(api: &Api, header: usize) -> Result<[u8; NAME_LEN], String> {
    let count = api.zero(809);
    let selected = u32_at(header, 0x64) as i32;
    let index = if (0..count).contains(&selected) { selected } else { 0 };
    let record = if count > 0 { api.word(849, &[index as usize]) } else { 0 };
    if record == 0 {
        return Err("PlayOnline has no connection setting to sign in with.".into());
    }
    let mut name = [0u8; NAME_LEN];
    unsafe { std::ptr::copy_nonoverlapping((record + 8) as *const u8, name.as_mut_ptr(), NAME_LEN) };
    Ok(name)
}

/// Connect screen (app 10199d02): 886(name) â†’ 891 must load the profile, and
/// the Open argument built at 835 must carry the LAN kind ([10099c80] == 1).
pub(crate) fn connect(api: &Api, name: &[u8; NAME_LEN]) -> Result<(), String> {
    api.call(886, &[name.as_ptr() as usize]);
    let code = api.zero(891);
    if code != 1 {
        return Err(format!("PlayOnline could not load its connection setting ({code})."));
    }
    let kind = unsafe { (api.word(835, &[]) as *const i32).read() };
    if kind != 1 {
        return Err(format!("PlayOnline's selected connection setting is not a LAN connection (kind {kind})."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn stock_header_layout_matches_the_documented_magic() {
        // The magic is NUL-terminated inside the 0x40-byte field.
        assert!(super::MAGIC.len() < 0x40);
        assert_eq!(super::HEADER_LEN + super::RECORD_LEN, 2704);
    }
}
