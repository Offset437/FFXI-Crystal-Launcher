//! Shared Win32 helpers: wide strings, code-page text, DPAPI, ShellExecuteEx, the registry
//! (`reg`) and processes (`process`).
pub(crate) mod process;
pub(crate) mod reg;

use std::ffi::{OsStr, OsString};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

/// `s` as a NUL-terminated UTF-16 string.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// `s` as a NUL-terminated wide string, unpaired surrogates kept.
pub(crate) fn wide_os(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

/// `path` as a NUL-terminated wide string.
pub(crate) fn wide_path(path: &Path) -> Vec<u16> {
    wide_os(path.as_os_str())
}

/// UTF-16 units from Windows (no terminator) as an OS string, unpaired surrogates kept.
pub(crate) fn from_wide(units: &[u16]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(units)
}

/// System32 as Windows reports it, never from %SystemRoot% or %windir%: an elevated Crystal
/// inherits the user's environment, which user-level code can point anywhere.
pub(crate) fn system32() -> std::path::PathBuf {
    let mut buffer = [0u16; 260];
    // SAFETY: the buffer and its length in characters.
    let len = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
    } as usize;
    if len == 0 || len >= buffer.len() {
        return r"C:\Windows\System32".into();
    }
    from_wide(&buffer[..len]).into()
}

/// Windows code pages used by more than one module.
pub(crate) mod codepage {
    /// The system ANSI code page.
    pub(crate) const ACP: u32 = 0;
    /// Windows' Shift_JIS, a superset of the standard; PlayOnline's names and mail.
    pub(crate) const SHIFT_JIS: u32 = 932;
    pub(crate) const EUC_JP: u32 = 20932;

    /// `bytes` in a Windows code page as text; `None` for empty input or a failed conversion.
    /// `strict` refuses invalid input instead of substituting characters.
    pub(crate) fn decode(code_page: u32, bytes: &[u8], strict: bool) -> Option<String> {
        use windows_sys::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};
        let length = i32::try_from(bytes.len()).ok()?;
        let flags = if strict { MB_ERR_INVALID_CHARS } else { 0 };
        // SAFETY: both calls read `length` bytes from `bytes`; the second writes at
        // most the `needed` u16 the first reported into a buffer of that size.
        unsafe {
            let needed = MultiByteToWideChar(code_page, flags, bytes.as_ptr(), length, std::ptr::null_mut(), 0);
            if needed <= 0 {
                return None;
            }
            let mut wide = vec![0u16; needed as usize];
            let written = MultiByteToWideChar(code_page, flags, bytes.as_ptr(), length, wide.as_mut_ptr(), needed);
            if written <= 0 {
                return None;
            }
            let wide = &wide[..written as usize];
            if strict {
                String::from_utf16(wide).ok()
            } else {
                Some(String::from_utf16_lossy(wide))
            }
        }
    }
}

/// DPAPI-decrypts `input` for the current user (no UI). `Err` carries the Windows error code.
/// The decrypted buffer Windows returns is wiped and freed on every path; a call that
/// succeeds with no output decrypts to an empty value.
pub(crate) fn unprotect(input: &[u8]) -> Result<Zeroizing<Vec<u8>>, u32> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, LocalFree, ERROR_INVALID_PARAMETER},
        Security::Cryptography::{CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };
    struct Output(CRYPT_INTEGER_BLOB);
    impl Drop for Output {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // SAFETY: a buffer of cbData bytes that DPAPI allocated with LocalAlloc.
                unsafe {
                    std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                    LocalFree(self.0.pbData as _);
                }
            }
        }
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: input.len().try_into().map_err(|_| ERROR_INVALID_PARAMETER)?,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output = Output(CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() });
    // SAFETY: `input` borrows a live slice for the call; `output` owns what DPAPI returns.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    };
    if ok == 0 {
        return Err(unsafe { GetLastError() });
    }
    if output.0.pbData.is_null() || output.0.cbData == 0 {
        return Ok(Zeroizing::new(Vec::new()));
    }
    // SAFETY: cbData bytes at pbData, owned by `output` until it drops.
    Ok(Zeroizing::new(unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec()))
}

/// Why `shell_execute` gave no process.
pub(crate) enum ShellError {
    /// ShellExecuteExW failed with this Windows error.
    Failed(u32),
    /// It succeeded but returned no process handle to wait on.
    NoProcess,
}

/// ShellExecuteExW `verb` on `file`, keeping the process handle. `no_ui` suppresses
/// Windows' own error dialogs (SEE_MASK_FLAG_NO_UI).
pub(crate) fn shell_execute(
    verb: &str,
    file: &Path,
    params: Option<&str>,
    dir: Option<&Path>,
    show: i32,
    no_ui: bool,
) -> Result<std::os::windows::io::OwnedHandle, ShellError> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    let verb = wide(verb);
    let file = wide_path(file);
    let params = params.map(wide);
    let directory = dir.map(wide_path);
    // SAFETY: every string buffer lives through the synchronous call; the returned process
    // handle is owned from here on.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | if no_ui { SEE_MASK_FLAG_NO_UI } else { 0 };
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
        info.lpDirectory = directory.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        info.nShow = show;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(ShellError::Failed(GetLastError()));
        }
        if info.hProcess.is_null() {
            return Err(ShellError::NoProcess);
        }
        Ok(std::os::windows::io::OwnedHandle::from_raw_handle(info.hProcess as _))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_terminated_once() {
        assert_eq!(wide("ab"), [97, 98, 0]);
        assert_eq!(wide_path(Path::new("C:\\")), [67, 58, 92, 0]);
        assert_eq!(from_wide(&[97, 0xD800]).len(), 4); // an unpaired surrogate survives
    }

    #[test]
    fn code_pages_decode_and_strict_mode_refuses_invalid_input() {
        assert_eq!(codepage::decode(codepage::SHIFT_JIS, &[0x82, 0xa0], true).as_deref(), Some("あ"));
        assert_eq!(codepage::decode(codepage::SHIFT_JIS, &[0x82], true), None);
        assert!(codepage::decode(codepage::SHIFT_JIS, &[0x82], false).is_some());
        assert_eq!(codepage::decode(codepage::ACP, b"", false), None);
    }

    #[test]
    fn unprotect_rejects_data_that_is_not_dpapi() {
        assert!(unprotect(b"not a dpapi blob").is_err());
    }
}
