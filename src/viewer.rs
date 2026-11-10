//! PlayOnline component readiness for the shared native updater.
//! FFXI's updater must not apply product-0001 files or records to product 1000.
use crate::{i18n::{t, tf}, patch::{self, PatchReport, PatchState}, updater};
use std::{fs, io::{Read, Seek, SeekFrom}, path::{Path, PathBuf}};

#[derive(Clone, Debug)]
pub(crate) struct Status {
    pub folder: Option<PathBuf>,
    pub version: PatchReport,
    pub core_error: Option<String>,
}
impl Status {
    pub fn requires_update(&self) -> bool {
        self.folder.is_none() || self.version.state == PatchState::Required || self.core_error.is_some()
    }
    pub fn retranslate(&mut self, from: crate::i18n::Lang) {
        let keys = ["Unavailable", "Not checked", "Not initialized",
            "PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.",
            "PlayOnline Viewer matches the official version.",
            "A PlayOnline Viewer update is required before a new sign-in. Choose Update.",
            "PlayOnline has not completed its initial update. Press Update to finish it.",
            "PlayOnline version has not been verified."];
        for text in [&mut self.version.local_version, &mut self.version.remote_version, &mut self.version.detail]
            .into_iter().chain(self.core_error.iter_mut()) {
            if let Some(shown) = crate::retranslated(text, from, &keys) { *text = shown.into(); }
        }
    }
}

/// The Viewer's own W2U (:54000) verdict for one Connect (pol-spec 07a
/// w2u-at-connect; 07c §3). Never cached: each sign-in asks exactly once, when
/// the loader's `native_viewer_check` arrives between Open(0) and Open(1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectVerdict {
    /// The server's version equals the installed one: Open(1) may follow.
    Current { version: String },
    /// The server reports another version, or the Viewer never completed its
    /// first update: stock updates instead of signing in.
    Outdated { local: String, remote: String },
    /// Nothing was sent: no Viewer, an unreadable record, or an update in progress.
    Failed(String),
    /// Sent, but no usable reply: connect or reply timeout, refused, or a malformed
    /// frame. The loader signs in anyway (owner 2026-10-12).
    Unreachable(String),
    /// Round 2 (pol-spec 07d §2.3): resolution or the local connect failed
    /// instantly, and kept failing through `patch::NETWORK_WAIT` of retries.
    /// Nothing reached Square Enix; the loader stops before Open(1). The text
    /// names the real cause (e.g. "pt008.pol.com: No such host is known. (11001)").
    NetworkNotReady(String),
}
impl ConnectVerdict {
    /// The `reason` on the loader pipe; `None` for `Current` (07c §1, 07d §2.3).
    pub(crate) fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Current { .. } => None,
            Self::Outdated { .. } => Some("outdated"),
            Self::Failed(_) => Some("failed"),
            Self::Unreachable(_) => Some("unreachable"),
            Self::NetworkNotReady(_) => Some("network"),
        }
    }
    /// The sign-in error the user sees for a verdict that stops the sign-in.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Current { .. } => String::new(),
            Self::Outdated { local, remote } => tf("PlayOnline update required: installed {local}, official {remote}.",
                &[("local", local), ("remote", remote)]),
            // A network failure with its real cause (08 §4 item 2: the 11001 was lost).
            Self::NetworkNotReady(cause) => tf("The network is not ready ({cause}). Check the connection, then sign in again.", &[("cause", cause)]),
            Self::Failed(error) | Self::Unreachable(error) => error.clone(),
        }
    }
}

/// `NetworkNotReady`'s sign-in error (its `tf` key; `is_network_not_ready_message` reads
/// it back in every language, and a test keeps the two equal).
const NETWORK_NOT_READY: &str = "The network is not ready ({cause}). Check the connection, then sign in again.";
/// `Outdated`'s sign-in error and `gate`'s refusal (their `tf` key; `outdated_versions`
/// reads it back in every language, and a test keeps the three equal).
const OUTDATED: &str = "PlayOnline update required: installed {local}, official {remote}.";

/// The installed and official versions in a "PlayOnline update required" sign-in error, in
/// any language and possibly wrapped (the loader's error line, a prefix): the window reads
/// the sign-in's W2U verdict into its PlayOnline status from it, so Play becomes Update.
pub(crate) fn outdated_versions(message: &str) -> Option<(String, String)> {
    crate::i18n::Lang::ALL.into_iter().find_map(|lang| {
        let template = crate::i18n::t_in(lang, OUTDATED);
        let head = template.split_once('{')?.0;
        let at = message.find(head)?;
        patch::parse_update_required_with(&message[at..], template)
    })
}

