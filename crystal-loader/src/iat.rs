//! Import-table entries of loaded modules, and the one way Crystal changes a
//! pointer slot: make it writable, compare-and-swap, restore the protection.
//! A slot another module changed since it was read is never overwritten.

use std::{ffi::{c_void, CStr}, sync::atomic::{AtomicUsize, Ordering}};
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_READWRITE};

/// One import of one DLL in a loaded module: its name (when the module kept its name table)
/// and the address of its IAT entry.
#[derive(Debug, Clone)]
pub struct ImportSlot {
    pub name: Option<String>,
    pub slot: *mut usize,
}

/// Reads a loaded module's import directory for `dll` (case-insensitive).
///
/// # Safety
/// `base` must be a mapped module image.
pub unsafe fn import_slots(base: usize, dll: &str) -> Vec<ImportSlot> {
    let rd32 = |at: usize| std::ptr::read_unaligned(at as *const u32) as usize;
    let rd16 = |at: usize| std::ptr::read_unaligned(at as *const u16);
    let mut out = Vec::new();
    if base == 0 || rd16(base) != 0x5A4D {
        return out;
    }
    let nt = base + rd32(base + 0x3C);
    let opt = nt + 24;
    let (dir_at, thunk) = match rd16(opt) {
        0x10B => (opt + 96 + 8, 4usize),
        0x20B => (opt + 112 + 8, 8usize),
        _ => return out,
    };
    if thunk != std::mem::size_of::<usize>() {
        return out;
    }
    let imports = rd32(dir_at);
    if imports == 0 {
        return out;
    }
    let mut desc = base + imports;
    loop {
        let (names, name, first) = (rd32(desc), rd32(desc + 12), rd32(desc + 16));
        if name == 0 || first == 0 {
            break;
        }
        let this = CStr::from_ptr((base + name) as *const _).to_bytes();
        if this.eq_ignore_ascii_case(dll.as_bytes()) {
            let mut i = 0;
            loop {
                let slot = (base + first + i * thunk) as *mut usize;
                if std::ptr::read_unaligned(slot) == 0 {
                    break;
                }
                let entry_name = if names == 0 {
                    None
                } else {
                    let e = std::ptr::read_unaligned((base + names + i * thunk) as *const usize);
                    let ordinal = e & (1usize << (thunk * 8 - 1)) != 0;
                    (!ordinal && e != 0).then(|| CStr::from_ptr((base + e + 2) as *const _).to_string_lossy().into_owned())
                };
                out.push(ImportSlot { name: entry_name, slot });
                i += 1;
            }
        }
        desc += 20;
    }
    out
}

/// What [`swap`] did once the slot was writable.
#[derive(Debug)]
pub(crate) struct Swapped {
    /// The slot held `expected` and now holds the replacement.
    pub changed: bool,
    /// Restoring the page protection afterwards; reported even when `changed`.
    pub restored: std::io::Result<()>,
}

/// Replaces the pointer at `slot` with `replacement` only while it still holds
/// `expected`. Err: the slot is misaligned, or Windows refused write access
/// (nothing was changed).
pub(crate) fn swap(slot: usize, expected: usize, replacement: usize) -> std::io::Result<Swapped> {
    let size = std::mem::size_of::<usize>();
    if slot == 0 || !slot.is_multiple_of(std::mem::align_of::<AtomicUsize>()) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "misaligned pointer slot"));
    }
    let mut protection = 0;
    // SAFETY: a pointer-sized slot; its protection is restored below.
    if unsafe { VirtualProtect(slot as *const c_void, size, PAGE_READWRITE, &mut protection) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: aligned and writable for the duration of the exchange.
    let changed = unsafe { AtomicUsize::from_ptr(slot as *mut usize) }
        .compare_exchange(expected, replacement, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    let mut ignored = 0;
    let restored = if unsafe { VirtualProtect(slot as *const c_void, size, protection, &mut ignored) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    };
    Ok(Swapped { changed, restored })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_slots_read_this_executable() {
        // The test binary imports kernel32 functions; every slot holds a resolved address.
        // SAFETY: GetModuleHandleW(NULL) is the executable.
        let base = unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize };
        // SAFETY: the running executable's image.
        let slots = unsafe { import_slots(base, "KERNEL32.dll") };
        assert!(!slots.is_empty());
        assert!(slots.iter().all(|s| unsafe { std::ptr::read_unaligned(s.slot) } != 0));
        assert!(slots.iter().any(|s| s.name.as_deref().is_some_and(|n| n.starts_with("Get"))), "{:?}", slots.iter().map(|s| &s.name).collect::<Vec<_>>());
        assert!(unsafe { import_slots(base, "no-such.dll") }.is_empty());
    }

    #[test]
    fn swap_changes_only_the_expected_value() {
        let cell = Box::new(AtomicUsize::new(123));
        let at = &*cell as *const AtomicUsize as usize;
        let done = swap(at, 123, 456).unwrap();
        assert!(done.changed && done.restored.is_ok());
        assert_eq!(cell.load(Ordering::Acquire), 456);
        let refused = swap(at, 123, 789).unwrap();
        assert!(!refused.changed && refused.restored.is_ok());
        assert_eq!(cell.load(Ordering::Acquire), 456);
        assert!(swap(at + 1, 456, 1).is_err());
        assert!(swap(0, 0, 1).is_err());
    }
}
