//! Registry access: an owned `Key`, bounded value reads, and PlayOnline's InstallFolder entries.
use super::{from_wide, wide};
use std::path::PathBuf;
pub(crate) use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE,
    KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_SZ, RRF_RT_REG_BINARY,
    RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY,
};
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};

/// An open registry key, closed on drop.
pub(crate) struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this value opened and owns.
        unsafe { RegCloseKey(self.0) };
    }
}

impl Key {
    /// Opens `hive\path` with `access` (include a KEY_WOW64_* flag to pick a view).
    pub(crate) fn open(hive: HKEY, path: &str, access: u32) -> Result<Self, u32> {
        let path = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated path and an out pointer.
        match unsafe { RegOpenKeyExW(hive, path.as_ptr(), 0, access, &mut key) } {
            ERROR_SUCCESS => Ok(Self(key)),
            status => Err(status),
        }
    }

    /// Opens or creates `hive\path`; `true` when this call created it.
    #[cfg(test)]
    pub(crate) fn create(hive: HKEY, path: &str, access: u32) -> Result<(Self, bool), u32> {
        use windows_sys::Win32::System::Registry::{RegCreateKeyExW, REG_CREATED_NEW_KEY};
        let path = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        let mut disposition = 0;
        // SAFETY: a NUL-terminated path and out pointers.
        let status = unsafe {
            RegCreateKeyExW(
                hive,
                path.as_ptr(),
                0,
                std::ptr::null(),
                0,
                access,
                std::ptr::null(),
                &mut key,
                &mut disposition,
            )
        };
        if status == ERROR_SUCCESS {
            Ok((Self(key), disposition == REG_CREATED_NEW_KEY))
        } else {
            Err(status)
        }
    }

    pub(crate) fn raw(&self) -> HKEY {
        self.0
    }

    /// Value `name` read with RegGetValueW `flags` (RRF_RT_* filter) into `buffer`; the
    /// byte count written. A larger value fails with ERROR_MORE_DATA.
    pub(crate) fn get_into(&self, name: &str, flags: u32, buffer: &mut [u16]) -> Result<usize, u32> {
        get_raw(self.0, None, name, flags, buffer.as_mut_ptr().cast(), std::mem::size_of_val(buffer))
    }

    /// A REG_SZ / REG_EXPAND_SZ value (RegGetValueW `flags`) up to its first NUL, read into a
    /// buffer of `capacity` UTF-16 units.
    pub(crate) fn string(&self, name: &str, flags: u32, capacity: usize) -> Option<Vec<u16>> {
        let mut buffer = vec![0u16; capacity];
        let bytes = self.get_into(name, flags, &mut buffer).ok()?;
        Some(until_nul(&buffer, bytes))
    }

    /// A REG_DWORD value; any other type or size is `None`.
    pub(crate) fn dword(&self, name: &str) -> Option<u32> {
        let name = wide(name);
        let (mut kind, mut data, mut len) = (0u32, 0u32, 4u32);
        // SAFETY: a 4-byte buffer and its length.
        let status = unsafe {
            RegQueryValueExW(self.0, name.as_ptr(), std::ptr::null(), &mut kind, (&mut data as *mut u32).cast(), &mut len)
        };
        (status == ERROR_SUCCESS && kind == REG_DWORD && len == 4).then_some(data)
    }

    /// The names of up to `limit` subkeys. Names longer than 511 units are skipped.
    pub(crate) fn subkeys(&self, limit: u32) -> Vec<String> {
        let mut keys = Vec::new();
        for index in 0..limit {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            // SAFETY: a name buffer and its length in units; nothing else is requested.
            let status = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            match status {
                ERROR_SUCCESS => keys.push(String::from_utf16_lossy(&name[..len as usize])),
                ERROR_MORE_DATA => continue,
                _ => break, // ERROR_NO_MORE_ITEMS
            }
        }
        keys
    }

    /// Sets value `name` to `kind` data `bytes`.
    pub(crate) fn set(&self, name: &str, kind: u32, bytes: &[u8]) -> Result<(), u32> {
        let name = wide(name);
        let len = u32::try_from(bytes.len()).map_err(|_| windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER)?;
        // SAFETY: a NUL-terminated name and a data pointer with its length.
        match unsafe { RegSetValueExW(self.0, name.as_ptr(), 0, kind, bytes.as_ptr(), len) } {
            ERROR_SUCCESS => Ok(()),
            status => Err(status),
        }
    }

    /// Sets REG_SZ value `name` to `value` (NUL-terminated).
    pub(crate) fn set_string(&self, name: &str, value: &str) -> Result<(), u32> {
        self.set(name, REG_SZ, &sz_bytes(value))
    }
}

