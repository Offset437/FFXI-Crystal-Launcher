//! Retail version checks, shaped like the stock PlayOnline Viewer's.
//! Packet framing is implemented by updater::polp.
//! These query Square Enix directly; they never apply a patch or use credentials.
//!
//! Stock sends exactly two version requests (one POLP frame each, to port
//! `53000 + product id` of that product's patch host, reply read, connection closed;
//! hosts in `dns`: `pt008.pol.com` for the Viewer, `pc001W2U.pol.com` for FFXI):
//! * the Viewer's own check (`W2U`, product `1000`, port 54000, the Viewer's installed
//!   version) at Connect, after Open(0) and 39-62 ms before the :51240 member login
//!   (pol-spec 07a w2u-at-connect; stock 18.917 FIN → 18.979 SYN). One per sign-in,
//!   never cached, never at launcher start (`viewer_connect_check`);
//! * the game's check (`W20`, product `0001`, port 53001, FFXI's installed version) when
//!   Play is pressed, after the main menu's publish and before the game starts
//!   (07a w20-after-publish; the launcher decides when).
//!
//! Neither is queried on refresh, on a timer, or after a settings save. Both go out on
//! `stock_socket`'s Viewer-shaped socket. Empty versions are sent only for a fresh install.

use crate::i18n::{t, tf};
use crate::updater::{polp, Product};
use std::io::{Read, Write};
use std::net::TcpStream;
#[cfg(test)]
use std::net::ToSocketAddrs;
use std::path::Path;
use std::time::{Duration, Instant};

const HEADER: usize = 16;
const MAX_VERSION_FRAME: usize = 4096;
/// How long a launch waits for the patch server before starting anyway (unreachable
/// server = unverified, exactly as before; it is never a reason to refuse a launch).
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// Leads the message that stops a start because the installed version is not the server's.
pub(crate) const UPDATE_REQUIRED: &str = "Update required:";
/// `gate`'s update-required sentence (its `tf` key; a test keeps the two equal).
const UPDATE_REQUIRED_TEMPLATE: &str = "Update required: installed {local}, official {remote}.";
/// Earlier builds' translated wording of that sentence, which a session worker started
/// before a launcher update can still send (English's older "official server" is read by
/// `parse_english_update_required`).
const LEGACY_UPDATE_REQUIRED: [&str; 1] = [
    "アップデートが必要です（インストール済み：{local}、公式サーバー：{remote}）。FINAL FANTASY XIを起動する前に、［ダウンロード］でゲームフォルダーを選択し、［アップデート］（または［修復］）を選んでください。",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchState {
    Current,
    Required,
    Unknown,
    /// An update was interrupted (its journal is in the folder). Not an update: the next
    /// Play finishes or undoes it locally first (main.rs `finish_interrupted_updates`).
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct PatchReport {
    pub state: PatchState,
    pub local_version: String,
    pub remote_version: String,
    pub detail: String,
    /// None for version-only checks; an actual file operation reports its
    /// observed result without changing the installed-version launch gate.
    pub files_verified: Option<bool>,
}

/// What is installed, as the stock client reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Local {
    Installed(String),
    /// No patch.ver at all: a fresh install, the one case stock sends an empty version for.
    Fresh,
    Unreadable(String),
    /// Staged/mixed files cannot be vouched for by the previous version record.
    Updating(String),
    /// An interrupted update's journal: Play finishes or undoes it before starting.
    Interrupted(String),
}

fn read_local(install: &Path, product: Product) -> Local {
    if let Err(error) = check_recovery_state(install) {
        let journal = std::fs::symlink_metadata(install.join(".xi-updater/transaction.json")).is_ok();
        return if journal { Local::Interrupted(error) } else { Local::Updating(error) };
    }
    match std::fs::metadata(install.join("patch.ver")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Local::Fresh,
        _ => match local_version_for(install, product) {
            Ok(version) => Local::Installed(version),
            Err(error) => Local::Unreadable(error),
        },
    }
}

fn check_recovery_state(install: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(install.join(".xi-updater/transaction.json")) {
        Ok(_) => Err(t("An interrupted update must be finished before starting. Press Play to finish it, or use Repair files under Downloads → Troubleshooting.").into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(tf("Cannot check updater recovery state: {error}", &[("error", &error)])),
    }
}

/// Keep an existing updater lock read-shared while querying the version. This
/// refuses another launcher's active update before reading an older patch.ver.
/// The updater's own version request deliberately uses read_local directly.
pub(crate) fn check_update_state(install: &Path) -> Result<Option<std::fs::File>, String> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    let lock = match options.open(install.join(".xi-updater/lock")) {
        Ok(lock) => Some(lock),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(t("The installation is being updated or its updater lock is unavailable. Wait for the update to finish before starting.").into()),
    };
    check_recovery_state(install)?;
    Ok(lock)
}

fn read_local_for_launch(install: &Path, product: Product) -> (Local, Option<std::fs::File>) {
    match check_update_state(install) {
        Ok(lock) => (read_local(install, product), lock),
        Err(error) => (Local::Updating(error), None),
    }
}

/// The version text the updater puts in its own version request: the installed version,
/// empty for a fresh install. A patch.ver that cannot be decoded (the case Repair exists
/// for) also sends empty, since there is no installed version to report.
pub(crate) fn installed_for_request(install: &Path) -> String {
    installed_for_request_for(install, Product::Ffxi)
}

pub(crate) fn installed_for_request_for(install: &Path, product: Product) -> String {
    match read_local(install, product) {
        Local::Installed(version) => version,
        Local::Fresh | Local::Unreadable(_) | Local::Updating(_) | Local::Interrupted(_) => String::new(),
    }
}

/// Reads the applied-version record only. No network: the server is asked when Play
/// is pressed (`check_before_launch`) and by Check files / Update / Repair.
pub fn local_report(install: &Path) -> PatchReport {
    let local = read_local(install, Product::Ffxi);
    let installed = matches!(local, Local::Installed(_));
    // An unfinished update or a journal refuses a launch, but the applied version is still
    // in patch.ver: show it instead of a placeholder when it decodes.
    let applied = matches!(local, Local::Updating(_) | Local::Interrupted(_))
        .then(|| local_version_for(install, Product::Ffxi).ok()).flatten();
    let mut result = game_report(local, Err(t("Not checked").into()));
    if let Some(version) = applied { result.local_version = version; }
    result.remote_version = t("Not checked").into();
    if installed {
        result.detail = t("The official patch server is asked when you press Play, before FINAL FANTASY XI starts.").into();
    }
    result
}

/// The installed record against a version the official server just reported (the updater's).
pub fn report_with_remote(install: &Path, remote: &str) -> PatchReport {
    game_report(read_local(install, Product::Ffxi), Ok(remote.to_owned()))
}

fn game_report(local: Local, remote: Result<String, String>) -> PatchReport {
    let (state, local_version, detail) = match local {
        Local::Installed(version) => return report(Ok(version), remote),
        Local::Fresh => (PatchState::Required, t("Not initialized").into(),
            t("FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.").into()),
        Local::Unreadable(error) => (PatchState::Required, t("Unavailable").into(),
            tf("The installed-version record cannot be read: {error}. Use Repair files under Downloads → Troubleshooting to verify the installation and rebuild it with backups.", &[("error", &error)])),
        Local::Updating(error) => (PatchState::Required, t("Unavailable").into(), error),
        Local::Interrupted(error) => (PatchState::Interrupted, t("Unavailable").into(), error),
    };
    PatchReport { state, local_version, remote_version: remote.unwrap_or_else(|_| t("Unavailable").into()), detail,
        files_verified: None }
}
/// Keep a valid version record separate from the outcome of an actual file
/// check. A matching patch.ver alone cannot make missing/outdated files ready.
pub(crate) fn report_after_operation(install: &Path, files: &crate::updater::Report, mode: crate::updater::Mode) -> PatchReport {
    if mode == crate::updater::Mode::Check {
        // Check compared the files with the installed file list, offline: it says nothing
        // about the official version, which stays as the installed record says.
        let mut result = local_report(install);
        result.files_verified = Some(files.verified);
        if files.total == 0 && !files.verified {
            result.detail = files.summary.clone();
        } else if files.outdated + files.missing + files.modified > 0 {
            result.detail = tf("{outdated} outdated, {missing} missing and {modified} modified FFXI files. Repair files restores the official files and keeps copies of modified ones.",
                &[("outdated", &files.outdated), ("missing", &files.missing), ("modified", &files.modified)]);
        }
        return result;
    }
    let mut result = report_with_remote(install, &files.version);
    // "Verified" only when every file was read and is official: a routine update checks
    // only what it changes (sizes for the rest), so it vouches for the version, not for
    // every file, and a modification it left in place is not an official file.
    result.files_verified = if !files.verified || files.left > 0 { Some(false) }
        else if files.all_files_checked { Some(true) }
        else { None };
    if files.verified && files.left > 0 && result.state == PatchState::Current {
        result.detail = tf("{left} modified FFXI files that this update doesn't change were left in place.", &[("left", &files.left)]);
    }
    result
}

/// What the stock FFXI check decides before the game starts.
pub enum LaunchCheck {
    /// Installed version equals the server's.
    Start(PatchReport),
    /// The server's version differs from the installed one: do not start; update first.
    Update(PatchReport),
    /// Could not compare (server unreachable, reply unusable, installed version unreadable).
    /// The launch proceeds, as it did when the patch server was unreachable before.
    Unverified(PatchReport),
}

// --- round 2: "network not ready" and phases on every path (pol-spec 07d §2) ---

/// How long a W2U whose first attempt failed instantly and locally
/// (`dns::NetError::network_not_ready`) keeps retrying (08 §4 item 2: 10-15 s;
/// VPN bring-up lasted ~1-4 s before NCSI saw the Internet, 08 §1 row 8).
pub(crate) const NETWORK_WAIT: Duration = Duration::from_secs(12);
/// Between W2U retries. Each retry resolves with `dns::resolve_fresh`.
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_secs(1);
/// The stock close after the reply is ≤ 115 ms (stock_socket.rs; 07c §1.4).
#[cfg(test)]
pub(crate) const STOCK_CLOSE_MAX: Duration = Duration::from_millis(115);
/// The longest `viewer::check_at_connect_timed` can block: a retry may start just
/// before `NETWORK_WAIT` ends, resolve for up to `CHECK_TIMEOUT`, then run a full
/// `CHECK_TIMEOUT` exchange and the stock close. The loader's VIEWER_CHECK_LIMIT
/// (crystal-loader native_auth.rs) must exceed this: 35 s (07d §3;
/// `tests::the_loader_waits_out_the_whole_w2u`).
#[cfg(test)]
pub(crate) const VIEWER_CHECK_BUDGET: Duration = Duration::from_millis(12_000 + 10_000 + 10_000 + 115);

/// One connection attempt of a version check, phase by phase (`None`: not reached).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Attempt {
    /// Resolved with `dns::resolve_fresh` (DNS_QUERY_BYPASS_CACHE): every W2U retry.
    pub fresh_dns: bool,
    pub resolve: Option<Duration>,
    pub connect: Option<Duration>,
    pub reply: Option<Duration>,
    /// Reply → closesocket (W2U waits for it; W20 closes in the background).
    pub close: Option<Duration>,
    pub close_in_background: bool,
    pub error: Option<crate::dns::NetError>,
}
impl Attempt {
    /// "resolve 3 ms, connect 180 ms, reply 164 ms, close 104 ms" (round 1's text),
    /// or the phases reached then "failed: <cause>" (07d §2.4).
    fn text(&self) -> String {
        let mut parts = Vec::new();
        for (name, phase) in [("resolve", self.resolve), ("connect", self.connect), ("reply", self.reply)] {
            if let Some(phase) = phase { parts.push(format!("{name} {} ms", phase.as_millis())); }
        }
        if self.close_in_background { parts.push("close in the background".into()); }
        else if let Some(close) = self.close { parts.push(format!("close {} ms", close.as_millis())); }
        if let Some(error) = &self.error { parts.push(format!("failed: {}", error.describe())); }
        parts.join(", ")
    }
}

/// Every attempt of one W2U or W20, for the launch log on every path (07d §2.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CheckTiming {
    pub attempts: Vec<Attempt>,
    /// Time spent waiting for the network between attempts (0 without a retry).
    pub network_wait: Duration,
}
impl CheckTiming {
    /// The launch-log text. One clean attempt keeps the round-1 wording
    /// ("resolve 3 ms, connect 180 ms, reply 164 ms, close 104 ms"); several read
    /// "attempt 1: …; attempt 2 (fresh DNS): …; waited 2.0 s for the network" (07d §2.4).
    /// `None` when nothing was attempted (no request was sent).
    pub(crate) fn phases(&self) -> Option<String> {
        match self.attempts.as_slice() {
            [] => None,
            [only] => Some(only.text()),
            attempts => {
                let mut text = attempts.iter().enumerate()
                    .map(|(i, attempt)| format!("attempt {}{}: {}", i + 1, if attempt.fresh_dns { " (fresh DNS)" } else { "" }, attempt.text()))
                    .collect::<Vec<_>>().join("; ");
                if !self.network_wait.is_zero() {
                    text.push_str(&format!("; waited {:.1} s for the network", self.network_wait.as_secs_f64()));
                }
                Some(text)
            }
        }
    }
}

/// Sends the stock FFXI check (`W20`/`0001`, FFXI's installed version, port 53001) and
/// reads the reply. Blocking, at most `CHECK_TIMEOUT`. Returns at the reply: the stock
/// close follows ~0.1 s later on its own thread, long before FFXI's first contact
/// (:54001 comes seconds after GameStart), so the start no longer waits for it.
/// The phases come back so a caller on any thread (main.rs xi-start-game) can log them
/// (08 §4 item 6). The W20 never retries (07d §2.3): one attempt, its failure is
/// Unverified as before.
pub(crate) fn check_before_launch_timed(install: &Path) -> (LaunchCheck, CheckTiming) {
    check_before_launch_at(install, Product::Ffxi.host())
}

/// `check_before_launch_timed` for the account's region (`retail_region::game_patch_host`):
/// the same `W20`/`0001` frame to the host name that region's Viewer looks up (US and EU
/// `pc001W2U.pol.com`, Japan `pc001W20.pol.com`). Only the US name's answer is remembered
/// (`official_versions`): the Downloads page shows the US install.
pub(crate) fn check_before_launch_at(install: &Path, host: &str) -> (LaunchCheck, CheckTiming) {
    if host != Product::Ffxi.host() {
        return ffxi_check(install, host, Product::Ffxi.port(), &mut RegionNet(host));
    }
    let (check, timing) = ffxi_check(install, host, Product::Ffxi.port(), &mut SystemNet);
    remember_game_reply(install, &check);
    // An update follows this refusal in the same session (proposal 09 phase 2): its first
    // connection keeps stock's FIN → SYN gap from this W20's FIN. The game is not starting, so
    // waiting for that FIN (≤ 115 ms after the reply) costs nothing.
    if matches!(check, LaunchCheck::Update(_)) {
        if let Some(at) = last_w20_close(Duration::from_millis(500)) {
            crate::updater::note_patch_closed(crate::updater::Product::Ffxi, at);
        }
    }
    (check, timing)
}

