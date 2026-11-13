//! PlayStation pads that Windows.Gaming.Input does not list, read from their
//! HID gamepad collection. Seen on the owner's PC: a Bluetooth DualSense whose
//! HID collection is present and readable while WGI reports no controllers.
//!
//! Read-only: the collection is opened for reading and nothing is ever sent
//! to the pad (no feature or output reports), so its report mode, and FFXI's
//! DirectInput view of it, stay as they are. A Bluetooth pad in its basic
//! mode reports no battery; one another program switched to the full report
//! does.
use super::{classify, q, BatterySource, BatteryState, Btn, Family, Input, Pad};

pub(super) const SONY: u16 = 0x054C;

/// Sony pads with a known report layout. The USB wireless adapter (0BA0)
/// carries a DualShock 4 over its own radio.
pub(super) fn known_pid(pid: u16) -> bool {
    matches!(pid, 0x05C4 | 0x09CC | 0x0BA0 | 0x0CE6 | 0x0DF2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Link {
    Usb,
    Bluetooth,
}

/// Bluetooth HID shows up as the HID service UUID (classic) or the HID over
/// GATT service (LE) in the interface path.
pub(super) fn link(path: &str) -> Link {
    let path = path.to_ascii_lowercase();
    if path.contains("{00001124-0000-1000-8000-00805f9b34fb}") || path.contains("{00001812-0000-1000-8000-00805f9b34fb}") {
        Link::Bluetooth
    } else {
        Link::Usb
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Reading {
    pub input: Input,
    pub battery: Option<u8>,
    pub state: BatteryState,
}

fn stick(v: u8) -> f32 {
    let x = ((f32::from(v) - 127.5) / 127.5).clamp(-1.0, 1.0);
    if x.abs() < 0.1 {
        0.0
    } else {
        q(x)
    }
}

/// Sticks at `s..s+4`, the three button bytes at `b..b+3`, triggers at `t`/`t+1`.
fn input(r: &[u8], s: usize, b: usize, t: usize, mute: bool) -> Input {
    let (b0, b1, b2) = (r[b], r[b + 1], r[b + 2]);
    let mut buttons = 0;
    let mut set = |on: bool, btn: Btn| {
        if on {
            buttons |= btn.bit();
        }
    };
    let hat = b0 & 0x0F;
    set(matches!(hat, 7 | 0 | 1), Btn::DUp);
    set(matches!(hat, 1..=3), Btn::DRight);
    set(matches!(hat, 3..=5), Btn::DDown);
    set(matches!(hat, 5..=7), Btn::DLeft);
    set(b0 & 0x10 != 0, Btn::West);
    set(b0 & 0x20 != 0, Btn::South);
    set(b0 & 0x40 != 0, Btn::East);
    set(b0 & 0x80 != 0, Btn::North);
    set(b1 & 0x01 != 0, Btn::LB);
    set(b1 & 0x02 != 0, Btn::RB);
    set(b1 & 0x10 != 0, Btn::Back);
    set(b1 & 0x20 != 0, Btn::Start);
    set(b1 & 0x40 != 0, Btn::LS);
    set(b1 & 0x80 != 0, Btn::RS);
    set(b2 & 0x01 != 0, Btn::Guide);
    set(b2 & 0x02 != 0, Btn::Touchpad);
    set(mute && b2 & 0x04 != 0, Btn::Mute);
    let (lt, rt) = (q(f32::from(r[t]) / 255.0), q(f32::from(r[t + 1]) / 255.0));
    // Triggers count as pressed past half travel, as on the WGI path.
    set(lt > 0.5, Btn::LT);
    set(rt > 0.5, Btn::RT);
    Input { buttons, lt, rt, lx: stick(r[s]), ly: -stick(r[s + 1]), rx: stick(r[s + 2]), ry: -stick(r[s + 3]) }
}

/// Battery nibble: level in tenths. DualSense high nibble 0 = discharging,
/// 1 = charging, 2 = full; DualShock 4 bit 4 = cable, level 11 = full.
fn battery(family: Family, byte: u8) -> (Option<u8>, BatteryState) {
    let level = byte & 0x0F;
    let pct = |l: u8| Some((l.min(10) * 10 + 5).min(100));
    if family == Family::DualShock4 {
        return match (byte & 0x10 != 0, level) {
            (true, 11..) => (Some(100), BatteryState::Idle),
            (true, l) => (pct(l), BatteryState::Charging),
            (false, l) => (pct(l), BatteryState::Discharging),
        };
    }
    match byte >> 4 {
        0 => (pct(level), BatteryState::Discharging),
        1 => (pct(level), BatteryState::Charging),
        2 => (Some(100), BatteryState::Idle),
        _ => (None, BatteryState::Unknown),
    }
}

/// Decodes one input report (report id first). `None` for reports of
/// another kind or too short for their layout.
pub(super) fn parse(family: Family, link: Link, r: &[u8]) -> Option<Reading> {
    let dualsense = matches!(family, Family::DualSense | Family::DualSenseEdge);
    let full = |o: usize, battery_at: usize| -> Option<Reading> {
        if r.len() <= battery_at {
            return None;
        }
        let input = if dualsense { input(r, o, o + 7, o + 4, true) } else { input(r, o, o + 4, o + 7, false) };
        let (battery, state) = battery(family, r[battery_at]);
        Some(Reading { input, battery, state })
    };
    match (dualsense, link, *r.first()?) {
        (true, Link::Usb, 0x01) => full(1, 53),
        (true, Link::Bluetooth, 0x31) => full(2, 54),
        (false, Link::Usb, 0x01) => full(1, 30),
        (false, Link::Bluetooth, 0x11) => full(3, 32),
        // Bluetooth basic mode, the same layout on both pads; no battery.
        (_, Link::Bluetooth, 0x01) if r.len() >= 10 => Some(Reading { input: input(r, 1, 5, 8, false), ..Reading::default() }),
        _ => None,
    }
}

/// Open HID pads, keyed by interface path, polled from the controller worker.
#[derive(Default)]
pub(super) struct HidPads {
    devices: Vec<win::Device>,
    next_id: usize,
}

#[cfg(test)]
pub(super) fn win_interfaces_for_probe() -> Vec<String> {
    win::interfaces()
}

/// Far above any gilrs id, so the two id spaces never meet.
const FIRST_ID: usize = 1 << 16;

impl HidPads {
    /// Opens newly present pads and drops removed ones. True when the set changed.
    pub(super) fn refresh(&mut self) -> bool {
        let paths = win::interfaces();
        let before = self.devices.len();
        self.devices.retain(|d| d.alive && paths.contains(&d.path));
        let mut changed = self.devices.len() != before;
        for path in paths {
            if self.devices.iter().any(|d| d.path == path) {
                continue;
            }
            let Some((SONY, pid)) = super::device_ids(&path) else { continue };
            if !known_pid(pid) {
                continue;
            }
            if let Some(device) = win::Device::open(path, pid, FIRST_ID + self.next_id) {
                self.next_id += 1;
                self.devices.push(device);
                changed = true;
            }
        }
        changed
    }

    /// Drains pending reports. True when any pad's state changed or one went away.
    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        for device in &mut self.devices {
            changed |= device.pump();
        }
        let before = self.devices.len();
        self.devices.retain(|d| d.alive);
        changed || self.devices.len() != before
    }

    /// Pads not already listed by gilrs: each gilrs pad with the same ids
    /// accounts for one HID pad.
    pub(super) fn pads(&self, listed: &[Pad]) -> Vec<Pad> {
        let keep = unlisted(self.devices.iter().map(|d| d.pid), listed);
        self.devices.iter().zip(keep).filter(|(_, k)| *k).map(|(d, _)| d.pad()).collect()
    }
}

/// For each HID pad (by Sony product id): true when no remaining gilrs pad
/// with the same ids accounts for it.
fn unlisted(pids: impl Iterator<Item = u16>, listed: &[Pad]) -> Vec<bool> {
    let mut taken: Vec<(Option<u16>, Option<u16>)> = listed.iter().map(|p| (p.vid, p.pid)).collect();
    pids.map(|pid| match taken.iter().position(|k| *k == (Some(SONY), Some(pid))) {
        Some(i) => {
            taken.swap_remove(i);
            false
        }
        None => true,
    })
    .collect()
}

mod win {
    use super::*;
    use std::mem::size_of;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
        SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HidD_FreePreparsedData, HidD_GetHidGuid, HidD_GetPreparsedData, HidD_GetProductString, HidP_GetCaps,
        HIDP_CAPS, HIDP_STATUS_SUCCESS,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn until_nul(units: &[u16]) -> String {
        let end = units.iter().position(|c| *c == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }

    /// Present HID interface paths.
    pub(super) fn interfaces() -> Vec<String> {
        let mut guid = unsafe { std::mem::zeroed() };
        unsafe { HidD_GetHidGuid(&mut guid) };
        let set = unsafe {
            SetupDiGetClassDevsW(&guid, std::ptr::null(), std::ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
        };
        if set == -1 {
            return Vec::new();
        }
        let mut paths = Vec::new();
        for index in 0..1024 {
            let mut data = SP_DEVICE_INTERFACE_DATA { cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..unsafe { std::mem::zeroed() } };
            if unsafe { SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut data) } == 0 {
                break;
            }
            let mut needed = 0u32;
            unsafe { SetupDiGetDeviceInterfaceDetailW(set, &data, std::ptr::null_mut(), 0, &mut needed, std::ptr::null_mut()) };
            if needed as usize <= size_of::<u32>() || needed > 4096 {
                continue;
            }
            // u32-aligned storage for the variable-length detail record.
            let mut buf = vec![0u32; (needed as usize).div_ceil(4)];
            let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
            unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
            if unsafe { SetupDiGetDeviceInterfaceDetailW(set, &data, detail, needed, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
                continue;
            }
            let units = unsafe {
                std::slice::from_raw_parts(std::ptr::addr_of!((*detail).DevicePath).cast::<u16>(), (needed as usize - 4) / 2)
            };
            paths.push(until_nul(units));
        }
        unsafe { SetupDiDestroyDeviceInfoList(set) };
        paths
    }

    pub(super) struct Device {
        pub path: String,
        pub pid: u16,
        pub alive: bool,
        id: usize,
        family: Family,
        link: Link,
        name: String,
        handle: HANDLE,
        // Boxed: the kernel writes to both while a read is pending.
        overlapped: Box<OVERLAPPED>,
        buf: Box<[u8]>,
        pending: bool,
        reading: Reading,
    }

    impl Device {
        pub(super) fn open(path: String, pid: u16, id: usize) -> Option<Device> {
            let handle = unsafe {
                CreateFileW(
                    wide(&path).as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return None;
            }
            let close = |h| {
                unsafe { CloseHandle(h) };
                None
            };
            // Only the gamepad collection (Generic Desktop / Game Pad).
            let mut caps: HIDP_CAPS = unsafe { std::mem::zeroed() };
            let mut parsed = 0;
            if !unsafe { HidD_GetPreparsedData(handle, &mut parsed) } {
                return close(handle);
            }
            let status = unsafe { HidP_GetCaps(parsed, &mut caps) };
            unsafe { HidD_FreePreparsedData(parsed) };
            if status != HIDP_STATUS_SUCCESS || caps.UsagePage != 0x01 || caps.Usage != 0x05 || caps.InputReportByteLength < 10 {
                return close(handle);
            }
            let mut name = [0u16; 128];
            let name = if unsafe { HidD_GetProductString(handle, name.as_mut_ptr().cast(), (name.len() * 2) as u32) } {
                until_nul(&name)
            } else {
                String::new()
            };
            let family = classify(Some(SONY), Some(pid), &name);
            let name = if name.trim().is_empty() { family.label().to_string() } else { name };
            Some(Device {
                link: link(&path),
                path,
                pid,
                alive: true,
                id,
                family,
                name,
                handle,
                overlapped: Box::new(unsafe { std::mem::zeroed() }),
                buf: vec![0; usize::from(caps.InputReportByteLength)].into_boxed_slice(),
                pending: false,
                reading: Reading::default(),
            })
        }

        /// Reads every queued report without blocking; keeps the latest.
        pub(super) fn pump(&mut self) -> bool {
            let mut changed = false;
            // The HID class queue holds a bounded number of reports.
            for _ in 0..128 {
                if !self.pending {
                    *self.overlapped = unsafe { std::mem::zeroed() };
                    let ok = unsafe {
                        ReadFile(self.handle, self.buf.as_mut_ptr(), self.buf.len() as u32, std::ptr::null_mut(), &mut *self.overlapped)
                    };
                    if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                        self.alive = false;
                        return true;
                    }
                    self.pending = true;
                }
                let mut read = 0u32;
                if unsafe { GetOverlappedResult(self.handle, &*self.overlapped, &mut read, 0) } == 0 {
                    if unsafe { GetLastError() } == ERROR_IO_INCOMPLETE {
                        return changed;
                    }
                    self.pending = false;
                    self.alive = false;
                    return true;
                }
                self.pending = false;
                let len = (read as usize).min(self.buf.len());
                if let Some(reading) = parse(self.family, self.link, &self.buf[..len]) {
                    changed |= reading != self.reading;
                    self.reading = reading;
                }
            }
            changed
        }

        pub(super) fn pad(&self) -> Pad {
            let wireless = self.link == Link::Bluetooth || self.pid == 0x0BA0;
            Pad {
                id: self.id,
                name: self.name.clone(),
                family: self.family,
                vid: Some(SONY),
                pid: Some(self.pid),
                wireless: Some(wireless),
                battery: self.reading.battery,
                battery_source: Some(BatterySource::Controller),
                battery_state: self.reading.state,
                battery_level: None,
                rumble: false,
                input: self.reading.input,
            }
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            unsafe {
                if self.pending {
                    CancelIoEx(self.handle, &*self.overlapped);
                    let mut read = 0u32;
                    GetOverlappedResult(self.handle, &*self.overlapped, &mut read, 1);
                }
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the owner's Bluetooth DualSense in basic mode, idle.
    const BT_BASIC_IDLE: [u8; 10] = [0x01, 0x82, 0x7D, 0x7E, 0x80, 0x08, 0x00, 0x18, 0x00, 0x00];

    #[test]
    fn gamepad_hid_bluetooth_basic_report_idle_is_centred_and_released() {
        let r = parse(Family::DualSense, Link::Bluetooth, &BT_BASIC_IDLE).unwrap();
        assert_eq!(r.input, Input::default());
        assert_eq!((r.battery, r.state), (None, BatteryState::Unknown));
        // Padded to the collection's 78-byte report length, as Windows delivers it.
        let mut padded = [0u8; 78];
        padded[..10].copy_from_slice(&BT_BASIC_IDLE);
        assert_eq!(parse(Family::DualSense, Link::Bluetooth, &padded).unwrap().input, Input::default());
    }

    #[test]
    fn gamepad_hid_bluetooth_basic_report_buttons_hat_sticks_triggers() {
        let mut r = BT_BASIC_IDLE;
        r[1] = 0x00; // left stick full left
        r[2] = 0x00; // and full up
        r[5] = 0x20 | 0x80 | 0x02; // Cross, Triangle, hat east
        r[6] = 0x01 | 0x20; // L1, Options
        r[7] = 0x18 | 0x01 | 0x02; // counter bits, PS, touchpad
        r[8] = 0xFF; // L2 fully pressed
        let i = parse(Family::DualShock4, Link::Bluetooth, &r).unwrap().input;
        for b in [Btn::South, Btn::North, Btn::DRight, Btn::LB, Btn::Start, Btn::Guide, Btn::Touchpad, Btn::LT] {
            assert!(i.pressed(b), "{b:?}");
        }
        for b in [Btn::East, Btn::West, Btn::DUp, Btn::DDown, Btn::RT, Btn::Mute] {
            assert!(!i.pressed(b), "{b:?}");
        }
        assert_eq!((i.lx, i.ly, i.lt), (-1.0, 1.0, 1.0));
    }

    #[test]
    fn gamepad_hid_dualsense_full_reports_carry_battery_and_mute() {
        let mut usb = [0u8; 64];
        usb[0] = 0x01;
        usb[1..5].copy_from_slice(&[0x80; 4]);
        usb[8] = 0x08 | 0x40; // hat neutral, Circle
        usb[10] = 0x04; // mute
        usb[53] = 0x17; // charging, level 7
        let r = parse(Family::DualSense, Link::Usb, &usb).unwrap();
        assert!(r.input.pressed(Btn::East) && r.input.pressed(Btn::Mute));
        assert_eq!((r.battery, r.state), (Some(75), BatteryState::Charging));
        let mut bt = [0u8; 78];
        bt[0] = 0x31;
        bt[2..65].copy_from_slice(&usb[1..]);
        bt[54] = 0x23; // full
        let r = parse(Family::DualSenseEdge, Link::Bluetooth, &bt).unwrap();
        assert!(r.input.pressed(Btn::East) && r.input.pressed(Btn::Mute));
        assert_eq!((r.battery, r.state), (Some(100), BatteryState::Idle));
        assert_eq!(parse(Family::DualSense, Link::Usb, &usb[..40]), None);
    }

    #[test]
    fn gamepad_hid_dualshock4_full_reports_carry_battery() {
        let mut usb = [0u8; 64];
        usb[0] = 0x01;
        usb[1..5].copy_from_slice(&[0x80; 4]);
        usb[5] = 0x08 | 0x10; // Square
        usb[30] = 0x05; // on battery, level 5
        let r = parse(Family::DualShock4, Link::Usb, &usb).unwrap();
        assert!(r.input.pressed(Btn::West));
        assert_eq!((r.battery, r.state), (Some(55), BatteryState::Discharging));
        let mut bt = [0u8; 78];
        bt[0] = 0x11;
        bt[3..66].copy_from_slice(&usb[1..]);
        bt[32] = 0x1B; // cable, full
        let r = parse(Family::DualShock4, Link::Bluetooth, &bt).unwrap();
        assert!(r.input.pressed(Btn::West));
        assert_eq!((r.battery, r.state), (Some(100), BatteryState::Idle));
    }

    /// Live, read-only: `cargo test gamepad_hid_live -- --ignored --nocapture`
    /// lists the HID PlayStation pads on this machine and their decoded state.
    #[test]
    #[ignore]
    fn gamepad_hid_live() {
        let mut pads = HidPads::default();
        pads.refresh();
        for _ in 0..20 {
            pads.poll();
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        for pad in pads.pads(&[]) {
            println!("{pad:?}\n  {} · {}", pad.family.label(), pad.connection());
        }
    }

    #[test]
    fn gamepad_hid_ignores_other_report_ids() {
        assert_eq!(parse(Family::DualSense, Link::Usb, &[0x05; 64]), None);
        assert_eq!(parse(Family::DualSense, Link::Bluetooth, &[0x31; 20]), None);
        assert_eq!(parse(Family::DualSense, Link::Bluetooth, &[]), None);
    }

    #[test]
    fn gamepad_hid_link_and_known_sony_pads() {
        assert_eq!(link(r"\\?\HID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002054c_PID&0ce6#c&34bfb810&e&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"), Link::Bluetooth);
        assert_eq!(link(r"\\?\HID#{00001812-0000-1000-8000-00805f9b34fb}_Dev_VID&02054c_PID&0ce6#x#{4d1e55b2-f16f-11cf-88cb-001111000030}"), Link::Bluetooth);
        assert_eq!(link(r"\\?\HID#VID_054C&PID_0CE6&MI_03#x#{4d1e55b2-f16f-11cf-88cb-001111000030}"), Link::Usb);
        for pid in [0x05C4, 0x09CC, 0x0BA0, 0x0CE6, 0x0DF2] {
            assert!(known_pid(pid));
        }
        assert!(!known_pid(0x0268)); // DualShock 3: different layout
    }

    #[test]
    fn gamepad_hid_pads_skip_ones_gilrs_already_lists() {
        let gilrs_pad = |vid, pid| Pad {
            id: 0, name: String::new(), family: classify(Some(vid), Some(pid), ""), vid: Some(vid), pid: Some(pid),
            wireless: None, battery: None, battery_source: None, battery_state: BatteryState::Unknown,
            battery_level: None, rumble: false, input: Input::default(),
        };
        assert_eq!(unlisted([0x0CE6, 0x0CE6, 0x09CC].into_iter(), &[]), [true, true, true]);
        // One WGI DualSense accounts for one of two HID DualSenses; an Xbox pad accounts for none.
        let listed = [gilrs_pad(SONY, 0x0CE6), gilrs_pad(0x045E, 0x0B12)];
        assert_eq!(unlisted([0x0CE6, 0x0CE6, 0x09CC].into_iter(), &listed), [false, true, true]);
    }
}