/// `value` as REG_SZ data: UTF-16LE with its terminator.
pub(crate) fn sz_bytes(value: &str) -> Vec<u8> {
    wide(value).iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

fn get_raw(key: HKEY, path: Option<&str>, name: &str, flags: u32, data: *mut core::ffi::c_void, size: usize) -> Result<usize, u32> {
    let path = path.map(wide);
    let name = wide(name);
    let mut bytes = u32::try_from(size).unwrap_or(u32::MAX);
    // SAFETY: NUL-terminated strings and a writable buffer of `size` bytes.
    let status = unsafe {
        RegGetValueW(
            key,
            path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            data,
            &mut bytes,
        )
    };
    match status {
        ERROR_SUCCESS if bytes as usize <= size => Ok(bytes as usize),
        ERROR_SUCCESS => Err(ERROR_MORE_DATA),
        status => Err(status),
    }
}

fn until_nul(buffer: &[u16], bytes: usize) -> Vec<u16> {
    let units = &buffer[..bytes.div_ceil(2).min(buffer.len())];
    units[..units.iter().position(|c| *c == 0).unwrap_or(units.len())].to_vec()
}

/// Value `hive\path\name` read with RegGetValueW `flags` (RRF_RT_* filter and RRF_SUBKEY_*
/// view) into `buffer`; the byte count written.
pub(crate) fn get_bytes(hive: HKEY, path: &str, name: &str, flags: u32, buffer: &mut [u8]) -> Result<usize, u32> {
    get_raw(hive, Some(path), name, flags, buffer.as_mut_ptr().cast(), buffer.len())
}

/// String value `hive\path\name` (RegGetValueW `flags`) up to its first NUL, read into a
/// buffer of `capacity` UTF-16 units; `None` when it is missing, another type or larger.
pub(crate) fn string(hive: HKEY, path: &str, name: &str, flags: u32, capacity: usize) -> Option<Vec<u16>> {
    let mut buffer = vec![0u16; capacity];
    let bytes = get_raw(hive, Some(path), name, flags, buffer.as_mut_ptr().cast(), capacity * 2).ok()?;
    Some(until_nul(&buffer, bytes))
}

/// Crystal's region numbers with PlayOnline's vendor key for each.
pub(crate) const REGIONS: [(u8, &str); 3] = [(0, "PlayOnlineUS"), (1, "PlayOnlineEU"), (2, "PlayOnline")];

/// PlayOnline's `InstallFolder\<title>` entry (`1000` the Viewer, `0001` FFXI) for each of
/// `regions`, read from HKLM then HKCU, each in the 32-bit then the 64-bit view. Absolute
/// paths only, one per distinct folder, in that order.
pub(crate) fn install_folders(title: &str, regions: &[(u8, &str)]) -> Vec<(u8, PathBuf)> {
    let mut folders: Vec<(u8, PathBuf)> = Vec::new();
    for &(region, vendor) in regions {
        let key = format!("SOFTWARE\\{vendor}\\InstallFolder");
        for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            for view in [RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY] {
                let Some(units) = string(hive, &key, title, RRF_RT_REG_SZ | view, 2048) else {
                    continue;
                };
                let path = PathBuf::from(from_wide(&units));
                if path.is_absolute() && !folders.iter().any(|(r, p)| *r == region && same_folder(p, &path)) {
                    folders.push((region, path));
                }
            }
        }
    }
    folders
}

fn same_folder(a: &std::path::Path, b: &std::path::Path) -> bool {
    let trim = |p: &std::path::Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    trim(a) == trim(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch key under HKCU\Software, deleted afterwards.
    struct Scratch(String);
    impl Scratch {
        fn new(tag: &str) -> Self {
            Self(format!("Software\\CrystalLauncherRegTest-{tag}-{}", std::process::id()))
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Registry::RegDeleteTreeW;
            let path = wide(&self.0);
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr()) };
        }
    }

    #[test]
    fn values_round_trip_and_reads_are_bounded_and_typed() {
        let scratch = Scratch::new("values");
        let (key, created) = Key::create(HKEY_CURRENT_USER, &scratch.0, KEY_READ | KEY_SET_VALUE).unwrap();
        assert!(created);
        key.set_string("Text", "C:\\Games\\PlayOnline").unwrap();
        key.set("Number", REG_DWORD, &7u32.to_le_bytes()).unwrap();
        key.set("Blob", REG_BINARY, &[1, 2, 3]).unwrap();
        Key::create(HKEY_CURRENT_USER, &format!("{}\\Child", scratch.0), KEY_READ).unwrap();

        assert_eq!(String::from_utf16(&key.string("Text", RRF_RT_REG_SZ, 64).unwrap()).unwrap(), "C:\\Games\\PlayOnline");
        assert_eq!(key.string("Text", RRF_RT_REG_SZ, 8), None, "too small a buffer is no value");
        assert_eq!(key.string("Number", RRF_RT_REG_SZ, 64), None, "the type filter applies");
        assert_eq!(key.dword("Number"), Some(7));
        assert_eq!(key.dword("Text"), None);
        let mut blob = [0u8; 8];
        assert_eq!(get_bytes(HKEY_CURRENT_USER, &scratch.0, "Blob", RRF_RT_REG_BINARY, &mut blob), Ok(3));
        assert_eq!(string(HKEY_CURRENT_USER, &scratch.0, "Missing", RRF_RT_REG_SZ, 64), None);
        assert_eq!(key.subkeys(16), ["Child"]);
        drop(key);
        assert!(Key::open(HKEY_CURRENT_USER, &scratch.0, KEY_READ).is_ok());
    }
}