/// The version the server just reported is remembered (`official_versions`), so the
/// Downloads page can show it later without asking again. Only a reply that was parsed
/// counts: an unreachable server or an unreadable local record leaves the memory alone.
fn remember_game_reply(install: &Path, check: &LaunchCheck) {
    if let LaunchCheck::Start(report) | LaunchCheck::Update(report) = check {
        crate::official_versions::learn_game(install, &report.remote_version);
    }
}

/// The check and the gate in one, for a start that has no use for the report, with its
/// phases (session_worker::launch_command, 07d §5.3).
pub(crate) fn gate_before_start_timed(install: &Path) -> (Result<String, String>, CheckTiming) {
    let (check, timing) = check_before_launch_timed(install);
    (gate(check), timing)
}

/// `gate_before_start_timed` for a Japanese install: the same check sent to Japan's name for
/// the game's patch host (`dns::GAME_PATCH_HOST_JP`).
pub(crate) fn gate_before_start_timed_jp(install: &Path) -> (Result<String, String>, CheckTiming) {
    let (check, timing) = check_before_launch_at(install, crate::dns::GAME_PATCH_HOST_JP);
    (gate(check), timing)
}

fn ffxi_check(install: &Path, host: &str, port: u16, net: &mut dyn Net) -> (LaunchCheck, CheckTiming) {
    let mut timing = CheckTiming::default();
    let check = check_with(install, |request| {
        let (result, attempt) = exchange_attempt(|_| net.resolve(host, port), request, CHECK_TIMEOUT, false, false);
        timing.attempts.push(attempt);
        result
    });
    (check, timing)
}

/// The DNS, sleep and cache seams of a version check. Production uses `SystemNet`;
/// tests inject fakes and never resolve for real (07d §6 S2).
pub(crate) trait Net {
    /// Attempt 1 of either check: the system resolver and its cache, as round 1, asking for
    /// A records only as stock does (`dns::resolve_a`).
    fn resolve(&mut self, host: &str, port: u16) -> Result<Vec<std::net::SocketAddrV4>, crate::dns::NetError> {
        crate::dns::resolve_a(host, port)
            .map_err(|e| crate::dns::NetError::from_io(crate::dns::NetStage::Resolve, &e, Duration::ZERO))
    }
    /// W2U retries: `dns::resolve_fresh` (DNS_QUERY_BYPASS_CACHE).
    fn resolve_fresh(&mut self, host: &str, limit: Duration) -> Result<Vec<std::net::Ipv4Addr>, crate::dns::NetError> {
        crate::dns::resolve_fresh(host, limit)
    }
    fn sleep(&mut self, pause: Duration) { std::thread::sleep(pause) }
    fn elapsed(&self, since: Instant) -> Duration { since.elapsed() }
    /// After a retried W2U: the 07d §2.1 negative-cache rule (`dns::forget`).
    fn forget_negative(&mut self) { crate::dns::forget(&[crate::dns::VIEWER_PATCH_HOST, crate::dns::GAME_PATCH_HOST]) }
}
pub(crate) struct SystemNet;
impl Net for SystemNet {}

/// Why a W2U has no reply to judge.
enum Unanswered {
    /// Sent or attempted with no usable reply: the sign-in goes on (Unreachable).
    Error(String),
    /// Every attempt failed locally through `NETWORK_WAIT`: the cause ("pt008.pol.com:
    /// No such host is known. (11001)"). Nothing reached Square Enix.
    NetworkNotReady(String),
}

/// The W2U exchange with the 07d §2.3 retry. Attempt 1 is round 1's (system resolver,
/// stock socket, `limit`). Only when it failed instantly and locally
/// (`network_not_ready`) does it retry, `RETRY_BACKOFF` apart for up to `NETWORK_WAIT`,
/// each retry resolving with `resolve_fresh` and connecting to those addresses. A retry
/// that fails at resolution or with a local connect code keeps waiting; any other outcome
/// ends the loop, so at most one attempt ever reaches Square Enix. A timeout, a refused
/// connect or a reply error never retries (owner: an unanswered W2U does not stop sign-in).
fn w2u_exchange(host: &str, port: u16, request: &[u8], limit: Duration, net: &mut dyn Net, timing: &mut CheckTiming) -> Result<Vec<u8>, Unanswered> {
    let started = Instant::now();
    let (first, attempt) = exchange_attempt(|_| net.resolve(host, port), request, limit, true, false);
    let mut last = attempt.error.clone();
    timing.attempts.push(attempt);
    if !last.as_ref().is_some_and(|error| first.is_err() && error.network_not_ready()) {
        return first.map_err(Unanswered::Error);
    }
    let outcome = loop {
        let waited = net.elapsed(started);
        if waited >= NETWORK_WAIT { break None; }
        // The last attempt starts by NETWORK_WAIT, so the whole check stays within
        // VIEWER_CHECK_BUDGET (07d §3).
        let pause = RETRY_BACKOFF.min(NETWORK_WAIT - waited);
        net.sleep(pause);
        timing.network_wait += pause;
        let (result, attempt) = exchange_attempt(|limit| net.resolve_fresh(host, limit)
            .map(|found| found.into_iter().map(|ip| std::net::SocketAddrV4::new(ip, port)).collect()), request, limit, true, true);
        let error = attempt.error.clone();
        timing.attempts.push(attempt);
        match error {
            Some(error) if result.is_err() && error.local_failure() => last = Some(error),
            _ => break Some(result),
        }
    };
    // The first attempt's cached lookup during the outage may have left a negative
    // answer for the patch hosts (07d §2.1).
    net.forget_negative();
    match outcome {
        Some(result) => result.map_err(Unanswered::Error),
        None => Err(Unanswered::NetworkNotReady(format!("{host}: {}",
            last.map_or_else(|| t("no address").to_owned(), |error| error.describe())))),
    }
}

fn check_with(install: &Path, send: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>) -> LaunchCheck {
    let (local, _update_lock) = read_local_for_launch(install, Product::Ffxi);
    let remote = match &local {
        Local::Installed(version) => send(&Product::Ffxi.request(version)).and_then(|frame| parse_reply(&frame)),
        Local::Fresh => send(&Product::Ffxi.request("")).and_then(|frame| parse_reply(&frame)),
        // Never fall back to an anonymous request for a record that cannot be read.
        Local::Unreadable(error) | Local::Updating(error) | Local::Interrupted(error) => Err(tf("No version request sent: {error}", &[("error", error)])),
    };
    let verdict: fn(PatchReport) -> LaunchCheck = match (&local, &remote) {
        (Local::Installed(l), Ok(r)) if l == r => LaunchCheck::Start,
        (Local::Installed(_), Err(_)) => LaunchCheck::Unverified,
        _ => LaunchCheck::Update,
    };
    verdict(game_report(local, remote))
}

/// Turns a check into the gate every game start passes: `Ok(note)` to start (the note is
/// for the status line and log), `Err(message)` to stop and send the user to Update.
pub fn gate(check: LaunchCheck) -> Result<String, String> {
    match check {
        LaunchCheck::Start(report) => Ok(tf(
            "Game version {version} matches the official server.",
            &[("version", &report.local_version)],
        )),
        // A refusal sent no request (unreadable, locked or interrupted record): its own cause,
        // never the parseable text, which would claim the server named a newer version.
        // A fresh install whose request got no reply keeps the parseable text (and the Update
        // button): Update is how it finishes, whatever the server names.
        LaunchCheck::Update(report) if !Product::Ffxi.valid_stamp(&report.remote_version)
            && !is_not_initialized(&report.local_version) => Err(report.detail),
        // Starts with `UPDATE_REQUIRED`; `parse_update_required` reads it back.
        // The main button turns into Update from the report this carries.
        LaunchCheck::Update(report) => Err(tf(
            "Update required: installed {local}, official {remote}.",
            &[("local", &report.local_version), ("remote", &report.remote_version)],
        )),
        LaunchCheck::Unverified(report) => Ok(tf(
            "Game version could not be compared with the official server ({reason}). Starting anyway.",
            &[("reason", &report.detail.trim_end_matches('.'))],
        )),
    }
}

/// The report the interface shows for `gate`'s outcome (`Err` is an update-required message).
pub fn parse_update_required(message: &str) -> Option<PatchReport> {
    // `gate` words the message in the language of the process that ran the
    // check (the window or a session worker started before a language change),
    // so every language's wording is tried, not only the one showing now.
    let (local, remote) = parse_english_update_required(message).or_else(|| {
        crate::i18n::Lang::ALL
            .into_iter()
            .map(|lang| crate::i18n::t_in(lang, UPDATE_REQUIRED_TEMPLATE))
            .chain(LEGACY_UPDATE_REQUIRED)
            .find_map(|template| parse_update_required_with(message, template))
    })?;
    // Only a version the server named is an update to show (an older worker may still word a
    // refusal that sent no request this way, with placeholders), or a fresh install, which
    // Update finishes even when the server did not answer.
    let fresh = is_not_initialized(&local);
    if !Product::Ffxi.valid_stamp(&remote) && !fresh { return None; }
    let detail = if fresh {
        t("FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.")
    } else {
        t("A newer official version is available.")
    };
    Some(PatchReport {
        state: PatchState::Required,
        local_version: local,
        remote_version: remote,
        detail: detail.into(),
        files_verified: None,
    })
}

/// `game_report`'s local version for a fresh install, in any language (a worker words it in
/// the language it started with).
fn is_not_initialized(local: &str) -> bool {
    local == "Not initialized"
        || crate::i18n::Lang::ALL.into_iter().any(|lang| crate::i18n::t_in(lang, "Not initialized") == local)
}

fn parse_english_update_required(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix(UPDATE_REQUIRED)?.trim_start().strip_prefix("installed ")?;
    // Older builds wrote "official server"; a session worker can outlive an update.
    let (local, rest) = rest.split_once(", official server ").or_else(|| rest.split_once(", official "))?;
    let (remote, _) = rest.split_once('.')?;
    // `remote` ends at the first '.', and versions contain none.
    Some((local.into(), remote.into()))
}

/// Reads the two versions out of a message built from `template` (its `{local}`
/// and `{remote}` placeholders), so a translated message parses as well. The
/// remote version ends at the first character the template puts after it.
/// The placeholders may come in either order.
pub(crate) fn parse_update_required_with(message: &str, template: &str) -> Option<(String, String)> {
    let (local_at, remote_at) = (template.find("{local}")?, template.find("{remote}")?);
    let (first, second) = if local_at < remote_at { ("{local}", "{remote}") } else { ("{remote}", "{local}") };
    let (head, rest) = template.split_once(first)?;
    let (middle, tail) = rest.split_once(second)?;
    let (one, after) = message.strip_prefix(head)?.split_once(middle)?;
    let two = match tail.chars().next() {
        Some(end) => after.split_once(end)?.0,
        None => after,
    };
    let (local, remote) = if local_at < remote_at { (one, two) } else { (two, one) };
    (!local.is_empty() && !remote.is_empty()).then(|| (local.to_owned(), remote.to_owned()))
}

/// The Viewer's W2U at Connect (pol-spec 07a w2u-at-connect; 07c §3): exactly
/// the stock 88-byte `W2U`/`1000` frame with the applied version from
/// patch.ver, on a stock-shaped socket (64 KB SO_RCVBUF/SO_SNDBUF before
/// connect, delayed ACK kept, FIN 100-115 ms after the reply), then the verdict.
/// Returns only after the connection is closed. Never cached. With the 07d §2.3
/// retry for "network not ready" and the phases of every attempt.
pub(crate) fn viewer_connect_check_timed(dir: &Path) -> crate::viewer::ConnectCheck {
    let check = viewer_connect_net(dir, Product::Viewer.host(), Product::Viewer.port(), CHECK_TIMEOUT, &mut SystemNet);
    // The server's answer is remembered for the Downloads page (`official_versions`).
    match &check.verdict {
        crate::viewer::ConnectVerdict::Current { version } => crate::official_versions::learn_playonline(version),
        crate::viewer::ConnectVerdict::Outdated { remote, .. } => crate::official_versions::learn_playonline(remote),
        _ => {}
    }
    check
}

/// An experimental EU/JP sign-in's Viewer check: the stock frame with that install's own
/// platform tag (`W2U` for an EU Viewer, `W20` for a Japanese one: `retail_region`), title
/// `1000` and the Viewer's applied version, to that install's own `PATCH_SERVER_DOMAIN`. The
/// answer is not remembered: the Downloads page shows the US Viewer.
pub(crate) fn viewer_connect_check_for(dir: &Path, host: &str, platform: [u8; 4]) -> crate::viewer::ConnectCheck {
    let tags = crate::updater::polp::Tags { platform, title: Product::Viewer.tags().title };
    let mut timing = CheckTiming::default();
    let mut net = RegionNet(host);
    let verdict = viewer_verdict(dir, &tags, |request| w2u_exchange(host, Product::Viewer.port(), request, CHECK_TIMEOUT, &mut net, &mut timing));
    crate::viewer::ConnectCheck { verdict, timing }
}

/// `SystemNet` that also forgets a region's own patch host after a retried W2U.
struct RegionNet<'a>(&'a str);
impl Net for RegionNet<'_> {
    fn forget_negative(&mut self) { crate::dns::forget(&[crate::dns::VIEWER_PATCH_HOST, crate::dns::GAME_PATCH_HOST, self.0]) }
}

fn viewer_connect_net(dir: &Path, host: &str, port: u16, limit: Duration, net: &mut dyn Net) -> crate::viewer::ConnectCheck {
    let mut timing = CheckTiming::default();
    let verdict = viewer_verdict(dir, &Product::Viewer.tags(), |request| w2u_exchange(host, port, request, limit, net, &mut timing));
    crate::viewer::ConnectCheck { verdict, timing }
}

#[cfg(test)]
pub(crate) fn viewer_connect_at(dir: &Path, server: &str) -> crate::viewer::ConnectVerdict {
    let (host, port) = server.rsplit_once(':').unwrap();
    viewer_connect_net(dir, host, port.parse().unwrap(), CHECK_TIMEOUT, &mut SystemNet).verdict
}

#[cfg(test)]
fn viewer_connect_with(dir: &Path, send: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>) -> crate::viewer::ConnectVerdict {
    viewer_verdict(dir, &Product::Viewer.tags(), |request| send(request).map_err(Unanswered::Error))
}

