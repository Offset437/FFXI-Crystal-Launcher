//! Read-only Windows controller metadata and battery reports.
//! A handheld match requires the host model and Windows' internal-device
//! evidence. An Xbox VID or the presence of an Ally host alone is insufficient.
use super::{classify, BatteryLevel, BatterySource, BatteryState, Family};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Power {
    pub percent: Option<u8>,
    pub level: Option<BatteryLevel>,
    pub state: BatteryState,
    pub source: Option<BatterySource>,
}

#[derive(Clone, Debug)]
struct Controller {
    vid: Option<u16>,
    pid: Option<u16>,
    name: String,
    wireless: Option<bool>,
    power: Power,
}

#[derive(Clone, Copy, Debug)]
struct UsbDevice {
    vid: u16,
    pid: u16,
    internal: Option<bool>,
    physical: bool,
}

#[derive(Default)]
pub(super) struct Snapshot {
    host: Option<Family>,
    controllers: Vec<Controller>,
    usb: Vec<UsbDevice>,
    handheld_power: Power,
}

impl Snapshot {
    pub(super) fn refresh() -> Self {
        win::refresh()
    }

    pub(super) fn metadata(
        &self,
        vid: Option<u16>,
        pid: Option<u16>,
        name: &str,
    ) -> (Family, Option<bool>, Power) {
        let family = classify(vid, pid, name);
        let mut matches = self
            .controllers
            .iter()
            .filter(|c| c.vid == vid && c.pid == pid && c.name == name);
        let Some(controller) = matches.next() else {
            return (family, None, Power::default());
        };
        // gilrs does not expose WGI's instance identity. A duplicate signature
        // cannot safely inherit a report from another physical controller.
        if matches.next().is_some() {
            return (family, None, Power::default());
        }
        if let Some(handheld) = self.internal_family(family, controller) {
            return (handheld, Some(false), self.handheld_power);
        }
        (family, controller.wireless, controller.power)
    }

    fn internal_family(&self, family: Family, controller: &Controller) -> Option<Family> {
        if family != Family::Xbox || controller.wireless != Some(false) {
            return None;
        }
        let host = self.host?;
        let (vid, pid) = (controller.vid?, controller.pid?);
        // Names can vary between the built-in pad and an external pad with the
        // same IDs. Refuse the match even when their display names differ.
        if self
            .controllers
            .iter()
            .filter(|c| c.vid == Some(vid) && c.pid == Some(pid))
            .count()
            != 1
        {
            return None;
        }
        let mut usb = self.usb.iter().filter(|d| d.vid == vid && d.pid == pid);
        let device = usb.next()?;
        if usb.next().is_some() || device.internal != Some(true) || !device.physical {
            return None;
        }
        Some(host)
    }
}

/// ASUS' official model names are RC73YA (Ally) and RC73XA (Ally X).
/// Product-name tokens, rather than a broad "ROG" substring, prevent unrelated
/// ASUS PCs from being treated as handhelds.
fn host_family(manufacturer: &str, product: &str) -> Option<Family> {
    let maker = manufacturer.trim().to_ascii_uppercase();
    if !matches!(
        maker.as_str(),
        "ASUS" | "ASUSTEK COMPUTER INC." | "ASUSTEK COMPUTER INC"
    ) {
        return None;
    }
    let model = product.to_ascii_uppercase();
    let mut family = None;
    for token in model.split(|c: char| !c.is_ascii_alphanumeric()) {
        let candidate = match token {
            "RC73YA" => Some(Family::RogXboxAlly),
            "RC73XA" => Some(Family::RogXboxAllyX),
            _ => None,
        };
        if let Some(candidate) = candidate {
            if family.is_some_and(|f| f != candidate) {
                return None;
            }
            family = Some(candidate);
        }
    }
    family
}

/// A physical USB node has VID/PID without an MI_ child-interface token.
/// Never assign a handheld source from a virtual ROOT/SWD controller.
fn physical_usb_ids(instance: &str) -> Option<(u16, u16)> {
    let instance = instance.to_ascii_uppercase();
    let mut parts = instance.split('\\');
    if parts.next()? != "USB" {
        return None;
    }
    let hardware = parts.next()?;
    if hardware.split('&').any(|token| token.starts_with("MI_")) {
        return None;
    }
    super::device_ids(hardware)
}

