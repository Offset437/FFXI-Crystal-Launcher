//! Small Win32 helpers shared across the loader: NUL-terminated wide strings
//! and module lookups. None of these take a module reference.

use std::{ffi::{OsStr, OsString}, os::windows::ffi::{OsStrExt, OsStringExt}, path::PathBuf};
use windows_sys::Win32::{
    Foundation::HMODULE,
    System::{LibraryLoader::{GetModuleFileNameW, GetModuleHandleW}, SystemInformation::GetSystemDirectoryW},
};

/// `s` as a NUL-terminated UTF-16 string.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A path or other OS string as a NUL-terminated UTF-16 string.
pub(crate) fn wide_os(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// The named module's base in this process, or 0.
pub(crate) fn module_base(name: &str) -> usize {
    let name = wide(name);
    // SAFETY: NUL-terminated name.
    unsafe { GetModuleHandleW(name.as_ptr()) as usize }
}

/// A loaded module's full path; None when it can't be read whole (a
/// truncated path must never match a selected file).
pub(crate) fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    (n != 0 && n < buffer.len()).then(|| PathBuf::from(OsString::from_wide(&buffer[..n])))
}

/// The system directory (System32, or SysWOW64 as this 32-bit process sees it).
pub(crate) fn system_dir() -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    (n != 0 && n < buffer.len()).then(|| PathBuf::from(OsString::from_wide(&buffer[..n])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_and_modules() {
        assert_eq!(wide("ab"), [b'a' as u16, b'b' as u16, 0]);
        assert_eq!(wide_os(std::path::Path::new("ab")), [b'a' as u16, b'b' as u16, 0]);
        let ntdll = module_base("ntdll.dll");
        assert_ne!(ntdll, 0);
        let path = module_path(ntdll as HMODULE).unwrap();
        assert!(path.file_name().unwrap().eq_ignore_ascii_case("ntdll.dll"));
        assert!(system_dir().unwrap().join("ntdll.dll").is_file());
        assert_eq!(module_base("no-such-module.dll"), 0);
    }
}
