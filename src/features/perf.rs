//! Per-client performance: process priority and CPU affinity per account, applied to its game
//! host when the game starts; optionally a lower priority while a client is in the background;
//! and muting background clients.
//!
//! What was chosen, and why (no new in-game hooks):
//! - Priority and affinity are plain process settings Windows applies to the host Crystal
//!   started (SetPriorityClass, SetProcessAffinityMask). Nothing in the game changes.
//! - Background mute uses FFXI's own setting `0035` ("play sound while the window is inactive"),
//!   served to that game in-process at launch like the other `ffxi_settings` values. It takes
//!   effect at the client's next launch; the registry and FFXI's files are never written.
//! - A background frame-rate limit is not offered: FFXI has no frame-rate setting, Windower's
//!   and Ashita's frame-rate add-ons are global (not per client, not background-aware) and live
//!   in their own config Crystal does not own, and limiting frames from outside would need a
//!   hook in the game's render loop. Lowering a background client's priority (`background`) is
//!   the safe lever Crystal has: the foreground game wins every contested core.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A process priority class. Realtime is never offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

impl Priority {
    pub(crate) const ALL: [Priority; 5] = [Self::Idle, Self::BelowNormal, Self::Normal, Self::AboveNormal, Self::High];

    fn class(self) -> u32 {
        use windows_sys::Win32::System::Threading::*;
        match self {
            Self::Idle => IDLE_PRIORITY_CLASS,
            Self::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Self::Normal => NORMAL_PRIORITY_CLASS,
            Self::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Self::High => HIGH_PRIORITY_CLASS,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::Idle => t("Idle"),
            Self::BelowNormal => t("Below normal"),
            Self::Normal => t("Normal"),
            Self::AboveNormal => t("Above normal"),
            Self::High => t("High"),
        }
    }
}

/// One account's choices.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Client {
    /// None: leave Windows' default (Normal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    /// Logical processors allowed, as a bit mask (bit 0 = CPU 0). None: all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affinity: Option<u64>,
    /// Mute this client while its window is inactive (FFXI's own `0035`, at its next launch).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mute_in_background: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// By account key.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, Client>,
    /// Background clients run at Below normal (or their own lower choice) until brought forward.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    /// Mute every background client (an account's own switch can mute it alone).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mute_background: bool,
}

impl Settings {
    pub(crate) fn client(&self, account: &str) -> Client { self.accounts.get(account).cloned().unwrap_or_default() }

    /// The priority `account`'s host should have now.
    pub(crate) fn priority_for(&self, account: &str, foreground: bool) -> Option<Priority> {
        let own = self.client(account).priority;
        if !self.background || foreground { return own; }
        // In the background: Below normal, or the account's own choice when that is lower.
        Some(match own {
            Some(Priority::Idle) => Priority::Idle,
            _ => Priority::BelowNormal,
        })
    }

    /// The values a launch of `account` serves its game for these choices (`ffxi_settings`).
    pub(crate) fn launch_values(&self, account: &str) -> Vec<(&'static str, u32)> {
        if self.mute_background || self.client(account).mute_in_background { vec![("0035", 0)] } else { Vec::new() }
    }
}

/// The machine's logical processors as a mask (the process's system mask), at most 64.
pub(crate) fn system_mask() -> u64 {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};
    let (mut process, mut system) = (0usize, 0usize);
    // SAFETY: the current-process pseudo handle and two out values.
    if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) } == 0 || system == 0 {
        return 1;
    }
    system as u64
}

/// `mask` limited to processors that exist; None when nothing would be left.
pub(crate) fn effective_mask(mask: u64, system: u64) -> Option<u64> { Some(mask & system).filter(|m| *m != 0) }

/// Apply `priority` and `affinity` to the host `pid`. Each part that fails is reported; nothing
/// else about the process changes.
pub(crate) fn apply(pid: u32, priority: Option<Priority>, affinity: Option<u64>) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::{SetPriorityClass, SetProcessAffinityMask, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION};
    if priority.is_none() && affinity.is_none() { return Ok(()); }
    let process = crate::win::process::open(pid, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION)
        .map_err(|_| crate::i18n::t("Could not change the game's priority or processors.").to_owned())?;
    let handle = process.as_raw_handle() as _;
    let mut failed = false;
    if let Some(priority) = priority {
        // SAFETY: a live handle opened with PROCESS_SET_INFORMATION.
        failed |= unsafe { SetPriorityClass(handle, priority.class()) } == 0;
    }
    if let Some(mask) = affinity {
        match effective_mask(mask, system_mask()) {
            // SAFETY: as above; the mask is non-zero and within the system mask.
            Some(mask) => failed |= unsafe { SetProcessAffinityMask(handle, mask as usize) } == 0,
            None => failed = true,
        }
    }
    if failed { Err(crate::i18n::t("Could not change the game's priority or processors.").into()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_priority_lowers_only_background_clients_and_keeps_lower_choices() {
        let mut s = Settings::default();
        s.accounts.insert("a".into(), Client { priority: Some(Priority::High), ..Default::default() });
        s.accounts.insert("b".into(), Client { priority: Some(Priority::Idle), ..Default::default() });
        assert_eq!(s.priority_for("a", false), Some(Priority::High), "off: the account's own choice");
        assert_eq!(s.priority_for("c", false), None, "nothing chosen: untouched");
        s.background = true;
        assert_eq!(s.priority_for("a", true), Some(Priority::High));
        assert_eq!(s.priority_for("a", false), Some(Priority::BelowNormal));
        assert_eq!(s.priority_for("b", false), Some(Priority::Idle));
        assert_eq!(s.priority_for("c", false), Some(Priority::BelowNormal));
        assert_eq!(s.priority_for("c", true), None);
    }

    #[test]
    fn background_mute_is_ffxis_own_inactive_sound_setting() {
        let mut s = Settings::default();
        assert!(s.launch_values("a").is_empty());
        s.accounts.insert("a".into(), Client { mute_in_background: true, ..Default::default() });
        assert_eq!(s.launch_values("a"), vec![("0035", 0)]);
        assert!(s.launch_values("b").is_empty());
        s.mute_background = true;
        assert_eq!(s.launch_values("b"), vec![("0035", 0)]);
    }

    #[test]
    fn affinity_masks_stay_within_the_machine() {
        assert_eq!(effective_mask(0b1111, 0b0011), Some(0b0011));
        assert_eq!(effective_mask(0b1100, 0b0011), None);
        assert!(system_mask() != 0);
    }

    #[test]
    fn priority_and_affinity_apply_to_a_process_crystal_started() {
        // A child this test starts (and ends): the only process it changes.
        // `set /p` waits on the piped stdin, so cmd starts no grandchild and ends with its pipe.
        let mut child = std::process::Command::new("cmd.exe").args(["/c", "set /p x="])
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).spawn().unwrap();
        let first = system_mask() & system_mask().wrapping_neg(); // lowest processor
        let result = apply(child.id(), Some(Priority::BelowNormal), Some(first));
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
        assert!(apply(child.id(), None, None).is_ok(), "nothing to apply touches nothing");
        assert!(apply(0xFFFF_FFF0, Some(Priority::Normal), None).is_err());
    }

    #[test]
    fn settings_default_to_nothing_and_save_compactly() {
        assert_eq!(serde_json::to_string(&Settings::default()).unwrap(), "{}");
        let s: Settings = serde_json::from_str(r#"{"accounts":{"a":{"priority":"AboveNormal","affinity":3}},"background":true}"#).unwrap();
        assert_eq!(s.client("a"), Client { priority: Some(Priority::AboveNormal), affinity: Some(3), mute_in_background: false });
    }
}