/// The window's PlayOnline status after a sign-in's W2U said "outdated": the server's
/// verdict, over what the window knew locally.
pub(crate) fn outdated(known: Option<Status>, local: String, remote: String) -> Status {
    let (folder, core_error) = known.map_or((None, None), |status| (status.folder, status.core_error));
    Status { folder, core_error, version: PatchReport { state: PatchState::Required, local_version: local, remote_version: remote,
        detail: t("A PlayOnline Viewer update is required before a new sign-in. Choose Update.").into(),
        files_verified: None } }
}

/// One W2U for one Connect with its attempts and phases (07d §2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectCheck {
    pub verdict: ConnectVerdict,
    pub timing: patch::CheckTiming,
}

/// One stock W2U for one sign-in plus the retry for "network not ready" (07d §2.3)
/// and the phases of every attempt. Blocking, at most `patch::VIEWER_CHECK_BUDGET`.
/// Called only off the UI thread, by the session worker answering the loader's
/// `native_viewer_check`; never at launcher start, on a gate, or from the window (07c §3).
pub(crate) fn check_at_connect_timed() -> ConnectCheck {
    match patch::install_folder(crate::updater::Product::Viewer) {
        Some(folder) => patch::viewer_connect_check_timed(&folder),
        None => ConnectCheck {
            verdict: ConnectVerdict::Failed(t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").into()),
            timing: patch::CheckTiming::default(),
        },
    }
}

/// The message is a "network not ready" sign-in error in any language
/// (`ConnectVerdict::NetworkNotReady::message`), as `patch::parse_update_required`
/// reads its message back. The message may be wrapped (the loader's error line, a
/// prefix). Used by the window to retry a startup sign-in once when the network
/// returns (07d §5.5).
pub(crate) fn is_network_not_ready_message(message: &str) -> bool {
    crate::i18n::Lang::ALL.into_iter().any(|lang| {
        let template = crate::i18n::t_in(lang, NETWORK_NOT_READY);
        let Some((head, tail)) = template.split_once("{cause}") else { return false };
        message.find(head).is_some_and(|at| message[at + head.len()..].contains(tail))
    })
}

/// Local readiness only: install folder, patch.ver record, updater state and
/// core compatibility. Never touches the network; the UI and every pre-launch
/// gate use this (07c §3). The server's verdict arrives only at Connect.
pub(crate) fn local_status() -> Status {
    match patch::install_folder(crate::updater::Product::Viewer) {
        Some(folder) => after_check(folder, None),
        None => Status { folder: None, version: unknown(t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").into()), core_error: None },
    }
}

fn unknown(detail: String) -> PatchReport {
    PatchReport { state: PatchState::Unknown, local_version: t("Unavailable").into(),
        remote_version: t("Not checked").into(), detail, files_verified: None }
}

pub(crate) fn unavailable(error: String) -> Status {
    Status { folder: patch::install_folder(crate::updater::Product::Viewer), version: unknown(error.clone()), core_error: Some(error) }
}

/// Read local readiness against the version already checked by the updater
/// (`None`: no server verdict; the sign-in's own W2U decides at Connect).
pub(crate) fn after_check(folder: PathBuf, remote: Option<&str>) -> Status {
    Status { version: patch::viewer_report_with_remote(&folder, remote),
        core_error: updater::ensure_plain(&folder).and_then(|()| core_compatibility(&folder)).err(),
        folder: Some(folder) }
}

/// Only new sign-ins are gated. Existing game and social sessions retain their
/// own loaded core; updates require them to exit through the ordinary controls.
pub(crate) fn gate(report: &Status) -> Result<(), String> {
    let Some(folder) = &report.folder else { return Err(report.version.detail.clone()); };
    // A cached ready report can predate an update or interrupted transaction.
    let _update_lock = patch::check_update_state(folder)?;
    if report.version.state == PatchState::Required {
        return Err(tf("PlayOnline update required: installed {local}, official {remote}.",
            &[("local", &report.version.local_version), ("remote", &report.version.remote_version)]));
    }
    if let Some(error) = &report.core_error { return Err(error.clone()); }
    // A failed network check stays unverified; it cannot claim an update or
    // override the native loader's compatibility checks.
    Ok(())
}

/// The installed core is a readable, plain 32-bit x86 PE file. Its build is
/// not judged here: the game host checks the loaded core's contract by
/// structure (crystal-loader pol_core) and names anything that fails.
fn core_compatibility(folder: &Path) -> Result<(), String> {
    core_timestamp(&folder.join("viewer/com/polcore.dll")).map(|_| ())
}

fn core_timestamp(path: &Path) -> Result<u32, String> {
    updater::ensure_plain(path)?;
    let read = || -> std::io::Result<u32> {
        let mut file = fs::File::open(path)?;
        let mut dos = [0u8; 64];
        file.read_exact(&mut dos)?;
        let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
        if &dos[..2] != b"MZ" || !(64..=1024 * 1024).contains(&offset) {
            return Err(std::io::Error::other("Invalid DOS/PE header"));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut pe = [0u8; 24];
        file.read_exact(&mut pe)?;
        if &pe[..4] != b"PE\0\0" || u16::from_le_bytes(pe[4..6].try_into().unwrap()) != 0x14c {
            return Err(std::io::Error::other("Expected a 32-bit PlayOnline core"));
        }
        Ok(u32::from_le_bytes(pe[8..12].try_into().unwrap()))
    };
    read().map_err(|e| tf("Cannot check PlayOnline components: {error}. Use official setup or Update.", &[("error", &e)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("crystal-viewer-test-{}-{}", std::process::id(),
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&root).unwrap();
        root
    }
    fn core_bytes(timestamp: u32) -> Vec<u8> {
        let mut bytes = vec![0; 88];
        bytes[..2].copy_from_slice(b"MZ"); bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0"); bytes[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
        bytes[72..76].copy_from_slice(&timestamp.to_le_bytes()); bytes
    }
    /// The 1.18.15e and 1.18.15f cores, and any later build: the launcher
    /// never refuses a core by its build stamp (the host checks its contract).
    const LEGACY_CORE: u32 = 0x4E4E_5917;
    const UPDATED_CORE: u32 = 0x6ABA_1641;

    #[test]
    fn any_plain_x86_core_build_may_start_and_missing_or_malformed_ones_cannot() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        let core = root.join("viewer/com/polcore.dll");
        let mut report = Status { folder: Some(root.clone()), version: unknown("offline".into()), core_error: None };
        for timestamp in [0x45c83316, LEGACY_CORE, UPDATED_CORE, UPDATED_CORE + 1, u32::MAX] {
            fs::write(&core, core_bytes(timestamp)).unwrap();
            report.core_error = core_compatibility(&root).err();
            assert!(!report.requires_update(), "{timestamp:08X}"); assert!(gate(&report).is_ok());
        }
        let mut wrong_machine = core_bytes(UPDATED_CORE); wrong_machine[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        for bytes in [wrong_machine, b"invalid core".to_vec()] {
            fs::write(&core, bytes).unwrap(); assert!(core_compatibility(&root).is_err());
        }
        fs::remove_file(&core).unwrap(); assert!(core_compatibility(&root).is_err());
        report.core_error = None; report.version.state = PatchState::Required;
        assert!(gate(&report).unwrap_err().contains("PlayOnline update required"));
        report.folder = None; assert!(gate(&report).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn update_readiness_reuses_the_updaters_remote_verdict() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcore.dll"), core_bytes(UPDATED_CORE)).unwrap();
        let seed = patch::applied_seed_for(&root, crate::updater::Product::Viewer).unwrap();
        fs::write(root.join("patch.ver"), patch::encode_applied_record_for("20110829_E", seed, crate::updater::Product::Viewer).unwrap()).unwrap();
        let report = after_check(root.clone(), Some("20110829_E"));
        assert_eq!(report.version.state, PatchState::Current); assert!(gate(&report).is_ok());
        let later = after_check(root.clone(), Some("20110830_0"));
        assert_eq!(later.version.state, PatchState::Required); assert!(gate(&later).is_err());
        let unknown = after_check(root.clone(), None);
        assert_eq!(unknown.version.state, PatchState::Unknown);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cached_ready_viewer_is_blocked_by_a_new_interrupted_update() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcore.dll"), core_bytes(UPDATED_CORE)).unwrap();
        let seed = patch::applied_seed_for(&root, crate::updater::Product::Viewer).unwrap();
        fs::write(root.join("patch.ver"), patch::encode_applied_record_for("20110829_E", seed, crate::updater::Product::Viewer).unwrap()).unwrap();
        let report = after_check(root.clone(), Some("20110829_E"));
        assert_eq!(report.version.state, PatchState::Current);
        assert!(gate(&report).is_ok());

        fs::create_dir(root.join(".xi-updater")).unwrap();
        let journal = root.join(".xi-updater/transaction.json");
        fs::write(&journal, b"interrupted journal fixture").unwrap();
        assert!(gate(&report).unwrap_err().contains("interrupted update"));
        fs::remove_file(journal).unwrap();
        assert!(gate(&report).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn local_readiness_never_opens_a_socket() {
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcore.dll"), core_bytes(UPDATED_CORE)).unwrap();
        let seed = patch::applied_seed_for(&root, crate::updater::Product::Viewer).unwrap();
        fs::write(root.join("patch.ver"), patch::encode_applied_record_for("20110829_E", seed, crate::updater::Product::Viewer).unwrap()).unwrap();
        // Counted per thread, so parallel network tests cannot mask an opened socket.
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        let report = after_check(root.clone(), None);
        let _ = (local_status(), gate(&report));
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()), opened);
        assert_eq!(report.version.state, PatchState::Unknown);
        assert_eq!(report.version.local_version, "20110829_E");
        assert!(gate(&report).is_ok(), "an unverified Viewer still signs in; W2U decides at Connect");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_network_not_ready_message_names_its_cause_and_reads_back_in_every_language() {
        use crate::i18n::{set, Lang};
        let cause = "pc001.pol.com: No such host is known. (11001)";
        let verdict = ConnectVerdict::NetworkNotReady(cause.into());
        set(Lang::En);
        assert_eq!(verdict.reason(), Some("network"));
        assert_eq!(verdict.message(), "The network is not ready (pc001.pol.com: No such host is known. (11001)). Check the connection, then sign in again.");
        // Built in one language, read while the other shows; also inside the loader's error line.
        for (built, shown) in [(Lang::Ja, Lang::En), (Lang::En, Lang::Ja), (Lang::Ja, Lang::Ja)] {
            set(built);
            let message = verdict.message();
            assert!(message.contains(cause), "{message}");
            set(shown);
            assert!(is_network_not_ready_message(&message), "{message}");
            assert!(is_network_not_ready_message(&format!("Sign-in failed: {message}")), "{message}");
        }
        set(Lang::En);
        for other in ["", "Could not verify PlayOnline: timed out",
            "The network is not ready", &ConnectVerdict::Outdated { local: "a".into(), remote: "b".into() }.message()] {
            assert!(!is_network_not_ready_message(other), "{other}");
        }
        // The reader's template is message()'s key (its definition and the tf call).
        assert_eq!(include_str!("viewer.rs").matches(&format!("\"{NETWORK_NOT_READY}\"")).count(), 2);
    }

    #[test]
    fn an_outdated_sign_in_reads_back_into_the_playonline_status_in_every_language() {
        use crate::i18n::{set, Lang};
        let verdict = ConnectVerdict::Outdated { local: "20110829_E".into(), remote: "20261001_0".into() };
        for (built, shown) in [(Lang::En, Lang::En), (Lang::Ja, Lang::En), (Lang::En, Lang::Ja), (Lang::Ja, Lang::Ja)] {
            set(built);
            let message = verdict.message();
            set(shown);
            for wrapped in [message.clone(), format!("Sign-in failed: {message}")] {
                assert_eq!(outdated_versions(&wrapped), Some(("20110829_E".into(), "20261001_0".into())), "{wrapped}");
            }
        }
        set(Lang::En);
        assert_eq!(outdated_versions("Could not verify PlayOnline: timed out"), None);
        let status = outdated(None, "20110829_E".into(), "20261001_0".into());
        assert!(status.requires_update());
        assert_eq!(status.version.remote_version, "20261001_0");
        // The reader's template is the message's and the gate's key.
        assert_eq!(include_str!("viewer.rs").matches(&format!("\"{OUTDATED}\"")).count(), 3);
    }

    #[test]
    #[ignore = "read-only official Viewer W2U; requires an installed PlayOnline Viewer and network"]
    fn installed_viewer_version_and_core() {
        let report = local_status();
        eprintln!("Viewer: folder={:?}; local={}; core={:?}", report.folder, report.version.local_version, report.core_error);
        let check = check_at_connect_timed();
        let verdict = check.verdict;
        eprintln!("W2U: {verdict:?} ({:?})", check.timing.phases());
        assert!(report.folder.is_some()); assert!(matches!(verdict, ConnectVerdict::Current { .. } | ConnectVerdict::Outdated { .. }));
    }
}
