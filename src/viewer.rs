//! PlayOnline component readiness for the shared native updater.
//! FFXI's updater must not apply product-0001 files or records to product 1000.
use crate::{i18n::{t, tf}, patch::{self, PatchReport, PatchState}, updater};
use std::{fs, io::{Read, Seek, SeekFrom}, path::{Path, PathBuf}};

#[derive(Clone, Debug)]
pub(crate) struct Status {
    pub folder: Option<PathBuf>,
    pub version: PatchReport,
    pub core_error: Option<String>,
    /// `version` says Required from local evidence alone, with no server verdict: the Viewer
    /// never finished its first update, its record is older than a version a real check told
    /// (`patch::viewer_report_remembered`), or the game host's core check found an old core
    /// (`core_check_outdated`). Its update asks the server itself, as any Update does.
    pub stale: bool,
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
/// A stale Viewer's sign-in refusal (`Status::stale`; `gate`, and the window's rewording of
/// a core check that found an old core): no server verdict, so no official version to name.
/// `local_update_required` reads it back in every language, and a test keeps the two equal.
const STALE: &str = "PlayOnline update required (installed {local}).";

/// `STALE` for the installed version `local`.
pub(crate) fn stale_message(local: &str) -> String {
    tf("PlayOnline update required (installed {local}).", &[("local", &local)])
}

/// The message is a stale Viewer's refusal (`stale_message`) in any language, possibly
/// wrapped: the window runs PlayOnline's update for that sign-in, as for a W2U's "outdated",
/// except that the update asks the server for its version itself.
pub(crate) fn local_update_required(message: &str) -> bool {
    crate::i18n::Lang::ALL.into_iter().any(|lang| {
        let template = crate::i18n::t_in(lang, STALE);
        let Some((head, tail)) = template.split_once("{local}") else { return false };
        message.find(head).is_some_and(|at| message[at + head.len()..].contains(tail))
    })
}

/// The game host's core check refused polcore.dll for what an old PlayOnline core lacks: a
/// required service missing from its function table, or a table without today's shape
/// (crystal-loader `pol_core::check_table`; its text is always English). An update replaces
/// the core. A service that points outside the core's code (a patched core) is not this.
pub(crate) fn core_check_outdated(message: &str) -> bool {
    message.find("PlayOnline core check failed:").is_some_and(|at| {
        let rest = &message[at..];
        rest.contains("is missing from the function table") || rest.contains("the function table does not have PlayOnline's shape")
    })
}

/// The window's PlayOnline status after a local verdict (a core check that found an old core):
/// Required, stale, over what the window knew.
pub(crate) fn stale(known: Option<Status>) -> Status {
    let (folder, local, remote, core_error) = known.map_or_else(|| (None, t("Unavailable").to_owned(), t("Not checked").to_owned(), None),
        |status| (status.folder, status.version.local_version, status.version.remote_version, status.core_error));
    Status { folder, core_error, stale: true, version: PatchReport { state: PatchState::Required, local_version: local, remote_version: remote,
        detail: t("A PlayOnline Viewer update is required before a new sign-in. Choose Update.").into(), files_verified: None } }
}

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
    Status { folder, core_error, stale: false, version: PatchReport { state: PatchState::Required, local_version: local, remote_version: remote,
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

/// The Viewer check for a sign-in's target: US exactly as `check_at_connect_timed`; an
/// experimental EU/JP install's own Viewer folder, patch host and platform tag
/// (`retail_region`), so the server answers for that region's Viewer and never for the US
/// one. An EU/JP Viewer the server calls outdated stops the sign-in without offering
/// Crystal's update, which deploys the US Viewer's files. An install whose tag is unknown
/// is not asked about at all: the sign-in goes on, as after a check that got no answer.
pub(crate) fn check_at_connect_for(target: &crate::retail_region::Target) -> ConnectCheck {
    let Some(region) = target.experimental() else { return check_at_connect_timed() };
    let name = region.region.wire();
    let (tag, asked) = match (&region.viewer_tag, region.viewer_check()) {
        (Ok(tag), Some(asked)) => (*tag, asked),
        (tag, _) => {
            let why = tag.as_ref().err().cloned().unwrap_or_default();
            return ConnectCheck { verdict: ConnectVerdict::Unreachable(not_checked(name, &why)), timing: patch::CheckTiming::default() };
        }
    };
    let mut check = patch::viewer_connect_check_for(&region.viewer, &region.patch_host, tag);
    if let ConnectVerdict::Outdated { local, remote } = &check.verdict {
        check.verdict = ConnectVerdict::Failed(official_update_required(name, local, remote, &asked));
    }
    check
}

/// Why an experimental region's Viewer was not checked (its platform tag is unknown).
fn not_checked(region: &str, why: &str) -> String {
    tf("PlayOnline ({region}) was not checked for updates: its patch platform is unknown ({why}).", &[("region", &region), ("why", &why)])
}

/// An experimental region's "update first" refusal: not the US wording, so the window never
/// turns Play into Crystal's Update for it. `asked` names the server and tags the answer came
/// from (`pt004.pol.com:54000 W20/1000`), so a report of it shows what was compared.
pub(crate) fn official_update_required(region: &str, local: &str, remote: &str, asked: &str) -> String {
    tf("PlayOnline ({region}) needs an update (installed {local}, official {remote}; asked {asked}). Update with the official PlayOnline Viewer.",
        &[("region", &region), ("local", &local), ("remote", &remote), ("asked", &asked)])
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
/// gate use this (07c §3). The server's verdict arrives only at Connect, except
/// that a Viewer local evidence already proves out of date is refused here
/// (`Status::stale`), before any game host loads its core.
pub(crate) fn local_status() -> Status {
    match patch::install_folder(crate::updater::Product::Viewer) {
        Some(folder) => {
            let official = crate::official_versions::load(&crate::official_versions::default_path());
            local_status_in(folder, official.playonline())
        }
        None => Status { folder: None, version: unknown(t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").into()), core_error: None, stale: false },
    }
}

/// `local_status` for the Viewer in `folder`, judged against `remembered`, the newest
/// official version a real check told.
pub(crate) fn local_status_in(folder: PathBuf, remembered: Option<&crate::official_versions::Learned>) -> Status {
    let (version, stale) = patch::viewer_report_remembered(&folder, remembered);
    Status { version, stale, core_error: updater::ensure_plain(&folder).and_then(|()| core_compatibility(&folder)).err(), folder: Some(folder) }
}

fn unknown(detail: String) -> PatchReport {
    PatchReport { state: PatchState::Unknown, local_version: t("Unavailable").into(),
        remote_version: t("Not checked").into(), detail, files_verified: None }
}

pub(crate) fn unavailable(error: String) -> Status {
    Status { folder: patch::install_folder(crate::updater::Product::Viewer), version: unknown(error.clone()), core_error: Some(error), stale: false }
}

/// Read local readiness against the version already checked by the updater
/// (`None`: no server verdict; the sign-in's own W2U decides at Connect).
pub(crate) fn after_check(folder: PathBuf, remote: Option<&str>) -> Status {
    Status { version: patch::viewer_report_with_remote(&folder, remote),
        core_error: updater::ensure_plain(&folder).and_then(|()| core_compatibility(&folder)).err(),
        folder: Some(folder), stale: false }
}

/// Only new sign-ins are gated. Existing game and social sessions retain their
/// own loaded core; updates require them to exit through the ordinary controls.
pub(crate) fn gate(report: &Status) -> Result<(), String> {
    // No US Viewer: when another edition is installed, the account's region is what is wrong.
    let Some(folder) = &report.folder else {
        let installed = crate::retail_region::installed_editions(None);
        return Err(crate::retail_region::mismatch(0, &installed).unwrap_or_else(|| report.version.detail.clone()));
    };
    // A cached ready report can predate an update or interrupted transaction.
    let _update_lock = patch::check_update_state(folder)?;
    // Local evidence alone: no official version to name, and its update asks the server.
    if report.version.state == PatchState::Required && report.stale {
        return Err(stale_message(&report.version.local_version));
    }
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
/// The core checked is the edition's own (`retail_region::core_file`); none at all is said plainly.
fn core_compatibility(folder: &Path) -> Result<(), String> {
    core_timestamp(&crate::retail_region::core_file(folder)?).map(|_| ())
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
        let mut report = Status { folder: Some(root.clone()), version: unknown("offline".into()), core_error: None, stale: false };
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
    /// Live 2026-11-13: an EU Viewer folder (polcoreEU.dll, no polcore.dll) got "Cannot check
    /// PlayOnline components: ... (os error 2)". Its own core is checked; none at all is named.
    #[test]
    fn an_eu_folder_checks_its_own_core_and_a_missing_core_is_named() {
        crate::i18n::set(crate::i18n::Lang::En);
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcoreEU.dll"), core_bytes(UPDATED_CORE)).unwrap();
        fs::write(root.join("viewer/com/appEU.dll"), b"").unwrap();
        assert_eq!(core_compatibility(&root), Ok(()));
        fs::write(root.join("viewer/com/polcoreEU.dll"), b"invalid core").unwrap();
        assert!(core_compatibility(&root).unwrap_err().starts_with("Cannot check PlayOnline components:"), "a damaged EU core is still judged");
        fs::remove_file(root.join("viewer/com/polcoreEU.dll")).unwrap();
        let missing = core_compatibility(&root).unwrap_err();
        assert!(missing.starts_with("PlayOnline's core file is missing from ") && missing.ends_with("Reinstall PlayOnline or check your antivirus."), "{missing}");
        assert!(!missing.contains("os error"), "{missing}");
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

    /// Live 2026-11-12: a reinstalled PlayOnline (no patch.ver, or a record older than the
    /// version a real check remembered) is refused before any host loads its old core, with a
    /// refusal the window reads back as PlayOnline's update. Nothing is sent for it.
    #[test]
    fn an_old_or_never_updated_viewer_is_refused_locally_as_an_update() {
        use crate::official_versions::Learned;
        crate::i18n::set(crate::i18n::Lang::En);
        let root = fixture(); fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/polcore.dll"), core_bytes(LEGACY_CORE)).unwrap();
        let official = Learned { version: "20261022_0".into(), at: 1 };
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        // Never updated: refused whatever is remembered.
        for remembered in [None, Some(&official)] {
            let status = local_status_in(root.clone(), remembered);
            assert!(status.stale && status.requires_update() && status.version.state == PatchState::Required);
            let refusal = gate(&status).unwrap_err();
            assert!(local_update_required(&refusal) && outdated_versions(&refusal).is_none(), "{refusal}");
            assert!(local_update_required(&format!("Sign-in failed: {refusal}")));
        }
        let seed = patch::applied_seed_for(&root, crate::updater::Product::Viewer).unwrap();
        fs::write(root.join("patch.ver"), patch::encode_applied_record_for("20110829_E", seed, crate::updater::Product::Viewer).unwrap()).unwrap();
        let old = local_status_in(root.clone(), Some(&official));
        assert!(old.stale && old.requires_update());
        assert_eq!((old.version.local_version.as_str(), old.version.remote_version.as_str()), ("20110829_E", "20261022_0"));
        assert_eq!(gate(&old).unwrap_err(), "PlayOnline update required (installed 20110829_E).");
        // Nothing remembered, or the remembered version itself: the W2U at Connect decides.
        for remembered in [None, Some(&Learned { version: "20110829_E".into(), at: 1 })] {
            let status = local_status_in(root.clone(), remembered);
            assert!(!status.stale && status.version.state == PatchState::Unknown && gate(&status).is_ok());
        }
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()), opened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_stale_refusal_reads_back_in_every_language_and_only_old_core_failures_count() {
        use crate::i18n::{set, Lang};
        for (built, shown) in [(Lang::En, Lang::En), (Lang::Ja, Lang::En), (Lang::En, Lang::Ja), (Lang::Ja, Lang::Ja)] {
            set(built);
            let message = stale_message("20110829_E");
            set(shown);
            assert!(local_update_required(&message) && outdated_versions(&message).is_none(), "{message}");
        }
        set(Lang::En);
        for other in ["", "PlayOnline update required", &ConnectVerdict::Outdated { local: "a".into(), remote: "b".into() }.message(),
            "Game loader error during startup: PlayOnline core check failed: service 463 (PlayOnline sign-in) is missing from the function table."] {
            assert!(!local_update_required(other), "{other}");
        }
        // The owner's 1.2.6 error, and a table of another shape: an old core.
        assert!(core_check_outdated("Game loader error during startup: PlayOnline core check failed: service 463 (PlayOnline sign-in) is missing from the function table. (Windower 4 via crystal-loader)."));
        assert!(core_check_outdated("PlayOnline core check failed: the function table does not have PlayOnline's shape (40 of 300 known entries differ)."));
        for other in ["PlayOnline core check failed: service 463 (PlayOnline sign-in) does not point into the core's code.",
            "PlayOnline core check failed: its COM vtable 0x1 is not in the core's data.", "is missing from the function table"] {
            assert!(!core_check_outdated(other), "{other}");
        }
        let status = stale(None);
        assert!(status.stale && status.requires_update());
        // The reader's template is the message's key (its definition and the tf call).
        assert_eq!(include_str!("viewer.rs").matches(&format!("\"{STALE}\"")).count(), 2);
    }

    /// An EU/JP install whose platform tag is unknown is not asked about (nothing is sent)
    /// and not refused: the loader signs in, as after a check that got no answer. A real
    /// "outdated" for such a region names what was asked and never reads as Crystal's update.
    #[test]
    fn an_experimental_viewer_with_an_unknown_tag_is_not_checked_and_not_refused() {
        use crate::retail_region::{Experimental, RetailRegion, Target};
        crate::i18n::set(crate::i18n::Lang::En);
        let folder = PathBuf::from(r"C:\crystal-no-such-folder\PlayOnlineViewer");
        let mut values = Experimental { region: RetailRegion::Jp, viewer: folder.clone(), source: "test".into(), env_file: folder.join(r"data\utf\env.dat"),
            patch_host: "pt004.pol.com".into(), ucs_area: "00".into(), viewer_tag: Err("sqpolcts.bin trailer validation failed".into()) };
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        let check = check_at_connect_for(&Target::Experimental(values.clone()));
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()), opened, "nothing is sent");
        let message = check.verdict.message();
        assert_eq!(check.verdict.reason(), Some("unreachable"), "{message}");
        assert_eq!(message, "PlayOnline (JP) was not checked for updates: its patch platform is unknown (sqpolcts.bin trailer validation failed).");
        assert!(outdated_versions(&message).is_none() && !local_update_required(&message));
        // With its tag, the check is named as it is sent.
        values.viewer_tag = Ok(*b"W20\0");
        let asked = values.viewer_check().unwrap();
        assert_eq!(asked, "pt004.pol.com:54000 W20/1000");
        let refusal = official_update_required("JP", "20260929_0", "20261201_0", &asked);
        assert_eq!(refusal, "PlayOnline (JP) needs an update (installed 20260929_0, official 20261201_0; asked pt004.pol.com:54000 W20/1000). Update with the official PlayOnline Viewer.");
        for lang in crate::i18n::Lang::ALL {
            crate::i18n::set(lang);
            let refusal = official_update_required("JP", "20260929_0", "20261201_0", &asked);
            assert!(outdated_versions(&refusal).is_none() && !local_update_required(&refusal) && !patch::is_update_refusal(&refusal), "{refusal}");
        }
        crate::i18n::set(crate::i18n::Lang::En);
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
