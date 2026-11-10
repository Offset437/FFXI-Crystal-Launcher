//! Header fields of a module mapped in this process, read through checked
//! reads so a module that is unloading yields None instead of a fault.

use crate::mem::read;

/// IMAGE_FILE_MACHINE and optional-header magic of modules this process can load.
#[cfg(target_pointer_width = "32")]
const NATIVE: (u16, u16) = (0x14C, 0x10B);
#[cfg(target_pointer_width = "64")]
const NATIVE: (u16, u16) = (0x8664, 0x20B);

/// The identity of one mapped image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// FileHeader.TimeDateStamp.
    pub timestamp: u32,
    /// OptionalHeader.SizeOfImage.
    pub size: usize,
}

/// The identity of the image mapped at `base`: an MZ/PE image for this
/// process's machine whose NT headers start within the first 64 KiB.
pub(crate) fn identity(base: usize) -> Option<Identity> {
    let offset = read::<u32>(base.checked_add(0x3C)?)?;
    if offset >= 0x10000 {
        return None;
    }
    let nt = base.checked_add(offset as usize)?;
    let valid = read::<u16>(base) == Some(0x5A4D)
        && read::<u32>(nt) == Some(0x4550)
        && read::<u16>(nt + 4) == Some(NATIVE.0)
        && read::<u16>(nt + 24) == Some(NATIVE.1);
    valid.then_some(Identity { timestamp: read::<u32>(nt + 8)?, size: read::<u32>(nt + 24 + 56)? as usize })
}

/// IMAGE_FILE_LARGE_ADDRESS_AWARE: a 32-bit process gets 4 GB of user
/// address space on 64-bit Windows.
pub(crate) const LARGE_ADDRESS_AWARE: u16 = 0x20;

/// FileHeader.Characteristics of the image mapped at `base`.
pub(crate) fn characteristics(base: usize) -> Option<u16> {
    identity(base)?;
    let nt = base.checked_add(read::<u32>(base + 0x3C)? as usize)?;
    read::<u16>(nt + 22)
}

/// This process was linked large-address-aware (the game host's 4 GB memory).
pub(crate) fn this_process_large_address_aware() -> bool {
    // SAFETY: GetModuleHandleW(NULL) is this process's executable.
    let base = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize };
    characteristics(base).is_some_and(|c| c & LARGE_ADDRESS_AWARE != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characteristics_of_this_executable() {
        let base = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize };
        let c = characteristics(base).unwrap();
        assert_ne!(c & 0x0002, 0, "IMAGE_FILE_EXECUTABLE_IMAGE");
        assert_eq!(characteristics(0), None);
    }

    #[test]
    fn identity_of_this_executable() {
        // SAFETY: GetModuleHandleW(NULL) is the executable.
        let base = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize };
        let found = identity(base).unwrap();
        assert!(found.size > 0x1000 && found.size.is_multiple_of(0x1000));
        assert_eq!(identity(0), None);
        let data = [0u8; 64];
        assert_eq!(identity(data.as_ptr() as usize), None);
    }
}
