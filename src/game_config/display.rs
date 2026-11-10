//! The primary monitor's display modes, for the resolution pickers.

/// Distinct (width, height) modes of the primary display, largest first, and
/// the desktop's current size.
pub(crate) fn modes() -> (Vec<(u32, u32)>, Option<(u32, u32)>) {
    let (mut list, current) = enumerate();
    list.retain(|&(w, h)| w >= 800 && h >= 600);
    list.sort_unstable_by_key(|&(w, h)| std::cmp::Reverse((w * h, w)));
    list.dedup();
    (list, current)
}

fn enumerate() -> (Vec<(u32, u32)>, Option<(u32, u32)>) {
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let blank = || {
        let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        mode
    };
    let mut list = Vec::new();
    let mut index = 0u32;
    loop {
        let mut mode = blank();
        // SAFETY: a zeroed DEVMODEW with dmSize set is what the API expects.
        if unsafe { EnumDisplaySettingsW(std::ptr::null(), index, &mut mode) } == 0 || index > 4096 {
            break;
        }
        if mode.dmBitsPerPel >= 24 || mode.dmBitsPerPel == 0 {
            list.push((mode.dmPelsWidth, mode.dmPelsHeight));
        }
        index += 1;
    }
    let mut mode = blank();
    // SAFETY: as above.
    let current = (unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) } != 0)
        .then_some((mode.dmPelsWidth, mode.dmPelsHeight))
        .filter(|&(w, h)| w > 0 && h > 0);
    (list, current)
}

/// What a preview shows: a typical 4K monitor's list.
pub(crate) fn synthetic() -> (Vec<(u32, u32)>, Option<(u32, u32)>) {
    (
        vec![
            (3840, 2160),
            (3200, 1800),
            (2560, 1440),
            (1920, 1200),
            (1920, 1080),
            (1680, 1050),
            (1600, 900),
            (1366, 768),
            (1280, 720),
        ],
        Some((3840, 2160)),
    )
}
