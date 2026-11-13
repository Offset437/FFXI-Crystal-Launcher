//! Read-only lookups of PlayOnline's registry keys in both machine/user views.
//! Strings stay ANSI because polcore takes ANSI paths.

use std::ffi::CString;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExA, RegQueryValueExA, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_DWORD,
    REG_EXPAND_SZ, REG_SZ,
};

use crate::proto::Region;

/// `SOFTWARE\PlayOnline{,US,EU}`: slot 367's argument and the root of the rest.
pub fn playonline_key(region: Region) -> &'static str {
    match region {
        Region::Jp => r"SOFTWARE\PlayOnline",
        Region::Us => r"SOFTWARE\PlayOnlineUS",
        Region::Eu => r"SOFTWARE\PlayOnlineEU",
    }
}

enum Data {
    Str(Vec<u8>),
    Dword(u32),
}

fn query_at(hive: HKEY, view: u32, path: &str, value: &str) -> Option<Data> {
    let path = CString::new(path).ok()?;
    let value = CString::new(value).ok()?;
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: valid C strings and out pointer.
    if unsafe { RegOpenKeyExA(hive, path.as_ptr().cast(), 0, KEY_QUERY_VALUE | view, &mut key) } != ERROR_SUCCESS {
        return None;
    }
    let mut buf = vec![0u8; 1024];
    let mut size = buf.len() as u32;
    let mut ty = 0u32;
    // SAFETY: buf is valid for `size` bytes.
    let rc = unsafe { RegQueryValueExA(key, value.as_ptr().cast(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut size) };
    // SAFETY: key was opened above.
    unsafe { RegCloseKey(key) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(size as usize);
    match ty {
        REG_SZ | REG_EXPAND_SZ => {
            while buf.last() == Some(&0) {
                buf.pop();
            }
            Some(Data::Str(buf))
        }
        REG_DWORD if buf.len() == 4 => Some(Data::Dword(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))),
        _ => None,
    }
}

/// `InstallFolder\<id>`: "1000" = PlayOnline Viewer, "0001" = FINAL FANTASY XI.
pub fn install_folder(region: Region, id: &str) -> Option<Vec<u8>> {
    let key = format!(r"{}\InstallFolder", playonline_key(region));
    for (hive, view) in registry_locations() {
        if let Some(Data::Str(bytes)) = query_at(hive, view, &key, id) {
            if bytes.is_empty() || bytes.contains(&0) { continue; }
            let path = std::path::PathBuf::from(crate::game::ansi_to_string(&bytes));
            let exists = match id {
                "1000" => path.join("viewer").join("com").join(crate::game::polcore_name(region)).is_file(),
                "0001" => path.join("FFXi.dll").is_file(),
                _ => path.is_dir(),
            };
            if path.is_absolute() && exists { return Some(bytes); }
        }
    }
    None
}

fn registry_locations() -> [(HKEY, u32); 4] {
    [(HKEY_LOCAL_MACHINE, KEY_WOW64_32KEY), (HKEY_CURRENT_USER, KEY_WOW64_32KEY),
        (HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY), (HKEY_CURRENT_USER, KEY_WOW64_64KEY)]
}

/// The Viewer's `Settings\Language` DWORD (slot 420's argument); defaults to
/// the region's area code.
pub fn viewer_language(region: Region) -> i32 {
    let vendor = if region == Region::Jp { "Square" } else { "SquareEnix" };
    let key = format!(r"{}\{vendor}\PlayOnlineViewer\Settings", playonline_key(region));
    for (hive, view) in registry_locations() {
        if let Some(Data::Dword(value)) = query_at(hive, view, &key, "Language") {
            return value as i32;
        }
    }
    region.area_code()
}