fn is_usb_root_hub(instance: &str) -> bool {
    let instance = instance.to_ascii_uppercase();
    let mut parts = instance.split('\\');
    parts.next() == Some("USB")
        && matches!(parts.next(), Some("ROOT_HUB" | "ROOT_HUB20" | "ROOT_HUB30"))
}

fn percent(full: Option<i32>, remaining: Option<i32>) -> Option<u8> {
    let (full, remaining) = (full?, remaining?);
    if full <= 0 || remaining < 0 || remaining > full {
        return None;
    }
    Some(((remaining as u64 * 100) / full as u64) as u8)
}

fn power(
    state: BatteryState,
    present: bool,
    source: BatterySource,
    full: Option<i32>,
    remaining: Option<i32>,
    detailed: bool,
) -> Power {
    let mut result = Power {
        source: Some(source),
        ..Power::default()
    };
    if !present || state == BatteryState::Unknown {
        return result;
    }
    result.state = state;
    let Some(pct) = percent(full, remaining) else {
        return result;
    };
    if source == BatterySource::Handheld || detailed {
        result.percent = Some(pct);
    } else {
        // Microsoft's documented WGI synthetic values represent levels only:
        // https://learn.microsoft.com/windows/uwp/gaming/input-practices-for-games
        result.level = match pct {
            10 => Some(BatteryLevel::Critical),
            40 => Some(BatteryLevel::Low),
            70 => Some(BatteryLevel::Medium),
            100 => Some(BatteryLevel::Full),
            _ => None,
        };
    }
    result
}