/// The Viewer's version frame (`tags`: `W2U`/`1000` for the US Viewer) with the Viewer's own
/// installed version and record key. The updater's read-shared lock is held across the
/// exchange, as for W20.
fn viewer_verdict(dir: &Path, tags: &crate::updater::polp::Tags, send: impl FnOnce(&[u8]) -> Result<Vec<u8>, Unanswered>) -> crate::viewer::ConnectVerdict {
    use crate::viewer::ConnectVerdict;
    let (local, _update_lock) = read_local_for_launch(dir, Product::Viewer);
    let request = match &local {
        Local::Installed(version) => crate::updater::polp::version(tags, version.as_bytes()),
        Local::Fresh => crate::updater::polp::version(tags, b""),
        // Nothing is sent for a record that cannot be vouched for (never an empty request).
        Local::Updating(error) | Local::Interrupted(error) => return ConnectVerdict::Failed(error.clone()),
        Local::Unreadable(error) => return ConnectVerdict::Failed(tf("Could not verify PlayOnline: {error}", &[("error", error)])),
    };
    let reply = match send(&request) {
        Err(Unanswered::NetworkNotReady(cause)) => return ConnectVerdict::NetworkNotReady(cause),
        Err(Unanswered::Error(error)) => Err(error),
        Ok(frame) => parse_reply_for(&frame, true),
    };
    match (local, reply) {
        (_, Err(error)) => ConnectVerdict::Unreachable(tf("Could not verify PlayOnline: {error}", &[("error", &error)])),
        (Local::Installed(version), Ok(remote)) if version == remote => ConnectVerdict::Current { version },
        (Local::Installed(local), Ok(remote)) => ConnectVerdict::Outdated { local, remote },
        // A fresh Viewer updates before it can sign in.
        (_, Ok(remote)) => ConnectVerdict::Outdated { local: t("Not initialized").into(), remote },
    }
}

/// Reuse the shared updater's validated version reply, including after a
/// partial/cancelled transaction, without starting a second network request.
pub(crate) fn viewer_report_with_remote(dir: &Path, remote: Option<&str>) -> PatchReport {
    viewer_report(read_local(dir, Product::Viewer), remote
        .filter(|version| Product::Viewer.valid_stamp(version)).map(|version| Ok(version.to_owned()))
        .unwrap_or_else(|| Err(t("Not checked").into())))
}

/// The Viewer's report with no request, judged against `remembered`, the newest official
/// version a real check told (`official_versions`). `(report, true)` when it surely needs
/// an update before a sign-in: it never finished its first update (no patch.ver; stock's
/// W2U answers outdated for an empty version), or its record is older than `remembered`
/// (an official version never goes back). Otherwise `viewer_report_with_remote(dir, None)`.
pub(crate) fn viewer_report_remembered(dir: &Path, remembered: Option<&crate::official_versions::Learned>) -> (PatchReport, bool) {
    let local = read_local(dir, Product::Viewer);
    let stale = match &local {
        Local::Fresh => true,
        Local::Installed(version) => crate::official_versions::compare(version, remembered) == Some(std::cmp::Ordering::Less),
        Local::Unreadable(_) | Local::Updating(_) | Local::Interrupted(_) => false,
    };
    if !stale { return (viewer_report(local, Err(t("Not checked").into())), false); }
    let official = remembered.map(|learned| learned.version.clone()).filter(|version| Product::Viewer.valid_stamp(version));
    let mut report = viewer_report(local, Ok(official.clone().unwrap_or_default()));
    if official.is_none() { report.remote_version = t("Not checked").into(); }
    (report, true)
}

fn viewer_report(local: Local, remote: Result<String, String>) -> PatchReport {
    let state = match (&local, &remote) {
        (Local::Updating(_), _) => PatchState::Required,
        (Local::Interrupted(_), _) => PatchState::Interrupted,
        (Local::Installed(l), Ok(r)) if l == r => PatchState::Current,
        (Local::Installed(_) | Local::Fresh, Ok(_)) => PatchState::Required,
        _ => PatchState::Unknown,
    };
    let detail = match (&local, &remote, state) {
        (Local::Updating(error) | Local::Interrupted(error), _, _) => error.clone(),
        (_, _, PatchState::Current) => t("PlayOnline Viewer matches the official version.").into(),
        (Local::Fresh, _, PatchState::Required) => t("PlayOnline has not completed its initial update. Press Update to finish it.").into(),
        (_, _, PatchState::Required) => t("A PlayOnline Viewer update is required before a new sign-in. Choose Update.").into(),
        (_, Err(error), _) => tf("Could not verify PlayOnline: {error}", &[("error", error)]),
        _ => t("PlayOnline version has not been verified.").into(),
    };
    PatchReport {
        state,
        local_version: match local {
            Local::Installed(v) => v,
            Local::Fresh => t("Not initialized").into(),
            Local::Unreadable(_) | Local::Updating(_) | Local::Interrupted(_) => t("Unavailable").into(),
        },
        remote_version: remote.unwrap_or_else(|_| t("Unavailable").into()),
        detail,
        files_verified: None,
    }
}

/// One request, one reply, then close like the Viewer's patcher (`stock_socket`: 64 KB
/// buffers before connect, FIN 100-115 ms after the last reply byte). The deadline
/// covers connect, send and receive; the close delay comes after it, and this returns
/// only once the socket is closed. A failed exchange closes at once.
#[cfg(test)]
fn exchange(server: &str, request: &[u8], limit: Duration) -> Result<Vec<u8>, String> {
    exchange_closing(server, request, limit, true).0
}

/// `exchange`; `wait_close: false` returns at the reply and leaves the stock close to a
/// short-lived thread. Returns the attempt's phases too.
#[cfg(test)]
fn exchange_closing(server: &str, request: &[u8], limit: Duration, wait_close: bool) -> (Result<Vec<u8>, String>, Attempt) {
    exchange_attempt(|_| server.to_socket_addrs().map(|found| found.filter_map(|address| match address { std::net::SocketAddr::V4(v4) => Some(v4), _ => None }).collect())
        .map_err(|e| crate::dns::NetError::from_io(crate::dns::NetStage::Resolve, &e, Duration::ZERO)), request, limit, wait_close, false)
}

/// One attempt of `exchange`, phase by phase: `resolve` (given the attempt's limit)
/// yields the addresses to try in order. Returns the user-facing error with the
/// attempt's phases and its `dns::NetError`.
fn exchange_attempt(resolve: impl FnOnce(Duration) -> Result<Vec<std::net::SocketAddrV4>, crate::dns::NetError>,
    request: &[u8], limit: Duration, wait_close: bool, fresh_dns: bool) -> (Result<Vec<u8>, String>, Attempt) {
    let mut attempt = Attempt { fresh_dns, ..Attempt::default() };
    match run_attempt(resolve, request, limit, wait_close, &mut attempt) {
        Ok(frame) => (Ok(frame), attempt),
        Err((error, shown)) => {
            attempt.error = Some(error);
            (Err(shown), attempt)
        }
    }
}

fn run_attempt(resolve: impl FnOnce(Duration) -> Result<Vec<std::net::SocketAddrV4>, crate::dns::NetError>,
    request: &[u8], limit: Duration, wait_close: bool, attempt: &mut Attempt) -> Result<Vec<u8>, (crate::dns::NetError, String)> {
    use crate::dns::{NetError, NetStage};
    let started = Instant::now();
    let addresses = resolve(limit);
    let resolved = Instant::now();
    // The exchange gets its whole `limit` from the answer: a late fresh resolution (DNS
    // retransmits during a network bring-up) never leaves an aborted or absent :54000
    // connect in front of Open(1). The resolve is bounded by its own `limit`.
    let deadline = resolved + limit;
    let left = || deadline.saturating_duration_since(Instant::now());
    attempt.resolve = Some(resolved - started);
    let addresses = match addresses {
        Ok(addresses) if !addresses.is_empty() => addresses,
        Ok(_) => {
            let error = NetError { stage: NetStage::Resolve, code: None, text: t("no address").into(), after: resolved - started };
            return Err((error, tf("Cannot find the official patch server: {error}", &[("error", &t("no address"))])));
        }
        Err(mut error) => {
            error.after = resolved - started;
            let shown = tf("Cannot find the official patch server: {error}", &[("error", &error.describe())]);
            return Err((error, shown));
        }
    };
    let mut stream = None;
    let mut failure: Option<NetError> = None;
    for address in addresses {
        if left().is_zero() {
            break;
        }
        match crate::stock_socket::connect(&address, left()) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(e) => {
                let error = NetError::from_io(NetStage::Connect, &e, Duration::ZERO);
                // A failure that may have reached the server (timeout, refused) is never
                // replaced by a later local one: only all-local failures may retry.
                if failure.as_ref().is_none_or(|kept| kept.local_failure()) { failure = Some(error); }
            }
        }
    }
    let connected = Instant::now();
    attempt.connect = Some(connected - resolved);
    let Some(mut stream) = stream else {
        let mut error = failure.unwrap_or_else(|| NetError::from_io(NetStage::Connect, &std::io::ErrorKind::TimedOut.into(), Duration::ZERO));
        error.after = connected - resolved;
        let shown = tf("Cannot connect to official patch server: {error}", &[("error", &error.describe())]);
        return Err((error, shown));
    };
    let failed = |stage: NetStage, e: &std::io::Error, shown: String| (NetError::from_io(stage, e, connected.elapsed()), shown);
    if let Err(e) = stream.set_write_timeout(Some(left().max(Duration::from_millis(1)))).and_then(|_| stream.write_all(request)) {
        return Err(failed(NetStage::Send, &e, tf("Cannot send patch version request: {error}", &[("error", &e)])));
    }
    let mut header = [0u8; HEADER];
    let read = read_exact_by(&mut stream, &mut header, deadline);
    attempt.reply = Some(connected.elapsed());
    if let Err(e) = read {
        return Err(failed(NetStage::Reply, &e, tf("Cannot read patch version reply: {error}", &[("error", &e)])));
    }
    let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    if !(HEADER..=MAX_VERSION_FRAME).contains(&len) {
        let shown: String = t("Official server returned an invalid version frame length").into();
        return Err((NetError { stage: NetStage::Reply, code: None, text: shown.clone(), after: connected.elapsed() }, shown));
    }
    let mut frame = vec![0; len];
    frame[..HEADER].copy_from_slice(&header);
    let read = read_exact_by(&mut stream, &mut frame[HEADER..], deadline);
    let replied = Instant::now();
    attempt.reply = Some(replied - connected);
    if let Err(e) = read {
        return Err(failed(NetStage::Reply, &e, tf("Incomplete patch version reply: {error}", &[("error", &e)])));
    }
    if wait_close {
        crate::stock_socket::close_after_reply(stream, replied);
        attempt.close = Some(replied.elapsed());
    } else {
        // A failed spawn closes at once (the stream drops with the closure).
        background_close_started();
        let spawned = std::thread::Builder::new().name("patch-close".into())
            .spawn(move || {
                crate::stock_socket::close_after_reply(stream, replied);
                background_close_done(Instant::now());
            });
        if spawned.is_err() { background_close_done(Instant::now()); }
        attempt.close_in_background = true;
    }
    Ok(frame)
}

/// W20 connections still closing on their own thread (`run_attempt`), and when the last one
/// closed (its FIN). An update a Play's W20 asked for opens its first connection
/// `updater::NEXT_CONNECTION_GAP` after that FIN, as stock opens its prompt connection.
static W20_CLOSES: (std::sync::Mutex<(usize, Option<Instant>)>, std::sync::Condvar) =
    (std::sync::Mutex::new((0, None)), std::sync::Condvar::new());

fn background_close_started() {
    W20_CLOSES.0.lock().unwrap_or_else(|e| e.into_inner()).0 += 1;
}

fn background_close_done(at: Instant) {
    let mut closes = W20_CLOSES.0.lock().unwrap_or_else(|e| e.into_inner());
    closes.0 = closes.0.saturating_sub(1);
    closes.1 = Some(closes.1.map_or(at, |known| known.max(at)));
    W20_CLOSES.1.notify_all();
}

