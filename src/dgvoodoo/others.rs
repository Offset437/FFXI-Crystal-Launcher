//! dgVoodoo copies Crystal didn't place: in the game folder, PlayOnline's, the configured
//! Windower and Ashita folders, and SysWOW64. They are recognized by their version resource
//! (product name "dgVoodoo"), never by file name, and are only reported, never touched.

use std::fs;
use std::path::{Path, PathBuf};

/// The DLL names dgVoodoo ships for DirectX 1-8; any of them may hold a copy.
const NAMES: [&str; 3] = ["D3D8.dll", "DDraw.dll", "D3DImm.dll"];

/// Where a copy is, which decides what it means for Crystal's launches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// In place of Windows' own DLL: every 32-bit program loads it.
    SysWow64,
    /// The game folder: Crystal's host preloads its own D3D8.dll, so this copy is not used (unless a tool loads a d3d8 first).
    Game,
    /// PlayOnline, Windower or Ashita: unused when Crystal launches.
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Found {
    pub place: Place,
    pub dir: PathBuf,
    /// The dgVoodoo DLLs in `dir`, as named on disk.
    pub files: Vec<String>,
    pub version: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Report {
    pub found: Vec<Found>,
    /// A tuned `dgVoodoo.conf` elsewhere, offered while Crystal's own is still a default.
    pub import: Option<PathBuf>,
}

/// "2.87.6" from a dgVoodoo version resource (product name "dgVoodoo", product version
/// 2.8.7.6); `None` for any other product.
pub(crate) fn label(product: &str, ms: u32, ls: u32) -> Option<String> {
    if !product.trim().eq_ignore_ascii_case("dgVoodoo") {
        return None;
    }
    let (a, b, c, d) = (ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff);
    let minor = if b < 10 && c < 10 { format!("{b}{c}") } else { format!("{b}.{c}") };
    Some(if d == 0 { format!("{a}.{minor}") } else { format!("{a}.{minor}.{d}") })
}

/// The dgVoodoo version of the DLL at `path`, read from its version resource without loading it.
pub(crate) fn version(path: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO};
    let name = crate::win::wide_path(path);
    let query = |data: &[u8], key: &str| -> Option<(*const u8, u32)> {
        let key = crate::win::wide(key);
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: `data` is the block GetFileVersionInfoW filled; VerQueryValueW points into it.
        let ok = unsafe { VerQueryValueW(data.as_ptr().cast(), key.as_ptr(), &mut ptr, &mut len) } != 0;
        (ok && !ptr.is_null() && len > 0).then_some((ptr as *const u8, len))
    };
    // SAFETY: plain Win32 calls on a NUL-terminated path, into a buffer of the size asked for.
    let data = unsafe {
        let size = GetFileVersionInfoSizeW(name.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(name.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        data
    };
    let (fixed, len) = query(&data, "\\")?;
    if (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }
    // SAFETY: VerQueryValueW returned at least a whole VS_FIXEDFILEINFO inside `data`.
    let fixed = unsafe { std::ptr::read_unaligned(fixed as *const VS_FIXEDFILEINFO) };
    // The first language/code page pair names the string table to read.
    let (pairs, len) = query(&data, "\\VarFileInfo\\Translation")?;
    if len < 4 {
        return None;
    }
    // SAFETY: at least 4 bytes inside `data`.
    let (lang, page) = unsafe { (std::ptr::read_unaligned(pairs as *const u16), std::ptr::read_unaligned(pairs.add(2) as *const u16)) };
    let (text, chars) = query(&data, &format!("\\StringFileInfo\\{lang:04x}{page:04x}\\ProductName"))?;
    // SAFETY: `chars` UTF-16 units inside `data`.
    let units = unsafe { std::slice::from_raw_parts(text as *const u16, chars as usize) };
    let product = String::from_utf16_lossy(units);
    label(product.trim_end_matches('\0'), fixed.dwProductVersionMS, fixed.dwProductVersionLS)
}

/// A `dgVoodoo.conf` worth importing: a regular file under 1 MB, text, with a `[DirectX]`
/// section that sets something.
pub(crate) fn read_conf(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    super::entries(text).iter().any(|(s, _, _)| s.eq_ignore_ascii_case("DirectX")).then_some(bytes)
}

/// Looks for dgVoodoo DLLs in each folder, and for a tuned conf to import when Crystal's own
/// conf is still a default (`stock` is the release's stock conf then).
pub(crate) fn scan(folders: &[(Place, PathBuf)], stock: Option<&str>) -> Report {
    let mut report = Report::default();
    let mut seen: Vec<String> = Vec::new();
    for (place, dir) in folders {
        let key = dir.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let mut found: Option<Found> = None;
        for name in NAMES {
            let path = dir.join(name);
            if !path.is_file() {
                continue;
            }
            if let Some(version) = version(&path) {
                let entry = found.get_or_insert_with(|| Found { place: *place, dir: dir.clone(), files: Vec::new(), version });
                entry.files.push(name.to_owned());
            }
        }
        report.found.extend(found);
        if let (Some(stock), None, false) = (stock, &report.import, *place == Place::SysWow64) {
            let conf = dir.join(super::CONF);
            if read_conf(&conf).is_some_and(|bytes| !super::is_default(&bytes, stock)) {
                report.import = Some(conf);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dgvoodoos_version_resource_counts() {
        assert_eq!(label("dgVoodoo", 0x0002_0008, 0x0007_0006).as_deref(), Some("2.87.6"));
        assert_eq!(label("dgVoodoo", 0x0002_0008, 0x0006_0000).as_deref(), Some("2.86"));
        assert_eq!(label("Microsoft® Windows® Operating System", 0x000a_0000, 0x6604_0000), None);
    }

    #[test]
    fn windows_own_d3d8_is_not_dgvoodoo_even_renamed_or_copied() {
        let system = crate::win::system32().with_file_name("SysWOW64").join("d3d8.dll");
        if !system.is_file() {
            return;
        }
        assert_eq!(version(&system), None);
        let dir = std::env::temp_dir().join(format!("crystal-dgvoodoo-others-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(&system, dir.join("D3D8.dll")).unwrap();
        // A non-DLL with the right name is not a copy either.
        fs::write(dir.join("DDraw.dll"), b"not a dll").unwrap();
        let report = scan(&[(Place::Game, dir.clone())], None);
        assert!(report.found.is_empty(), "{report:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tuned_conf_elsewhere_is_offered_and_a_default_one_is_not() {
        let stock = "[General]\r\nOutputAPI = bestavailable\r\n[DirectX]\r\nVRAM = 256\r\ndgVoodooWatermark = true\r\n";
        let base = std::env::temp_dir().join(format!("crystal-dgvoodoo-import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (plain, tuned, system) = (base.join("plain"), base.join("tuned"), base.join("sys"));
        for dir in [&plain, &tuned, &system] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(plain.join(super::super::CONF), stock).unwrap();
        fs::write(tuned.join(super::super::CONF), stock.replace("256", "512")).unwrap();
        fs::write(system.join(super::super::CONF), stock.replace("256", "4096")).unwrap();
        let folders = [(Place::SysWow64, system.clone()), (Place::Other, plain.clone()), (Place::Other, tuned.clone())];
        assert_eq!(scan(&folders, Some(stock)).import, Some(tuned.join(super::super::CONF)));
        // Nothing is offered once Crystal's conf is the user's own (no stock passed).
        assert_eq!(scan(&folders, None).import, None);
        assert!(read_conf(&base.join("missing.conf")).is_none());
        fs::write(base.join("notes.conf"), "hello").unwrap();
        assert!(read_conf(&base.join("notes.conf")).is_none());
        fs::remove_dir_all(&base).unwrap();
    }

    /// The real release's D3D8.dll, offline: `DGVOODOO_DLL=<path to MS\x86\D3D8.dll>`.
    #[test]
    #[ignore]
    fn the_real_dll_is_recognized_under_any_name() {
        let Some(path) = std::env::var_os("DGVOODOO_DLL") else { return };
        let dir = std::env::temp_dir().join(format!("crystal-dgvoodoo-real-dll-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(&path, dir.join("DDraw.dll")).unwrap();
        let report = scan(&[(Place::SysWow64, dir.clone())], None);
        assert_eq!(report.found.len(), 1);
        assert_eq!((report.found[0].version.as_str(), report.found[0].files.as_slice()), ("2.87.6", ["DDraw.dll".to_owned()].as_slice()));
        fs::remove_dir_all(&dir).unwrap();
    }
}
