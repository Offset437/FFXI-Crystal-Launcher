//! Checked reads of this process's own memory. ReadProcessMemory rejects
//! unmapped, guard or partially mapped ranges, so game-owned pointers are
//! never dereferenced in Rust.

use std::ffi::c_void;
use windows_sys::Win32::System::{Diagnostics::Debug::ReadProcessMemory, Threading::GetCurrentProcess};

/// Fills `out` from `address`; false unless every byte was read.
pub(crate) fn read_into<T: Copy>(address: usize, out: &mut [T]) -> bool {
    let size = std::mem::size_of_val(out);
    let mut done = 0;
    // SAFETY: reads this process into `out`, which is valid for `size` bytes.
    unsafe {
        ReadProcessMemory(GetCurrentProcess(), address as *const c_void, out.as_mut_ptr().cast(), size, &mut done) != 0
            && done == size
    }
}

/// One `T` at `address`. `T` must be valid for any bit pattern (integers,
/// arrays of them).
pub(crate) fn read<T: Copy>(address: usize) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let size = std::mem::size_of::<T>();
    let mut done = 0;
    // SAFETY: as in read_into; the value is used only when fully written.
    let ok = unsafe {
        ReadProcessMemory(GetCurrentProcess(), address as *const c_void, value.as_mut_ptr().cast(), size, &mut done)
    };
    (ok != 0 && done == size).then(|| unsafe { value.assume_init() })
}

/// A 32-bit pointer stored at `address`: aligned, at least 0x10000, and at
/// least `headroom` below the top of the address space.
pub(crate) fn pointer(address: usize, headroom: usize) -> Option<usize> {
    read::<u32>(address)
        .map(|value| value as usize)
        .filter(|value| *value >= 0x10000 && *value <= usize::MAX - headroom && value % 4 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_memory_and_rejects_unmapped() {
        let value: u32 = 0x1234_5678;
        let at = &value as *const u32 as usize;
        assert_eq!(read::<u32>(at), Some(0x1234_5678));
        let mut bytes = [0u8; 4];
        assert!(read_into(at, &mut bytes));
        assert_eq!(bytes, 0x1234_5678u32.to_le_bytes());
        assert_eq!(read::<u32>(0), None);
        assert!(!read_into(0, &mut bytes));
        let word: usize = 0x0002_0000;
        let at = &word as *const usize as usize;
        assert_eq!(pointer(at, 0x1000), Some(0x0002_0000));
        let low: usize = 0x8000;
        assert_eq!(pointer(&low as *const usize as usize, 0x1000), None);
        let odd: usize = 0x0002_0002;
        assert_eq!(pointer(&odd as *const usize as usize, 0x1000), None);
    }
}