/// When this process's last W20 closed, once every W20 still closing has (at most `limit`).
pub(crate) fn last_w20_close(limit: Duration) -> Option<Instant> {
    let deadline = Instant::now() + limit;
    let mut closes = W20_CLOSES.0.lock().unwrap_or_else(|e| e.into_inner());
    while closes.0 > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() { break; }
        closes = W20_CLOSES.1.wait_timeout(closes, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    closes.1
}

/// Where a session worker or the batch coordinator leaves the FIN of a W20 that asked for an
/// update, for the window's update to space its first connection from (`take_update_close`).
const UPDATE_CLOSE_FILE: &str = "w20-closed.json";
/// An older note is not this Play's.
const UPDATE_CLOSE_FRESH: Duration = Duration::from_secs(120);

/// After a W20 that asked for an update in this process: leave its FIN in the account's
/// `directory` for the window (another process) that runs the update. Local only; best effort.
pub(crate) fn save_update_close(directory: &Path) {
    let Some(at) = last_w20_close(Duration::from_millis(500)) else { return };
    let Some(stamp) = crate::launch_timing::MonotonicStamp::capture(at) else { return };
    if let Ok(bytes) = serde_json::to_vec(&stamp) { let _ = std::fs::write(directory.join(UPDATE_CLOSE_FILE), bytes); }
}

/// The W20 FIN `save_update_close` left in `directory` (and remove it), when it is recent.
pub(crate) fn take_update_close(directory: &Path) -> Option<Instant> {
    let path = directory.join(UPDATE_CLOSE_FILE);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let stamp: crate::launch_timing::MonotonicStamp = serde_json::from_slice(&bytes).ok()?;
    let at = crate::launch_timing::MonotonicStamp::restore(Some(stamp), 0);
    (at.elapsed() < UPDATE_CLOSE_FRESH).then_some(at)
}

/// Whether `message` is a W20 refusal (`gate`'s update-required text).
pub(crate) fn is_update_refusal(message: &str) -> bool {
    parse_update_required(message).is_some()
}

/// The official version a Play's refusal (`report`) asks the game folder to update to, when an
/// update can follow in the same session: the server named a valid version, and the folder is
/// installed at another one (or freshly, without a record). `None` when no in-session update
/// applies (no reply, an unreadable or locked record, or the folder is already that version).
pub(crate) fn in_session_update(install: &Path, report: &PatchReport) -> Option<String> {
    if report.state != PatchState::Required || !Product::Ffxi.valid_stamp(&report.remote_version) { return None; }
    match read_local(install, Product::Ffxi) {
        Local::Installed(version) if version != report.remote_version => Some(report.remote_version.clone()),
        Local::Fresh => Some(report.remote_version.clone()),
        _ => None,
    }
}

fn read_exact_by(stream: &mut TcpStream, mut buffer: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(left))?;
        match stream.read(buffer) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buffer = &mut buffer[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(std::io::ErrorKind::TimedOut.into())
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Locate the installed product on this PC: registry entries first, with validated local
/// evidence as fallback.
pub(crate) fn install_folder(product: Product) -> Option<std::path::PathBuf> {
    crate::discovery::find(product.discovery(), None)
}

/// Keep a valid selected installation, otherwise discover it on this PC.
/// Markers identify the game folder even when its first update has not run or
/// a binary is missing and needs repair. The version record is checked separately.
pub(crate) fn find_game_installation(preferred: Option<&Path>) -> Option<std::path::PathBuf> {
    crate::discovery::find(crate::discovery::Product::Ffxi, preferred)
}

#[cfg(test)]
fn find_game_installation_from(preferred: Option<&Path>, registered: Option<&Path>) -> Option<std::path::PathBuf> {
    [preferred, registered].into_iter().flatten()
        .find_map(|path| crate::discovery::validated_folder(crate::discovery::Product::Ffxi, path))
}

fn report(local: Result<String, String>, remote: Result<String, String>) -> PatchReport {
    let mut result = PatchReport {
        state: PatchState::Unknown,
        local_version: local
            .as_ref()
            .cloned()
            .unwrap_or_else(|_| t("Unavailable").into()),
        remote_version: remote
            .as_ref()
            .cloned()
            .unwrap_or_else(|_| t("Unavailable").into()),
        detail: String::new(),
        files_verified: None,
    };
    result.detail = match (local, remote) {
        (Ok(local), Ok(remote)) if local == remote => {
            result.state = PatchState::Current;
            t("FFXI's installed-version record matches the official patch server. Check files verifies file integrity.").into()
        }
        (Ok(local), Ok(remote)) => {
            match (Product::Ffxi.stamp_order(&local), Product::Ffxi.stamp_order(&remote)) {
                (Some(a), Some(b)) if a < b => {
                    result.state = PatchState::Required;
                    t("A newer version is available.").into()
                }
                _ => t("Local metadata is newer than the official server response; update status cannot be established.").into(),
            }
        }
        (Err(local), Ok(_)) => local,
        (Ok(_), Err(remote)) => remote,
        (Err(local), Err(remote)) => tf("{local}. {remote}", &[("local", &local), ("remote", &remote)]),
    };
    result
}

/// The applied version in `install\patch.ver`, decoded with the key for `product`
/// (`0001` FFXI, `1000` the PlayOnline Viewer; the registry holds one key per product).
fn local_version_for(install: &Path, product: Product) -> Result<String, String> {
    let path = install.join("patch.ver");
    // A writer that briefly denies sharing (the updater replacing the record, a scanner)
    // must not leave the installed version a placeholder: retry a few times.
    let mut attempt = 0;
    let file = loop {
        match std::fs::File::open(&path) {
            Ok(file) => break file,
            Err(e) if attempt < 4 && (e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(32 | 33))) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(tf("Cannot read applied-version record {path}: {error}", &[("path", &path.display()), ("error", &e)])),
        }
    };
    let mut bytes = Vec::new();
    file.take(289)
        .read_to_end(&mut bytes)
        .map_err(|e| tf("Cannot read patch.ver: {error}", &[("error", &e)]))?;
    if bytes.len() != 288 {
        return Err(t("Applied-version record patch.ver has an unsupported length").into());
    }
    // The title-specific Interface value is a patch-record codec seed, not an
    // account credential. The native decoder also tries the no-registry seed.
    if let Some(version) = patch_seeds_for(product).into_iter().find_map(|seed| decode_record_for(&bytes, seed, product).ok()) {
        return Ok(version);
    }
    Err(t("Cannot validate/decode applied-version record patch.ver with the installed patch keys. Downloaded patch.cfg does not prove an update was applied.").into())
}

/// Preserve the key that validates the installed applied-version record.
/// A missing record uses the installed title key (or POL's seed-1 fallback).
#[cfg(test)]
pub(crate) fn applied_seed(install: &Path) -> Result<u64, String> {
    applied_seed_for(install, Product::Ffxi)
}

pub(crate) fn applied_seed_for(install: &Path, product: Product) -> Result<u64, String> {
    let path = install.join("patch.ver");
    match std::fs::read(&path) {
        Ok(bytes) => patch_seeds_for(product).into_iter()
            .find(|seed| decode_record_for(&bytes, *seed, product).is_ok())
            .ok_or_else(|| t("Existing patch.ver cannot be validated; refusing to replace its applied-version record").into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(patch_seeds_for(product)[0]),
        Err(e) => Err(tf("Cannot read {path}: {error}", &[("path", &path.display()), ("error", &e)])),
    }
}

/// Explicit Repair may replace an invalid applied record after every official
/// file verifies. Prefer the installed title key, retaining the old record as
/// a transaction backup rather than treating it as a successfully applied version.
#[cfg(test)]
pub(crate) fn repair_seed(install: &Path) -> Result<u64, String> {
    repair_seed_for(install, Product::Ffxi)
}

pub(crate) fn repair_seed_for(install: &Path, product: Product) -> Result<u64, String> {
    match applied_seed_for(install, product) {
        Ok(seed) => Ok(seed),
        Err(_) => {
            // Still report read/access errors; only codec-invalid regular files
            // are eligible for explicit metadata repair.
            let bytes = std::fs::read(install.join("patch.ver")).map_err(|e| e.to_string())?;
            if patch_seeds_for(product)
                .iter()
                .any(|s| decode_record_for(&bytes, *s, product).is_ok())
            {
                return Err(t("Applied record unexpectedly changed during repair preparation").into());
            }
            Ok(patch_seeds_for(product)[0])
        }
    }
}

/// Inverse of the validated retail codec below; used only after all files verify.
#[cfg(test)]
pub(crate) fn encode_applied_record(version: &str, seed: u64) -> Result<Vec<u8>, String> {
    encode_applied_record_for(version, seed, Product::Ffxi)
}

pub(crate) fn encode_applied_record_for(version: &str, seed: u64, product: Product) -> Result<Vec<u8>, String> {
    if !product.valid_stamp(version) { return Err(t("Unsupported applied version").into()); }
    encode_record(version, seed)
}

fn decode_record_for(bytes: &[u8], seed: u64, product: Product) -> Result<String, String> {
    let version = decode_patch_text(bytes, seed)?;
    if !product.valid_stamp(&version) { return Err(t("Unsupported applied version").into()); }
    Ok(version)
}


/// The record codec's per-word keys from `seed` (polcore key schedule 10007bc0).
fn key_schedule(seed: u64) -> [u64; 32] {
    let key = seed.to_le_bytes();
    let mut first = [0u8; 8];
    first[..4].copy_from_slice(&u32::from_le_bytes(key[4..].try_into().unwrap()).rotate_left(16).to_le_bytes());
    first[4..].copy_from_slice(&u32::from_le_bytes(key[..4].try_into().unwrap()).rotate_left(8).to_le_bytes());
    first[0] = first[0].wrapping_add(0x45);
    for i in 1..8 {
        first[i] = first[i].wrapping_add(first[i - 1]).wrapping_sub(0x2c) ^ first[i - 1].wrapping_shl(2) ^ 0x45;
    }
    let mut schedule = [0u64; 32];
    schedule[0] = u64::from_le_bytes(first);
    for i in 1..32 {
        schedule[i] = schedule[i - 1].wrapping_mul(5);
    }
    schedule
}

/// Word `i`'s position mask: its byte offset repeated in four 10-bit steps, plus the codec's
/// constant (the same in both directions).
fn counter_mask(i: usize) -> u64 {
    let counter = (i * 8) as u64;
    let mut expanded = counter;
    for _ in 0..3 {
        expanded = (expanded << 10) | counter;
    }
    expanded.wrapping_add(0xa1652347)
}

fn encode_record(version: &str, seed: u64) -> Result<Vec<u8>, String> {
    if version.len() >= 256 {
        return Err(t("Applied version is too long").into());
    }
    let schedule = key_schedule(seed);
    let mut plain = [0u8; 288];
    plain[24..24 + version.len()].copy_from_slice(version.as_bytes());
    plain[280..284].copy_from_slice(&280u32.to_le_bytes());
    let checksum = (0..35).fold(0u32, |s, i| {
        s.wrapping_add(u32::from(plain[i * 8]))
            .wrapping_add(u32::from(plain[i * 8 + 4]))
    });
    plain[284..].copy_from_slice(&checksum.to_le_bytes());
    let mut cipher = vec![0u8; 288];
    for i in 0..36 {
        let word = schedule[i % 32];
        let key = word.to_le_bytes();
        let value =
            u64::from_le_bytes(plain[i * 8..i * 8 + 8].try_into().unwrap()).rotate_right(32);
        let block = ((value ^ word ^ counter_mask(i)).wrapping_add(word)).to_le_bytes();
        let mut chain = (i as u8) ^ 0x45;
        for j in 0..8 {
            let mut v = block[j];
            for k in key {
                v = v.wrapping_sub(0x78).wrapping_add(k);
            }
            cipher[i * 8 + j] = v ^ chain;
            chain = cipher[i * 8 + j];
        }
    }
    if decode_patch_text(&cipher, seed)? != version {
        return Err(t("Applied-version encoder validation failed").into());
    }
    Ok(cipher)
}
/// Candidate keys for the record of product `title`, as polcore builds them (slot 1171:
/// 1004a170 / 1004a090 / 10049f60): the registry string `Interface\<title>` folded into
/// eight bytes by `key[i % 8] += c`, plus the product number (1 for FFXI, 1000 for the
/// Viewer); with no usable string, the product number alone.
fn patch_seeds_for(product: Product) -> Vec<u64> {
    let (title, number) = (product.title(), product.number());
    use crate::win::reg::{self, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut seeds = Vec::new();
    for (_, region) in reg::REGIONS {
        let path = format!(r"SOFTWARE\WOW6432Node\{region}\Interface");
        let Some(value) = reg::string(HKEY_LOCAL_MACHINE, &path, title, RRF_RT_REG_SZ, 128) else {
            continue;
        };
        // The installed viewer reads an ANSI key; published patch keys are ASCII.
        // Unsupported encodings remain Unknown instead of inventing a seed.
        if value.iter().any(|c| *c > 127) {
            continue;
        }
        let mut key = [0u8; 8];
        for (i, c) in value.iter().enumerate() {
            key[i % 8] = key[i % 8].wrapping_add(*c as u8);
        }
        seeds.push(u64::from_le_bytes(key).wrapping_add(number));
    }
    seeds.push(number);
    seeds
}

/// The applied-version codec, without a product's version rule (the Viewer's own record holds
/// a different style of version, `20110829_E`; `decode_record_for` applies the product's).
/// Traced statically from installed polcore.dll
/// SHA256 73b1864bf522d3aa6ec28e9f4bb226fcee956f7f4848a238c9c2d756dc703b06:
/// table slot +0x124c -> 1004a0e0, 1004a170, 1004a220, 100081e0;
/// key schedule 10007bc0; inverse byte/mix stages 100080c0/10008000;
/// trailer validation 10008130. app.dll 1029da51 supplies title index 1;
/// its completion path 102a4cbb writes this record after patching.
/// No original DLL is loaded, and no account/profile/session data is read.
fn decode_patch_text(cipher: &[u8], seed: u64) -> Result<String, String> {
    if cipher.len() != 288 {
        return Err("unsupported patch.ver length".into());
    }
    let mut plain = [0u8; 288];
    let checksum = decode_blocks(cipher, seed, &mut plain);
    if u32::from_le_bytes(plain[280..284].try_into().unwrap()) != 280
        || u32::from_le_bytes(plain[284..288].try_into().unwrap()) != checksum
        || plain[..24].iter().any(|b| *b != 0)
    {
        return Err("patch.ver checksum/header validation failed".into());
    }
    let text = &plain[24..280];
    let end = text
        .iter()
        .position(|b| *b == 0)
        .ok_or("patch.ver version is not terminated")?;
    if text[end..].iter().any(|b| *b != 0) {
        return Err("patch.ver has unexpected version padding".into());
    }
    let version =
        std::str::from_utf8(&text[..end]).map_err(|_| "patch.ver version is not ASCII")?;
    Ok(version.to_owned())
}

/// The codec's inverse over whole 8-byte blocks of `cipher` into `plain` (same length).
/// Returns the trailer checksum: bytes 0 and 4 of every block but the last.
fn decode_blocks(cipher: &[u8], seed: u64, plain: &mut [u8]) -> u32 {
    let schedule = key_schedule(seed);
    let blocks = cipher.len() / 8;
    let mut checksum = 0u32;
    for (i, chunk) in cipher.as_chunks::<8>().0.iter().enumerate() {
        let word = schedule[i % 32];
        let key = word.to_le_bytes();
        let mut chain = (i as u8) ^ 0x45;
        let mut block = [0u8; 8];
        for (j, c) in chunk.iter().enumerate() {
            let mut value = *c ^ chain;
            chain = *c;
            for k in key {
                value = value.wrapping_add(0x78).wrapping_sub(k);
            }
            block[j] = value;
        }
        let mixed = u64::from_le_bytes(block).wrapping_sub(word) ^ counter_mask(i) ^ word;
        block = mixed.rotate_left(32).to_le_bytes();
        plain[i * 8..i * 8 + 8].copy_from_slice(&block);
        if i + 1 < blocks {
            checksum = checksum
                .wrapping_add(u32::from(block[0]))
                .wrapping_add(u32::from(block[4]));
        }
    }
    checksum
}

/// polcore's configuration-file key (8 bytes at 1009af90 in the unpacked 1.18.15e core,
/// SHA256 73b1864b…, the same core as `decode_patch_text`). With it the patch.ver codec
/// above (its +0x78 byte stage is that core's 256-byte table at 100715e4) decodes the
/// installed US `data\utf\env.dat` with a valid trailer, giving `PATCH_SERVER_DOMAIN,
/// pt008.pol.com` and `POL_UCS_AREA_KBN,01`: the values `dns::VIEWER_PATCH_HOST` and the
/// loader's 1309 already use.
pub(crate) const ENV_DAT_SEED: u64 = 0x9786_7564_5342_31fd;
/// Decoded env.dat files are a few KB (US: 2,414 bytes).
const MAX_ENV_DAT: usize = 64 * 1024;

/// A PlayOnline `env.dat` (app 100198b6 via polcore 1024/472/473/1025): whole blocks, the
/// last one `(length u32, checksum u32)`. Only a valid trailer counts as decoded, so a
/// region whose core used another key is refused rather than misread.
pub(crate) fn decode_env_dat(cipher: &[u8]) -> Result<Vec<u8>, String> {
    decode_trailed(cipher, ENV_DAT_SEED).map_err(|problem| format!("env.dat {problem}"))
}

/// A PlayOnline file of whole 8-byte blocks whose last block is `(length u32, checksum u32)`,
/// decoded with `seed`: the body, only when the trailer is valid.
fn decode_trailed(cipher: &[u8], seed: u64) -> Result<Vec<u8>, &'static str> {
    if cipher.len() < 16 || cipher.len() > MAX_ENV_DAT || !cipher.len().is_multiple_of(8) {
        return Err("has an unsupported length");
    }
    let mut plain = vec![0u8; cipher.len()];
    let checksum = decode_blocks(cipher, seed, &mut plain);
    let body = plain.len() - 8;
    let length = u32::from_le_bytes(plain[body..body + 4].try_into().unwrap()) as usize;
    if u32::from_le_bytes(plain[body + 4..].try_into().unwrap()) != checksum || length > body {
        return Err("trailer validation failed");
    }
    plain.truncate(length);
    Ok(plain)
}

/// One title record of the Viewer's contents table is this long (polcore 1479/1189:
/// 10019431 `add esi, 0x124`).
const CONTENTS_RECORD: usize = 0x124;

/// The patch-platform flag of title `title` in a Viewer's contents table,
/// `data\doc\sqpolcts.bin`: the byte at +0x120 of its record. The stock Viewer reads the
/// same byte for its own version check (app 1019b481: title 1000, `movsx` of record+0x120)
/// and passes it to its tag picker (app 1027b731: flag 1 → `W2U`, 0 → `W20`). The file is
/// the env.dat codec with an all-zero key (polcore 100193c0); its body is an 8-byte header
/// (record count at +4) and 0x124-byte records that start with the title number. Decoded
/// from each region's official installer (FFXIFullSetup_US/EU/JP, PlayOnlineViewer.msi):
/// US and EU carry 1 for title 1000, Japan 0, and all three 0 for title 1 (FFXI).
pub(crate) fn contents_platform_flag(cipher: &[u8], title: u32) -> Result<u8, String> {
    let body = decode_trailed(cipher, 0).map_err(|problem| format!("sqpolcts.bin {problem}"))?;
    let count = body.get(4..8).map(|n| u32::from_le_bytes(n.try_into().unwrap()) as usize).ok_or("sqpolcts.bin has no header")?;
    let records = body.get(8..).unwrap_or_default();
    if count == 0 || count > records.len() / CONTENTS_RECORD {
        return Err("sqpolcts.bin's record count does not fit the file".into());
    }
    records.chunks_exact(CONTENTS_RECORD).take(count)
        .find(|record| u32::from_le_bytes(record[..4].try_into().unwrap()) == title)
        .map(|record| record[0x120])
        .ok_or_else(|| format!("sqpolcts.bin has no record for title {title}"))
}

/// A contents table holding `records` as `(title, platform flag)` (test fixtures only).
#[cfg(test)]
pub(crate) fn encode_contents(records: &[(u32, u8)]) -> Vec<u8> {
    let mut plain = vec![0u8; 8];
    plain[2] = 1;
    plain[4..8].copy_from_slice(&(records.len() as u32).to_le_bytes());
    for (title, flag) in records {
        let mut record = [0u8; CONTENTS_RECORD];
        record[..4].copy_from_slice(&title.to_le_bytes());
        record[0x120] = *flag;
        plain.extend_from_slice(&record);
    }
    encode_trailed(&plain, 0)
}

/// The inverse of `decode_env_dat` (test fixtures only).
#[cfg(test)]
pub(crate) fn encode_env_dat(body: &[u8]) -> Vec<u8> {
    encode_trailed(body, ENV_DAT_SEED)
}

/// The inverse of `decode_trailed` (test fixtures only).
#[cfg(test)]
fn encode_trailed(body: &[u8], seed: u64) -> Vec<u8> {
    let mut plain = body.to_vec();
    plain.resize(body.len().div_ceil(8) * 8, 0);
    let checksum = plain.chunks(8).fold(0u32, |s, b| s.wrapping_add(u32::from(b[0])).wrapping_add(u32::from(b[4])));
    plain.extend_from_slice(&(body.len() as u32).to_le_bytes());
    plain.extend_from_slice(&checksum.to_le_bytes());
    encode_blocks(&plain, seed)
}

/// `encode_record`'s block stage over any whole number of blocks.
#[cfg(test)]
fn encode_blocks(plain: &[u8], seed: u64) -> Vec<u8> {
    let schedule = key_schedule(seed);
    let mut cipher = vec![0u8; plain.len()];
    for i in 0..plain.len() / 8 {
        let word = schedule[i % 32];
        let key = word.to_le_bytes();
        let value = u64::from_le_bytes(plain[i * 8..i * 8 + 8].try_into().unwrap()).rotate_right(32);
        let block = ((value ^ word ^ counter_mask(i)).wrapping_add(word)).to_le_bytes();
        let mut chain = (i as u8) ^ 0x45;
        for j in 0..8 {
            let mut v = block[j];
            for k in key {
                v = v.wrapping_sub(0x78).wrapping_add(k);
            }
            cipher[i * 8 + j] = v ^ chain;
            chain = cipher[i * 8 + j];
        }
    }
    cipher
}

fn parse_reply(frame: &[u8]) -> Result<String, String> {
    parse_reply_for(frame, false)
}

fn parse_reply_for(frame: &[u8], viewer: bool) -> Result<String, String> {
    if frame.len() < HEADER
        || frame.len() > MAX_VERSION_FRAME
        || u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize != frame.len()
        || &frame[8..12] != b"POLP"
    {
        return Err(t("Official server returned an invalid POLP frame").into());
    }
    if md5::compute(&frame[8..]).0[..4] != frame[4..8] {
        return Err(t("Official patch reply checksum failed").into());
    }
    match u32::from_le_bytes(frame[12..16].try_into().unwrap()) {
        polp::kind::VERSION_REPLY => {}
        polp::kind::VERSION_REJECTED => return Err((if viewer { t("Official patch server rejected the PlayOnline request") } else { t("Official patch server rejected the Windows FFXI request") }).into()),
        _ => return Err(t("Official patch server returned an unexpected reply type").into()),
    }
    let body = &frame[HEADER..];
    if body.len() < 76 {
        return Err(t("Official version reply is truncated").into());
    }
    let label = body[8..72].split(|b| *b == 0).next().unwrap_or_default();
    if label == b"unknown" {
        return Err(t("Official patch server did not recognize the version request").into());
    }
    let size = u32::from_le_bytes(body[72..76].try_into().unwrap()) as usize;
    if size == 0 || size > 64 || body.len() != 76 + size {
        return Err(t("Official version string has an invalid length").into());
    }
    let bytes = &body[76..];
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let version = std::str::from_utf8(bytes)
        .map_err(|_| t("Official version string is not ASCII").to_string())?;
    let valid = if viewer { Product::Viewer.valid_stamp(version) } else { Product::Ffxi.valid_stamp(version) };
    if !valid { return Err(t("Official patch server returned an unsupported version format").into()); }
    Ok(version.into())
}

/// A PlayOnline Viewer version stamp (`Product::valid_stamp`).
pub(crate) fn valid_viewer_version(version: &str) -> bool {
    Product::Viewer.valid_stamp(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_detection_accepts_fresh_installs_and_keeps_a_valid_selected_copy() {
        let root = install_with(None, false);
        let selected = root.join("selected");
        let registered = root.join("registered");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&registered).unwrap();
        std::fs::write(selected.join("FFXiMain.dll"), b"game marker").unwrap();
        std::fs::write(registered.join("FFXi.dll"), b"game marker").unwrap();
        let plain = |path: &Path| crate::backend::plain_windows_path(&std::fs::canonicalize(path).unwrap());
        assert_eq!(find_game_installation_from(Some(&selected), Some(&registered)), Some(plain(&selected)));
        std::fs::remove_file(selected.join("FFXiMain.dll")).unwrap();
        assert_eq!(find_game_installation_from(Some(&selected), Some(&registered)), Some(plain(&registered)));
        std::fs::remove_file(registered.join("FFXi.dll")).unwrap();
        std::fs::create_dir_all(registered.join("ROM/0")).unwrap();
        std::fs::write(registered.join("ROM/0/0.DAT"), b"repairable game marker").unwrap();
        assert_eq!(find_game_installation_from(None, Some(&registered)), Some(plain(&registered)));
        assert!(!registered.join("patch.ver").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn game_detection_rejects_stale_registrations_and_non_game_folders() {
        let root = install_with(None, false);
        let missing = root.join("removed");
        let viewer = root.join("viewer");
        std::fs::create_dir(&viewer).unwrap();
        std::fs::write(viewer.join("pol.exe"), b"viewer marker").unwrap();
        assert!(find_game_installation_from(Some(&missing), Some(&viewer)).is_none());
        assert!(find_game_installation_from(Some(Path::new("relative/client")), None).is_none());
        assert!(find_game_installation_from(None, Some(&viewer.join("pol.exe"))).is_none());
        assert!(find_game_installation_from(Some(&viewer.join("../viewer")), None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn applied_encoder_roundtrips_installed_and_fallback_seeds() {
        for seed in [1, 0x123456789abcdef0, 0xffffffffffffffff] {
            let bytes = encode_applied_record("30260904_1", seed).unwrap();
            assert_eq!(decode_record_for(&bytes, seed, Product::Ffxi).unwrap(), "30260904_1");
            assert!(decode_record_for(&bytes, seed.wrapping_add(1), Product::Ffxi).is_err());
        }
    }

    #[test]
    fn product_records_keep_viewer_revisions_and_title_keys_separate() {
        let root = install_with(None, false);
        for version in ["20061212_3", "20110829_E"] {
            let seed = applied_seed_for(&root, Product::Viewer).unwrap();
            let bytes = encode_applied_record_for(version, seed, Product::Viewer).unwrap();
            std::fs::write(root.join("patch.ver"), &bytes).unwrap();
            assert_eq!(local_version_for(&root, Product::Viewer).unwrap(), version);
            assert_eq!(applied_seed_for(&root, Product::Viewer).unwrap(), seed);
            assert!(decode_record_for(&bytes, seed, Product::Ffxi).is_err());
            assert!(encode_applied_record_for(version, seed, Product::Ffxi).is_err());
        }
        assert!(encode_applied_record_for("30260904_1", 1, Product::Viewer).is_err());
        std::fs::write(root.join("patch.ver"), b"corrupt record").unwrap();
        assert!(applied_seed_for(&root, Product::Viewer).is_err());
        assert!(repair_seed_for(&root, Product::Viewer).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn reply() -> Vec<u8> {
        let mut frame = vec![0; HEADER + 76];
        frame[8..12].copy_from_slice(b"POLP");
        frame[12..16].copy_from_slice(&8u32.to_le_bytes());
        frame[16..20].copy_from_slice(&1788518731u32.to_le_bytes());
        frame[20..24].copy_from_slice(&1u32.to_le_bytes());
        frame[24..29].copy_from_slice(b"empty");
        frame[88..92].copy_from_slice(&11u32.to_le_bytes());
        frame.extend_from_slice(b"30260904_1\0");
        let len = frame.len() as u32;
        frame[..4].copy_from_slice(&len.to_le_bytes());
        let digest = md5::compute(&frame[8..]);
        frame[4..8].copy_from_slice(&digest.0[..4]);
        frame
    }

    #[test]
    fn decodes_applied_version_record_and_rejects_wrong_keys_or_corruption() {
        // Independent encoder fixture derived from native routines, seed 1;
        // not a real registry key or a captured account/session record.
        let hex = "93d1f53dc7021e030368992fdb33a909a01cc9c400909ab19d1466c3c1cfdeca519d825677206ae569b460062824a77c74b7fd85a5e9772fbeee98d934bf99500fb9ad69fb38f00a2e90fcb0f52edb34bd6bb94b6e38ef566c3ab3ff942916bd9446bb66932f6c0de63c291e823c5ca507fe653157947991a2befea820c175e9f471000c2155c1da69f89d654d1dee78efe2fabedb448e725555b916ac82837fd6f205c9f80d1fe72d5cc9732af399718f566bc9e8a7851e663e1f9afe67dfcbbc62e403ddd2780f2034a8f34c167717224f0d2bb067996082a18a87743134d8da2febe096f1d57fbbe2df3bcdc67a24010994481b52ed0a79c4b180a2d1109bb3f0e8300acfd3ce234da00632da40e08039f00d891913384f2d7bf25984bb5d";
        let mut fixture: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>().0.iter()
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(decode_record_for(&fixture, 1, Product::Ffxi).unwrap(), "30260904_1");
        assert_eq!(encode_applied_record("30260904_1", 1).unwrap(), fixture);
        assert!(decode_record_for(&fixture, 2, Product::Ffxi).is_err());
        fixture[24] ^= 1;
        assert!(decode_record_for(&fixture, 1, Product::Ffxi).is_err());
        assert!(decode_record_for(&fixture[..287], 1, Product::Ffxi).is_err());
    }

    #[test]
    fn compares_versions_without_claiming_unknown_metadata_is_current() {
        assert_eq!(
            report(Ok("30260904_1".into()), Ok("30260904_1".into())).state,
            PatchState::Current
        );
        assert_eq!(
            report(Ok("30260805_0".into()), Ok("30260904_1".into())).state,
            PatchState::Required
        );
        assert_eq!(
            report(Err("missing".into()), Ok("30260904_1".into())).state,
            PatchState::Unknown
        );
        assert_eq!(
            report(Ok("30260904_1".into()), Err("offline".into())).state,
            PatchState::Unknown
        );
        assert_eq!(
            report(Ok("30260904_10".into()), Ok("30260904_9".into())).state,
            PatchState::Unknown
        );
    }

    #[test]
    fn validates_official_reply_layout_and_detects_corruption() {
        let frame = reply();
        assert_eq!(parse_reply(&frame).unwrap(), "30260904_1");
        let mut corrupted = frame.clone();
        corrupted[95] ^= 1;
        assert!(parse_reply(&corrupted).unwrap_err().contains("checksum"));
        assert!(parse_reply(&frame[..frame.len() - 1]).is_err());
        assert!(parse_reply(&[]).is_err());
    }

    #[test]
    fn a_translated_update_required_message_still_parses() {
        // The template a translation would use: placeholders moved, other punctuation.
        let template = "更新が必要です: インストール済み {local}、公式サーバー {remote}。ダウンロードを開いてください。";
        let message = "更新が必要です: インストール済み 30260904_1、公式サーバー 30260905_0。ダウンロードを開いてください。";
        assert_eq!(
            parse_update_required_with(message, template),
            Some(("30260904_1".to_owned(), "30260905_0".to_owned()))
        );
        assert_eq!(parse_update_required_with("something else", template), None);
        // A translation may name the server's version first.
        assert_eq!(
            parse_update_required_with("公式 30260905_0（現在 30260904_1）。更新してください。", "公式 {remote}（現在 {local}）。更新してください。"),
            Some(("30260904_1".to_owned(), "30260905_0".to_owned()))
        );
        // The parser's template is gate's key, and a message gate built in
        // Japanese still parses after the window has switched to English (and
        // the other way round).
        let report = PatchReport {
            state: PatchState::Required,
            local_version: "30260904_1".into(),
            remote_version: "30260905_0".into(),
            detail: String::new(),
            files_verified: None,
        };
        crate::i18n::set(crate::i18n::Lang::En);
        assert!(gate(LaunchCheck::Update(report.clone())).unwrap_err().starts_with(UPDATE_REQUIRED_TEMPLATE.split_once('{').unwrap().0));
        for (built, shown) in [(crate::i18n::Lang::Ja, crate::i18n::Lang::En), (crate::i18n::Lang::En, crate::i18n::Lang::Ja)] {
            crate::i18n::set(built);
            let message = gate(LaunchCheck::Update(report.clone())).unwrap_err();
            crate::i18n::set(shown);
            let parsed = parse_update_required(&message).unwrap_or_else(|| panic!("unparsed: {message}"));
            assert_eq!((parsed.local_version.as_str(), parsed.remote_version.as_str()), ("30260904_1", "30260905_0"));
        }
        crate::i18n::set(crate::i18n::Lang::En);
        // A session worker from before this wording (any UI language) still turns Play into
        // Update: the older Japanese and English refusals parse.
        for message in [
            "アップデートが必要です（インストール済み：30260904_1、公式サーバー：30260905_0）。FINAL FANTASY XIを起動する前に、［ダウンロード］でゲームフォルダーを選択し、［アップデート］（または［修復］）を選んでください。",
            "Update required: installed 30260904_1, official server 30260905_0. Open Downloads, select the game folder, then choose Update (or Repair) before starting FINAL FANTASY XI.",
        ] {
            for shown in [crate::i18n::Lang::En, crate::i18n::Lang::Ja] {
                crate::i18n::set(shown);
                let parsed = parse_update_required(message).unwrap_or_else(|| panic!("unparsed: {message}"));
                assert_eq!((parsed.local_version.as_str(), parsed.remote_version.as_str()), ("30260904_1", "30260905_0"));
            }
        }
        crate::i18n::set(crate::i18n::Lang::En);
        // A gate key edit must update the parser's copy too.
        // (Its definition, and gate's tf call.)
        assert_eq!(include_str!("patch.rs").matches(&format!("\"{UPDATE_REQUIRED_TEMPLATE}\"")).count(), 2);
    }

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>().0.iter()
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect()
    }

    /// Golden bytes: the 88-byte frames the stock Viewer sent in the live capture
    /// (`W2U`/`1000` to :54000 and `W20`/`0001` to :53001), checksum included.
    const STOCK_VIEWER: &str = "58000000a6d67388504f4c5007000000573255003130303032303131303832395f45000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    const STOCK_FFXI: &str = "5800000061d437a3504f4c5007000000573230003030303133303236303930345f31000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    /// What Crystal used to send at launcher start, on refresh and every 600 s.
    const OLD_EMPTY: &str = "580000007c6e8b19504f4c5007000000573230003030303100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn stock_frames_are_reproduced_byte_for_byte() {
        assert_eq!(Product::Viewer.request("20110829_E"), hex(STOCK_VIEWER));
        assert_eq!(Product::Ffxi.request("30260904_1"), hex(STOCK_FFXI));
        // The names stock looks up (captures 2026-10-13 00:21 and 00:49).
        assert_eq!(Product::Viewer.server(), "pt008.pol.com:54000");
        assert_eq!(Product::Ffxi.server(), "pc001W2U.pol.com:53001");
        // Empty is the fresh-install shape only; it is never produced for an installed version.
        assert_eq!(Product::Ffxi.request(""), hex(OLD_EMPTY));
        assert_ne!(Product::Ffxi.request("30260904_1"), Product::Ffxi.request(""));
    }

    fn install_with(version: Option<&str>, stamped: bool) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crystal-patch-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(version) = version {
            let bytes = if stamped {
                encode_applied_record(version, 1).unwrap()
            } else {
                // The Viewer's key is its registry string plus 1000; 1000 alone is the fallback.
                encode_record(version, 1000).unwrap()
            };
            std::fs::write(dir.join("patch.ver"), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn same_version_starts_and_a_different_one_goes_to_update() {
        let install = install_with(Some("30260904_1"), true);
        let mut sent = Vec::new();
        let outcome = check_with(&install, |request| {
            sent = request.to_vec();
            Ok(reply())
        });
        // The request carried the installed version in the stock shape.
        assert_eq!(sent, hex(STOCK_FFXI));
        assert!(matches!(outcome, LaunchCheck::Start(_)));
        assert!(gate(outcome).unwrap().contains("30260904_1"));

        let mut newer = reply();
        newer[92..102].copy_from_slice(b"30260905_0");
        let len = newer.len();
        let digest = md5::compute(&newer[8..len]);
        newer[4..8].copy_from_slice(&digest.0[..4]);
        let outcome = check_with(&install, |_| Ok(newer.clone()));
        let LaunchCheck::Update(report) = &outcome else { panic!("expected an update") };
        assert_eq!((report.local_version.as_str(), report.remote_version.as_str()), ("30260904_1", "30260905_0"));
        let message = gate(outcome).unwrap_err();
        assert!(message.starts_with(UPDATE_REQUIRED) && message.contains("Update"));
        let shown = parse_update_required(&message).unwrap();
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!((shown.local_version.as_str(), shown.remote_version.as_str()), ("30260904_1", "30260905_0"));
    }

    #[test]
    fn an_interrupted_transaction_blocks_game_and_viewer_even_with_current_records() {
        for (version, game) in [("30260904_1", true), ("20110829_E", false)] {
            let install = install_with(Some(version), game);
            let original = std::fs::read(install.join("patch.ver")).unwrap();
            std::fs::create_dir(install.join(".xi-updater")).unwrap();
            let journal = install.join(".xi-updater/transaction.json");
            // Existence, including a damaged journal, requires recovery first.
            std::fs::write(&journal, b"interrupted journal fixture").unwrap();
            assert!(check_update_state(&install).unwrap_err().contains("interrupted update"));
            if game {
                let outcome = check_with(&install, |_| panic!("unfinished update cannot query or start"));
                let LaunchCheck::Update(report) = &outcome else { panic!("expected recovery refusal") };
                assert!(report.detail.contains("interrupted update"), "{}", report.detail);
                assert!(gate(outcome).is_err());
                // The window's report is not "update required": Play finishes it first.
                let shown = report_with_remote(&install, version);
                assert_eq!(shown.state, PatchState::Interrupted);
                assert!(shown.detail.contains("Press Play"), "{}", shown.detail);
                assert_eq!(local_report(&install).state, PatchState::Interrupted);
            } else {
                let verdict = viewer_connect_with(&install, |_| panic!("unfinished Viewer update cannot query"));
                assert!(matches!(&verdict, crate::viewer::ConnectVerdict::Failed(e) if e.contains("interrupted update")), "{verdict:?}");
                assert_eq!(viewer_report_with_remote(&install, Some(version)).state, PatchState::Interrupted);
            }
            assert_eq!(std::fs::read(install.join("patch.ver")).unwrap(), original);
            assert!(journal.is_file());
            std::fs::remove_dir_all(install).unwrap();
        }
    }

    #[test]
    fn another_launcher_update_blocks_version_checks_without_changing_its_record() {
        use std::os::windows::fs::OpenOptionsExt;
        let install = install_with(Some("30260904_1"), true);
        std::fs::create_dir(install.join(".xi-updater")).unwrap();
        let path = install.join(".xi-updater/lock");
        let updating = || std::fs::OpenOptions::new().read(true).write(true)
            .create(true).truncate(false).share_mode(0).open(&path);
        let exclusive = updating().unwrap();
        assert!(matches!(check_with(&install, |_| panic!("active updater cannot launch")), LaunchCheck::Update(_)));
        // The updater itself still uses the installed version under its lock.
        assert_eq!(installed_for_request(&install), "30260904_1");
        drop(exclusive);
        let held = check_update_state(&install).unwrap().unwrap();
        assert!(updating().is_err(), "a version check keeps the existing lock read-shared");
        drop(held);
        assert!(matches!(check_with(&install, |_| Ok(reply())), LaunchCheck::Start(_)));
        std::fs::remove_dir_all(install).unwrap();
    }

    #[test]
    fn an_unreachable_server_starts_unverified_never_with_an_empty_request() {
        let install = install_with(Some("30260904_1"), true);
        let mut requests = Vec::new();
        let outcome = check_with(&install, |request| {
            requests.push(request.to_vec());
            Err("Cannot connect to official patch server: refused".into())
        });
        assert_eq!(requests, vec![hex(STOCK_FFXI)]);
        let LaunchCheck::Unverified(report) = &outcome else { panic!("expected unverified") };
        assert_eq!(report.state, PatchState::Unknown);
        let note = gate(outcome).unwrap();
        assert!(note.contains("Starting anyway") && note.contains("refused"));
    }

    #[test]
    fn invalid_and_fresh_records_require_initialization_without_changing_stock_request_shapes() {
        let broken = install_with(None, true);
        std::fs::write(broken.join("patch.ver"), [7u8; 288]).unwrap();
        let outcome = check_with(&broken, |_| panic!("no request may be sent for an unreadable record"));
        assert!(matches!(outcome, LaunchCheck::Update(_)));
        // The refusal keeps its own cause (Repair), never a parseable "newer version" text.
        let refusal = gate(outcome).unwrap_err();
        assert!(!refusal.starts_with(UPDATE_REQUIRED) && refusal.contains("Repair"), "{refusal}");
        assert!(parse_update_required(&refusal).is_none() && !is_update_refusal(&refusal));
        // An older worker's placeholder wording does not become an update either.
        assert!(parse_update_required("Update required: installed Unavailable, official Unavailable.").is_none());
        assert_eq!(installed_for_request(&broken), "");

        let fresh = install_with(None, true);
        let mut sent = Vec::new();
        let outcome = check_with(&fresh, |request| {
            sent = request.to_vec();
            Ok(reply())
        });
        assert_eq!(sent, hex(OLD_EMPTY));
        assert!(matches!(outcome, LaunchCheck::Update(_)));
        assert_eq!(installed_for_request(&fresh), "");
        assert_eq!(installed_for_request(&install_with(Some("30260904_1"), true)), "30260904_1");
    }

    #[test]
    fn missing_record_is_not_ready_even_before_a_network_check() {
        let root = install_with(None, true);
        let local = local_report(&root);
        assert_eq!(local.state, PatchState::Required);
        assert_eq!(local.local_version, t("Not initialized"));
        assert_eq!(local.remote_version, t("Not checked"));
        let shown = report_with_remote(&root, "30260904_1");
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!(shown.remote_version, "30260904_1");
        // A fresh install keeps the Update affordance when the server did not answer.
        let refusal = gate(check_with(&root, |_| Err("offline".into()))).unwrap_err();
        let parsed = parse_update_required(&refusal).expect("a fresh install's refusal offers Update");
        assert_eq!(parsed.local_version, t("Not initialized"));
        assert!(parsed.detail.contains("initial update"), "{}", parsed.detail);
        assert!(!root.join("patch.ver").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unfinished_update_still_shows_the_installed_version() {
        let root = install_with(Some("30260904_1"), true);
        std::fs::create_dir_all(root.join(".xi-updater")).unwrap();
        std::fs::write(root.join(".xi-updater/transaction.json"), b"journal").unwrap();
        let shown = local_report(&root);
        assert_eq!(shown.state, PatchState::Interrupted);
        assert_eq!(shown.local_version, "30260904_1", "the record decodes, so the page shows it");
        assert_eq!(shown.remote_version, t("Not checked"));
        // An undecodable record stays a placeholder: there is no version to show.
        std::fs::write(root.join("patch.ver"), vec![7u8; 288]).unwrap();
        assert_eq!(local_report(&root).local_version, t("Unavailable"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_files_report_reads_the_record_and_opens_no_connection() {
        let root = install_with(Some("30260904_1"), true);
        let files = crate::updater::Report { version: "30260904_1".into(), total: 2, current: 2, verified: true, ..Default::default() };
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Check);
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()), opened);
        assert_eq!(shown.local_version, "30260904_1");
        assert_eq!(shown.remote_version, t("Not checked"), "the official version is the page's remembered one, not this report's");
        assert!(!crate::official_versions::is_version(&shown.remote_version));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_file_outcomes_override_a_matching_version_record() {
        let root = install_with(Some("30260904_1"), true);
        let mut files = crate::updater::Report { version: "30260904_1".into(), total: 2,
            current: 1, missing: 1, ..Default::default() };
        // Check is offline: it reports the files, never a server version.
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Check);
        assert_eq!(shown.state, PatchState::Unknown);
        assert_eq!(shown.remote_version, t("Not checked"));
        assert_eq!(shown.files_verified, Some(false));
        assert!(shown.detail.contains("1 missing") && shown.detail.contains("Repair files"), "{}", shown.detail);
        // An update that left a modification it doesn't change in place commits the version,
        // but its files are not all official.
        files.missing = 0; files.modified = 2; files.kept = 1; files.left = 1; files.verified = true;
        files.all_files_checked = true;
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Update);
        assert_eq!(shown.state, PatchState::Current);
        assert_eq!(shown.files_verified, Some(false));
        assert!(shown.detail.contains("1 modified") && shown.detail.contains("left in place"), "{}", shown.detail);
        // A routine update read only what it changed: the version, not every file.
        files.modified = 0; files.kept = 0; files.left = 0; files.all_files_checked = false;
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Update);
        assert_eq!(shown.state, PatchState::Current);
        assert_eq!(shown.files_verified, None);
        // Every file read in full and official.
        files.all_files_checked = true;
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Update);
        assert_eq!(shown.state, PatchState::Current);
        assert_eq!(shown.files_verified, Some(true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_clean_file_check_does_not_hide_a_missing_applied_version_record() {
        let root = install_with(None, true);
        let files = crate::updater::Report { version: "30260904_1".into(), total: 2,
            current: 2, verified: true, ..Default::default() };
        let shown = report_after_operation(&root, &files, crate::updater::Mode::Check);
        assert_eq!(shown.files_verified, Some(true));
        assert_eq!(shown.state, PatchState::Required);
        assert_eq!(local_report(&root).files_verified, None);
        assert_eq!(report_with_remote(&root, &files.version).files_verified, None);
        assert!(!root.join("patch.ver").exists(), "Check files never initializes an applied record");
        std::fs::remove_dir_all(root).unwrap();
    }

    use crate::viewer::ConnectVerdict;

    /// The 103-byte replies in the same capture (W2U 04:59:18.656, W20 05:01:34.363):
    /// type 8, label "registered", the server address, then the version.
    const STOCK_VIEWER_REPLY: &str = "670000001cc5a086504f4c500800000097644e4e0100000072656769737465726564003132342e3135302e3135362e31303700300000000000000000000000000000000000000000000000000000000000000000000000000b00000032303131303832395f4500";
    const STOCK_FFXI_REPLY: &str = "67000000067e0cd2504f4c50080000004ba19a6a0100000072656769737465726564003132342e3135302e3135362e31303700300000000000000000000000000000000000000000000000000000000000000000000000000b00000033303236303930345f3100";

    #[test]
    fn stock_replies_parse_to_their_versions() {
        let (viewer, ffxi) = (hex(STOCK_VIEWER_REPLY), hex(STOCK_FFXI_REPLY));
        assert_eq!((viewer.len(), ffxi.len()), (103, 103));
        assert_eq!(parse_reply_for(&viewer, true).unwrap(), "20110829_E");
        assert_eq!(parse_reply(&ffxi).unwrap(), "30260904_1");
        // Each product's validator refuses the other's version format.
        assert!(parse_reply(&viewer).is_err() && parse_reply_for(&ffxi, true).is_err());
        // Short, long and re-lengthed frames are malformed.
        let mut long = viewer.clone(); long.push(0);
        let mut relength = long.clone(); relength[..4].copy_from_slice(&104u32.to_le_bytes());
        for frame in [&viewer[..102], &viewer[..HEADER], &long[..], &relength[..]] {
            assert!(parse_reply_for(frame, true).is_err(), "{} bytes", frame.len());
        }
    }

    #[test]
    fn the_viewer_check_sends_the_viewers_own_installed_version() {
        let viewer = install_with(Some("20110829_E"), false);
        assert_eq!(read_local(&viewer, Product::Viewer), Local::Installed("20110829_E".into()));
        let mut sent = Vec::new();
        let verdict = viewer_connect_with(&viewer, |request| { sent = request.to_vec(); Ok(hex(STOCK_VIEWER_REPLY)) });
        assert_eq!(sent, hex(STOCK_VIEWER));
        assert_eq!(verdict, ConnectVerdict::Current { version: "20110829_E".into() });
        assert_eq!((verdict.reason(), verdict.message().as_str()), (None, ""));
        let down = viewer_connect_with(&viewer, |_| Err("down".into()));
        assert!(matches!(&down, ConnectVerdict::Unreachable(e) if e.contains("down")), "{down:?}");
        assert_eq!(down.reason(), Some("unreachable"));
        let unreadable = install_with(None, true);
        std::fs::write(unreadable.join("patch.ver"), [1u8; 288]).unwrap();
        let failed = viewer_connect_with(&unreadable, |_| panic!("nothing may be sent"));
        assert!(matches!(failed, ConnectVerdict::Failed(_)) && failed.reason() == Some("failed"), "{failed:?}");
        std::fs::remove_dir_all(viewer).unwrap(); std::fs::remove_dir_all(unreadable).unwrap();
    }

    fn viewer_reply(version: &str) -> Vec<u8> {
        let mut frame = reply();
        assert_eq!(version.len(), 10);
        frame[92..102].copy_from_slice(version.as_bytes());
        let digest = md5::compute(&frame[8..]);
        frame[4..8].copy_from_slice(&digest.0[..4]);
        frame
    }

    #[test]
    fn viewer_initialization_and_a_version_mismatch_require_its_own_update() {
        let fresh = install_with(None, false);
        let verdict = viewer_connect_with(&fresh, |request| {
            assert_eq!(request, Product::Viewer.request(""));
            Ok(viewer_reply("20110829_E"))
        });
        assert_eq!(verdict, ConnectVerdict::Outdated { local: "Not initialized".into(), remote: "20110829_E".into() });
        let old = install_with(Some("20070101_E"), false);
        let verdict = viewer_connect_with(&old, |request| {
            assert_eq!(request, Product::Viewer.request("20070101_E"));
            Ok(viewer_reply("20110829_E"))
        });
        assert_eq!(verdict, ConnectVerdict::Outdated { local: "20070101_E".into(), remote: "20110829_E".into() });
        assert_eq!(verdict.reason(), Some("outdated"));
        assert!(verdict.message().contains("20070101_E") && verdict.message().contains("20110829_E"));
        std::fs::remove_dir_all(fresh).unwrap(); std::fs::remove_dir_all(old).unwrap();
    }

    /// The tester's case (2026-11-12): a current Japanese Viewer (20260929_0), asked about as
    /// the US/EU Viewer (`W2U`), was told to update to that Viewer's 20261022_0. Asked about
    /// with its own platform tag it is compared with its own product's version. The US frame
    /// keeps its tag.
    #[test]
    fn an_eu_or_jp_viewer_is_asked_about_with_its_own_platform_tag() {
        let viewer = install_with(Some("20260929_0"), false);
        let japan = crate::updater::polp::Tags { platform: *b"W20\0", title: *b"1000" };
        let mut sent = Vec::new();
        let verdict = viewer_verdict(&viewer, &japan, |request| { sent = request.to_vec(); Ok(viewer_reply("20260929_0")) });
        assert_eq!(verdict, ConnectVerdict::Current { version: "20260929_0".into() });
        assert_eq!((sent.len(), &sent[8..12], &sent[16..24], &sent[24..34]), (88, &b"POLP"[..], &b"W20\x001000"[..], &b"20260929_0"[..]));
        // The US frame for the same version differs in the tag (and the checksum over it) alone.
        let us = Product::Viewer.request("20260929_0");
        assert_eq!((&us[16..20], &us[20..]), (&b"W2U\0"[..], &sent[20..]));
        assert_ne!(us[4..8], sent[4..8]);
        // Its own product's newer version is still an update, named with what it has.
        let newer = viewer_verdict(&viewer, &japan, |_| Ok(viewer_reply("20261201_0")));
        assert_eq!(newer, ConnectVerdict::Outdated { local: "20260929_0".into(), remote: "20261201_0".into() });
        // Japan's name for the game's patch host; the game's frame is the same everywhere.
        assert_eq!((crate::dns::GAME_PATCH_HOST_JP, crate::dns::GAME_PATCH_HOST), ("pc001W20.pol.com", "pc001W2U.pol.com"));
        std::fs::remove_dir_all(viewer).unwrap();
    }

    /// One real question to the Japanese Viewer's patch server, as a Japanese Viewer asks it:
    /// `W20`/`1000` with `CRYSTAL_JP_VIEWER_VERSION` (the tester's installed 20260929_0 by
    /// default) to pt004.pol.com:54000. Prints the server's answer for the Japanese Viewer;
    /// "current" for a version a Japanese tester's up-to-date Viewer reports confirms the tag.
    /// Reads no install and sends nothing but the version frame.
    #[test]
    #[ignore = "one read-only version check to the official Japanese Viewer patch server; requires network"]
    fn the_japanese_viewer_stream_answers_for_its_own_tag() {
        let version = std::env::var("CRYSTAL_JP_VIEWER_VERSION").unwrap_or_else(|_| "20260929_0".into());
        let viewer = install_with(Some(&version), false);
        let check = viewer_connect_check_for(&viewer, "pt004.pol.com", *b"W20\0");
        eprintln!("pt004.pol.com:54000 W20/1000 with {version}: {:?} ({})", check.verdict, check.timing.phases().unwrap_or_default());
        assert!(matches!(check.verdict, ConnectVerdict::Current { .. } | ConnectVerdict::Outdated { .. }), "{:?}", check.verdict);
        std::fs::remove_dir_all(viewer).unwrap();
    }

    #[test]
    fn viewer_rejection_bad_checksum_and_ffxi_version_cannot_claim_it_is_current() {
        let viewer = install_with(Some("20110829_E"), false);
        let mut reject = viewer_reply("20110829_E"); reject[12..16].copy_from_slice(&5u32.to_le_bytes());
        let digest = md5::compute(&reject[8..]); reject[4..8].copy_from_slice(&digest.0[..4]);
        let mut bad = viewer_reply("20110829_E"); bad[4] ^= 1;
        for frame in [reject, bad, reply(), Vec::new()] {
            assert!(matches!(viewer_connect_with(&viewer, |_| Ok(frame)), ConnectVerdict::Unreachable(_)));
        }
        assert!(parse_reply(&viewer_reply("20110829_E")).is_err());
        assert_eq!(parse_reply_for(&viewer_reply("20110829_E"), true).unwrap(), "20110829_E");
        std::fs::remove_dir_all(viewer).unwrap();
    }

    fn serve(reply: Vec<u8>, chunk: usize, linger: Duration) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 88];
            stream.read_exact(&mut request).unwrap();
            for part in reply.chunks(chunk.max(1)) {
                stream.write_all(part).unwrap();
                stream.flush().unwrap();
            }
            std::thread::sleep(linger);
            request
        });
        (address, server)
    }

    #[test]
    fn handles_a_fragmented_tcp_version_reply() {
        let (address, server) = serve(reply(), 3, Duration::ZERO);
        let frame = exchange(&address, &Product::Ffxi.request("30260904_1"), Duration::from_secs(5)).unwrap();
        assert_eq!(parse_reply(&frame).unwrap(), "30260904_1");
        assert_eq!(server.join().unwrap(), hex(STOCK_FFXI));
    }

    #[test]
    fn a_silent_server_times_out_instead_of_hanging_a_launch() {
        let (address, server) = serve(Vec::new(), 1, Duration::from_millis(800));
        let started = Instant::now();
        let error = exchange(&address, &Product::Ffxi.request("30260904_1"), Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("reply"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(790));
        server.join().unwrap();
    }

    /// A loopback patch server for `connections` exchanges: per connection, the request,
    /// the reply's write time and when the client's FIN (EOF) arrived.
    fn serve_closes(reply: Vec<u8>, connections: usize) -> (String, std::thread::JoinHandle<Vec<(Vec<u8>, Instant, Instant)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || (0..connections).map(|_| {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 88];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(&reply).unwrap();
            let replied = Instant::now();
            let mut rest = Vec::new();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.read_to_end(&mut rest).unwrap();
            assert!(rest.is_empty(), "nothing follows the request");
            (request, replied, Instant::now())
        }).collect());
        (address, server)
    }

    #[test]
    fn the_client_closes_like_stock_after_the_reply_and_returns_only_once_closed() {
        let (address, server) = serve_closes(hex(STOCK_FFXI_REPLY), 1);
        let frame = exchange(&address, &Product::Ffxi.request("30260904_1"), Duration::from_secs(5)).unwrap();
        let returned = Instant::now();
        assert_eq!(parse_reply(&frame).unwrap(), "30260904_1");
        let (request, replied, eof) = server.join().unwrap().remove(0);
        assert_eq!(request, hex(STOCK_FFXI));
        // Stock: 111.9-114.7 ms from reply to FIN (stock_socket.rs). Slack above for load.
        let (low, _) = crate::stock_socket::CLOSE_AFTER_REPLY;
        assert!(eof - replied >= low - Duration::from_millis(5) && eof - replied <= Duration::from_millis(400), "{:?}", eof - replied);
        assert!(returned >= replied + low - Duration::from_millis(5), "returned {:?} after the reply", returned - replied);
    }

    #[test]
    fn the_ffxi_check_returns_at_the_reply_and_still_closes_like_stock() {
        let (address, server) = serve_closes(hex(STOCK_FFXI_REPLY), 1);
        let (frame, attempt) = exchange_closing(&address, &Product::Ffxi.request("30260904_1"), Duration::from_secs(5), false);
        let returned = Instant::now();
        let frame = frame.unwrap();
        let phases = CheckTiming { attempts: vec![attempt], ..CheckTiming::default() }.phases().unwrap();
        assert!(phases.starts_with("resolve ") && phases.contains(", reply ") && phases.ends_with("close in the background"), "{phases}");
        assert_eq!(parse_reply(&frame).unwrap(), "30260904_1");
        let (_, replied, eof) = server.join().unwrap().remove(0);
        let (low, _) = crate::stock_socket::CLOSE_AFTER_REPLY;
        assert!(returned < replied + low - Duration::from_millis(20), "the start does not wait for the close");
        assert!(eof - replied >= low - Duration::from_millis(5) && eof - replied <= Duration::from_millis(400), "{:?}", eof - replied);
    }

    #[test]
    fn every_connect_check_is_its_own_w2u_on_a_stock_socket() {
        let viewer = install_with(Some("20110829_E"), false);
        let (address, server) = serve_closes(hex(STOCK_VIEWER_REPLY), 2);
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        for _ in 0..2 {
            assert_eq!(viewer_connect_at(&viewer, &address), ConnectVerdict::Current { version: "20110829_E".into() });
        }
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()) - opened, 2, "no cached verdict, no std socket");
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|(request, _, _)| request == &hex(STOCK_VIEWER)));
        // A closed port is Unreachable after a send attempt, never Current.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
        assert!(matches!(viewer_connect_at(&viewer, &free), ConnectVerdict::Unreachable(_)));
        std::fs::remove_dir_all(viewer).unwrap();
    }

    // --- round 2 (07d §2, §6 S2): loopback plus an injected resolver and sleeper, never real DNS ---

    use crate::dns::{NetError, NetStage};
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// Attempt 1 answers `first` after `first_delay`; retries take `fresh` in order (the
    /// last one repeats). Sleeps are counted, not slept, and advance `elapsed`.
    struct FakeNet {
        first: Result<Vec<SocketAddrV4>, NetError>,
        first_delay: Duration,
        fresh: Vec<Result<Vec<Ipv4Addr>, NetError>>,
        /// How long each fresh lookup takes (really slept: the exchange deadline sees it).
        fresh_delay: Duration,
        fresh_calls: u32,
        sleeps: Vec<Duration>,
        forgot: u32,
    }
    impl FakeNet {
        fn new(first: Result<Vec<SocketAddrV4>, NetError>, fresh: Vec<Result<Vec<Ipv4Addr>, NetError>>) -> FakeNet {
            FakeNet { first, first_delay: Duration::ZERO, fresh, fresh_delay: Duration::ZERO, fresh_calls: 0, sleeps: Vec::new(), forgot: 0 }
        }
    }
    impl Net for FakeNet {
        fn resolve(&mut self, host: &str, port: u16) -> Result<Vec<SocketAddrV4>, NetError> {
            // Each check looks up its own product's host, as stock does.
            assert_eq!(host, if port == Product::Ffxi.port() { crate::dns::GAME_PATCH_HOST } else { crate::dns::VIEWER_PATCH_HOST });
            std::thread::sleep(self.first_delay);
            self.first.clone()
        }
        fn resolve_fresh(&mut self, host: &str, limit: Duration) -> Result<Vec<Ipv4Addr>, NetError> {
            assert_eq!(host, crate::dns::VIEWER_PATCH_HOST);
            assert!(limit <= CHECK_TIMEOUT && !limit.is_zero(), "{limit:?}");
            std::thread::sleep(self.fresh_delay);
            self.fresh_calls += 1;
            if self.fresh.len() > 1 { self.fresh.remove(0) } else { self.fresh[0].clone() }
        }
        fn sleep(&mut self, pause: Duration) { self.sleeps.push(pause); }
        fn elapsed(&self, since: Instant) -> Duration { since.elapsed() + self.sleeps.iter().sum::<Duration>() }
        fn forget_negative(&mut self) { self.forgot += 1; }
    }
    fn dns_error(code: i32) -> NetError {
        NetError::from_io(NetStage::Resolve, &std::io::Error::from_raw_os_error(code), Duration::ZERO)
    }
    fn os_text(code: i32) -> String { dns_error(code).text }
    fn w2u(dir: &Path, port: u16, limit: Duration, net: &mut FakeNet) -> crate::viewer::ConnectCheck {
        viewer_connect_net(dir, crate::dns::VIEWER_PATCH_HOST, port, limit, net)
    }

    #[test]
    fn an_instant_dns_failure_retries_with_fresh_dns_until_one_connection_reaches_the_server() {
        let viewer = install_with(Some("20110829_E"), false);
        let (address, server) = serve_closes(hex(STOCK_VIEWER_REPLY), 1);
        let port = address.rsplit_once(':').unwrap().1.parse().unwrap();
        let mut net = FakeNet::new(Err(dns_error(11001)), vec![Err(dns_error(9003)), Err(dns_error(9003)), Ok(vec![Ipv4Addr::LOCALHOST])]);
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        let check = w2u(&viewer, port, CHECK_TIMEOUT, &mut net);
        assert_eq!(check.verdict, ConnectVerdict::Current { version: "20110829_E".into() });
        assert_eq!(net.fresh_calls, 3, "each retry resolves fresh");
        assert_eq!(net.sleeps, vec![RETRY_BACKOFF; 3], "1 s apart");
        assert_eq!(net.forgot, 1, "the negative-cache rule after a retried W2U");
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()) - opened, 1, "one connection, the successful one");
        let seen = server.join().unwrap();
        assert_eq!((seen.len(), &seen[0].0), (1, &hex(STOCK_VIEWER)));
        let timing = &check.timing;
        assert_eq!(timing.attempts.iter().map(|a| a.fresh_dns).collect::<Vec<_>>(), [false, true, true, true]);
        assert_eq!(timing.network_wait, Duration::from_secs(3));
        assert!(timing.attempts[3].close.is_some() && timing.attempts[3].error.is_none());
        let phases = timing.phases().unwrap();
        // The fake lookup fails at once, but a loaded machine can still measure a millisecond.
        let first = format!(" ms, failed: {} (11001); attempt 2 (fresh DNS): resolve ", os_text(11001));
        assert!(phases.starts_with("attempt 1: resolve ") && phases.contains(&first), "{phases}");
        assert!(phases.contains(&format!("failed: {} (9003); attempt 3 (fresh DNS)", os_text(9003))), "{phases}");
        assert!(phases.contains("; attempt 4 (fresh DNS): resolve ") && phases.contains(" ms, reply ") && phases.ends_with(" ms; waited 3.0 s for the network"), "{phases}");
        std::fs::remove_dir_all(viewer).unwrap();
    }

    /// Finding: a retry whose fresh lookup answers late (DNS retransmits during a network
    /// bring-up) still gets a whole exchange, never an aborted or absent :54000 connect
    /// in front of Open(1).
    #[test]
    fn a_late_fresh_resolution_still_gets_a_whole_exchange() {
        let viewer = install_with(Some("20110829_E"), false);
        let (address, server) = serve_closes(hex(STOCK_VIEWER_REPLY), 1);
        let port = address.rsplit_once(':').unwrap().1.parse().unwrap();
        let limit = Duration::from_millis(400);
        let mut net = FakeNet::new(Err(dns_error(11001)), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
        // The answer comes after the attempt's whole limit has passed.
        net.fresh_delay = limit + Duration::from_millis(50);
        let check = w2u(&viewer, port, limit, &mut net);
        assert_eq!(check.verdict, ConnectVerdict::Current { version: "20110829_E".into() }, "{:?}", check.timing.phases());
        assert_eq!(net.fresh_calls, 1);
        assert_eq!(server.join().unwrap().len(), 1, "one whole exchange");
        std::fs::remove_dir_all(viewer).unwrap();
    }

    #[test]
    fn the_retry_gives_up_at_network_wait_with_the_last_cause_and_sends_nothing() {
        let viewer = install_with(Some("20110829_E"), false);
        let mut net = FakeNet::new(Err(dns_error(11001)), vec![Err(dns_error(11001)), Err(dns_error(9003))]);
        let opened = crate::stock_socket::OPENED.with(|n| n.get());
        let check = w2u(&viewer, 54000, CHECK_TIMEOUT, &mut net);
        let cause = format!("pt008.pol.com: {} (9003)", os_text(9003));
        assert_eq!(check.verdict, ConnectVerdict::NetworkNotReady(cause.clone()));
        assert_eq!(check.verdict.reason(), Some("network"));
        assert!(check.verdict.message().contains(&cause) && crate::viewer::is_network_not_ready_message(&check.verdict.message()));
        assert_eq!(crate::stock_socket::OPENED.with(|n| n.get()), opened, "no socket while the network is not ready");
        // The last attempt starts by NETWORK_WAIT (VIEWER_CHECK_BUDGET, 07d §3).
        let waited = net.sleeps.iter().sum::<Duration>();
        assert!(waited <= NETWORK_WAIT && waited > NETWORK_WAIT - Duration::from_millis(200), "{waited:?}");
        assert_eq!(check.timing.network_wait, waited);
        assert!(net.sleeps.iter().all(|pause| *pause <= RETRY_BACKOFF));
        assert_eq!((net.fresh_calls as usize, check.timing.attempts.len()), (net.sleeps.len(), net.sleeps.len() + 1));
        assert_eq!(net.forgot, 1);
        assert!(check.timing.phases().unwrap().ends_with("; waited 12.0 s for the network"));
        std::fs::remove_dir_all(viewer).unwrap();
    }

    #[test]
    fn a_local_connect_failure_also_waits_for_the_network() {
        // Connecting to 0.0.0.0 fails in the local stack (WSAEADDRNOTAVAIL 10049) at once.
        let viewer = install_with(Some("20110829_E"), false);
        let unusable = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 54000);
        let mut net = FakeNet::new(Ok(vec![unusable]), vec![Ok(vec![Ipv4Addr::UNSPECIFIED])]);
        let check = w2u(&viewer, 54000, CHECK_TIMEOUT, &mut net);
        let first = check.timing.attempts[0].error.clone().unwrap();
        assert_eq!((first.stage, first.code), (NetStage::Connect, Some(10049)));
        assert!(net.fresh_calls > 0, "a local connect failure retries");
        assert!(matches!(&check.verdict, ConnectVerdict::NetworkNotReady(cause) if cause.ends_with("(10049)")), "{:?}", check.verdict);
        std::fs::remove_dir_all(viewer).unwrap();
    }

    #[test]
    fn an_unanswered_w2u_never_retries() {
        let viewer = install_with(Some("20110829_E"), false);
        let at = |address: &str| -> SocketAddrV4 { address.parse().unwrap() };
        let no_retry = |net: &FakeNet| { assert_eq!((net.fresh_calls, net.sleeps.len(), net.forgot), (0, 0, 0)); };
        // Refused (10061: the server answered).
        let free = at(&std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string());
        let mut net = FakeNet::new(Ok(vec![free]), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
        let check = w2u(&viewer, free.port(), CHECK_TIMEOUT, &mut net);
        assert!(matches!(check.verdict, ConnectVerdict::Unreachable(_)), "{:?}", check.verdict);
        assert_eq!(check.timing.attempts[0].error.as_ref().unwrap().stage, NetStage::Connect);
        no_retry(&net);
        // A silent server: reply timeout.
        let (address, server) = serve(Vec::new(), 1, Duration::from_millis(800));
        let mut net = FakeNet::new(Ok(vec![at(&address)]), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
        let check = w2u(&viewer, 0, Duration::from_millis(300), &mut net);
        assert!(matches!(check.verdict, ConnectVerdict::Unreachable(_)));
        assert_eq!(check.timing.phases().unwrap().rsplit_once(", ").unwrap().1, "failed: timed out");
        no_retry(&net); server.join().unwrap();
        // Reply errors: a closed connection, then an invalid frame.
        for reply in [Vec::new(), vec![0u8; HEADER]] {
            let (address, server) = serve(reply, 16, Duration::ZERO);
            let mut net = FakeNet::new(Ok(vec![at(&address)]), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
            let check = w2u(&viewer, 0, CHECK_TIMEOUT, &mut net);
            assert!(matches!(check.verdict, ConnectVerdict::Unreachable(_)), "{:?}", check.verdict);
            assert_eq!(check.timing.attempts[0].error.as_ref().unwrap().stage, NetStage::Reply);
            no_retry(&net); server.join().unwrap();
        }
        // A slow (> 100 ms) DNS failure: the resolver answered; not "not ready".
        let mut net = FakeNet::new(Err(dns_error(11001)), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
        net.first_delay = Duration::from_millis(150);
        let check = w2u(&viewer, 54000, CHECK_TIMEOUT, &mut net);
        assert!(matches!(&check.verdict, ConnectVerdict::Unreachable(e) if e.contains("11001")), "{:?}", check.verdict);
        assert!(!check.timing.attempts[0].error.as_ref().unwrap().network_not_ready());
        no_retry(&net);
        std::fs::remove_dir_all(viewer).unwrap();
    }

    #[test]
    fn the_w20_never_retries_and_returns_its_phases_on_any_thread() {
        let install = install_with(Some("30260904_1"), true);
        let (address, server) = serve_closes(hex(STOCK_FFXI_REPLY), 1);
        let address: SocketAddrV4 = address.parse().unwrap();
        let dir = install.clone();
        let (clean, failed) = std::thread::spawn(move || {
            let mut net = FakeNet::new(Ok(vec![address]), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
            let clean = ffxi_check(&dir, Product::Ffxi.host(), Product::Ffxi.port(), &mut net);
            let mut down = FakeNet::new(Err(dns_error(11001)), vec![Ok(vec![Ipv4Addr::LOCALHOST])]);
            let failed = ffxi_check(&dir, Product::Ffxi.host(), Product::Ffxi.port(), &mut down);
            assert_eq!((down.fresh_calls, down.sleeps.len(), down.forgot), (0, 0, 0), "W20 never retries");
            (clean, failed)
        }).join().unwrap();
        assert!(matches!(clean.0, LaunchCheck::Start(_)));
        let phases = clean.1.phases().unwrap();
        assert!(phases.starts_with("resolve ") && phases.contains(", connect ") && phases.ends_with(", close in the background"), "{phases}");
        assert!(matches!(failed.0, LaunchCheck::Unverified(_)));
        assert_eq!(failed.1.phases().unwrap(), format!("resolve 0 ms, failed: {} (11001)", os_text(11001)));
        assert_eq!(server.join().unwrap().len(), 1);
        std::fs::remove_dir_all(install).unwrap();
    }

    #[test]
    fn phase_texts_follow_07d() {
        let ms = Duration::from_millis;
        let clean = Attempt { resolve: Some(ms(3)), connect: Some(ms(180)), reply: Some(ms(164)), close: Some(ms(104)), ..Attempt::default() };
        let one = |attempt: Attempt| CheckTiming { attempts: vec![attempt], ..CheckTiming::default() }.phases().unwrap();
        // Round 1's text, byte for byte.
        assert_eq!(one(clean.clone()), "resolve 3 ms, connect 180 ms, reply 164 ms, close 104 ms");
        assert_eq!(one(Attempt { close: None, close_in_background: true, ..clean.clone() }),
            "resolve 3 ms, connect 180 ms, reply 164 ms, close in the background");
        let error = |stage, code, text: &str| Some(NetError { stage, code, text: text.into(), after: Duration::ZERO });
        let no_host = Attempt { resolve: Some(ms(0)), error: error(NetStage::Resolve, Some(11001), "No such host is known."), ..Attempt::default() };
        assert_eq!(one(no_host.clone()), "resolve 0 ms, failed: No such host is known. (11001)");
        assert_eq!(one(Attempt { resolve: Some(ms(2)), connect: Some(ms(10003)), error: error(NetStage::Connect, None, "timed out"), ..Attempt::default() }),
            "resolve 2 ms, connect 10003 ms, failed: timed out");
        let several = CheckTiming {
            attempts: vec![
                no_host,
                Attempt { fresh_dns: true, resolve: Some(ms(1)), error: error(NetStage::Resolve, Some(9003), "DNS name does not exist."), ..Attempt::default() },
                Attempt { fresh_dns: true, resolve: Some(ms(38)), connect: Some(ms(181)), reply: Some(ms(165)), close: Some(ms(103)), ..Attempt::default() },
            ],
            network_wait: Duration::from_secs(2),
        };
        assert_eq!(several.phases().unwrap(), "attempt 1: resolve 0 ms, failed: No such host is known. (11001); attempt 2 (fresh DNS): resolve 1 ms, failed: DNS name does not exist. (9003); attempt 3 (fresh DNS): resolve 38 ms, connect 181 ms, reply 165 ms, close 103 ms; waited 2.0 s for the network");
        // Nothing sent, nothing to log.
        assert_eq!(CheckTiming::default().phases(), None);
    }

    #[test]
    fn the_w2u_budget_covers_the_retry_window_and_the_hosts_agree() {
        assert_eq!(VIEWER_CHECK_BUDGET, NETWORK_WAIT + CHECK_TIMEOUT + CHECK_TIMEOUT + STOCK_CLOSE_MAX);
        assert!(STOCK_CLOSE_MAX >= crate::stock_socket::CLOSE_AFTER_REPLY.1);
        // One product identity: its server string is its stock host and `53000 + number` port.
        for product in [Product::Viewer, Product::Ffxi] {
            assert_eq!(product.server(), format!("{}:{}", product.host(), product.port()));
        }
        assert_eq!((Product::Viewer.port(), Product::Ffxi.port()), (54000, 53001));
        assert_eq!((Product::Viewer.host(), Product::Ffxi.host()), (crate::dns::VIEWER_PATCH_HOST, crate::dns::GAME_PATCH_HOST));
    }

    /// 07d §3: the loader pumps frames for its VIEWER_CHECK_LIMIT while the W2U runs, so
    /// that limit must outlast the longest `check_at_connect_timed` (a retry started just
    /// before NETWORK_WAIT, a full exchange and the stock close).
    #[test]
    fn the_loader_waits_out_the_whole_w2u() {
        let source = include_str!("../crystal-loader/src/native_auth.rs");
        let line = source.lines().find(|line| line.contains("const VIEWER_CHECK_LIMIT: Duration ="))
            .expect("crystal-loader native_auth.rs defines VIEWER_CHECK_LIMIT");
        let secs: u64 = line.split("Duration::from_secs(").nth(1).and_then(|rest| rest.split(')').next())
            .and_then(|n| n.trim().parse().ok()).unwrap_or_else(|| panic!("VIEWER_CHECK_LIMIT is a whole-second Duration::from_secs: {line}"));
        assert!(Duration::from_secs(secs) > VIEWER_CHECK_BUDGET, "loader limit {secs} s, W2U budget {VIEWER_CHECK_BUDGET:?}");
    }
}
