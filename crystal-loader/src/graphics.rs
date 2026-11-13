//! Which d3d8.dll the game draws with. FFXiMain is loaded with
//! LOAD_WITH_ALTERED_SEARCH_PATH, so its `d3d8.dll` import searches the game
//! folder and the system folder, never this host's folder. Crystal's dgVoodoo
//! D3D8.dll beside the host is therefore loaded here by full path first, and
//! the import binds to the module already loaded under that name. Once
//! FFXiMain is loaded, the d3d8 it bound is logged and sent as one notice.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::{
    Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO},
    System::LibraryLoader::{GetModuleHandleExW, LoadLibraryExW, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, LOAD_WITH_ALTERED_SEARCH_PATH},
};

use crate::win::wide_os;

fn host_d3d8() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("D3D8.dll"))
}

/// The loaded `d3d8.dll`, if any (GetModuleHandleExW: Hook.dll detours GetModuleHandleW).
fn loaded() -> Option<PathBuf> {
    let mut module = std::ptr::null_mut();
    // SAFETY: NUL-terminated name; no reference is taken.
    let found = unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, wide_os("d3d8.dll").as_ptr(), &mut module) } != 0;
    if found && !module.is_null() { crate::win::module_path(module) } else { None }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(a).to_string_lossy().eq_ignore_ascii_case(&canonical(b).to_string_lossy())
}

/// Before FFXi.dll loads: the host folder's D3D8.dll, kept for the process.
/// Never a second d3d8 beside one a launch tool already loaded.
pub(crate) fn preload() {
    let Some(path) = host_d3d8().filter(|path| path.is_file()) else { return };
    match loaded() {
        Some(current) if same_file(&current, &path) => {}
        Some(current) => crate::log!("graphics: {} is already loaded; {} left unloaded", current.display(), path.display()),
        None => {
            // SAFETY: an absolute, NUL-terminated path; its own imports resolve beside it.
            let module = unsafe { LoadLibraryExW(wide_os(&path).as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
            if module.is_null() {
                crate::log!("graphics: could not load {} ({})", path.display(), std::io::Error::last_os_error());
            } else {
                crate::log!("graphics: loaded {} before the game", path.display());
            }
        }
    }
}

static REPORTED: AtomicUsize = AtomicUsize::new(0);

/// Once per FFXiMain mapping (`base`), after its imports are bound.
pub(crate) fn report(base: usize) {
    if REPORTED.swap(base, Ordering::AcqRel) == base { return; }
    let line = match loaded() {
        Some(path) => {
            let line = describe(&path, version(&path).as_deref(), host_d3d8().as_deref(), crate::win::system_dir().as_deref());
            crate::log!("{line} ({})", path.display());
            line
        }
        None => { crate::log!("graphics: no d3d8.dll is loaded"); return; }
    };
    crate::events::send(crate::proto::notice_line(&line));
}

fn describe(path: &Path, version: Option<&str>, host: Option<&Path>, system: Option<&Path>) -> String {
    let version = version.map_or_else(String::new, |v| format!(" {v}"));
    if host.is_some_and(|host| same_file(path, host)) {
        format!("graphics: dgVoodoo{version} active")
    } else if system.zip(path.parent()).is_some_and(|(system, dir)| same_file(dir, system)) {
        "graphics: Windows D3D8 (dgVoodoo not loaded)".into()
    } else {
        format!("graphics: D3D8 from {}{version} (dgVoodoo not loaded)", path.display())
    }
}

fn version(path: &Path) -> Option<String> {
    let name = wide_os(path);
    // SAFETY: sizes match the buffer; VerQueryValueW points into it.
    unsafe {
        let size = GetFileVersionInfoSizeW(name.as_ptr(), std::ptr::null_mut());
        if size == 0 { return None; }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(name.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 { return None; }
        let (mut info, mut len) = (std::ptr::null_mut(), 0u32);
        if VerQueryValueW(data.as_ptr().cast(), wide_os("\\").as_ptr(), &mut info, &mut len) == 0
            || info.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() { return None; }
        let info = &*(info as *const VS_FIXEDFILEINFO);
        Some(format!("{}.{}.{}.{}", info.dwFileVersionMS >> 16, info.dwFileVersionMS & 0xffff, info.dwFileVersionLS >> 16, info.dwFileVersionLS & 0xffff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_names_the_d3d8_the_game_bound() {
        let system = crate::win::system_dir().unwrap();
        let windows = system.join("d3d8.dll");
        let host = std::env::current_exe().unwrap().parent().unwrap().join("D3D8.dll");
        assert_eq!(describe(&windows, Some("10.0.1"), Some(&host), Some(&system)), "graphics: Windows D3D8 (dgVoodoo not loaded)");
        assert_eq!(describe(&host, Some("2.87.1.0"), Some(&host), Some(&system)), "graphics: dgVoodoo 2.87.1.0 active");
        let other = Path::new(r"C:\FFXI\d3d8.dll");
        assert_eq!(describe(other, None, Some(&host), Some(&system)), r"graphics: D3D8 from C:\FFXI\d3d8.dll (dgVoodoo not loaded)");
        assert!(version(&windows).is_some_and(|v| v.split('.').count() == 4), "the system d3d8 has a version");
    }
}