mod win {
    use super::*;
    use std::mem::size_of;
    use windows::Devices::Power::{Battery, BatteryReport};
    use windows::Gaming::Input::RawGameController;
    use windows::System::Power::BatteryStatus;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_Get_Device_IDW, CM_Get_Parent, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, CR_SUCCESS, DIGCF_ALLCLASSES, DIGCF_PRESENT,
        HDEVINFO, SP_DEVINFO_DATA,
    };
    use windows_sys::Win32::Devices::Properties::{
        DEVPKEY_Device_InLocalMachineContainer, DEVPROP_TYPE_BOOLEAN,
    };
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_ITEMS};

    pub(super) fn refresh() -> Snapshot {
        // The host model cannot change while the process runs: read it once.
        static HOST: std::sync::OnceLock<Option<Family>> = std::sync::OnceLock::new();
        let host = *HOST.get_or_init(|| {
            registry_string("SystemManufacturer")
                .zip(registry_string("SystemProductName"))
                .and_then(|(maker, model)| host_family(&maker, &model))
        });
        let controllers = controllers().unwrap_or_default();
        // USB metadata is relevant only on one of the known Ally host models.
        let usb = if host.is_some() {
            usb_devices().unwrap_or_default()
        } else {
            Vec::new()
        };
        let handheld_power = if host.is_some() {
            Battery::AggregateBattery()
                .and_then(|battery| battery.GetReport())
                .ok()
                .map_or(
                    Power {
                        source: Some(BatterySource::Handheld),
                        ..Power::default()
                    },
                    |report| report_power(&report, BatterySource::Handheld),
                )
        } else {
            Power::default()
        };
        Snapshot {
            host,
            controllers,
            usb,
            handheld_power,
        }
    }

    fn report_power(report: &BatteryReport, source: BatterySource) -> Power {
        let status = report.Status().ok();
        let state = match status {
            Some(BatteryStatus::Charging) => BatteryState::Charging,
            Some(BatteryStatus::Discharging) => BatteryState::Discharging,
            Some(BatteryStatus::Idle) => BatteryState::Idle,
            _ => BatteryState::Unknown,
        };
        let full = report
            .FullChargeCapacityInMilliwattHours()
            .and_then(|v| v.Value())
            .ok();
        let remaining = report
            .RemainingCapacityInMilliwattHours()
            .and_then(|v| v.Value())
            .ok();
        let design = report
            .DesignCapacityInMilliwattHours()
            .and_then(|v| v.Value())
            .ok();
        let rate = report.ChargeRateInMilliwatts().and_then(|v| v.Value()).ok();
        let detailed = design.is_some_and(|v| v > 0) || rate.is_some();
        power(
            state,
            status != Some(BatteryStatus::NotPresent),
            source,
            full,
            remaining,
            detailed,
        )
    }

    fn controllers() -> Option<Vec<Controller>> {
        let raw = RawGameController::RawGameControllers().ok()?;
        let count = raw.Size().ok()?;
        if count > 128 {
            return None;
        }
        let mut result = Vec::with_capacity(count as usize);
        // Indexed access also follows gilrs' workaround for WGI iteration
        // crashes when the application is started through Steam.
        for index in 0..count {
            let raw = raw.GetAt(index).ok()?;
            result.push(Controller {
                vid: raw.HardwareVendorId().ok(),
                pid: raw.HardwareProductId().ok(),
                name: raw
                    .DisplayName()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_else(|_| "unknown".into()),
                wireless: raw.IsWireless().ok(),
                power: raw.TryGetBatteryReport().ok().map_or(
                    Power {
                        source: Some(BatterySource::Controller),
                        ..Power::default()
                    },
                    |report| report_power(&report, BatterySource::Controller),
                ),
            });
        }
        Some(result)
    }

    fn registry_string(value: &str) -> Option<String> {
        use crate::win::reg::{self, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
        let units = reg::string(HKEY_LOCAL_MACHINE, "HARDWARE\\DESCRIPTION\\System\\BIOS", value, RRF_RT_REG_SZ, 512)?;
        Some(String::from_utf16_lossy(&units))
    }

    struct Devices(HDEVINFO);
    impl Drop for Devices {
        fn drop(&mut self) {
            unsafe {
                SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }

    fn on_usb_host(mut node: u32) -> bool {
        for _ in 0..32 {
            let mut parent = 0;
            if unsafe { CM_Get_Parent(&mut parent, node, 0) } != CR_SUCCESS || parent == node {
                return false;
            }
            let mut id = [0u16; 1024];
            if unsafe { CM_Get_Device_IDW(parent, id.as_mut_ptr(), id.len() as u32, 0) }
                != CR_SUCCESS
            {
                return false;
            }
            let Some(length) = id.iter().position(|c| *c == 0) else {
                return false;
            };
            if is_usb_root_hub(&String::from_utf16_lossy(&id[..length])) {
                return true;
            }
            node = parent;
        }
        false
    }

    fn usb_devices() -> Option<Vec<UsbDevice>> {
        let enumerator: Vec<u16> = "USB\0".encode_utf16().collect();
        let handle = unsafe {
            SetupDiGetClassDevsW(
                std::ptr::null(),
                enumerator.as_ptr(),
                std::ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_ALLCLASSES,
            )
        };
        if handle == -1 {
            return None;
        }
        let devices = Devices(handle);
        let mut result = Vec::new();
        for index in 0..4096 {
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..SP_DEVINFO_DATA::default()
            };
            if unsafe { SetupDiEnumDeviceInfo(devices.0, index, &mut info) } == 0 {
                return if unsafe { GetLastError() } == ERROR_NO_MORE_ITEMS {
                    Some(result)
                } else {
                    None
                };
            }
            let mut id = [0u16; 1024];
            if unsafe { CM_Get_Device_IDW(info.DevInst, id.as_mut_ptr(), id.len() as u32, 0) }
                != CR_SUCCESS
            {
                return None;
            }
            let length = id.iter().position(|c| *c == 0)?;
            let Some((vid, pid)) = physical_usb_ids(&String::from_utf16_lossy(&id[..length]))
            else {
                continue;
            };
            let mut flag = 0u8;
            let mut kind = 0u32;
            let mut bytes = 1u32;
            let status = unsafe {
                CM_Get_DevNode_PropertyW(
                    info.DevInst,
                    &DEVPKEY_Device_InLocalMachineContainer,
                    &mut kind,
                    &mut flag,
                    &mut bytes,
                    0,
                )
            };
            let internal = (status == CR_SUCCESS && kind == DEVPROP_TYPE_BOOLEAN && bytes == 1)
                .then_some(flag != 0);
            result.push(UsbDevice {
                vid,
                pid,
                internal,
                physical: on_usb_host(info.DevInst),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xbox_controller() -> Controller {
        Controller {
            vid: Some(0x045e),
            pid: Some(0x0b12),
            name: "Xbox Controller".into(),
            wireless: Some(false),
            power: Power {
                source: Some(BatterySource::Controller),
                ..Power::default()
            },
        }
    }

    fn ally() -> Snapshot {
        Snapshot {
            host: Some(Family::RogXboxAlly),
            controllers: vec![xbox_controller()],
            usb: vec![UsbDevice {
                vid: 0x045e,
                pid: 0x0b12,
                internal: Some(true),
                physical: true,
            }],
            handheld_power: power(
                BatteryState::Discharging,
                true,
                BatterySource::Handheld,
                Some(60_000),
                Some(25_200),
                true,
            ),
        }
    }

    #[test]
    fn gamepad_ally_host_requires_the_official_asus_model() {
        assert_eq!(
            host_family("ASUSTeK COMPUTER INC.", "ROG Xbox Ally RC73YA_RC73YA"),
            Some(Family::RogXboxAlly)
        );
        assert_eq!(
            host_family("ASUS", "ROG Xbox Ally X RC73XA"),
            Some(Family::RogXboxAllyX)
        );
        for product in [
            "ROG Laptop",
            "RC73YAX",
            "RC73XA RC73YA",
            "ROG Ally RC71L",
            "",
        ] {
            assert_eq!(host_family("ASUS", product), None);
        }
        assert_eq!(host_family("Other vendor", "RC73YA"), None);
    }

    #[test]
    fn gamepad_ally_requires_unique_internal_physical_controller() {
        let mut snapshot = ally();
        let metadata = || snapshot.metadata(Some(0x045e), Some(0x0b12), "Xbox Controller");
        assert_eq!(
            metadata(),
            (Family::RogXboxAlly, Some(false), snapshot.handheld_power)
        );
        snapshot.host = Some(Family::RogXboxAllyX);
        assert_eq!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .0,
            Family::RogXboxAllyX
        );
        snapshot.host = None;
        assert_eq!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .0,
            Family::Xbox
        );
    }

    #[test]
    fn gamepad_external_xbox_on_ally_never_inherits_system_battery() {
        for internal in [None, Some(false)] {
            let mut snapshot = ally();
            snapshot.usb[0].internal = internal;
            let (family, _, reading) =
                snapshot.metadata(Some(0x045e), Some(0x0b12), "Xbox Controller");
            assert_eq!(family, Family::Xbox);
            assert_eq!(reading.source, Some(BatterySource::Controller));
            assert_eq!(reading.percent, None);
        }
        let mut snapshot = ally();
        snapshot.controllers.push(Controller {
            name: "Another Xbox".into(),
            ..xbox_controller()
        });
        let (family, _, reading) = snapshot.metadata(Some(0x045e), Some(0x0b12), "Xbox Controller");
        assert_eq!(family, Family::Xbox);
        assert_ne!(reading.source, Some(BatterySource::Handheld));
        snapshot.controllers[1].name = "Xbox Controller".into();
        assert_eq!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .2,
            Power::default()
        );
        let mut snapshot = ally();
        snapshot.usb[0].physical = false;
        assert_eq!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .0,
            Family::Xbox
        );
        assert_ne!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .2
                .source,
            Some(BatterySource::Handheld)
        );
        let mut snapshot = ally();
        snapshot.usb.push(UsbDevice {
            physical: false,
            ..snapshot.usb[0]
        });
        assert_eq!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .0,
            Family::Xbox
        );
    }

    #[test]
    fn gamepad_external_sony_and_wireless_xbox_on_ally_keep_their_sources() {
        let mut snapshot = ally();
        snapshot.controllers.push(Controller {
            vid: Some(0x054c),
            pid: Some(0x0ce6),
            name: "Wireless Controller".into(),
            wireless: Some(true),
            power: power(
                BatteryState::Charging,
                true,
                BatterySource::Controller,
                Some(2000),
                Some(820),
                true,
            ),
        });
        let (family, wireless, reading) =
            snapshot.metadata(Some(0x054c), Some(0x0ce6), "Wireless Controller");
        assert_eq!(family, Family::DualSense);
        assert_eq!(wireless, Some(true));
        assert_eq!(reading.percent, Some(41));
        assert_eq!(reading.source, Some(BatterySource::Controller));
        snapshot.controllers[0].wireless = Some(true);
        assert_eq!(
            snapshot
                .metadata(Some(0x045e), Some(0x0b12), "Xbox Controller")
                .0,
            Family::Xbox
        );
    }

    #[test]
    fn gamepad_usb_identity_excludes_child_interfaces_and_virtual_nodes() {
        assert_eq!(
            physical_usb_ids("USB\\VID_045E&PID_0B12\\fixture"),
            Some((0x045e, 0x0b12))
        );
        for id in [
            "USB\\VID_045E&PID_0B12&MI_00\\fixture",
            "HID\\VID_045E&PID_0B12\\fixture",
            "ROOT\\VID_045E&PID_0B12\\fixture",
            "USB\\VID_045EX&PID_0B12\\fixture",
        ] {
            assert_eq!(physical_usb_ids(id), None);
        }
        assert!(is_usb_root_hub("USB\\ROOT_HUB30\\fixture"));
        for id in [
            "ROOT\\VIGEMBUS\\fixture",
            "USB\\ROOT_HUB_FAKE\\fixture",
            "ROOT\\ROOT_HUB30\\fixture",
        ] {
            assert!(!is_usb_root_hub(id));
        }
    }

    #[test]
    fn gamepad_battery_capacities_reject_unknown_missing_and_invalid_values() {
        for (full, remaining) in [
            (None, Some(1)),
            (Some(1), None),
            (Some(0), Some(0)),
            (Some(-1), Some(0)),
            (Some(100), Some(-1)),
            (Some(100), Some(101)),
        ] {
            assert_eq!(percent(full, remaining), None);
        }
        assert_eq!(percent(Some(i32::MAX), Some(i32::MAX)), Some(100));
        assert_eq!(percent(Some(60_000), Some(0)), Some(0));
        assert_eq!(percent(Some(60_000), Some(59_999)), Some(99));
        for (state, present) in [
            (BatteryState::Unknown, true),
            (BatteryState::Charging, false),
        ] {
            let reading = power(
                state,
                present,
                BatterySource::Handheld,
                Some(60_000),
                Some(25_200),
                true,
            );
            assert_eq!(reading.percent, None);
            assert_eq!(reading.state, BatteryState::Unknown);
        }
    }

    #[test]
    fn gamepad_idle_and_charging_preserve_actual_capacity_percentage() {
        for state in [
            BatteryState::Idle,
            BatteryState::Charging,
            BatteryState::Discharging,
        ] {
            let reading = power(
                state,
                true,
                BatterySource::Handheld,
                Some(60_000),
                Some(25_200),
                false,
            );
            assert_eq!(reading.percent, Some(42));
            assert_eq!(reading.state, state);
        }
    }

    #[test]
    fn gamepad_wgi_coarse_levels_never_claim_an_exact_percentage() {
        for (remaining, level) in [
            (10, BatteryLevel::Critical),
            (40, BatteryLevel::Low),
            (70, BatteryLevel::Medium),
            (100, BatteryLevel::Full),
        ] {
            let reading = power(
                BatteryState::Discharging,
                true,
                BatterySource::Controller,
                Some(100),
                Some(remaining),
                false,
            );
            assert_eq!(reading.percent, None);
            assert_eq!(reading.level, Some(level));
        }
        let unknown = power(
            BatteryState::Discharging,
            true,
            BatterySource::Controller,
            Some(100),
            Some(55),
            false,
        );
        assert_eq!(unknown.percent, None);
        assert_eq!(unknown.level, None);
    }
}
