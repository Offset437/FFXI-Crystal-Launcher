//! Crystal's unified PlayOnline and FFXI verification, update, and repair.
//! The packet, manifest, and patch codecs are implemented in this crate.
//! Downloaded manifests describe desired files; only a verified transaction can
//! advance patch.ver. Check never creates updater state or changes the install.
mod lz;
mod manifest;
mod installer_baseline;
pub(crate) mod polp;
#[cfg(test)]
mod live_test;
#[cfg(test)]
mod product_tests;
#[cfg(test)]
mod viewer_live_test;
#[cfg(test)]
mod fresh_install_bench;
#[cfg(test)]
mod traffic_tests;

use crate::i18n::{t, tf};
use crate::operation::{Operation, Stage, UpdateWork};
use manifest::{FileHistory, FileSig, Manifest};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(test)]
const SERVER: &str = "pc001W2U.pol.com:53001";
const MAX_MANIFEST: usize = 64 * 1024 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const STATE_DIR: &str = ".xi-updater";
/// User copies of modified files an update or repair replaced, by their relative path.
/// Never cleaned up by Crystal.
const KEPT_DIR: &str = ".xi-updater/kept";
/// Present while the next update must check every file in full: an interrupted
/// update was found, or a transaction failed after touching the install.
const FULL_CHECK_MARKER: &str = ".xi-updater/full-check";
const SIGNATURE_BUFFER_LEN: usize = 1 << 20;
const MAX_CHECK_WORKERS: usize = 8;
const ACTIVE_GAME_CHECK_WORKERS: usize = 4;
// Patch-server traffic follows stock POL (captures 2026-10-13 00:21 and the complete
// update stock-update-20261013-004932: stock POL updating FFXI 30260904_1 → 30261006_0
// on pc001W2U.pol.com:53001 = 124.150.156.107, every SYN with the `stock_socket`
// options): the W20 version request on its own connection, FIN ~100 ms
// after its 103 B reply; a 24 B POLP command 1 on a new connection for the file list,
// FIN ~114 ms after the reply; then ONE connection carries every download as POLP
// command 3 requests (byte-identical to `polp::file_request`), strictly one in flight:
// request → the whole reply (≤ one 64 KB chunk) → next request. A file is fetched as
// consecutive 64 KB chunks from offset 0, each next chunk requested at once (median
// 0.7 ms after the reply); the next file follows its predecessor's local processing
// (median 18 ms, growing with size, 3-239 ms). 422 requests for 360 files (298 deltas,
// 62 full files), 14 MB, FIN 100 ms after the last reply. No version check followed the
// update. Crystal used 8 parallel plain sockets before. Each rule here is one setting.
// Every wire-visible pause runs on a high-resolution timer (`precise_sleep`).
// Downloads use that one stop-and-wait connection (`stage_plan_shared`, `Session::exchange`).
/// Least time from the last reply of one file to the request for the next file, drawn per
/// file by what the previous file was (`next_file_pause`); Crystal's own processing of the
/// finished file counts toward it. Stock's gap there is processing time
/// (stock-update-20261013-004932, 359 new-file requests): after a delta never under
/// 11.5 ms (delta → delta min 13.4, median 19.4, n=245; delta → direct min 11.5, n=52),
/// longer after big files; after a 24 B direct file median 8.4, min 3.3 (n=46).
const PAUSE_AFTER_DELTA: (Duration, Duration) = (Duration::from_millis(12), Duration::from_millis(25));
const PAUSE_AFTER_SMALL_DIRECT: (Duration, Duration) = (Duration::from_millis(3), Duration::from_millis(9));
/// A direct file up to this transfer size is "small" for `next_file_pause`.
const SMALL_DIRECT: u64 = 1024;
/// From Crystal's FIN on a patch connection to the SYN of its next connection to the same
/// server. Stock (captures 2026-10-13 00:21 and 00:49): W20 FIN → idle prompt SYN +109 and
/// +110 ms, its FIN → file-list SYN +107 and +105 ms; never a SYN within 100 ms of a FIN.
const NEXT_CONNECTION_GAP: (Duration, Duration) = (Duration::from_millis(103), Duration::from_millis(112));
/// Resolve plus connect for one patch-server connection.
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const MAX_BENCHMARK_CHECK_WORKERS: usize = 12;

/// The two products the patch service knows, and everything that identifies one: its patch
/// host and port (`53000 + product number`), its POLP tags, its registry title and record key,
/// and its version-stamp rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Product { Ffxi, Viewer }
impl Product {
    fn label(self) -> &'static str {
        match self { Self::Ffxi => t("FINAL FANTASY XI"), Self::Viewer => t("PlayOnline") }
    }
    /// The patch host stock looks up (`dns::GAME_PATCH_HOST`, `dns::VIEWER_PATCH_HOST`).
    pub(crate) fn host(self) -> &'static str {
        match self { Self::Ffxi => crate::dns::GAME_PATCH_HOST, Self::Viewer => crate::dns::VIEWER_PATCH_HOST }
    }
    /// W20 :53001, W2U :54000 (53000 + the product number).
    pub(crate) fn port(self) -> u16 { 53000 + self.number() as u16 }
    /// "host:port" (a test keeps it equal to `host()` and `port()`).
    pub(crate) fn server(self) -> &'static str {
        match self { Self::Ffxi => "pc001W2U.pol.com:53001", Self::Viewer => "pt008.pol.com:54000" }
    }
    /// The product id: the registry `Interface` value name and the POLP title tag.
    pub(crate) fn title(self) -> &'static str {
        match self { Self::Ffxi => "0001", Self::Viewer => "1000" }
    }
    /// The product number (1 FFXI, 1000 the Viewer): the port offset and the record-key addend.
    pub(crate) fn number(self) -> u64 {
        match self { Self::Ffxi => 1, Self::Viewer => 1000 }
    }
    pub(crate) fn tags(self) -> polp::Tags {
        polp::Tags { platform: match self { Self::Ffxi => *b"W20\0", Self::Viewer => *b"W2U\0" },
            title: match self { Self::Ffxi => *b"0001", Self::Viewer => *b"1000" } }
    }
    /// The stock 88-byte version frame carrying `installed` (empty only for a fresh install).
    pub(crate) fn request(self, installed: &str) -> Vec<u8> {
        polp::version(&self.tags(), installed.as_bytes())
    }
    pub(crate) fn discovery(self) -> crate::discovery::Product {
        match self { Self::Ffxi => crate::discovery::Product::Ffxi, Self::Viewer => crate::discovery::Product::PlayOnline }
    }
    /// A version stamp of this product: FFXI `30260904_1` (8 digits starting 3, decimal
    /// revision), the Viewer `20110829_E` (8 digits starting 2, up to 16 letters or digits).
    pub(crate) fn valid_stamp(self, version: &str) -> bool {
        version.split_once('_').is_some_and(|(date, revision)| {
            date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && !revision.is_empty()
                && match self {
                    Self::Ffxi => date.starts_with('3') && revision.bytes().all(|b| b.is_ascii_digit()),
                    Self::Viewer => date.starts_with('2') && revision.len() <= 16 && revision.bytes().all(|b| b.is_ascii_alphanumeric()),
                }
        })
    }
    /// The newest stamp in `manifest` (validated: every stamp is this product's).
    fn latest_stamp(self, manifest: &Manifest) -> Option<&str> {
        manifest.files.iter().flat_map(|h| &h.versions).map(|v| v.stamp.as_str()).max_by_key(|s| self.stamp_order(s))
    }
    /// A stamp's place in time (date, then revision), `None` when it is not this product's.
    /// Viewer revisions are hexadecimal digits in the live manifest.
    pub(crate) fn stamp_order(self, stamp: &str) -> Option<(u64, u64)> {
        if !self.valid_stamp(stamp) { return None; }
        let (date, revision) = stamp.split_once('_')?;
        let radix = match self { Self::Ffxi => 10, Self::Viewer => 16 };
        Some((date.parse().ok()?, u64::from_str_radix(revision, radix).ok()?))
    }
    /// Whether an update from `installed` changes `history`: its current version is newer
    /// than the installed one. Without an installed version every file counts as changed.
    fn changed_since(self, installed: Option<&str>, history: &FileHistory) -> bool {
        match installed.and_then(|version| self.stamp_order(version)) {
            Some(base) => self.stamp_order(&history.current().stamp).is_none_or(|stamp| stamp > base),
            None => true,
        }
    }
    fn parse_manifest(self, text: &str) -> Result<Manifest, String> {
        match self { Self::Ffxi => manifest::parse(text), Self::Viewer => manifest::parse_viewer(text) }
    }
    fn deployment(self, root: &Path, manifest: Manifest) -> Result<Manifest, String> {
        if self == Self::Ffxi { return Ok(manifest); }
        let mut files = Vec::new();
        for mut history in manifest.files {
            let lower = history.path.to_ascii_lowercase();
            // The same region=1 filter used by app.dll for PlayOnlineUS.
            if lower.starts_with("eu/") || matches!(lower.as_str(),
                "patchfiles/playonlineviewer/viewer/com/appeu.dll" |
                "patchfiles/playonlineviewer/viewer/com/polcoreeu.dll") { continue; }
            if lower.starts_with("patchfiles/") {
                let target = match lower.as_str() {
                    "patchfiles/playonlineviewer/viewer/com/app.dll" => "viewer/com/app.dll",
                    "patchfiles/playonlineviewer/viewer/com/polcore.dll" => "viewer/com/polcore.dll",
                    _ => return Err(tf("Unsafe patch path {path}", &[("path", &history.path)])),
                };
                // A stock updater may have left a pending staged DLL. Keep it
                // in sync so the next official Viewer cannot copy an old DLL
                // over Crystal's verified deployment. Its absence is normal.
                if confined(root, &history.path)?.is_file() { files.push(history.clone()); }
                history.path = target.into();
                history.optional = false;
            }
            if history.optional && !confined(root, &history.path)?.is_file() { continue; }
            files.push(history);
        }
        let manifest = Manifest { files };
        validate_manifest_for(self, &manifest)?;
        Ok(manifest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Check,
    Update,
    Repair,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub summary: String,
    pub version: String,
    pub total: usize,
    pub current: usize,
    pub outdated: usize,
    pub missing: usize,
    pub modified: usize,
    pub repaired: usize,
    pub bytes: u64,
    pub verified: bool,
    pub issues: Vec<String>,
    /// Modified files replaced with official ones whose copies are in `.xi-updater/kept`.
    pub kept: usize,
    /// Modified files an update doesn't change, left in place.
    pub left: usize,
    /// The plan read every file in full (not only what the update changes), so a verified
    /// commit vouches for every official file.
    pub all_files_checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Direct,
    Deltas(usize),
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    index: usize,
    action: Action,
    original: Option<FileSig>,
    /// The file is a user modification being replaced: its copy goes to
    /// `.xi-updater/kept/<path>` before the version advances (owner, 2026-10-13).
    keep: bool,
}

#[derive(Clone)]
struct StageGroup { source: Planned, targets: Vec<Planned> }

/// The Viewer deploys one payload to both its live and retained staged DLL.
/// Their old signatures may differ, so choose the cheapest valid source plan
/// while retaining each target's own transaction precondition.
fn stage_groups(product: Product, manifest: &Manifest, planned: &[Planned]) -> Vec<StageGroup> {
    let mut groups: Vec<StageGroup> = Vec::new();
    let mut payloads = std::collections::HashMap::new();
    for item in planned {
        let current = manifest.files[item.index].current();
        let key = (current.direct.clone(), current.sig);
        let existing = if product == Product::Viewer { payloads.get(&key).copied() } else { None };
        if let Some(index) = existing {
            let group: &mut StageGroup = &mut groups[index];
            if download_size(&manifest.files[item.index], &item.action)
                < download_size(&manifest.files[group.source.index], &group.source.action) {
                group.source = item.clone();
            }
            group.targets.push(item.clone());
        } else {
            payloads.insert(key, groups.len());
            groups.push(StageGroup { source: item.clone(), targets: vec![item.clone()] });
        }
    }
    groups
}

/// Read-only full verification, or a recoverable update/explicit repair, of FFXI alone
/// (`live_test`; the app goes through `run_combined`).
#[cfg(test)]
pub async fn run(install: PathBuf, mode: Mode, operation: Operation) -> Result<Report, String> {
    let prepared = prepare_at(Product::Ffxi, install, mode, &operation, Product::Ffxi.server(), ensure_idle).await?;
    let mut reports = execute(vec![prepared], &operation, true, ensure_idle).await?;
    Ok(reports.remove(0))
}

pub(crate) struct CombinedReport { pub viewer: Option<Report>, pub game: Option<Report> }
impl CombinedReport {
    pub fn summary(&self) -> String {
        match (&self.viewer, &self.game) {
            (Some(viewer), Some(game)) => tf("PlayOnline: {pol}\nFINAL FANTASY XI: {ffxi}", &[("pol", &viewer.summary), ("ffxi", &game.summary)]),
            (Some(viewer), None) => tf("PlayOnline: {pol}", &[("pol", &viewer.summary)]),
            (None, Some(game)) => tf("FINAL FANTASY XI: {ffxi}", &[("ffxi", &game.summary)]),
            (None, None) => String::new(),
        }
    }
}

pub(crate) struct CombinedFailure { pub message: String, pub viewer_version: Option<String> }
impl From<String> for CombinedFailure {
    fn from(message: String) -> Self { Self { message, viewer_version: None } }
}

/// One engine and one operation for PlayOnline, FINAL FANTASY XI or both. PlayOnline goes
/// first and finishes (version, list, downloads, commit) before anything goes to FFXI's
/// patch service: in stock the Viewer updates at Connect and restarts before any FFXI W20,
/// so :53001 traffic never sits between the Viewer's list and its downloads. Each
/// component's own traffic is in stock's order: version, list, then its downloads.
pub(crate) async fn run_combined(viewer: Option<PathBuf>, game: Option<PathBuf>, mode: Mode, operation: Operation) -> Result<CombinedReport, CombinedFailure> {
    run_combined_at(viewer, game, mode, operation, (Product::Viewer.server(), Product::Ffxi.server()), ensure_idle).await
}

/// `run_combined` with the patch servers (PlayOnline's, FFXI's) and the liveness guard as
/// seams for local-server tests.
async fn run_combined_at(viewer: Option<PathBuf>, game: Option<PathBuf>, mode: Mode, operation: Operation,
    servers: (&str, &str), guard: Guard) -> Result<CombinedReport, CombinedFailure> {
    let canonical = |path: Option<PathBuf>| -> Result<Option<PathBuf>, String> {
        path.map(|path| { ensure_plain(&path)?; fs::canonicalize(path).map_err(|e| e.to_string()) }).transpose()
    };
    let (viewer, game) = (canonical(viewer)?, canonical(game)?);
    if let (Some(viewer), Some(game)) = (&viewer, &game) {
        if viewer == game || viewer.starts_with(game) || game.starts_with(viewer) {
            return Err(t("Choose separate PlayOnline and FINAL FANTASY XI installation folders.").to_owned().into());
        }
    }
    operation.check_cancelled()?;
    if mode != Mode::Check {
        operation.begin_update();
        for root in viewer.iter().chain(&game) { guard(root)?; }
    }
    let mut combined = CombinedReport { viewer: None, game: None };
    let mut viewer_version = None;
    if let Some(viewer) = viewer {
        let pol = prepare_at(Product::Viewer, viewer, mode, &operation.component(t("PlayOnline")), servers.0, guard).await?;
        viewer_version = Some(pol.report.version.clone());
        let reports = execute(vec![pol], &operation, game.is_none(), guard).await
            .map_err(|message| CombinedFailure { message, viewer_version: viewer_version.clone() })?;
        combined.viewer = reports.into_iter().next();
    }
    if let Some(game) = game {
        let failure = |message| CombinedFailure { message, viewer_version: viewer_version.clone() };
        let ffxi = prepare_at(Product::Ffxi, game, mode, &operation.component(t("FINAL FANTASY XI")), servers.1, guard).await.map_err(failure)?;
        combined.game = execute(vec![ffxi], &operation, true, guard).await.map_err(failure)?.into_iter().next();
    }
    Ok(combined)
}

struct PreparedUpdate {
    product: Product,
    root: PathBuf,
    _lock: Option<fs::File>,
    mode: Mode,
    report: Report,
    manifest: Manifest,
    text: Vec<u8>,
    planned: Vec<Planned>,
    seed: Option<u64>,
    /// The patch server ("host:port") its downloads come from.
    server: Arc<str>,
    /// Every file was read in full (not a version plan): success clears a pending full check.
    full_scan: bool,
}
impl PreparedUpdate {
    /// Repair (and the small PlayOnline component) verifies every file after applying;
    /// an FFXI update verifies the files it wrote (Phase 3).
    fn full_verify(&self) -> bool { self.mode == Mode::Repair || self.product == Product::Viewer }
}

/// The liveness check a step that changes an install runs first (`ensure_idle`; fixture
/// tests pass their own).
type Guard = fn(&Path) -> Result<(), String>;

async fn prepare_at(product: Product, install: PathBuf, mode: Mode, operation: &Operation, server: &str, guard: Guard) -> Result<PreparedUpdate, String> {
    prepare_with(product, install, mode, operation, server, guard, None).await
}

/// What a Play's own version check already told an update that follows it in the same
/// signed-in session (proposal 09 phase 2): the official version, and how long the "update?"
/// prompt's idle connection stays open (`PROMPT_HOLD`).
pub(crate) struct AfterPlay {
    pub official: String,
    pub prompt: Duration,
}

/// `prepare_at`, or with `play` the update a Play's W20 asked for: no second version request
/// (stock sends none: capture stock-update-20261013-004932), the idle prompt connection, then
/// the file list. Its downloads may go ahead while a game uses the folder: only an
/// interrupted update's recovery and the commit wait for `guard`.
async fn prepare_with(product: Product, install: PathBuf, mode: Mode, operation: &Operation, server: &str, guard: Guard,
    play: Option<&AfterPlay>) -> Result<PreparedUpdate, String> {
    operation.check_cancelled()?;
    if mode != Mode::Check { operation.begin_update(); }
    ensure_plain(&install)?;
    let root = fs::canonicalize(&install).map_err(|e| tf("Install directory: {error}", &[("error", &e)]))?;
    ensure_plain(&root)?;
    if !root.is_dir() { return Err(t("Install must be a directory").into()); }
    validate_installation(product, &root)?;
    let lock = if mode != Mode::Check {
        if play.is_none() { guard(&root)?; }
        let state = confined(&root, STATE_DIR)?;
        fs::create_dir_all(&state).map_err(|e| e.to_string())?;
        // Another account's Play check holds the lock read-shared for at most its W20; the
        // update that Play asked for waits that out rather than failing.
        let wait = if play.is_some() { PLAY_LOCK_WAIT } else { Duration::ZERO };
        Some(wait_for_update_lock(&confined(&root, &format!("{STATE_DIR}/lock"))?, wait, operation).await?)
    } else { None };
    if mode != Mode::Check {
        if play.is_none() { guard(&root)?; }
        // An interrupted update leaves files nobody vouches for: check them all next.
        if recover_with_guard(&root, guard)? { mark_full_check(&root); }
    }
    // The record this update replaces must be one it can re-encode; refuse before any network
    // or scan work. After recovery (which can restore patch.ver); the lock keeps it stable.
    let seed = if mode == Mode::Check { None } else {
        confined(&root, "patch.ver")?;
        Some(if mode == Mode::Repair { crate::patch::repair_seed_for(&root, product)? }
            else { crate::patch::applied_seed_for(&root, product)? })
    };
    let installed = crate::patch::installed_for_request_for(&root, product);
    let (version, text, manifest) = if mode == Mode::Check {
        // Check compares the files with the installed file list (the patch.cfg the last
        // update wrote) and never contacts Square Enix: stock has no such request.
        operation.progress(Stage::Manifest, 0, 1, t("Reading the installed file list"));
        let Some(text) = read_installed_list(&root)? else {
            let report = Report { summary: t("No installed file list (patch.cfg) to compare with yet, so nothing was checked. Press Update or use Repair files.").into(),
                ..Default::default() };
            return Ok(PreparedUpdate { product, root, _lock: lock, mode, report, manifest: Manifest::default(),
                text: Vec::new(), planned: Vec::new(), seed: None, server: server.into(), full_scan: false });
        };
        let manifest = parse_list(product, &text)?;
        let version = product.latest_stamp(&manifest).unwrap_or_default().to_owned();
        (version, text, manifest)
    } else {
        let version = match play {
            // Play is the update (phase 2): the Play's W20 told the version. Stock then opens one
            // more connection ~110 ms after that W20's FIN and holds it idle, no data, while its
            // "update?" prompt shows (captures 2026-10-13: 5.5 s at 00:21, ~1 s at 00:49), closes
            // it and opens the file-list connection ~105-110 ms later (`NEXT_CONNECTION_GAP`
            // spaces both from the FIN before them; the W20's is noted by `note_patch_closed`).
            Some(play) => {
                if !product.valid_stamp(&play.official) { return Err(t("Official version response is invalid").into()); }
                operation.progress(Stage::Manifest, 0, 1, t("Starting the update"));
                hold_prompt(product, server, play.prompt, operation).await?;
                play.official.clone()
            }
            None => {
                operation.progress(Stage::Manifest, 0, 1, t("Downloading the official file manifest"));
                official_version(product, server, &installed, operation).await?
            }
        };
        if play.is_some() { operation.progress(Stage::Manifest, 0, 1, t("Downloading the official file manifest")); }
        let text = official_list(product, server, operation).await?;
        let manifest = parse_list(product, &text)?;
        if product.latest_stamp(&manifest) != Some(version.as_str()) {
            return Err(t("Official version changed while fetching the manifest; retry").into());
        }
        (version, text, manifest)
    };
    let manifest = product.deployment(&root, manifest)?;
    operation.progress(Stage::Manifest, 1, 1, tf("{count} files, version {version}",
        &[("count", &manifest.files.len()), ("version", &version)]));
    let scan_root = root.clone(); let scan_operation = operation.clone();
    // POL's official update restores canonical components. Fresh installer
    // DLLs are not necessarily in the retained patch history. POL originals
    // still receive transaction backups.
    let scan_mode = if product == Product::Viewer && mode == Mode::Update { Mode::Repair } else { mode };
    let installed = (!installed.is_empty()).then_some(installed);
    // A routine FFXI update checks only what changed (Phase 3). Everything else is a full
    // scan: Check, Repair, a fresh install, a list that is not the installed version's, a
    // pending full check (interrupted or failed update), or any failure of the plan.
    let applied = (scan_mode == Mode::Update && product == Product::Ffxi && !full_check_due(&root))
        .then(|| applied_list(&root, product, installed.as_deref())).flatten();
    // The manifest moves to the scan's thread and comes back with the plan (no copy).
    let (((mut report, planned), full_scan), manifest) = tokio::task::spawn_blocking(move || {
        let planned = (|| {
            if let Some(applied) = applied {
                let workers = check_workers(&scan_root);
                match plan_from_applied(product, &scan_root, &applied, &manifest, scan_mode, &scan_operation, workers) {
                    Ok(plan) => return Ok((plan, false)),
                    Err(error) if scan_operation.cancelled() => return Err(error),
                    Err(_) => {}
                }
            }
            scan_for(product, &scan_root, &manifest, scan_mode, &scan_operation, installed.as_deref()).map(|plan| (plan, true))
        })();
        planned.map(|planned| (planned, manifest))
    }).await.map_err(|e| e.to_string())??;
    report.version = version;
    if mode == Mode::Check {
        report.verified = report.current == report.total;
        let counts: [(&str, &dyn std::fmt::Display); 5] = [("total", &report.total), ("current", &report.current),
            ("outdated", &report.outdated), ("missing", &report.missing), ("modified", &report.modified)];
        report.summary = if report.verified {
            tf("Checked {total} files: {current} current, {outdated} outdated, {missing} missing, {modified} modified. All official files match.", &counts)
        } else {
            tf("Checked {total} files: {current} current, {outdated} outdated, {missing} missing, {modified} modified. No files changed.", &counts)
        };
    }
    Ok(PreparedUpdate { product, root, _lock: lock, mode, report, manifest, text, planned, seed, server: server.into(), full_scan })
}

/// The installed list when it is the installed version's (its newest stamp is the one
/// patch.ver records), the base a version plan compares with.
fn applied_list(root: &Path, product: Product, installed: Option<&str>) -> Option<Manifest> {
    let installed = installed?;
    let text = read_installed_list(root).ok()??;
    let manifest = parse_list(product, &text).ok()?;
    (product.latest_stamp(&manifest) == Some(installed)).then_some(manifest)
}

fn full_check_due(root: &Path) -> bool {
    confined(root, FULL_CHECK_MARKER).map_or(true, |marker| marker.exists())
}

/// Best effort: without the marker, an interrupted update's journal or the failure
/// itself still forces the full scan once.
fn mark_full_check(root: &Path) {
    if let Ok(marker) = confined(root, FULL_CHECK_MARKER) { let _ = fs::write(marker, b""); }
}

/// The installed file list (`patch.cfg`, written by the last verified update), or `None`
/// when there is none yet. Read from disk only.
fn read_installed_list(root: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = confined(root, "patch.cfg")?;
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut text = Vec::new();
    file.take(MAX_MANIFEST as u64 + 1).read_to_end(&mut text).map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() > MAX_MANIFEST { return Err(t("Decoded payload exceeds expected size").into()); }
    Ok(Some(text))
}

fn parse_list(product: Product, text: &[u8]) -> Result<Manifest, String> {
    let utf8 = std::str::from_utf8(text).map_err(|_| t("Official manifest is not UTF-8"))?;
    let manifest = product.parse_manifest(utf8)?;
    validate_manifest_for(product, &manifest)?;
    Ok(manifest)
}

/// The version request on its own stock connection, closed like stock after the reply
/// (capture 2026-10-13 00:21: 88 B request, 103 B reply, FIN ~110 ms later).
async fn official_version(product: Product, server: &str, installed: &str, operation: &Operation) -> Result<String, String> {
    let mut session = Session::connect_to(product, server, operation).await?;
    let frame = session.exchange(&polp::version(&product.tags(), installed.as_bytes()), polp::kind::VERSION_REPLY, operation).await?;
    session.close().await;
    let version = polp::parse_version_reply(&frame[16..])?;
    if version.label == "unknown" || !product.valid_stamp(&version.version) {
        return Err(t("Official version response is invalid").into());
    }
    Ok(version.version)
}

/// The idle connection stock opens while its "update?" prompt shows (`PROMPT_HOLD`): connect
/// (after `NEXT_CONNECTION_GAP` from the W20's FIN), send nothing for `hold`, then FIN. A
/// cancel closes it at once.
async fn hold_prompt(product: Product, server: &str, hold: Duration, operation: &Operation) -> Result<(), String> {
    let session = Session::connect_to(product, server, operation).await?;
    let held = tokio::select! {
        _ = tokio::time::sleep(hold) => Ok(()),
        cancelled = operation.cancelled_wait() => Err(cancelled),
    };
    session.close().await;
    held
}

/// The file list (POLP command 1, 24 B) on a new stock connection, closed like stock
/// ~0.1 s after the reply. Returns the decoded list text.
async fn official_list(product: Product, server: &str, operation: &Operation) -> Result<Vec<u8>, String> {
    let mut session = Session::connect_to(product, server, operation).await?;
    let frame = session.exchange(&polp::hello(&product.tags()), polp::kind::MANIFEST_REPLY, operation).await?;
    session.close().await;
    decode_payload(&frame[16..], MAX_MANIFEST)
}

// A directory selection is not evidence of an installed product. Allow repair
// when individual binaries are missing, but never populate an unrelated folder.
fn validate_installation(product: Product, root: &Path) -> Result<(), String> {
    let markers: &[&str] = match product {
        Product::Ffxi => &["FFXi.dll", "FFXiMain.dll", "ROM/0/0.DAT"],
        Product::Viewer => &["pol.exe", "viewer/com/polcore.dll", "viewer/com/app.dll"],
    };
    for marker in markers {
        let path = confined(root, marker)?;
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(()),
            Ok(_) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err(tf("The selected folder does not contain an installed {product}. Run official setup or select its existing installation folder.",
        &[("product", &product.label())]))
}

/// Stage, then commit, `updates` in order. `finish`: this completes the whole operation
/// (`false` while another component follows). `guard`: the liveness check of each commit.
async fn execute(updates: Vec<PreparedUpdate>, operation: &Operation, finish: bool, guard: Guard) -> Result<Vec<Report>, String> {
    if updates.iter().all(|update| update.mode == Mode::Check) {
        return Ok(updates.into_iter().map(|u| u.report).collect());
    }
    let staged_updates = stage_updates(updates, operation).await?;
    let mut reports = Vec::new();
    for (update, ready) in staged_updates {
        let op = operation.component(update.product.label());
        reports.push(tokio::task::spawn_blocking(move || commit_with_guard(update, ready, &op, guard)).await.map_err(|e| e.to_string())??);
    }
    if finish {
        operation.progress(Stage::Finish, 0, 0, t("Finalizing update…"));
        operation.finish_update();
    }
    Ok(reports)
}

/// An update a Play asked for, downloaded into the verified cache and waiting to be installed
/// (`install_staged`). It holds the install's updater lock.
pub(crate) struct Staged {
    update: PreparedUpdate,
    ready: Vec<Ready>,
}
impl Staged {
    /// The canonical game folder it installs into.
    pub(crate) fn root(&self) -> &Path { &self.update.root }

    /// Installed files this update replaces that another process holds open (or mapped): an
    /// exclusive read-only open fails with a sharing or lock violation. Nothing is changed.
    /// The process scan (`folder_users`) finds what maps a module from the folder; this finds
    /// a plain open data file too.
    pub(crate) fn held_files(&self) -> Vec<String> {
        self.ready.iter().filter(|(path, ..)| confined(&self.update.root, path).is_ok_and(|file| held(&file)))
            .map(|(path, ..)| path.clone()).collect()
    }
}

/// How long a Play's update waits for another account's W20, which holds the updater lock
/// read-shared for at most its exchange (`patch::CHECK_TIMEOUT`) plus the record read.
const PLAY_LOCK_WAIT: Duration = Duration::from_secs(crate::patch::CHECK_TIMEOUT.as_secs() + 2);

/// The install's exclusive updater lock.
fn open_update_lock(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).share_mode(0).open(path)
}

/// `open_update_lock`, retrying a sharing violation (another holder) for up to `wait`.
async fn wait_for_update_lock(path: &Path, wait: Duration, operation: &Operation) -> Result<fs::File, String> {
    let deadline = Instant::now() + wait;
    loop {
        match open_update_lock(path) {
            Err(error) if error.raw_os_error() == Some(32) && Instant::now() < deadline => {
                operation.check_cancelled()?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            result => return result.map_err(|e| tf("Another updater may be using this install: {error}", &[("error", &e)])),
        }
    }
}

/// Another process has `file` open in a way that would stop its replacement.
fn held(file: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION. A missing or read-only file is not held.
    matches!(fs::OpenOptions::new().read(true).share_mode(0).open(file).map_err(|e| e.raw_os_error()), Err(Some(32 | 33)))
}

/// The idle "update?" prompt connection: stock holds it open, sending nothing, for as long as
/// the prompt shows, about one human confirmation (1.0 s and 5.5 s observed). Crystal has no
/// prompt (Play already said yes), so it holds a plausible quick confirmation instead: never
/// one machine constant, and kept short because the user is waiting. Below 1 s would be faster
/// than the quickest stock answer seen; past 3 s only makes the user wait.
pub(crate) const PROMPT_HOLD: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(3));

/// Download the update a Play's W20 asked for (`official`): the prompt connection, the file
/// list and the downloads, in stock's order, with no second version request. Games may keep
/// running from the folder meanwhile; `install_staged` waits for them.
#[cfg_attr(test, allow(dead_code))]
pub(crate) async fn stage_after_play(install: PathBuf, official: String, operation: &Operation) -> Result<Staged, String> {
    let play = AfterPlay { official, prompt: crate::stock_socket::uniform(PROMPT_HOLD) };
    stage_after_play_at(install, play, operation, Product::Ffxi.server(), ensure_idle).await
}

/// `stage_after_play` with the patch server and the recovery guard as test seams.
async fn stage_after_play_at(install: PathBuf, play: AfterPlay, operation: &Operation, server: &str, guard: Guard) -> Result<Staged, String> {
    let update = prepare_with(Product::Ffxi, install, Mode::Update, operation, server, guard, Some(&play)).await?;
    let (update, ready) = stage_updates(vec![update], operation).await?.remove(0);
    Ok(Staged { update, ready })
}

/// Install a staged update once nothing uses its folder (the commit's own liveness check
/// refuses otherwise), and finish the operation. No version check follows (stock sends none
/// between its update and the game start).
#[cfg_attr(test, allow(dead_code))]
pub(crate) async fn install_staged(staged: Staged, operation: &Operation) -> Result<Report, String> {
    install_staged_with(staged, operation, ensure_idle).await
}

async fn install_staged_with(staged: Staged, operation: &Operation, guard: Guard) -> Result<Report, String> {
    let Staged { update, ready } = staged;
    let op = operation.component(update.product.label());
    let report = tokio::task::spawn_blocking(move || commit_with_guard(update, ready, &op, guard)).await.map_err(|e| e.to_string())??;
    operation.progress(Stage::Finish, 0, 0, t("Finalizing update…"));
    operation.finish_update();
    Ok(report)
}

/// What a PlayOnline update says when its files stay in use.
pub(crate) fn viewer_in_use() -> &'static str {
    t("PlayOnline is in use. Close FINAL FANTASY XI and PlayOnline Viewer, then press Play.")
}

/// How long PlayOnline's own update waits for its folder to be free before it gives up
/// (`viewer_in_use`): the hosts of the sign-ins the W2U stopped are still exiting when their
/// refusal reaches the window (each has polcore.dll loaded from PlayOnline's folder).
const VIEWER_FREE_WAIT: Duration = Duration::from_secs(15);

/// Play is the PlayOnline update too: the update a sign-in's W2U asked for (`official`, the
/// version that W2U's reply named). Stock's Viewer updates itself at Connect, before any
/// credentials go out, with the patch client FFXI's update uses (capture
/// stock-update-20261013-004932): no second version request, the idle prompt connection,
/// the file list, one stop-and-wait download connection, then the install. The sign-in that
/// follows asks its own W2U, as the restarted stock Viewer does at its next Connect.
/// Nothing is sent while PlayOnline's files are in use (`viewer_in_use`).
pub(crate) async fn update_viewer_after_connect(viewer: PathBuf, official: String, operation: Operation) -> Result<CombinedReport, CombinedFailure> {
    let play = AfterPlay { official, prompt: crate::stock_socket::uniform(PROMPT_HOLD) };
    update_viewer_after_connect_at(viewer, play, operation, Product::Viewer.server(), ensure_idle, VIEWER_FREE_WAIT).await
}

/// `update_viewer_after_connect` with the patch server, the liveness guard and the wait for a
/// free folder as test seams.
async fn update_viewer_after_connect_at(viewer: PathBuf, play: AfterPlay, operation: Operation, server: &str, guard: Guard,
    free_wait: Duration) -> Result<CombinedReport, CombinedFailure> {
    ensure_plain(&viewer)?;
    let viewer = fs::canonicalize(viewer).map_err(|e| e.to_string())?;
    operation.check_cancelled()?;
    operation.begin_update();
    wait_until_idle(&viewer, guard, free_wait, &operation).await?;
    let failure = |message| CombinedFailure { message, viewer_version: Some(play.official.clone()) };
    let pol = prepare_with(Product::Viewer, viewer, Mode::Update, &operation.component(t("PlayOnline")), server, guard, Some(&play))
        .await.map_err(failure)?;
    let reports = execute(vec![pol], &operation, true, guard).await.map_err(failure)?;
    Ok(CombinedReport { viewer: reports.into_iter().next(), game: None })
}

/// Until `guard` finds nothing using `root`, looking again every 250 ms for up to `limit`.
async fn wait_until_idle(root: &Path, guard: Guard, limit: Duration, operation: &Operation) -> Result<(), String> {
    let deadline = Instant::now() + limit;
    loop {
        operation.check_cancelled()?;
        let scanned = root.to_path_buf();
        if tokio::task::spawn_blocking(move || guard(&scanned)).await.map_err(|e| e.to_string())?.is_ok() { return Ok(()); }
        if Instant::now() >= deadline { return Err(viewer_in_use().into()); }
        operation.progress(Stage::Waiting, 0, 0, t("Waiting for PlayOnline to close"));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            cancelled = operation.cancelled_wait() => return Err(cancelled),
        }
    }
}

/// Stage every planned download of `updates`, in order, into each install's verified cache.
async fn stage_updates(updates: Vec<PreparedUpdate>, operation: &Operation) -> Result<Vec<(PreparedUpdate, Vec<Ready>)>, String> {
    let groups: Vec<Vec<StageGroup>> = updates.iter().map(|u| stage_groups(u.product, &u.manifest, &u.planned)).collect();
    let sizes: Vec<u64> = updates.iter().zip(&groups).flat_map(|(u, groups)| groups.iter()
        .map(|g| download_size(&u.manifest.files[g.source.index], &g.source.action))).collect();
    operation.plan_update(sizes.iter().sum(),
        updates.iter().flat_map(|u| u.planned.iter().map(|p| staging_work(&u.manifest.files[p.index], p))).sum(),
        updates.iter().flat_map(|u| u.planned.iter().map(|p| applying_work(u.manifest.files[p.index].current().sig, p.original))).sum(),
        updates.iter().map(|u| if u.full_verify() {
            u.manifest.files.iter().map(|h| io_work(h.current().sig)).sum::<u64>()
        } else {
            u.planned.iter().map(|p| io_work(u.manifest.files[p.index].current().sig)).sum()
        }).sum());
    let downloads = Downloads::new(operation.clone(), sizes);
    let mut staged_updates = Vec::new(); let mut first = 0;
    for (mut update, groups) in updates.into_iter().zip(groups) {
        operation.check_cancelled()?;
        let op = operation.component(update.product.label());
        let staged = confined(&update.root, &format!("{STATE_DIR}/staged"))?;
        fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
        let (ready, bytes) = stage_plan_shared(update.product, &update.server, &update.root, &staged, &update.manifest,
            &groups, &op, &downloads, first).await?;
        first += groups.len(); update.report.bytes = bytes;
        staged_updates.push((update, ready));
    }
    Ok(staged_updates)
}

#[cfg(test)]
fn commit(update: PreparedUpdate, ready: Vec<Ready>, operation: &Operation) -> Result<Report, String> {
    commit_with_guard(update, ready, operation, ensure_idle)
}

/// `commit` with the liveness guard as a seam for isolated fixture tests.
fn commit_with_guard(mut update: PreparedUpdate, ready: Vec<Ready>, operation: &Operation,
    guard: impl Fn(&Path) -> Result<(), String>) -> Result<Report, String> {
    operation.check_cancelled()?; guard(&update.root)?;
    let keep: HashSet<String> = update.planned.iter().filter(|item| item.keep)
        .map(|item| update.manifest.files[item.index].path.clone()).collect();
    let full_verify = update.full_verify();
    // What the cleanup after the commit may treat as official bytes rather than a user's.
    let baseline = if update.product == Product::Ffxi { installer_baseline::load().ok() } else { None };
    let official = |path: &str, file: &LocalFile| update.manifest.files.iter()
        .find(|history| history.path.eq_ignore_ascii_case(path))
        .is_some_and(|history| history.versions.iter().any(|version| version.sig == file.sig))
        || baseline.is_some_and(|baseline| baseline.matches(path, file.sig.len, file.md5));
    let applied = apply_keeping(&update.root, &ready, &keep, &official, operation, || {
        if full_verify {
            // Repair proves every official file, on the same worker pool as a check.
            let workers = file_check_workers(std::thread::available_parallelism().map_or(1, |n| n.get()), false);
            let found = scan_signatures(&update.root, &update.manifest.files.iter().collect::<Vec<_>>(), operation, workers, Stage::Verify)?;
            for (history, local) in update.manifest.files.iter().zip(found) {
                if local.map(|file| file.sig) != Some(history.current().sig) {
                    return Err(tf("{path}: final file verification failed", &[("path", &history.path)]));
                }
            }
        } else {
            // A routine update verifies what it wrote; the plan vouched for the rest.
            let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
            for (i, (path, _, sig, _)) in ready.iter().enumerate() {
                operation.check_cancelled()?;
                operation.progress(Stage::Verify, i as u64, ready.len() as u64, path.clone());
                let file = confined(&update.root, path)?;
                if !file.is_file() || signature_with_buffer(&file, operation, &mut buffer)? != *sig {
                    return Err(tf("{path}: final file verification failed", &[("path", path)]));
                }
                operation.complete_update_work(UpdateWork::Verify, io_work(*sig), None);
            }
        }
        operation.progress(Stage::Verify, 1, 1, t("Verifying downloaded files"));
        operation.check_cancelled()?;
        Ok(Some((update.text.clone(), crate::patch::encode_applied_record_for(&update.report.version, update.seed.unwrap(), update.product)?)))
    }, &guard);
    // A failure that touched the install marked the full check (`apply_keeping`).
    let committed = applied?;
    if committed && update.full_scan {
        if let Ok(marker) = confined(&update.root, FULL_CHECK_MARKER) { let _ = fs::remove_file(marker); }
    }
    update.report.repaired = update.planned.len(); update.report.verified = committed;
    update.report.all_files_checked = update.full_scan;
    let mut summary = vec![tf("{repaired} files updated. Applied version {version}.",
        &[("repaired", &update.report.repaired), ("version", &update.report.version)])];
    if update.report.kept > 0 {
        summary.push(tf("{kept} modified files were replaced with official ones; your copies are in .xi-updater\\kept.",
            &[("kept", &update.report.kept)]));
    }
    let left = update.report.left;
    if left > 0 {
        summary.push(tf("{left} modified files this update doesn't change were left in place.", &[("left", &left)]));
    }
    update.report.summary = summary.join(" ");
    Ok(update.report)
}

fn hex_md5(b: &[u8]) -> String {
    format!("{:x}", md5::compute(b))
}

type Ready = (String, PathBuf, FileSig, Option<FileSig>);

fn io_work(sig: FileSig) -> u64 { sig.len.max(1) }
fn applying_work(sig: FileSig, original: Option<FileSig>) -> u64 {
    // Three original-file signature reads plus the backup read/write; two
    // target signature reads plus the replacement copy's read/write.
    original.map(io_work).unwrap_or(0) * 5 + io_work(sig) * 4
}
fn staging_work(history: &FileHistory, item: &Planned) -> u64 {
    let base = if matches!(item.action, Action::Deltas(_)) {
        item.original.map(io_work).unwrap_or(0)
    } else { 0 };
    // Staged output write and its independent signature read, plus a delta base.
    io_work(history.current().sig) * 2 + base
}

/// One stable byte total shared by all download workers. File chunks must not
/// replace aggregate progress with the current file's unrelated denominator.
#[derive(Clone)]
struct Downloads {
    operation: Operation,
    state: Arc<Mutex<DownloadState>>,
}
struct DownloadState {
    files: Vec<(u64, u64)>, // (received, expected), including planned deltas
    done: u64,
    total: u64,
}
struct FileProgress {
    downloads: Downloads,
    index: usize,
    operation: Operation,
}
impl Downloads {
    fn new(operation: Operation, sizes: Vec<u64>) -> Self {
        let total = sizes.iter().sum();
        let downloads = Self {
            operation,
            state: Arc::new(Mutex::new(DownloadState {
                files: sizes.into_iter().map(|size| (0, size)).collect(), done: 0, total,
            })),
        };
        downloads.operation.progress_bytes(Stage::Download, 0, total, t("Downloading official files"));
        downloads
    }
    #[cfg(test)]
    fn file(&self, index: usize) -> FileProgress { self.file_scoped(index, self.operation.clone()) }
    fn file_scoped(&self, index: usize, operation: Operation) -> FileProgress {
        FileProgress { downloads: self.clone(), index, operation }
    }
}
impl FileProgress {
    fn advance(&self, bytes: u64, detail: String, reused: bool) {
        let mut state = self.downloads.state.lock().unwrap_or_else(|e| e.into_inner());
        let (received, expected) = state.files[self.index];
        let next = if reused { expected } else { received.saturating_add(bytes).min(expected) };
        state.files[self.index].0 = next;
        state.done += next - received;
        // Published under this lock, so progress stays monotonic.
        if reused {
            self.operation.progress_bytes_rebased(Stage::Download, state.done, state.total, detail);
        } else {
            self.operation.progress_bytes(Stage::Download, state.done, state.total, detail);
        }
    }
    fn fallback(&self, full_size: u64, detail: String) {
        let mut state = self.downloads.state.lock().unwrap_or_else(|e| e.into_inner());
        let (received, expected) = state.files[self.index];
        state.files[self.index].1 = received + full_size;
        // Keep transferred bytes, replace only the untransferred delta work.
        state.total = state.total - (expected - received) + full_size;
        self.operation.progress_bytes_rebased(Stage::Download, state.done, state.total, detail);
    }
}
fn download_size(history: &FileHistory, action: &Action) -> u64 {
    match action {
        Action::Direct => history.current().direct_len,
        Action::Deltas(from) => history.versions[*from..].iter()
            .filter_map(|version| version.indirect.as_ref()).map(|delta| delta.len).sum(),
    }
}

#[cfg(test)]
async fn stage_plan(
    root: &Path,
    staged: &Path,
    m: &Manifest,
    planned: &[Planned],
    op: &Operation,
) -> Result<(Vec<Ready>, u64), String> {
    let product = Product::Ffxi;
    let groups = stage_groups(product, m, planned);
    let downloads = Downloads::new(op.clone(), groups.iter()
        .map(|g| download_size(&m.files[g.source.index], &g.source.action)).collect());
    stage_plan_shared(product, product.server(), root, staged, m, &groups, op, &downloads, 0).await
}

/// Stage `groups` (`stage_groups`) in order into the verified cache `staged`: each one reused
/// when its verified copy is there, else downloaded on the one stop-and-wait connection
/// (opened at the first download, closed like stock at the end). `first` is the first group's
/// index in `downloads`.
#[allow(clippy::too_many_arguments)]
async fn stage_plan_shared(
    product: Product,
    server: &str,
    root: &Path,
    staged: &Path,
    m: &Manifest,
    groups: &[StageGroup],
    op: &Operation,
    downloads: &Downloads,
    first: usize,
) -> Result<(Vec<Ready>, u64), String> {
    let mut session: Option<Session> = None;
    let mut ready = Vec::new();
    let mut bytes = 0u64;
    for (n, group) in groups.iter().enumerate() {
        op.check_cancelled()?;
        let item = &group.source;
        let history = &m.files[item.index];
        let current = history.current();
        let transfer = downloads.file_scoped(first + n, op.clone());
        let cache = staged.join(format!(
            "{}-{}.bin",
            hex_md5(if product == Product::Viewer { current.direct.as_bytes() } else { history.path.as_bytes() }),
            hex_md5(format!("{:?}", current.sig).as_bytes())
        ));
        ensure_plain(&cache)?;
        if cache.is_file() && signature(&cache, op)? == current.sig {
            transfer.advance(0, tf("Reusing verified {path}", &[("path", &history.path)]), true);
        } else {
            if session.is_none() { session = Some(Session::connect_to(product, server, op).await?); }
            let produced = produce(session.as_mut().unwrap(), root, history, &item.action, op, Some(&transfer)).await;
            let data = match produced {
                Ok(data) => data,
                Err(e) if matches!(item.action, Action::Deltas(_)) && !op.cancelled() => {
                    transfer.fallback(
                        current.direct_len,
                        tf("Delta failed for {path}; using full file ({error})", &[("path", &history.path), ("error", &e)]),
                    );
                    // A connection whose exchange stopped partway may still carry that reply:
                    // close it like stock and open a new one, never a second request on it.
                    if session.as_ref().is_none_or(|session| session.broken || session.stream.is_none()) {
                        if let Some(old) = session.take() { old.close().await; }
                        session = Some(Session::connect_to(product, server, op).await?);
                    }
                    produce(session.as_mut().unwrap(), root, history, &Action::Direct, op, Some(&transfer)).await?
                }
                Err(e) => return Err(e),
            };
            if FileSig::of(&data) != current.sig {
                return Err(tf("{path}: downloaded signature mismatch", &[("path", &history.path)]));
            }
            op.check_cancelled()?;
            save(&cache, &data)?;
            if signature(&cache, op)? != current.sig {
                return Err(tf("{path}: staged file did not verify", &[("path", &history.path)]));
            }
            bytes = bytes.saturating_add(data.len() as u64);
        }
        for target in &group.targets {
            let history = &m.files[target.index];
            op.complete_update_work(UpdateWork::Staging, staging_work(history, target), None);
            ready.push((history.path.clone(), cache.clone(), current.sig, target.original));
        }
    }
    if let Some(session) = session { session.close().await; }
    Ok((ready, bytes))
}

#[cfg(test)]
fn validate_manifest(m: &Manifest) -> Result<(), String> {
    validate_manifest_for(Product::Ffxi, m)
}

fn validate_manifest_for(product: Product, m: &Manifest) -> Result<(), String> {
    if m.files.is_empty() {
        return Err(t("Official manifest contains no files").into());
    }
    let mut names = HashSet::new();
    for h in &m.files {
        validate_relative(&h.path)?;
        if h.path.eq_ignore_ascii_case("patch.ver")
            || h.path.to_ascii_lowercase().starts_with(STATE_DIR)
        {
            return Err(t("Manifest conflicts with updater state").into());
        }
        if !names.insert(h.path.to_ascii_lowercase()) {
            return Err(tf("Duplicate manifest path {path}", &[("path", &h.path)]));
        }
        for v in &h.versions {
            if !product.valid_stamp(&v.stamp)
                || v.sig.len > MAX_FILE
                || v.direct_len == 0
                || v.direct_len > MAX_FILE
            {
                return Err(tf("{path}: unsupported version or size", &[("path", &h.path)]));
            }
            validate_relative(&v.direct)?;
            if let Some(d) = &v.indirect {
                validate_relative(&d.path)?;
                if d.len == 0 || d.len > MAX_FILE {
                    return Err(t("Unsupported delta length").into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn scan(
    root: &Path,
    m: &Manifest,
    mode: Mode,
    op: &Operation,
) -> Result<(Report, Vec<Planned>), String> {
    scan_for(Product::Ffxi, root, m, mode, op, None)
}

/// The full scan: every file read in full. `installed` (the applied version) tells
/// which files this update changes, for the modified-file rule; `None` counts all.
fn scan_for(product: Product, root: &Path, m: &Manifest, mode: Mode, op: &Operation, installed: Option<&str>) -> Result<(Report, Vec<Planned>), String> {
    op.check_cancelled()?;
    scan_with_workers_for(product, root, m, mode, op, check_workers(root), installed)
}

fn check_workers(root: &Path) -> usize {
    let available = std::thread::available_parallelism().map_or(1, |count| count.get());
    // This snapshot is only a scheduling hint. Check, and a Play's update (which downloads and
    // plans while other accounts play from the folder), read beside running games; the
    // mutation guards still run independently of it. Other updates already passed the guard.
    let game_may_be_running = ensure_idle(root).is_err();
    file_check_workers(available, game_may_be_running)
}

fn file_check_workers(available: usize, game_may_be_running: bool) -> usize {
    available.clamp(1, if game_may_be_running { ACTIVE_GAME_CHECK_WORKERS } else { MAX_CHECK_WORKERS })
}

/// Phase 3's routine update plan (owner, 2026-10-13: "check only changed files").
/// A file whose entry (current version stamp and signature) is the same in the installed
/// list and the new one only has its size checked; a matching size takes it as that
/// version. Every other file is read in full, so each file the update changes, and each
/// one a delta will patch, is classified from its real bytes. Classification and planning
/// are `plan_from_signatures`', so the plan equals a full scan's whenever the size-checked
/// files hold what the installed list says.
fn plan_from_applied(product: Product, root: &Path, installed: &Manifest, m: &Manifest, mode: Mode, op: &Operation, workers: usize) -> Result<(Report, Vec<Planned>), String> {
    op.check_cancelled()?;
    let before: std::collections::HashMap<String, &FileHistory> = installed.files.iter()
        .map(|history| (history.path.to_ascii_lowercase(), history)).collect();
    let changed: Vec<bool> = m.files.iter().map(|history| before.get(&history.path.to_ascii_lowercase())
        .is_none_or(|old| old.current().stamp != history.current().stamp || old.current().sig != history.current().sig)).collect();
    op.progress(Stage::Check, 0, m.files.len() as u64, t("Checking game files"));
    let mut signatures = vec![None; m.files.len()];
    let mut read = Vec::new();
    for (index, history) in m.files.iter().enumerate() {
        op.check_cancelled()?;
        if changed[index] { read.push(index); continue; }
        let path = confined(root, &history.path)?;
        match fs::metadata(&path) {
            // Only the signature's identity matters for a file taken as current; the
            // full MD5 is consulted for unmatched files, which are always read.
            Ok(metadata) if metadata.is_file() && metadata.len() == history.current().sig.len =>
                signatures[index] = Some(LocalFile { sig: history.current().sig, md5: [0; 16] }),
            Ok(metadata) if metadata.is_file() => read.push(index),
            Ok(_) => return Err(tf("{path} is not a regular file", &[("path", &path.display())])),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let subset: Vec<&FileHistory> = read.iter().map(|&index| &m.files[index]).collect();
    for (local, &index) in scan_signatures(root, &subset, op, workers, Stage::Check)?.into_iter().zip(&read) {
        signatures[index] = local;
    }
    plan_from_signatures(product, m, mode, op, signatures, &|index| changed[index], true)
}

#[cfg(test)]
fn scan_with_workers(
    root: &Path,
    m: &Manifest,
    mode: Mode,
    op: &Operation,
    workers: usize,
) -> Result<(Report, Vec<Planned>), String> {
    scan_with_workers_for(Product::Ffxi, root, m, mode, op, workers, None)
}

fn scan_with_workers_for(product: Product, root: &Path, m: &Manifest, mode: Mode, op: &Operation, workers: usize, installed: Option<&str>) -> Result<(Report, Vec<Planned>), String> {
    let signatures = scan_signatures(root, &m.files.iter().collect::<Vec<_>>(), op, workers, Stage::Check)?;
    // A PlayOnline Viewer without an installed version is a fresh install: its installer's
    // files are not in the retained patch history (and there is no installer baseline for
    // them), so they read as modified though nobody changed them. They are replaced like
    // any other, with transaction backups, but are not presented as the user's copies.
    let keep = product == Product::Ffxi || installed.is_some();
    plan_from_signatures(product, m, mode, op, signatures, &|index| product.changed_since(installed, &m.files[index]), keep)
}

/// Classify each file and plan the work. Missing and outdated (an older official or
/// stock-installer version) files are always planned. A modified file (no official
/// identity) is the user's: Repair replaces it, an Update replaces it only when the
/// update `changes` that file, and either way its copy is kept (`Planned::keep`) when
/// `keep_modified`. A modification the update doesn't change stays (`Report::left`) and
/// no longer holds the version back.
fn plan_from_signatures(product: Product, m: &Manifest, mode: Mode, op: &Operation, signatures: Vec<Option<LocalFile>>,
    changes: &dyn Fn(usize) -> bool, keep_modified: bool) -> Result<(Report, Vec<Planned>), String> {
    let baseline = if product == Product::Ffxi { Some(installer_baseline::load()?) } else { None };
    let mut report = Report {
        total: m.files.len(),
        ..Default::default()
    };
    let mut planned = Vec::new();
    for (index, (h, local)) in m.files.iter().zip(signatures).enumerate() {
        op.check_cancelled()?;
        let matched = local.and_then(|file| h.versions.iter().rposition(|v| v.sig == file.sig));
        if matched == Some(h.versions.len() - 1) {
            report.current += 1;
            continue;
        }
        // Retained patch history omits some bytes from the official full
        // installer. Recognize those only by the pinned per-path full MD5 and
        // length, never by missing patch.ver, timestamps, or a local file list.
        let stock = matched.is_none() && local.is_some_and(|file| baseline.is_some_and(|b|
            b.matches(&h.path, file.sig.len, file.md5)));
        let outdated = matched.is_some() || stock;
        if local.is_none() {
            report.missing += 1;
        } else if outdated {
            report.outdated += 1;
        } else {
            report.modified += 1;
        }
        if report.issues.len() < 200 {
            report.issues.push(format!(
                "{}: {}",
                h.path,
                if local.is_none() {
                    "missing"
                } else if outdated {
                    "outdated"
                } else {
                    "modified"
                }
            ));
        }
        let modified = local.is_some() && !outdated;
        if mode == Mode::Check { continue; }
        if mode == Mode::Update && modified && !changes(index) {
            report.left += 1;
            continue;
        }
        let mut action = Action::Direct;
        if let Some(i) = matched {
            let later = &h.versions[i + 1..];
            let delta_bytes = later.iter().try_fold(0u64, |sum, v| {
                v.indirect.as_ref().and_then(|d| sum.checked_add(d.len))
            });
            if delta_bytes.is_some_and(|n| n < h.current().direct_len) {
                action = Action::Deltas(i + 1);
            }
        }
        let keep = modified && keep_modified;
        planned.push(Planned {
            index,
            action,
            original: local.map(|file| file.sig),
            keep,
        });
        report.kept += usize::from(keep);
    }
    op.progress(
        Stage::Check,
        m.files.len() as u64,
        m.files.len() as u64,
        t("File verification complete"),
    );
    Ok((report, planned))
}

/// All bytes are still read and checked. Each bounded worker reuses its buffer;
/// the coordinator publishes completed-file progress and preserves manifest order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalFile { sig: FileSig, md5: [u8; 16] }

/// `stage` is Check for planning or Verify for Repair's final verification, which
/// also credits each file's verify work to the whole-update estimate.
fn scan_signatures(
    root: &Path,
    files: &[&FileHistory],
    op: &Operation,
    workers: usize,
    stage: Stage,
) -> Result<Vec<Option<LocalFile>>, String> {
    op.check_cancelled()?;
    op.progress(stage, 0, files.len() as u64, if stage == Stage::Verify { t("Verifying downloaded files") } else { t("Checking game files") });
    if files.is_empty() { return Ok(Vec::new()); }
    // Opt-in timing tests can compare larger pools without increasing the
    // production limit selected by scan.
    #[cfg(test)]
    let limit = MAX_BENCHMARK_CHECK_WORKERS;
    #[cfg(not(test))]
    let limit = MAX_CHECK_WORKERS;
    let workers = workers.clamp(1, limit).min(files.len());
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(workers * 2);
        let mut handles = Vec::with_capacity(workers);
        let mut error = None;
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            let stopped = &stopped;
            let spawned = std::thread::Builder::new().name("crystal-file-check".into())
                .spawn_scoped(scope, move || {
                    let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
                    while !stopped.load(Ordering::Acquire) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(history) = files.get(index) else { break; };
                        let result = (|| {
                            op.check_cancelled()?;
                            let path = confined(root, &history.path)?;
                            match fs::metadata(&path) {
                                Ok(metadata) if metadata.is_file() =>
                                    local_file_with_buffer(&path, op, &mut buffer)
                                        .map(|file| Some(without_local_laa(&path, history, file, op, &mut buffer))),
                                Ok(_) => Err(tf("{path} is not a regular file", &[("path", &path.display())])),
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                                Err(e) => Err(format!("{}: {e}", path.display())),
                            }
                        })();
                        if result.is_err() { stopped.store(true, Ordering::Release); }
                        if sender.send((index, result)).is_err() { break; }
                    }
                });
            match spawned {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    stopped.store(true, Ordering::Release);
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        drop(sender);
        let mut signatures = vec![None; files.len()];
        let mut done = 0;
        let mut last_progress = Instant::now();
        // Drain even after a failure so a worker in send cannot block a scoped join.
        for (index, result) in receiver {
            match result {
                Ok(signature) => {
                    signatures[index] = Some(signature);
                    done += 1;
                    if stage == Stage::Verify {
                        op.complete_update_work(UpdateWork::Verify, io_work(files[index].current().sig), None);
                    }
                    if last_progress.elapsed() >= Duration::from_millis(100) {
                        op.progress(stage, done, files.len() as u64, files[index].path.clone());
                        last_progress = Instant::now();
                    }
                }
                Err(e) => { if error.is_none() { error = Some(e); } }
            }
        }
        for handle in handles {
            if handle.join().is_err() && error.is_none() {
                error = Some(t("File verification worker failed.").into());
            }
        }
        if let Some(error) = error { return Err(error); }
        op.check_cancelled()?;
        signatures.into_iter().map(|signature| signature
            .ok_or_else(|| t("File verification did not complete.").into())).collect()
    })
}

/// The PlayOnline Viewer's `pol.exe` with IMAGE_FILE_LARGE_ADDRESS_AWARE set
/// locally (the community 4 GB patch) and nothing else changed is the current
/// official file: no Check reports it and no Repair or Update replaces it.
/// Recognised only when clearing that one flag gives exactly the current
/// official identity (length, byte sum, MD5 word); any other difference stays
/// a modification. Crystal never sets the flag on Square Enix's files; its own
/// game host is built large-address-aware instead.
fn without_local_laa(path: &Path, history: &FileHistory, file: LocalFile, op: &Operation, buffer: &mut [u8]) -> LocalFile {
    let official = history.current().sig;
    if !history.path.eq_ignore_ascii_case("pol.exe") || file.sig == official || file.sig.len != official.len {
        return file;
    }
    let Ok(mut bytes) = fs::read(path) else { return file };
    let Some(at) = laa_flag_byte(&bytes) else { return file };
    bytes[at] &= !0x20;
    match read_local_file(&bytes[..], op, buffer) {
        Ok(cleared) if cleared.sig == official => cleared,
        _ => file,
    }
}

/// The offset of the low byte of a PE file's Characteristics when it has
/// IMAGE_FILE_LARGE_ADDRESS_AWARE (0x20) set.
fn laa_flag_byte(bytes: &[u8]) -> Option<usize> {
    let nt = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    let at = nt.checked_add(22)?;
    (bytes.get(..2)? == b"MZ" && bytes.get(nt..nt.checked_add(4)?)? == b"PE\0\0" && bytes.get(at)? & 0x20 != 0).then_some(at)
}

fn signature(path: &Path, op: &Operation) -> Result<FileSig, String> {
    let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
    signature_with_buffer(path, op, &mut buffer)
}

fn signature_with_buffer(path: &Path, op: &Operation, buffer: &mut [u8]) -> Result<FileSig, String> {
    local_file_with_buffer(path, op, buffer).map(|file| file.sig)
}

/// `path` is one every caller already checked (`confined`, or `ensure_plain` for a cache file).
fn local_file_with_buffer(path: &Path, op: &Operation, buffer: &mut [u8]) -> Result<LocalFile, String> {
    let started = Instant::now();
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let signature = read_local_file(file, op, buffer)?;
    // Keep per-file durations: later Apply and Verify still execute serially.
    op.record_disk_io(signature.sig.len.max(1), started.elapsed());
    Ok(signature)
}

#[cfg(test)]
fn read_signature(file: impl Read, op: &Operation, buffer: &mut [u8]) -> Result<FileSig, String> {
    read_local_file(file, op, buffer).map(|file| file.sig)
}

fn read_local_file(mut file: impl Read, op: &Operation, buffer: &mut [u8]) -> Result<LocalFile, String> {
    debug_assert!(!buffer.is_empty());
    let mut hasher = md5::Context::new();
    let mut sum = 0i32;
    let mut len = 0u64;
    loop {
        op.check_cancelled()?;
        let n = file.read(buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        for b in &buffer[..n] {
            sum = sum.wrapping_add(*b as i8 as i32);
        }
        hasher.consume(&buffer[..n]);
        len += n as u64;
    }
    let d = hasher.compute().0;
    Ok(LocalFile { sig: FileSig {
        len,
        byte_sum: sum,
        md5_word: i32::from_le_bytes(d[..4].try_into().unwrap()),
    }, md5: d })
}

/// One patch-server connection on a stock-shaped socket (`stock_socket`: bound, 64 KB
/// SO_RCVBUF/SO_SNDBUF before connect, Nagle and delayed ACK left alone). One request
/// is in flight at a time: `exchange` writes a request and reads its whole reply.
/// Every end closes like stock (`close`, or the same close on its own thread on `Drop`).
struct Session {
    /// `None` once closed.
    stream: Option<tokio::net::TcpStream>,
    product: Product,
    server: Arc<str>,
    /// When the last reply on this connection ended: paces the next file
    /// (`next_file_pause`) and times the close (`stock_socket::CLOSE_AFTER_REPLY`).
    replied: Option<Instant>,
    /// The last file fetched on this connection, for `next_file_pause`.
    previous: Option<Transfer>,
    /// An exchange stopped partway (I/O error, timeout or cancel): the stream may still carry
    /// that reply, so nothing is sent on it again, and its close reads the rest first.
    broken: bool,
}
/// One file a fetch asks for: a direct (full) file of its transfer size, or a delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer { Direct(u64), Delta }

/// The pause before a new file, by the previous one (`PAUSE_AFTER_DELTA`).
fn next_file_pause(previous: Option<Transfer>) -> (Duration, Duration) {
    match previous {
        Some(Transfer::Direct(len)) if len <= SMALL_DIRECT => PAUSE_AFTER_SMALL_DIRECT,
        _ => PAUSE_AFTER_DELTA,
    }
}

/// A wire-visible pause on a high-resolution timer. tokio's timer on Windows wakes only on
/// the ~15.6 ms system tick (its GetQueuedCompletionStatusEx timeout: 7 ms waits ~15.6,
/// 115 ms up to ~131) unless the process raised the timer resolution, which Crystal never
/// does; std's sleep uses a high-resolution waitable timer.
async fn precise_sleep(duration: Duration) {
    if duration.is_zero() { return; }
    let _ = tokio::task::spawn_blocking(move || std::thread::sleep(duration)).await;
}

/// When Crystal last closed a connection to each patch server (`NEXT_CONNECTION_GAP`).
static LAST_CLOSE: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

fn note_closed(server: &str, at: Instant) {
    let mut closes = LAST_CLOSE.lock().unwrap_or_else(|e| e.into_inner());
    closes.retain(|(known, _)| known != server);
    closes.push((server.to_owned(), at));
}

/// A connection to `product`'s patch server Crystal closed elsewhere (`at`: its FIN), such as a
/// Play's W20 (patch.rs), so the next connection here keeps `NEXT_CONNECTION_GAP` from it too.
/// A later close already known wins.
pub(crate) fn note_patch_closed(product: Product, at: Instant) {
    note_server_closed(product.server(), at);
}

fn note_server_closed(server: &str, at: Instant) {
    let mut closes = LAST_CLOSE.lock().unwrap_or_else(|e| e.into_inner());
    if closes.iter().any(|(known, when)| known == server && *when >= at) { return; }
    closes.retain(|(known, _)| known != server);
    closes.push((server.to_owned(), at));
}

/// How long a new connection to `server` still waits for `NEXT_CONNECTION_GAP` after
/// Crystal's last FIN to it.
fn connect_gap(server: &str) -> Duration {
    LAST_CLOSE.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(known, _)| known == server)
        .map_or(Duration::ZERO, |(_, at)| crate::stock_socket::uniform(NEXT_CONNECTION_GAP).saturating_sub(at.elapsed()))
}

/// After an exchange stopped partway: read what is left of its reply (at most `DRAIN_LIMIT`
/// bytes, until the peer goes quiet for `DRAIN_IDLE`) so closesocket sends a FIN, not a reset
/// for unread data. Returns when the last byte arrived, if any did.
fn drain(stream: &std::net::TcpStream) -> Option<Instant> {
    const DRAIN_LIMIT: usize = 256 * 1024;
    const DRAIN_IDLE: Duration = Duration::from_millis(200);
    if stream.set_nonblocking(false).and_then(|_| stream.set_read_timeout(Some(DRAIN_IDLE))).is_err() { return None; }
    let (mut buffer, mut read, mut last) = (vec![0u8; 64 * 1024], 0, None);
    while read < DRAIN_LIMIT {
        match (&*stream).read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => { read += n; last = Some(Instant::now()); }
        }
    }
    last
}

/// The stock close of one patch connection, on the calling (blocking) thread: drain a broken
/// exchange's leftover reply, then FIN 100-115 ms after the last byte received. Returns when
/// the socket closed.
fn close_stream(stream: std::net::TcpStream, replied: Option<Instant>, broken: bool) -> Instant {
    let drained = if broken { drain(&stream) } else { None };
    match drained.or(replied) {
        Some(last) => crate::stock_socket::close_after_reply(stream, last),
        None => drop(stream),
    }
    Instant::now()
}

impl Drop for Session {
    /// An early return (error or cancel) still closes like stock, on its own thread; a next
    /// connection to the server waits for that FIN, then `NEXT_CONNECTION_GAP` from it.
    fn drop(&mut self) {
        let Some(stream) = self.stream.take() else { return };
        let server = self.server.clone();
        let Ok(stream) = stream.into_std() else { note_closed(&server, Instant::now()); return };
        let (replied, broken) = (self.replied, self.broken);
        // A next connection to this server waits for this FIN (`wait_closing`).
        closing(&server, 1);
        let thread_server = server.clone();
        let spawned = std::thread::Builder::new().name("patch-close".into()).spawn(move || {
            let closed = close_stream(stream, replied, broken);
            note_closed(&thread_server, closed);
            closing(&thread_server, -1);
        });
        if spawned.is_err() {
            // The stream was dropped with the closure: closed now.
            note_closed(&server, Instant::now());
            closing(&server, -1);
        }
    }
}

/// Closes still in flight on a `patch-close` thread, per server (`Session::drop`).
static CLOSING: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
static CLOSED: std::sync::Condvar = std::sync::Condvar::new();

fn closing(server: &str, delta: isize) {
    let mut pending = CLOSING.lock().unwrap_or_else(|e| e.into_inner());
    match pending.iter_mut().find(|(known, _)| known == server) {
        Some((_, count)) => *count = count.saturating_add_signed(delta),
        None if delta > 0 => pending.push((server.to_owned(), delta as usize)),
        None => {}
    }
    pending.retain(|(_, count)| *count > 0);
    if delta < 0 { CLOSED.notify_all(); }
}

/// Block until no close to `server` is in flight (bounded by `limit`), so the next SYN is
/// spaced from its FIN, not from when the session was dropped.
fn wait_closing(server: &str, limit: Duration) {
    let pending = CLOSING.lock().unwrap_or_else(|e| e.into_inner());
    let _ = CLOSED.wait_timeout_while(pending, limit, |pending| pending.iter().any(|(known, _)| known == server));
}

impl Session {
    async fn connect_to(product: Product, server: &str, op: &Operation) -> Result<Self, String> {
        let stream = cancel_io(stock_connect(server.to_owned()), op).await?;
        Ok(Self { stream: Some(stream), product, server: server.into(), replied: None, previous: None, broken: false })
    }
    /// The open stream (a closed session has none).
    fn stream(&mut self) -> Result<&mut tokio::net::TcpStream, String> {
        self.stream.as_mut().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotConnected).to_string())
    }
    /// Close like stock: FIN 100-115 ms after the last reply byte, never in its millisecond,
    /// timed and sent on one blocking thread (`stock_socket::close_after_reply`). Returns
    /// after the FIN, which a next connection to this server spaces itself from.
    async fn close(mut self) {
        self.shut().await;
    }
    async fn shut(&mut self) {
        let Some(stream) = self.stream.take() else { return };
        let (replied, broken) = (self.replied, self.broken);
        let closed = match stream.into_std() {
            Ok(stream) => tokio::task::spawn_blocking(move || close_stream(stream, replied, broken))
                .await.unwrap_or_else(|_| Instant::now()),
            Err(_) => Instant::now(),
        };
        note_closed(&self.server, closed);
    }
    /// Close this connection like stock, then open a new one to the same server (after
    /// `NEXT_CONNECTION_GAP` from that FIN). A failed connect leaves the session closed.
    async fn reconnect(&mut self, op: &Operation) -> Result<(), String> {
        self.shut().await;
        let mut fresh = Self::connect_to(self.product, &self.server.clone(), op).await?;
        self.stream = fresh.stream.take();
        (self.replied, self.broken) = (None, false);
        Ok(())
    }
    async fn exchange(
        &mut self,
        request: &[u8],
        expected: u32,
        op: &Operation,
    ) -> Result<Vec<u8>, String> {
        if self.broken { return Err(std::io::Error::from(std::io::ErrorKind::NotConnected).to_string()); }
        // Until the whole reply is read, an early return leaves it on the wire.
        self.broken = true;
        let stream = self.stream()?;
        cancel_io(stream.write_all(request), op).await?;
        let mut head = [0u8; 4];
        cancel_io(stream.read_exact(&mut head), op).await?;
        let len = polp::declared_len(&head);
        let cap = if expected == polp::kind::MANIFEST_REPLY {
            16 * 1024 * 1024
        } else {
            128 * 1024
        };
        if !(16..=cap).contains(&len) {
            return Err(tf("Invalid patch frame length {length}", &[("length", &len)]));
        }
        let mut data = vec![0u8; len];
        data[..4].copy_from_slice(&head);
        if expected == polp::kind::MANIFEST_REPLY {
            read_manifest_body(stream, &mut data, op, Duration::from_secs(30)).await?;
        } else {
            cancel_io(stream.read_exact(&mut data[4..]), op).await?;
        }
        self.replied = Some(Instant::now());
        self.broken = false;
        let p = polp::parse(&data)?;
        if p.kind != expected {
            return Err(tf(
                "Patch server reply {kind}, expected {expected}",
                &[("kind", &p.kind), ("expected", &expected)],
            ));
        }
        Ok(data)
    }
    /// A direct (full) file.
    #[cfg(test)]
    async fn fetch(&mut self, path: &str, len: u64, op: &Operation) -> Result<Vec<u8>, String> {
        self.fetch_tracked(path, len, Transfer::Direct(len), op, None).await
    }
    async fn fetch_tracked(&mut self, path: &str, len: u64, kind: Transfer, op: &Operation, progress: Option<&FileProgress>) -> Result<Vec<u8>, String> {
        if len > MAX_FILE {
            return Err(t("Download exceeds file limit").into());
        }
        let mut data = Vec::with_capacity(len as usize);
        if let Some(replied) = self.replied {
            // A new file: stock asks for it after processing the last one.
            let pause = crate::stock_socket::uniform(next_file_pause(self.previous)).saturating_sub(replied.elapsed());
            precise_sleep(pause).await;
        }
        while (data.len() as u64) < len {
            op.check_cancelled()?;
            let offset = data.len() as u32;
            let want = (len - data.len() as u64).min(polp::CHUNK_LEN as u64) as u32;
            let request = polp::file_request(&self.product.tags(), path, offset, want);
            // Up to 3 attempts per chunk. After a failed exchange the old connection closes
            // like stock (its leftover reply read first) before the next SYN, and the new
            // connection asks for the same chunk at the same offset.
            let mut attempt = 0;
            let frame = loop {
                match self.exchange(&request, polp::kind::FILE_REPLY, op).await {
                    Ok(frame) => break frame,
                    Err(_) if attempt < 2 => {
                        op.check_cancelled()?;
                        attempt += 1;
                        self.reconnect(op).await?;
                    }
                    Err(error) => return Err(tf("{path}: {error}", &[("path", &path), ("error", &error)])),
                }
            };
            let body = &frame[16..];
            let (got, chunk) = polp::file_chunk(body)?;
            let name_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
            let echoed = body
                .get(12..12 + name_len)
                .ok_or(t("Invalid echoed patch path"))?;
            if got != offset
                || chunk.len() != want as usize
                || echoed != [path.as_bytes(), &[0]].concat()
                || body.len() != 12 + name_len + chunk.len()
            {
                return Err(t("Patch chunk path, offset, or size mismatch").into());
            }
            data.extend_from_slice(chunk);
            if let Some(progress) = progress {
                progress.advance(chunk.len() as u64, path.to_owned(), false);
            } else {
                op.progress_bytes(Stage::Download, data.len() as u64, len, path.to_owned());
            }
        }
        self.previous = Some(kind);
        Ok(data)
    }
}
/// Resolve `server` through the system resolver (A records only, `dns::resolve_server`),
/// then connect a `stock_socket` after `NEXT_CONNECTION_GAP` from Crystal's last FIN to that
/// server (on the blocking pool, so on a high-resolution clock) and hand it to tokio.
async fn stock_connect(server: String) -> std::io::Result<tokio::net::TcpStream> {
    let stream = tokio::task::spawn_blocking(move || {
        let addresses = crate::dns::resolve_server(&server)?;
        // A drained close takes at most ~1 s (`drain` idle reads plus the stock close delay).
        wait_closing(&server, Duration::from_secs(2));
        std::thread::sleep(connect_gap(&server));
        let started = Instant::now();
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, t("no address"));
        for address in addresses {
            let left = CONNECT_LIMIT.saturating_sub(started.elapsed());
            if left.is_zero() { break; }
            match crate::stock_socket::connect(&address, left) {
                Ok(stream) => return Ok(stream),
                Err(error) => last = error,
            }
        }
        Err(last)
    }).await.map_err(std::io::Error::other)??;
    stream.set_nonblocking(true)?;
    tokio::net::TcpStream::from_std(stream)
}

// A manifest can take longer than 30 seconds while bytes keep arriving. Limit
// inactivity between reads, not the duration of the entire bounded frame.
async fn read_manifest_body<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
    data: &mut [u8],
    op: &Operation,
    idle_limit: Duration,
) -> Result<(), String> {
    let mut received = 4;
    let detail = t("Downloading the official file manifest");
    op.progress_bytes(Stage::Manifest, received as u64, data.len() as u64, detail);
    while received < data.len() {
        op.check_cancelled()?;
        let end = (received + 64 * 1024).min(data.len());
        let count = cancel_io_with_timeout(stream.read(&mut data[received..end]), op, idle_limit).await?;
        if count == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string());
        }
        received += count;
        op.progress_bytes(Stage::Manifest, received as u64, data.len() as u64, detail);
    }
    Ok(())
}

async fn cancel_io<F, T>(future: F, op: &Operation) -> Result<T, String>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    cancel_io_with_timeout(future, op, Duration::from_secs(30)).await
}

async fn cancel_io_with_timeout<F, T>(future: F, op: &Operation, timeout: Duration) -> Result<T, String>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    tokio::select! {
        result = future => result.map_err(|e| e.to_string()),
        _ = tokio::time::sleep(timeout) => Err(t("Patch server timed out").into()),
        cancelled = op.cancelled_wait() => Err(cancelled),
    }
}

fn decode_payload(data: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let (&flag, body) = data.split_first().ok_or(t("Empty compressed payload"))?;
    let out = match flag {
        1 => {
            if body.len() > max {
                return Err(t("Stored payload exceeds expected size").into());
            }
            body.to_vec()
        }
        3 => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(body)
                .take(max as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|e| e.to_string())?;
            out
        }
        0 | 2 => lz::decode_direct_bounded(body, max)?,
        _ => return Err(tf("Unsupported patch compression flag {flag}", &[("flag", &flag)])),
    };
    if out.len() > max {
        return Err(t("Decoded payload exceeds expected size").into());
    }
    Ok(out)
}
async fn produce(
    session: &mut Session,
    root: &Path,
    h: &FileHistory,
    action: &Action,
    op: &Operation,
    progress: Option<&FileProgress>,
) -> Result<Vec<u8>, String> {
    match action {
        Action::Direct => {
            let len = h.current().direct_len;
            let raw = session
                .fetch_tracked(&h.current().direct, len, Transfer::Direct(len), op, progress)
                .await?;
            decode_payload(&raw, h.current().sig.len as usize)
        }
        Action::Deltas(from) => {
            let mut data = read_delta_base(&confined(root, &h.path)?, h.versions[from - 1].sig, op)?;
            for v in &h.versions[*from..] {
                op.check_cancelled()?;
                let d = v.indirect.as_ref().ok_or(t("Missing delta"))?;
                let raw = session.fetch_tracked(&d.path, d.len, Transfer::Delta, op, progress).await?;
                data = lz::apply_indirect_bounded(&raw, &data, v.sig.len as usize)?;
                if FileSig::of(&data) != v.sig {
                    return Err(tf("{path}: delta signature mismatch", &[("path", &h.path)]));
                }
            }
            Ok(data)
        }
    }
}

fn read_delta_base(path: &Path, expected: FileSig, op: &Operation) -> Result<Vec<u8>, String> {
    op.check_cancelled()?;
    ensure_plain(path)?;
    let input = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::with_capacity(expected.len as usize);
    // Read one extra byte to reject growth without reading an arbitrarily
    // enlarged file after the scan selected its old canonical signature.
    input.take(expected.len.saturating_add(1)).read_to_end(&mut data).map_err(|e| e.to_string())?;
    op.check_cancelled()?;
    if FileSig::of(&data) != expected {
        return Err(t("Delta base changed after verification").into());
    }
    Ok(data)
}

fn validate_relative(path: &str) -> Result<(), String> {
    let unsafe_path = || tf("Unsafe patch path {path}", &[("path", &format!("{path:?}"))]);
    if path.is_empty()
        || path.contains('\\')
        || path.bytes().any(|b| b < 32)
        || path.contains([':', '<', '>', '"', '|', '?', '*'])
    {
        return Err(unsafe_path());
    }
    for p in path.split('/') {
        let stem = p.split('.').next().unwrap_or("").to_ascii_uppercase();
        if p.is_empty()
            || p == "."
            || p == ".."
            || p.ends_with(['.', ' '])
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(unsafe_path());
        }
    }
    if Path::new(path)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(unsafe_path());
    }
    Ok(())
}
pub(crate) fn ensure_plain(path: &Path) -> Result<(), String> {
    let mut at = PathBuf::new();
    for part in path.components() {
        at.push(part);
        // A Windows verbatim drive prefix (`\\?\C:`) is not a directory.
        // Keep it in the path, then inspect only after RootDir has completed it.
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        plain_component(&at)?;
    }
    Ok(())
}
/// `at`, if it exists, is neither a symlink nor another reparse point.
fn plain_component(at: &Path) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    match fs::symlink_metadata(at) {
        Ok(m) if m.file_type().is_symlink() => Err(tf("Symlink path {path}", &[("path", &at.display())])),
        Ok(m) if m.file_attributes() & 0x400 != 0 => Err(tf("Reparse path {path}", &[("path", &at.display())])),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", at.display())),
    }
}
/// `root\rel`, with each component below `root` checked (`plain_component`). The root itself was
/// checked in full where it was accepted (`prepare_with`, `recover_interrupted`), so it is not
/// stat'ed again for every file.
fn confined(root: &Path, rel: &str) -> Result<PathBuf, String> {
    validate_relative(rel)?;
    let mut at = root.to_path_buf();
    for part in rel.split('/') {
        at.push(part);
        plain_component(&at)?;
    }
    Ok(root.join(rel))
}
fn save(path: &Path, data: &[u8]) -> Result<(), String> {
    // The temporary file shares `path`'s parents; `publish` checks both leaves.
    let tmp = path.with_extension("xi-part");
    ensure_plain(&tmp)?;
    let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(data).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    publish(&tmp, path)
}

/// Publish flushed bytes without an unflushed Windows rename between a
/// recovered file and deletion of its sole transaction journal.
fn publish(from: &Path, to: &Path) -> Result<(), String> {
    ensure_plain(from)?;
    ensure_plain(to)?;
    crate::fsutil::replace(from, to)
        .map_err(|error| tf("Publish {path}: {error}", &[("path", &to.display()), ("error", &error)]))
}

#[derive(Serialize, Deserialize)]
struct Journal {
    id: String,
    committed: bool,
    entries: Vec<Entry>,
    /// `RECORDS_FORMAT` when the user modifications this transaction replaced went to
    /// `KEPT_DIR`; 0 (absent) in older builds' records, whose backups may hold the only
    /// copy of a modification their Repair replaced.
    #[serde(default)]
    format: u32,
}
/// See `Journal::format`.
const RECORDS_FORMAT: u32 = 1;
#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    backup: Option<String>,
    // Older journals did not record the backup identity. Preserve recovery
    // compatibility, while checking every backup written by this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original: Option<Sig>,
    expected: Sig,
    /// The replaced file was a user modification, copied to `KEPT_DIR` on commit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    kept: bool,
}
#[derive(Serialize, Deserialize)]
struct Sig {
    len: u64,
    sum: i32,
    word: i32,
}
impl From<FileSig> for Sig {
    fn from(s: FileSig) -> Self {
        Self {
            len: s.len,
            sum: s.byte_sum,
            word: s.md5_word,
        }
    }
}
impl Sig {
    fn matches(&self, s: FileSig) -> bool {
        self.len == s.len && self.sum == s.byte_sum && self.word == s.md5_word
    }
}
fn write_journal(root: &Path, j: &Journal) -> Result<(), String> {
    save(
        &confined(root, &format!("{STATE_DIR}/transaction.json"))?,
        &serde_json::to_vec(j).map_err(|e| e.to_string())?,
    )
}
/// Transaction mechanics share an internal liveness seam with isolated fixture
/// tests. Every production call goes through `commit` and its real process guard.
#[cfg(test)]
fn apply_with_guard<F, G>(
    root: &Path,
    ready: &[(String, PathBuf, FileSig, Option<FileSig>)],
    op: &Operation,
    verify: F,
    guard: G,
) -> Result<bool, String>
where
    F: FnOnce() -> Result<Option<(Vec<u8>, Vec<u8>)>, String>,
    G: Fn(&Path) -> Result<(), String>,
{
    apply_keeping(root, ready, &HashSet::new(), &|_, _| false, op, verify, guard)
}

/// `keep`: paths of user modifications being replaced. After every file verifies and
/// before the version advances, their originals (the transaction's backups) are copied
/// to `KEPT_DIR` (`keep_copy`). A successful commit then cleans up; `official` tells the
/// cleanup which older backups hold official bytes rather than a user's.
fn apply_keeping<F, G>(
    root: &Path,
    ready: &[(String, PathBuf, FileSig, Option<FileSig>)],
    keep: &HashSet<String>,
    official: &dyn Fn(&str, &LocalFile) -> bool,
    op: &Operation,
    verify: F,
    guard: G,
) -> Result<bool, String>
where
    F: FnOnce() -> Result<Option<(Vec<u8>, Vec<u8>)>, String>,
    G: Fn(&Path) -> Result<(), String>,
{
    let id = format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos(),
        std::process::id()
    );
    let backup_dir = confined(root, &format!("{STATE_DIR}/backups/{id}"))?;
    fs::create_dir_all(&backup_dir).map_err(|e| e.to_string())?;
    let mut j = Journal {
        id,
        committed: false,
        entries: Vec::new(),
        format: RECORDS_FORMAT,
    };
    write_journal(root, &j)?;
    // The final verification failed: a file is not what the plan expected, even with nothing
    // replaced (a Repair whose scan said current), so the next update checks every file.
    let mut verify_failed = false;
    let result = (|| {
        for (i, (path, cache, sig, original)) in ready.iter().enumerate() {
            let file_started = Instant::now();
            op.progress(Stage::Apply, i as u64, ready.len() as u64, path.clone());
            op.check_cancelled()?;
            guard(root)?;
            let dest = confined(root, path)?;
            let now = if dest.is_file() {
                Some(signature(&dest, op)?)
            } else {
                None
            };
            if now != *original {
                return Err(tf("{path} changed since the check; retry", &[("path", path)]));
            }
            replace(root, &mut j, path, cache, *sig, now, op)?;
            op.complete_update_work(UpdateWork::Apply, applying_work(*sig, *original), Some(file_started.elapsed()));
        }
        op.progress(Stage::Apply, ready.len() as u64, ready.len() as u64, t("Applying files"));
        let metadata = verify().inspect_err(|_| verify_failed = true)?;
        // The user's copies go to KEPT_DIR before anything can commit. A copy written
        // before a later failure duplicates a file the rollback then restores; an older
        // kept copy is never replaced, so a rollback cannot lose one.
        for entry in j.entries.iter_mut().filter(|entry| keep.contains(&entry.path)) {
            if let Some(backup) = &entry.backup {
                op.check_cancelled()?;
                keep_copy(root, &entry.path, backup, &j.id)?;
                entry.kept = true;
            }
        }
        if let Some((cfg, ver)) = &metadata {
            for (name, data) in [("patch.cfg", cfg), ("patch.ver", ver)] {
                op.check_cancelled()?;
                guard(root)?;
                // An unchanged record needs neither a backup nor a write.
                let dest = confined(root, name)?;
                if dest.is_file() && fs::read(&dest).is_ok_and(|now| now == *data) { continue; }
                let tmp = confined(root, &format!("{STATE_DIR}/staged/{name}"))?;
                save(&tmp, data)?;
                replace(root, &mut j, name, &tmp, FileSig::of(data), None, op)?;
            }
        }
        op.check_cancelled()?;
        j.committed = true;
        save(
            &backup_dir.join("records.json"),
            &serde_json::to_vec(&j).map_err(|e| e.to_string())?,
        )?;
        write_journal(root, &j)?;
        fs::remove_file(confined(root, &format!("{STATE_DIR}/transaction.json"))?)
            .map_err(|e| e.to_string())?;
        if metadata.is_some() { clean_up_after_commit(root, &j.id, official); }
        Ok(metadata.is_some())
    })();
    if let Err(e) = result {
        // A file was replaced, the final verification failed, or recovery fails below: the
        // next update checks every file. Any other failure before the first replacement
        // changed nothing and keeps the version plan.
        let touched = !j.entries.is_empty();
        // Rollback ignores user cancellation but preserves the recorded backups.
        let recovered = recover_with_guard(root, &guard);
        if touched || verify_failed || recovered.is_err() { mark_full_check(root); }
        return match recovered {
            Ok(_) => Err(tf(
                "{error}. Transaction recovered; verified downloads retained.",
                &[("error", &e)],
            )),
            Err(r) => Err(tf(
                "{error}. Recovery required: {recovery}. Backups and transaction retained.",
                &[("error", &e), ("recovery", &r)],
            )),
        };
    }
    result
}
/// `known`: the signature the caller just read from the file being replaced (`None` reads it
/// here). The backup is still checked against it, and the file read again after copying.
#[allow(clippy::too_many_arguments)]
fn replace(
    root: &Path,
    j: &mut Journal,
    path: &str,
    cache: &Path,
    sig: FileSig,
    known: Option<FileSig>,
    op: &Operation,
) -> Result<(), String> {
    let dest = confined(root, path)?;
    ensure_plain(cache)?;
    if signature(cache, op)? != sig {
        return Err(tf("{path}: staged content changed", &[("path", &path)]));
    }
    let (backup, original) = if dest.exists() {
        let rel = format!("{STATE_DIR}/backups/{}/{}.bin", j.id, j.entries.len());
        let p = confined(root, &rel)?;
        let original = match known { Some(known) => known, None => signature(&dest, op)? };
        fs::copy(&dest, &p).map_err(|e| e.to_string())?;
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        if signature(&p, op)? != original || signature(&dest, op)? != original {
            return Err(tf("{path}: original changed while backing up; retry", &[("path", &path)]));
        }
        (Some(rel), Some(original.into()))
    } else {
        (None, None)
    };
    j.entries.push(Entry {
        path: path.to_string(),
        backup,
        original,
        expected: sig.into(),
        kept: false,
    });
    write_journal(root, j)?;
    if let Some(parent) = dest.parent() {
        ensure_plain(parent)?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = dest.with_extension(format!("xi-{}", j.id));
    ensure_plain(&tmp)?;
    if tmp.exists() {
        return Err(t("Replacement temporary path already exists").into());
    }
    fs::copy(cache, &tmp).map_err(|e| e.to_string())?;
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    op.check_cancelled()?;
    publish(&tmp, &dest)?;
    if signature(&dest, op)? != sig {
        return Err(tf("{path}: replacement did not verify", &[("path", &path)]));
    }
    Ok(())
}
/// Copy a replaced modification's backup into `KEPT_DIR` (flushed, then published). It goes
/// to `<path>` when that is free, else beside it as `<path>.<id>` (then `<path>.<id>-2`, …):
/// an earlier kept copy is never replaced. Nothing is written when one of those already
/// holds these bytes. `id` is the transaction that replaced the file.
fn keep_copy(root: &Path, path: &str, backup: &str, id: &str) -> Result<(), String> {
    let source = confined(root, backup)?;
    let cannot = |error: &dyn std::fmt::Display| tf("{path}: cannot keep your copy: {error}", &[("path", &path), ("error", error)]);
    let sig = FileSig::of_path(&source).map_err(|e| cannot(&e))?;
    let mut free = None;
    for attempt in 0..100u32 {
        let name = match attempt {
            0 => format!("{KEPT_DIR}/{path}"),
            1 => format!("{KEPT_DIR}/{path}.{id}"),
            n => format!("{KEPT_DIR}/{path}.{id}-{n}"),
        };
        let candidate = confined(root, &name)?;
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => { free = Some(candidate); break; }
            Ok(metadata) if metadata.is_file() && FileSig::of_path(&candidate).map_err(|e| cannot(&e))? == sig => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(cannot(&error)),
        }
    }
    let dest = free.ok_or_else(|| cannot(&"no free name"))?;
    if let Some(parent) = dest.parent() {
        ensure_plain(parent)?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".xi-part");
    let tmp = dest.with_file_name(name);
    ensure_plain(&tmp)?;
    fs::copy(&source, &tmp).map_err(|e| cannot(&e))?;
    fs::OpenOptions::new().write(true).open(&tmp).and_then(|file| file.sync_all()).map_err(|e| e.to_string())?;
    if FileSig::of_path(&tmp)? != sig {
        let _ = fs::remove_file(&tmp);
        return Err(cannot(&"the copy did not verify"));
    }
    publish(&tmp, &dest)
}

/// After a verified commit: drop the staged downloads (all of them; nothing staged is
/// useful once every file is current) and older transactions' backups. Keeps `KEPT_DIR`
/// and this transaction's backups and records until the next commit. An older build's
/// transaction (`Journal::format` 0) may hold the only copy of a modification its Repair
/// replaced: those copies move to `KEPT_DIR` first (`retire_backups`). Best effort: a
/// leftover is removed by the next successful update.
fn clean_up_after_commit(root: &Path, id: &str, official: &dyn Fn(&str, &LocalFile) -> bool) {
    if let Ok(staged) = confined(root, &format!("{STATE_DIR}/staged")) {
        for entry in fs::read_dir(staged).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) { let _ = fs::remove_file(entry.path()); }
        }
    }
    if let Ok(backups) = confined(root, &format!("{STATE_DIR}/backups")) {
        for entry in fs::read_dir(backups).into_iter().flatten().flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            let older = name != id && !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit() || b == b'-');
            if older && entry.file_type().is_ok_and(|kind| kind.is_dir()) && ensure_plain(&entry.path()).is_ok()
                && retire_backups(root, &name, official).is_ok() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// `Ok` when the older transaction `name`'s backups may be deleted: it has no records (it
/// rolled back: every backup was restored) or this build's records (its user modifications
/// are in `KEPT_DIR`). An older build's records kept nothing there: each of its backups
/// that is no official identity (`official`) and is not marked kept is copied to
/// `KEPT_DIR` first. `Err` for records that cannot be read or a copy that fails: the
/// folder stays for the next commit to try again.
fn retire_backups(root: &Path, name: &str, official: &dyn Fn(&str, &LocalFile) -> bool) -> Result<(), String> {
    let records = confined(root, &format!("{STATE_DIR}/backups/{name}/records.json"))?;
    let bytes = match fs::read(&records) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if journal.format >= RECORDS_FORMAT { return Ok(()); }
    let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
    let operation = Operation::new();
    for entry in &journal.entries {
        // The version records are Crystal's own, never a user's file.
        if entry.kept || ["patch.cfg", "patch.ver"].iter().any(|record| entry.path.eq_ignore_ascii_case(record)) { continue; }
        let Some(backup) = &entry.backup else { continue };
        if !backup.starts_with(&format!("{STATE_DIR}/backups/{name}/")) {
            return Err(t("Invalid backup path in transaction").into());
        }
        let source = confined(root, backup)?;
        if !source.is_file() { continue; }
        let file = local_file_with_buffer(&source, &operation, &mut buffer)?;
        if official(&entry.path, &file) { continue; }
        keep_copy(root, &entry.path, backup, name)?;
    }
    Ok(())
}

/// Finish or undo an interrupted transaction. `true` when there was one.
fn recover(root: &Path) -> Result<bool, String> {
    recover_with_guard(root, ensure_idle)
}

/// Before a Play: finish or undo an interrupted update in `install` under the updater's
/// lock, instead of refusing the start. `Ok(false)`: there was none.
pub(crate) fn recover_interrupted(install: &Path) -> Result<bool, String> {
    ensure_plain(install)?;
    let root = fs::canonicalize(install).map_err(|e| tf("Install directory: {error}", &[("error", &e)]))?;
    ensure_plain(&root)?;
    if !confined(&root, &format!("{STATE_DIR}/transaction.json"))?.exists() { return Ok(false); }
    let _lock = open_update_lock(&confined(&root, &format!("{STATE_DIR}/lock"))?)
        .map_err(|e| tf("Another updater may be using this install: {error}", &[("error", &e)]))?;
    let recovered = recover(&root)?;
    if recovered { mark_full_check(&root); }
    Ok(recovered)
}

fn recover_with_guard(root: &Path, guard: impl Fn(&Path) -> Result<(), String>) -> Result<bool, String> {
    let path = confined(root, &format!("{STATE_DIR}/transaction.json"))?;
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(t("Updater transaction is unexpectedly large").into());
    }
    let j: Journal = serde_json::from_slice(&bytes)
        .map_err(|e| tf("Invalid updater transaction: {error}", &[("error", &e)]))?;
    if j.id.is_empty() || !j.id.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return Err(t("Invalid updater transaction identity").into());
    }
    if !j.committed {
        for entry in j.entries.iter().rev() {
            guard(root)?;
            let dest = confined(root, &entry.path)?;
            if let Some(backup) = &entry.backup {
                if !backup.starts_with(&format!("{STATE_DIR}/backups/{}/", j.id)) {
                    return Err(t("Invalid backup path in transaction").into());
                }
                let source = confined(root, backup)?;
                if !source.is_file() {
                    return Err(tf("Missing recovery backup {backup}", &[("backup", backup)]));
                }
                let restore_sig = FileSig::of_path(&source)?;
                if entry.original.as_ref().is_some_and(|original| !original.matches(restore_sig)) {
                    return Err(tf("{path}: recovery backup changed; backups and transaction were retained",
                        &[("path", &entry.path)]));
                }
                if dest.is_file() {
                    let s = FileSig::of_path(&dest)?;
                    if !entry.expected.matches(s) && s != restore_sig {
                        return Err(tf(
                            "{path} changed after interruption; preserve it before recovery",
                            &[("path", &entry.path)],
                        ));
                    }
                }
                let tmp = dest.with_extension("xi-recover");
                ensure_plain(&tmp)?;
                fs::copy(&source, &tmp).map_err(|e| e.to_string())?;
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(&tmp)
                    .map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
                drop(file);
                if FileSig::of_path(&tmp)? != restore_sig {
                    return Err(tf("{path}: recovery copy did not verify", &[("path", &entry.path)]));
                }
                guard(root)?;
                publish(&tmp, &dest)?;
                if FileSig::of_path(&dest)? != restore_sig {
                    return Err(tf("{path}: restored file did not verify", &[("path", &entry.path)]));
                }
            } else if dest.exists() {
                if !entry.expected.matches(FileSig::of_path(&dest)?) {
                    return Err(tf("{path} changed after interruption", &[("path", &entry.path)]));
                }
                guard(root)?;
                fs::remove_file(&dest).map_err(|e| e.to_string())?;
            }
            let leftover = dest.with_extension(format!("xi-{}", j.id));
            ensure_plain(&leftover)?;
            if leftover.exists() {
                fs::remove_file(leftover).map_err(|e| e.to_string())?;
            }
        }
    }
    fs::remove_file(path).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Nothing runs from `root` or has a module loaded from it (`folder_users`).
pub(crate) fn ensure_idle(root: &Path) -> Result<(), String> {
    if folder_users(root)?.is_empty() { Ok(()) } else { Err(t("Close the game using this install before updating files").into()) }
}

/// The processes that use `root`: a game, POL host or loader whose image or a loaded module is
/// in that folder, or one of those that cannot be inspected (it may be). Only processes that
/// actually use the folder count: a signed-in host at the PlayOnline main menu maps polcore from
/// the PlayOnline folder and nothing from FINAL FANTASY XI's until GameStart loads FFXi.dll and
/// FFXiMain.dll (crystal-loader game.rs `run_game`), so it does not hold a game update back.
/// Earlier builds counted every `host32-`/`host64-` process as busy whatever its folder.
/// A process that is starting (created suspended) or exiting while it is inspected is retried
/// (`module_snapshot`), and one that exited meanwhile is not a user.
pub(crate) fn folder_users(root: &Path) -> Result<Vec<u32>, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{Module32FirstW, Module32NextW, MODULEENTRY32W};
    let processes = crate::win::process::processes().ok_or_else(|| t("Cannot verify that the game is stopped"))?;
    unsafe {
        let mut users = Vec::new();
        let mut incomplete = false;
        for entry in processes {
            let name = entry.name.to_ascii_lowercase();
            let pid = entry.pid;
            let mut busy = false;
            if name == "pol.exe"
                || name == "startpol.exe"
                || name == "crystal-loader.exe"
                || name.contains("ffxi")
                || name.starts_with("xi_recomp")
                || name.starts_with("xi-client")
            {
                match process_image(pid) {
                    Image::Path(exe) => busy |= under_root(&exe, root),
                    Image::Gone => {}
                    Image::Unknown => busy = true,
                }
                match module_snapshot(pid) {
                    ModuleSnapshot::Handle(modules) => {
                        let mut module: MODULEENTRY32W = std::mem::zeroed();
                        module.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
                        let mut found = Module32FirstW(modules, &mut module);
                        // No module at all (a process just starting or ending) is an empty list.
                        if found == 0 && GetLastError() != ERROR_NO_MORE_FILES { incomplete = true; }
                        while found != 0 {
                            let end = module
                                .szExePath
                                .iter()
                                .position(|c| *c == 0)
                                .unwrap_or(module.szExePath.len());
                            let path =
                                PathBuf::from(String::from_utf16_lossy(&module.szExePath[..end]));
                            if under_root(&path, root) {
                                busy = true;
                                break;
                            }
                            found = Module32NextW(modules, &mut module);
                            if found == 0 && GetLastError() != ERROR_NO_MORE_FILES { incomplete = true; }
                        }
                        CloseHandle(modules);
                    }
                    ModuleSnapshot::Gone => busy = false,
                    ModuleSnapshot::Failed => busy = true,
                }
            }
            if busy { users.push(pid); }
            if incomplete {
                break;
            }
        }
        if incomplete {
            return Err(t("Cannot verify that the game is stopped").into());
        }
        Ok(users)
    }
}

/// How often, and how far apart, a process caught starting or exiting is inspected again.
const INSPECT_RETRIES: u32 = 10;
const INSPECT_RETRY_PAUSE: Duration = Duration::from_millis(25);

enum Image { Path(PathBuf), Gone, Unknown }

/// `pid`'s image path; `Gone` when the process no longer exists.
fn process_image(pid: u32) -> Image {
    use crate::win::process::{image_path_of, open, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
    for attempt in 0..INSPECT_RETRIES {
        if attempt > 0 { std::thread::sleep(INSPECT_RETRY_PAUSE); }
        let process = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            Ok(process) => process,
            Err(error) => match error.raw_os_error().map(|code| code as u32) {
                // No such process (any more): it exited after the process list was taken.
                Some(ERROR_INVALID_PARAMETER) => return Image::Gone,
                // Not transient: it stays uninspectable.
                Some(ERROR_ACCESS_DENIED) => return Image::Unknown,
                _ => continue,
            },
        };
        if let Ok(path) = image_path_of(&process) { return Image::Path(path); }
    }
    Image::Unknown
}

enum ModuleSnapshot { Handle(windows_sys::Win32::Foundation::HANDLE), Gone, Failed }

/// A module snapshot of `pid`. ERROR_BAD_LENGTH and ERROR_PARTIAL_COPY (a process being created
/// or ending) are retried, as CreateToolhelp32Snapshot's documentation asks; `Gone` when the
/// process exited meanwhile.
fn module_snapshot(pid: u32) -> ModuleSnapshot {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_BAD_LENGTH, ERROR_INVALID_PARAMETER, ERROR_PARTIAL_COPY, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32};
    for attempt in 0..INSPECT_RETRIES {
        if attempt > 0 { std::thread::sleep(INSPECT_RETRY_PAUSE); }
        let modules = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if modules != INVALID_HANDLE_VALUE { return ModuleSnapshot::Handle(modules); }
        match unsafe { GetLastError() } {
            ERROR_BAD_LENGTH | ERROR_PARTIAL_COPY => {
                if matches!(process_image(pid), Image::Gone) { return ModuleSnapshot::Gone; }
            }
            ERROR_INVALID_PARAMETER => return ModuleSnapshot::Gone,
            _ => return ModuleSnapshot::Failed,
        }
    }
    ModuleSnapshot::Failed
}

fn under_root(path: &Path, root: &Path) -> bool {
    let normalized = |p: &Path| {
        p.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_ascii_lowercase()
    };
    let root = normalized(root);
    let path = normalized(path);
    path == root || path.starts_with(&(root + "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_base_reads_require_the_exact_previous_signature() {
        let root = fixture("bounded-delta-base");
        let path = root.join("base.DAT");
        let expected = FileSig::of(b"old");
        fs::write(&path, b"old").unwrap();
        assert_eq!(read_delta_base(&path, expected, &Operation::new()).unwrap(), b"old");
        let mut enlarged = vec![0; 2 * SIGNATURE_BUFFER_LEN];
        enlarged[..3].copy_from_slice(b"old");
        for data in [b"bad".as_slice(), b"ol".as_slice(), enlarged.as_slice()] {
            fs::write(&path, data).unwrap();
            assert!(read_delta_base(&path, expected, &Operation::new()).unwrap_err().contains("Delta base changed"));
        }
        let cancelled = Operation::new();
        cancelled.cancel();
        assert!(read_delta_base(&path, expected, &cancelled).unwrap_err().contains("Cancelled"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cache_only_staging_reuses_verified_files_without_a_server_connection() {
        let root = fixture("offline-staging-cache");
        let staged = root.join("staged");
        fs::create_dir(&staged).unwrap();
        let mut manifest = Manifest::default();
        let mut planned = Vec::new();
        for index in 0..24 {
            let path = format!("{index}.DAT");
            let data = format!("official cached bytes {index}").into_bytes();
            let sig = FileSig::of(&data);
            let history = FileHistory { path: path.clone(), optional: false, versions: vec![manifest::Version {
                stamp: "30260904_1".into(), sig,
                direct: format!("30260904_1/Direct/{path}.slc"), direct_len: data.len() as u64 + 1, indirect: None,
            }] };
            let cache = staged.join(format!("{}-{}.bin", hex_md5(path.as_bytes()), hex_md5(format!("{sig:?}").as_bytes())));
            fs::write(&cache, &data).unwrap();
            manifest.files.push(history);
            planned.push(Planned { index, action: Action::Direct, original: None, keep: false });
        }
        let expected_total = planned.iter().map(|p| download_size(&manifest.files[p.index], &p.action)).sum();
        let op = Operation::new();
        let (ready, bytes) = stage_plan(&root, &staged, &manifest, &planned, &op).await.unwrap();
        assert_eq!(bytes, 0);
        assert_eq!(ready.len(), planned.len());
        for (index, (path, cache, sig, original)) in ready.iter().enumerate() {
            assert_eq!(path, &manifest.files[index].path);
            assert_eq!(*sig, FileSig::of(&fs::read(cache).unwrap()));
            assert_eq!(*original, None);
        }
        assert_eq!((op.snapshot().done, op.snapshot().total), (expected_total, expected_total));
        assert!(!root.join("patch.ver").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interleaved_file_chunks_keep_the_whole_download_denominator() {
        let op = Operation::new();
        let downloads = Downloads::new(op.clone(), vec![100, 300]);
        let first = downloads.file(0);
        let second = downloads.file(1);
        first.advance(20, "first".into(), false);
        second.advance(80, "second".into(), false);
        let progress = op.snapshot();
        assert_eq!((progress.done, progress.total), (100, 400));
        assert_eq!(progress.remaining_percent(), Some(75));
        assert!(progress.bytes);
        first.advance(80, "first".into(), false);
        assert_eq!((op.snapshot().done, op.snapshot().total), (180, 400));
        second.advance(220, "second".into(), false);
        assert_eq!(op.snapshot().remaining_percent(), Some(0));
    }

    #[test]
    fn six_download_workers_publish_all_of_their_progress() {
        let op = Operation::new();
        let downloads = Downloads::new(op.clone(), vec![1000; 6]);
        std::thread::scope(|scope| {
            for index in 0..6 {
                let file = downloads.file(index);
                let op = op.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        file.advance(10, format!("file {index}"), false);
                        let progress = op.snapshot();
                        assert_eq!(progress.total, 6000);
                        assert!(progress.done <= progress.total);
                    }
                });
            }
        });
        assert_eq!((op.snapshot().done, op.snapshot().total), (6000, 6000));
    }

    #[test]
    fn full_file_fallback_and_verified_cache_keep_correct_remaining_bytes() {
        let op = Operation::new();
        let downloads = Downloads::new(op.clone(), vec![100, 200]);
        let delta = downloads.file(0);
        delta.advance(30, "delta".into(), false);
        delta.fallback(90, "full file".into());
        assert_eq!((op.snapshot().done, op.snapshot().total), (30, 320));
        delta.advance(90, "full file".into(), false);
        assert_eq!((op.snapshot().done, op.snapshot().total), (120, 320));
        downloads.file(1).advance(0, "verified cache".into(), true);
        assert_eq!((op.snapshot().done, op.snapshot().total), (320, 320));
        assert_eq!(op.snapshot().remaining, Some(Duration::ZERO));
    }

    #[tokio::test]
    async fn manifest_body_accepts_progress_beyond_one_io_timeout() {
        let expected = polp::seal(polp::kind::MANIFEST_REPLY, &[1; 252]);
        let mut data = vec![0; expected.len()];
        data[..4].copy_from_slice(&expected[..4]);
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let op = Operation::new();
        let idle_limit = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let send = async {
            let mut sent = 4;
            for chunk in expected[4..].chunks(32) {
                tokio::time::sleep(Duration::from_millis(60)).await;
                writer.write_all(chunk).await.unwrap();
                sent += chunk.len();
                // Observe progress before supplying the next chunk, as the UI does.
                while op.snapshot().done < sent as u64 {
                    tokio::task::yield_now().await;
                }
                let progress = op.snapshot();
                assert_eq!(progress.stage, Stage::Manifest);
                assert_eq!(progress.total, expected.len() as u64);
            }
        };
        let received = read_manifest_body(&mut reader, &mut data, &op, idle_limit);
        let ((), result) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(send, received)
        }).await.expect("the progressing manifest must complete");
        result.unwrap();
        assert!(started.elapsed() > idle_limit);
        assert_eq!(data, expected);
        assert_eq!(polp::parse(&data).unwrap().kind, polp::kind::MANIFEST_REPLY);
    }

    #[tokio::test]
    async fn manifest_body_still_stops_on_idle_eof_and_cancellation() {
        let (mut reader, writer) = tokio::io::duplex(64);
        let mut data = [0; 32];
        let op = Operation::new();
        let error = read_manifest_body(&mut reader, &mut data, &op, Duration::from_millis(40)).await.unwrap_err();
        assert_eq!(error, t("Patch server timed out"));
        assert_eq!(op.snapshot().done, 4);
        drop(writer);
        let error = read_manifest_body(&mut reader, &mut data, &op, Duration::from_secs(30)).await.unwrap_err();
        assert_eq!(error, std::io::Error::from(std::io::ErrorKind::UnexpectedEof).to_string());
        let (mut reader, _writer) = tokio::io::duplex(64);
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                op.cancel();
            };
            let (_, result) = tokio::join!(cancel,
                read_manifest_body(&mut reader, &mut data, &op, Duration::from_secs(30)));
            result
        }).await;
        assert!(result.expect("cancellation must interrupt the pending read").is_err());
    }

    fn fixture_apply<F>(
        root: &Path,
        ready: &[(String, PathBuf, FileSig, Option<FileSig>)],
        op: &Operation,
        verify: F,
    ) -> Result<bool, String>
    where
        F: FnOnce() -> Result<Option<(Vec<u8>, Vec<u8>)>, String>,
    {
        // These fixtures have no game process. Other suite tests may start a
        // host64 self-test process that deliberately trips production liveness.
        assert!(root.starts_with(std::env::temp_dir()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("xi-updater-"));
        apply_with_guard(root, ready, op, verify, |_| Ok(()))
    }
    #[test]
    fn unsafe_paths_are_rejected() {
        for path in [
            "../x", "/x", "C:/x", "a/../x", "a:b", "NUL.dat", "a\\b", "a/", "a.", "CON",
        ] {
            assert!(validate_relative(path).is_err(), "{path}");
        }
        assert!(validate_relative("Tools/FINAL FANTASY XI Config.chm").is_ok());
    }
    #[test]
    fn payload_limits_and_modes() {
        assert_eq!(decode_payload(&[1, 2, 3], 2).unwrap(), [2, 3]);
        assert!(decode_payload(&[1, 2, 3], 1).is_err());
        assert!(decode_payload(&[4, 0], 10).is_err());
    }
    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "xi-updater-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".xi-updater/staged")).unwrap();
        root
    }
    #[test]
    fn canonicalized_temp_root_can_be_confined_and_checked() {
        let root = fixture("canonical-root");
        let canonical = fs::canonicalize(&root).unwrap();
        ensure_plain(&canonical).unwrap();
        let path = confined(&canonical, "ROM/1/test.DAT").unwrap();
        assert!(path.starts_with(&canonical));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"fixture bytes").unwrap();
        ensure_plain(&path).unwrap();
        assert_eq!(
            signature(&path, &Operation::new()).unwrap(),
            FileSig::of(b"fixture bytes")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_validation_rejects_unrelated_and_other_product_folders() {
        let root = fixture("product-selection");
        assert!(validate_installation(Product::Ffxi, &root).is_err());
        assert!(validate_installation(Product::Viewer, &root).is_err());
        fs::write(root.join("pol.exe"), b"damaged viewer fixture").unwrap();
        assert!(validate_installation(Product::Viewer, &root).is_ok());
        assert!(validate_installation(Product::Ffxi, &root).is_err());
        fs::remove_file(root.join("pol.exe")).unwrap();
        fs::create_dir_all(root.join("ROM/0")).unwrap();
        fs::write(root.join("ROM/0/0.DAT"), b"surviving game data").unwrap();
        assert!(validate_installation(Product::Ffxi, &root).is_ok());
        assert!(validate_installation(Product::Viewer, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_waits_for_a_game_started_after_replacement() {
        let root = fixture("busy-recovery");
        let old = b"original";
        let new = b"replacement";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![("game.dat".into(), cache.clone(), FileSig::of(new), Some(FileSig::of(old)))];
        let running = std::cell::Cell::new(false);
        let guard = |_: &Path| if running.get() { Err("fixture game started".into()) } else { Ok(()) };
        let error = apply_with_guard(&root, &ready, &Operation::new(), || {
            running.set(true);
            Err("fixture verification interrupted".into())
        }, guard).unwrap_err();
        assert!(error.contains("Recovery required") && error.contains("fixture game started"), "{error}");
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
        assert_eq!(fs::read(&cache).unwrap(), new);
        assert!(root.join(".xi-updater/transaction.json").is_file());
        assert!(!root.join("patch.ver").exists());
        running.set(false);
        recover_with_guard(&root, guard).unwrap();
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), old);
        assert_eq!(fs::read(cache).unwrap(), new);
        assert!(!root.join(".xi-updater/transaction.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rejects_a_changed_backup_without_replacing_the_new_file() {
        let root = fixture("backup-integrity");
        let old = b"original";
        let new = b"replacement";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![("game.dat".into(), cache.clone(), FileSig::of(new), Some(FileSig::of(old)))];
        let journal = root.join(".xi-updater/transaction.json");
        let error = fixture_apply(&root, &ready, &Operation::new(), || {
            let recorded: Journal = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            let entry = &recorded.entries[0];
            assert!(entry.original.as_ref().unwrap().matches(FileSig::of(old)));
            fs::write(root.join(entry.backup.as_ref().unwrap()), b"corrupt!").unwrap();
            Err("fixture verification interrupted".into())
        }).unwrap_err();
        assert!(error.contains("recovery backup changed"), "{error}");
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
        assert!(journal.is_file());
        // An older on-disk journal remains recoverable after a launcher update.
        let mut recorded: serde_json::Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        let backup = recorded["entries"][0]["backup"].as_str().unwrap();
        fs::write(root.join(backup), old).unwrap();
        recorded["entries"][0].as_object_mut().unwrap().remove("original");
        fs::write(&journal, serde_json::to_vec(&recorded).unwrap()).unwrap();
        recover_with_guard(&root, |_| Ok(())).unwrap();
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), old);
        assert_eq!(fs::read(cache).unwrap(), new);
        assert!(!journal.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_after_replacement_restores_original_and_keeps_download() {
        let root = fixture("cancel");
        let op = Operation::new();
        op.begin_update();
        let old = b"old original";
        let new = b"verified replacement";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![(
            "game.dat".into(),
            cache.clone(),
            FileSig::of(new),
            Some(FileSig::of(old)),
        )];
        let result = fixture_apply(&root, &ready, &op, || {
            assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
            op.cancel();
            Ok(None)
        });
        assert!(result.is_err());
        assert!(!op.snapshot().overall.unwrap().complete, "a rolled-back update must not report completion");
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), old);
        assert_eq!(fs::read(cache).unwrap(), new);
        assert!(!root.join("patch.ver").exists());
        assert!(!root.join(".xi-updater/transaction.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_liveness_refusal_preserves_files_and_applied_metadata() {
        let root = fixture("liveness");
        let op = Operation::new();
        let original = b"original";
        let update = b"update";
        fs::write(root.join("game.dat"), original).unwrap();
        let cache = root.join(".xi-updater/staged/update.bin");
        fs::write(&cache, update).unwrap();
        let ready = vec![(
            "game.dat".into(),
            cache,
            FileSig::of(update),
            Some(FileSig::of(original)),
        )];
        let result = apply_with_guard(
            &root,
            &ready,
            &op,
            || panic!("Refused transaction must not reach verification"),
            |_| Err("fixture liveness refusal".into()),
        );
        assert!(result.unwrap_err().contains("fixture liveness refusal"));
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), original);
        assert!(!root.join("patch.ver").exists());
        assert!(!root.join(".xi-updater/transaction.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn successful_transaction_retains_original_backup_and_commits_metadata_last() {
        let root = fixture("commit");
        let op = Operation::new();
        op.begin_update();
        let old = b"old";
        let new = b"new";
        fs::write(root.join("game.dat"), old).unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, new).unwrap();
        let ready = vec![(
            "game.dat".into(),
            cache,
            FileSig::of(new),
            Some(FileSig::of(old)),
        )];
        let encoded = crate::patch::encode_applied_record("30260904_1", 1).unwrap();
        assert!(fixture_apply(&root, &ready, &op, || {
            assert!(!root.join("patch.ver").exists());
            assert!(!op.snapshot().overall.unwrap().complete);
            Ok(Some((b"end\n".to_vec(), encoded.clone())))
        })
        .unwrap());
        assert_eq!(fs::read(root.join("patch.ver")).unwrap(), encoded);
        assert_eq!(fs::read(root.join("game.dat")).unwrap(), new);
        let backup = fs::read_dir(root.join(".xi-updater/backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("0.bin");
        assert_eq!(fs::read(backup).unwrap(), old);
        assert!(!op.snapshot().overall.unwrap().complete, "the full run declares completion after transaction return");
        op.finish_update();
        assert!(op.snapshot().overall.unwrap().complete);
        assert_eq!(op.snapshot().overall.unwrap().remaining_percent(), Some(0));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_update_replaces_only_the_modifications_it_changes_and_repair_replaces_all() {
        let root = fixture("modified");
        fs::write(root.join("f.dat"), b"custom").unwrap();
        let m = Manifest {
            files: vec![FileHistory {
                optional: false,
                path: "f.dat".into(),
                versions: vec![manifest::Version {
                    stamp: "30260904_1".into(),
                    sig: FileSig::of(b"official"),
                    direct: "30260904_1/Direct/f.dat.slc".into(),
                    direct_len: 9,
                    indirect: None,
                }],
            }],
        };
        let op = Operation::new();
        // Installed 30260904_1 already: this update does not change f.dat, so the mod stays.
        let (report, plan) = scan_for(Product::Ffxi, &root, &m, Mode::Update, &op, Some("30260904_1")).unwrap();
        assert_eq!((report.modified, report.kept), (1, 0));
        assert!(plan.is_empty());
        // From 30260903_0 the update changes f.dat: official file in, the user's copy kept.
        let (report, plan) = scan_for(Product::Ffxi, &root, &m, Mode::Update, &op, Some("30260903_0")).unwrap();
        assert_eq!((report.modified, report.kept), (1, 1));
        assert_eq!(plan, vec![Planned { index: 0, action: Action::Direct, original: Some(FileSig::of(b"custom")), keep: true }]);
        // No installed version (fresh or unreadable record): every file counts as changed.
        assert_eq!(scan(&root, &m, Mode::Update, &op).unwrap().1.len(), 1);
        let (_, plan) = scan_for(Product::Ffxi, &root, &m, Mode::Repair, &op, Some("30260904_1")).unwrap();
        assert!(plan.len() == 1 && plan[0].keep);
        assert!(scan(&root, &m, Mode::Check, &op).unwrap().1.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "read-only timing run; requires CRYSTAL_FILE_CHECK_BENCH_INSTALL with a saved patch.cfg"]
    fn read_only_file_check_benchmark() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_FILE_CHECK_BENCH_INSTALL")
            .expect("Set CRYSTAL_FILE_CHECK_BENCH_INSTALL to the installation to read"));
        assert!(root.is_absolute());
        ensure_plain(&root).unwrap();
        let m = manifest::parse(&fs::read_to_string(root.join("patch.cfg")).unwrap()).unwrap();
        validate_manifest(&m).unwrap();
        assert!(!m.files.is_empty());
        let bytes: u64 = m.files.iter().map(|h| h.current().sig.len).sum();
        let mut reference = None;
        let workers: Vec<usize> = std::env::var("CRYSTAL_FILE_CHECK_BENCH_WORKERS")
            .unwrap_or_else(|_| format!("{MAX_CHECK_WORKERS},{MAX_CHECK_WORKERS}")).split(',')
            .map(|count| count.parse::<usize>().expect("worker counts must be integers"))
            .collect();
        assert!(!workers.is_empty() && workers.iter().all(|count| (1..=MAX_BENCHMARK_CHECK_WORKERS).contains(count)));
        for (repeat, workers) in workers.into_iter().enumerate() {
            let started = Instant::now();
            let (report, planned) = scan_with_workers(&root, &m, Mode::Check, &Operation::new(), workers).unwrap();
            assert!(planned.is_empty());
            if let Some(reference) = &reference { assert_eq!(&report, reference); }
            else { reference = Some(report.clone()); }
            eprintln!("file-check benchmark repeat={} workers={workers} elapsed={:.3}s files={} bytes={bytes} current={} outdated={} missing={} modified={}", repeat + 1,
                started.elapsed().as_secs_f64(), report.total, report.current, report.outdated,
                report.missing, report.modified);
        }
        // Sample evenly across the manifest to compare concurrency without five
        // additional full-install reads. Finish with a warm serial control.
        let sample = Manifest { files: m.files.iter().step_by(16).cloned().collect() };
        let mut reference = None;
        for workers in [1, 2, 4, 1] {
            let started = Instant::now();
            let result = scan_with_workers(&root, &sample, Mode::Check, &Operation::new(), workers).unwrap();
            if let Some(reference) = &reference { assert_eq!(&result, reference); }
            else { reference = Some(result.clone()); }
            eprintln!("file-check sample workers={workers} elapsed={:.3}s files={}", started.elapsed().as_secs_f64(), sample.files.len());
        }
    }

    #[test]
    #[ignore = "read-only component/concurrency timing; requires CRYSTAL_FILE_CHECK_BENCH_INSTALL"]
    fn read_only_file_check_profile() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_FILE_CHECK_BENCH_INSTALL")
            .expect("Set CRYSTAL_FILE_CHECK_BENCH_INSTALL to the installation to read"));
        assert!(root.is_absolute());
        ensure_plain(&root).unwrap();
        let m = manifest::parse(&fs::read_to_string(root.join("patch.cfg")).unwrap()).unwrap();
        validate_manifest(&m).unwrap();
        let mut indices: Vec<usize> = (0..m.files.len()).step_by(32).collect();
        let mut largest: Vec<usize> = (0..m.files.len()).collect();
        largest.sort_unstable_by_key(|&index| std::cmp::Reverse(m.files[index].current().sig.len));
        indices.extend(largest.into_iter().take(8));
        indices.sort_unstable();
        indices.dedup();
        let sample = Manifest { files: indices.into_iter().map(|index| m.files[index].clone()).collect() };
        let bytes: u64 = sample.files.iter().map(|h| h.current().sig.len).sum();
        let mut guards = Duration::ZERO;
        let mut opens = Duration::ZERO;
        let mut reads = Duration::ZERO;
        let mut sums = Duration::ZERO;
        let mut hashes = Duration::ZERO;
        let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
        for history in &sample.files {
            let started = Instant::now();
            let path = confined(&root, &history.path).unwrap();
            assert!(fs::metadata(&path).unwrap().is_file());
            ensure_plain(&path).unwrap();
            guards += started.elapsed();
            let started = Instant::now();
            let mut file = fs::File::open(&path).unwrap();
            opens += started.elapsed();
            let mut hasher = md5::Context::new();
            let mut sum = 0i32;
            let mut len = 0;
            loop {
                let started = Instant::now();
                let n = file.read(&mut buffer).unwrap();
                reads += started.elapsed();
                if n == 0 { break; }
                let started = Instant::now();
                for byte in &buffer[..n] { sum = sum.wrapping_add(*byte as i8 as i32); }
                sums += started.elapsed();
                let started = Instant::now();
                hasher.consume(&buffer[..n]);
                hashes += started.elapsed();
                len += n as u64;
            }
            let started = Instant::now();
            let digest = hasher.compute().0;
            hashes += started.elapsed();
            assert_eq!(FileSig { len, byte_sum: sum, md5_word: i32::from_le_bytes(digest[..4].try_into().unwrap()) }, history.current().sig);
        }
        eprintln!("file-check profile files={} bytes={bytes} guards={:.3}s open={:.3}s read={:.3}s sum={:.3}s md5={:.3}s", sample.files.len(),
            guards.as_secs_f64(), opens.as_secs_f64(), reads.as_secs_f64(), sums.as_secs_f64(), hashes.as_secs_f64());
        let mut reference = None;
        for workers in [4, 6, 8, 12, 4] {
            let started = Instant::now();
            let result = scan_with_workers(&root, &sample, Mode::Check, &Operation::new(), workers).unwrap();
            if let Some(reference) = &reference { assert_eq!(&result, reference); }
            else { reference = Some(result.clone()); }
            eprintln!("file-check concurrency workers={workers} elapsed={:.3}s files={} bytes={bytes}", started.elapsed().as_secs_f64(), sample.files.len());
        }
    }

    #[test]
    fn file_check_worker_count_scales_with_cpu_and_keeps_the_playing_limit() {
        for (available, idle, playing) in [(0, 1, 1), (1, 1, 1), (2, 2, 2), (4, 4, 4),
            (6, 6, 4), (8, 8, 4), (12, 8, 4), (24, 8, 4)] {
            assert_eq!(file_check_workers(available, false), idle);
            assert_eq!(file_check_workers(available, true), playing);
        }
    }

    #[test]
    fn parallel_checks_preserve_manifest_order_classification_and_delta_plans() {
        let root = fixture("parallel-check");
        let mut m = Manifest::default();
        for index in 0..272 {
            let path = format!("{index}.DAT");
            let old = manifest::Version {
                stamp: "30260903_0".into(), sig: FileSig::of(b"old"),
                direct: format!("30260903_0/Direct/{path}.slc"), direct_len: 20, indirect: None,
            };
            let current = manifest::Version {
                stamp: "30260904_1".into(), sig: FileSig::of(b"official"),
                direct: format!("30260904_1/Direct/{path}.slc"), direct_len: 20,
                indirect: Some(manifest::Delta { path: format!("30260904_1/Indirect/{path}.olc"), len: 2 }),
            };
            match index % 4 {
                0 => fs::write(root.join(&path), b"official").unwrap(),
                1 => fs::write(root.join(&path), b"old").unwrap(),
                2 => {},
                _ => fs::write(root.join(&path), b"modified").unwrap(),
            }
            m.files.push(FileHistory { path, optional: false, versions: vec![old, current] });
        }
        // Installed 30260904_1: the modifications are of files this update doesn't change.
        let installed = Some("30260904_1");
        for mode in [Mode::Check, Mode::Update, Mode::Repair] {
            let expected = scan_with_workers_for(Product::Ffxi, &root, &m, mode, &Operation::new(), 1, installed).unwrap();
            assert_eq!((expected.0.current, expected.0.outdated, expected.0.missing, expected.0.modified), (68, 68, 68, 68));
            assert_eq!(expected.0.issues.len(), 200);
            match mode {
                Mode::Check => assert!(expected.1.is_empty()),
                Mode::Update => {
                    assert_eq!(expected.1.len(), 136);
                    assert!(expected.1.iter().all(|p| p.index % 4 != 3 && !p.keep));
                    assert!(expected.1.iter().filter(|p| p.index % 4 == 1).all(|p| p.action == Action::Deltas(1)));
                }
                Mode::Repair => {
                    assert_eq!(expected.1.len(), 204);
                    assert!(expected.1.iter().all(|p| p.keep == (p.index % 4 == 3)));
                }
            }
            for workers in [2, 4, 6, 8, 12] {
                let op = Operation::new();
                assert_eq!(scan_with_workers_for(Product::Ffxi, &root, &m, mode, &op, workers, installed).unwrap(), expected);
                assert_eq!((op.snapshot().done, op.snapshot().total), (272, 272));
            }
        }
        assert_eq!(fs::read(root.join("3.DAT")).unwrap(), b"modified");
        assert!(!root.join("patch.ver").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reusable_checksum_buffer_checks_large_tiny_empty_and_same_size_changes() {
        let root = fixture("checksum-buffer");
        let mut buffer = vec![0u8; SIGNATURE_BUFFER_LEN];
        let large: Vec<u8> = (0..SIGNATURE_BUFFER_LEN * 2 + 73).map(|index| index as u8).collect();
        let mut changed = large.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        let mut results = Vec::new();
        for data in [&large[..], b"tiny", b"", &changed[..]] {
            let path = root.join("file.DAT");
            fs::write(&path, data).unwrap();
            let signature = signature_with_buffer(&path, &Operation::new(), &mut buffer).unwrap();
            assert_eq!(signature, FileSig::of(data));
            assert_eq!(local_file_with_buffer(&path, &Operation::new(), &mut buffer).unwrap().md5, md5::compute(data).0);
            results.push(signature);
        }
        assert_ne!(results[0], results[3]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stock_installer_files_omitted_from_patch_history_update_directly_but_mods_are_preserved() {
        // Independent installed MSI/CAB identity; the server has retained only
        // a later target and offers no recognized base for this DLL.
        let stock = LocalFile { sig: FileSig { len: 2_869_840, byte_sum: 29_617_305, md5_word: -361_925_474 },
            md5: [0x9e, 0x74, 0x6d, 0xea, 0x4c, 0xf3, 0xe3, 0x28, 0x6d, 0xc0, 0x28, 0x95, 0x14, 0x69, 0x2b, 0xb1] };
        let manifest = Manifest { files: vec![FileHistory { path: "FFXiMain.dll".into(), optional: false,
            versions: vec![manifest::Version { stamp: "30260904_1".into(), sig: FileSig::of(b"new official DLL"),
                direct: "30260904_1/Direct/FFXiMain.dll.slc".into(), direct_len: 100,
                indirect: Some(manifest::Delta { path: "30260904_1/Indirect/FFXiMain.dll.olc".into(), len: 1 }) }] }] };
        for mode in [Mode::Check, Mode::Update, Mode::Repair] {
            let (report, planned) = plan_from_signatures(Product::Ffxi, &manifest, mode, &Operation::new(), vec![Some(stock)], &|_| false, true).unwrap();
            assert_eq!((report.outdated, report.modified), (1, 0));
            if mode == Mode::Check { assert!(planned.is_empty()); }
            else { assert_eq!(planned, vec![Planned { index: 0, action: Action::Direct, original: Some(stock.sig), keep: false }]); }
        }
        let mut modified = stock;
        modified.md5[15] ^= 1; // Same length and stock protocol MD5 prefix is insufficient.
        let (report, planned) = plan_from_signatures(Product::Ffxi, &manifest, Mode::Update, &Operation::new(), vec![Some(modified)], &|_| false, true).unwrap();
        assert_eq!((report.outdated, report.modified), (0, 1));
        assert!(planned.is_empty());
        let (report, _) = plan_from_signatures(Product::Viewer, &manifest, Mode::Check, &Operation::new(), vec![Some(stock)], &|_| true, true).unwrap();
        assert_eq!(report.modified, 1, "FFXI's installer identities never classify PlayOnline files");
    }

    /// Stage `planned` as verified downloads of each file's current bytes (`official`).
    fn stage_fixture(root: &Path, m: &Manifest, planned: &[Planned], official: &dyn Fn(&str) -> Vec<u8>) -> Vec<Ready> {
        let staged = root.join(".xi-updater/staged");
        planned.iter().map(|item| {
            let history = &m.files[item.index];
            let cache = staged.join(format!("{}.bin", hex_md5(history.path.as_bytes())));
            fs::write(&cache, official(&history.path)).unwrap();
            (history.path.clone(), cache, history.current().sig, item.original)
        }).collect()
    }

    #[test]
    fn modifications_no_longer_hold_the_version_back_and_replaced_ones_are_kept_then_cleaned_up() {
        let root = fixture("modified-policy");
        let official = |path: &str| format!("official {path}").into_bytes();
        let history = |path: &str, stamp: &str| FileHistory { path: path.into(), optional: false, versions: vec![
            manifest::Version { stamp: "30260904_1".into(), sig: FileSig::of(b"old"),
                direct: format!("30260904_1/Direct/{path}.slc"), direct_len: 4, indirect: None },
            manifest::Version { stamp: stamp.into(), sig: FileSig::of(&official(path)),
                direct: format!("{stamp}/Direct/{path}.slc"), direct_len: 30, indirect: None }] };
        // untouched.DAT last changed before the installed version; texture.DAT changes now.
        let m = Manifest { files: vec![history("untouched.DAT", "30260904_1"), history("ROM/1/texture.DAT", "30261006_0"),
            history("old.DAT", "30261006_0")] };
        let m = Manifest { files: m.files.into_iter().map(|mut h| { if h.path == "untouched.DAT" { h.versions.remove(1); h.versions[0].sig = FileSig::of(&official("untouched.DAT")); } h }).collect() };
        fs::create_dir_all(root.join("ROM/1")).unwrap();
        fs::write(root.join("untouched.DAT"), b"my untouched mod").unwrap();
        fs::write(root.join("ROM/1/texture.DAT"), b"my texture mod").unwrap();
        fs::write(root.join("old.DAT"), b"old").unwrap();
        // Leftovers of earlier runs: a stale download, a rolled-back transaction's backups (no
        // records), and an older build's Repair whose backups hold the only copy of a mod.
        fs::write(root.join(".xi-updater/staged/stale.bin"), b"stale").unwrap();
        fs::create_dir_all(root.join(".xi-updater/backups/1-1")).unwrap();
        fs::write(root.join(".xi-updater/backups/1-1/0.bin"), b"older backup").unwrap();
        legacy_repair(&root, "2-2", &[("ROM/1/texture.DAT", b"my first texture mod"), ("old.DAT", b"old"), ("patch.ver", b"old record")]);
        fs::write(root.join(FULL_CHECK_MARKER), b"").unwrap();
        let op = Operation::new();
        let (mut report, planned) = scan_for(Product::Ffxi, &root, &m, Mode::Update, &op, Some("30260904_1")).unwrap();
        assert_eq!((report.modified, report.kept, report.outdated, planned.len()), (2, 1, 1, 2));
        report.version = "30261006_0".into();
        let ready = stage_fixture(&root, &m, &planned, &official);
        let seed = crate::patch::applied_seed_for(&root, crate::updater::Product::Ffxi).unwrap();
        let prepared = PreparedUpdate { product: Product::Ffxi, root: root.clone(), _lock: None, mode: Mode::Update,
            report, manifest: m.clone(), text: b"list 30261006_0".to_vec(), planned, seed: Some(seed),
            server: SERVER.into(), full_scan: true };
        let report = commit_with_guard(prepared, ready, &op, |_| Ok(())).unwrap();
        assert!(report.verified && report.all_files_checked, "{}", report.summary);
        assert_eq!((report.kept, report.left), (1, 1));
        assert_eq!(crate::patch::installed_for_request(&root), "30261006_0");
        assert!(report.summary.contains("your copies are in") && report.summary.contains("left in place"), "{}", report.summary);
        assert_eq!(fs::read(root.join("untouched.DAT")).unwrap(), b"my untouched mod");
        assert_eq!(fs::read(root.join("ROM/1/texture.DAT")).unwrap(), official("ROM/1/texture.DAT"));
        assert_eq!(fs::read(root.join(".xi-updater/kept/ROM/1/texture.DAT")).unwrap(), b"my texture mod");
        // The older build's backup of an earlier mod moved to kept beside it, not over it;
        // its official original and the version record did not.
        assert_eq!(fs::read(root.join(".xi-updater/kept/ROM/1/texture.DAT.2-2")).unwrap(), b"my first texture mod");
        assert!(!root.join(".xi-updater/kept/old.DAT").exists() && !root.join(".xi-updater/kept/patch.ver").exists());
        assert_eq!(fs::read(root.join("old.DAT")).unwrap(), official("old.DAT"));
        assert!(!root.join(FULL_CHECK_MARKER).exists(), "a verified full scan clears the pending full check");
        // Cleanup: no staged downloads, only this transaction's backups.
        assert_eq!(fs::read_dir(root.join(".xi-updater/staged")).unwrap().count(), 0);
        let backups: Vec<PathBuf> = fs::read_dir(root.join(".xi-updater/backups")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(backups.len(), 1);
        let records: Journal = serde_json::from_slice(&fs::read(backups[0].join("records.json")).unwrap()).unwrap();
        assert!(records.entries.iter().any(|e| e.path == "ROM/1/texture.DAT" && e.kept));
        assert!(records.entries.iter().any(|e| e.path == "patch.ver") && records.entries.iter().any(|e| e.path == "patch.cfg"));

        // The same version again: nothing to write, and unchanged records get no backup.
        let (mut report, planned) = scan_for(Product::Ffxi, &root, &m, Mode::Update, &op, Some("30261006_0")).unwrap();
        assert!(planned.is_empty());
        report.version = "30261006_0".into();
        let seed = crate::patch::applied_seed_for(&root, crate::updater::Product::Ffxi).unwrap();
        let again = PreparedUpdate { product: Product::Ffxi, root: root.clone(), _lock: None, mode: Mode::Update,
            report, manifest: m, text: b"list 30261006_0".to_vec(), planned, seed: Some(seed),
            server: SERVER.into(), full_scan: false };
        let again = commit_with_guard(again, Vec::new(), &op, |_| Ok(())).unwrap();
        assert!(again.verified && !again.all_files_checked, "a version plan vouches for the version only");
        let backups: Vec<PathBuf> = fs::read_dir(root.join(".xi-updater/backups")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(backups.len(), 1, "the earlier transaction's backups are gone");
        let records: Journal = serde_json::from_slice(&fs::read(backups[0].join("records.json")).unwrap()).unwrap();
        assert!(records.entries.is_empty(), "unchanged patch.cfg/patch.ver are not backed up");
        assert_eq!(fs::read(root.join(".xi-updater/kept/ROM/1/texture.DAT")).unwrap(), b"my texture mod");
        assert_eq!(fs::read_dir(root.join(".xi-updater/kept/ROM/1")).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    /// An older build's committed Repair (records without a format): `files` are (path,
    /// backed-up bytes), recorded as `backups/<id>/<n>.bin`.
    fn legacy_repair(root: &Path, id: &str, files: &[(&str, &[u8])]) {
        let dir = root.join(format!(".xi-updater/backups/{id}"));
        fs::create_dir_all(&dir).unwrap();
        let entries: Vec<serde_json::Value> = files.iter().enumerate().map(|(n, (path, bytes))| {
            fs::write(dir.join(format!("{n}.bin")), bytes).unwrap();
            let sig = FileSig::of(bytes);
            serde_json::json!({ "path": path, "backup": format!(".xi-updater/backups/{id}/{n}.bin"),
                "expected": { "len": sig.len, "sum": sig.byte_sum, "word": sig.md5_word } })
        }).collect();
        fs::write(dir.join("records.json"), serde_json::to_vec(&serde_json::json!({ "id": id, "committed": true, "entries": entries })).unwrap()).unwrap();
    }

    #[test]
    fn cleanup_keeps_what_older_builds_backed_up_of_user_files_and_never_replaces_a_kept_copy() {
        let root = fixture("legacy-cleanup");
        let official_bytes = b"official bytes";
        let official = |_: &str, file: &LocalFile| file.sig == FileSig::of(official_bytes);
        fs::create_dir_all(root.join(".xi-updater/kept/a")).unwrap();
        fs::write(root.join(".xi-updater/kept/a/mod.DAT"), b"kept earlier").unwrap();
        // An older Repair replaced a mod (kept beside the earlier copy), an official file
        // and a mod whose identical copy is already kept (nothing new).
        legacy_repair(&root, "10-1", &[("a/mod.DAT", b"replaced by an older repair"), ("b.DAT", official_bytes), ("c.DAT", b"kept earlier")]);
        fs::create_dir_all(root.join(".xi-updater/kept")).unwrap();
        fs::write(root.join(".xi-updater/kept/c.DAT"), b"kept earlier").unwrap();
        // Unreadable records stay for the next commit; this build's records go as they are.
        fs::create_dir_all(root.join(".xi-updater/backups/11-1")).unwrap();
        fs::write(root.join(".xi-updater/backups/11-1/records.json"), b"damaged").unwrap();
        fs::write(root.join(".xi-updater/backups/11-1/0.bin"), b"unknown").unwrap();
        legacy_repair(&root, "12-1", &[("d.DAT", b"not kept: this build's policy")]);
        let records = root.join(".xi-updater/backups/12-1/records.json");
        let mut current: serde_json::Value = serde_json::from_slice(&fs::read(&records).unwrap()).unwrap();
        current["format"] = RECORDS_FORMAT.into();
        fs::write(&records, serde_json::to_vec(&current).unwrap()).unwrap();
        fs::create_dir_all(root.join(".xi-updater/backups/13-1")).unwrap();
        clean_up_after_commit(&root, "13-1", &official);
        let mut left: Vec<String> = fs::read_dir(root.join(".xi-updater/backups")).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["11-1", "13-1"]);
        let kept = |path: &str| fs::read(root.join(".xi-updater/kept").join(path)).ok();
        assert_eq!(kept("a/mod.DAT").as_deref(), Some(&b"kept earlier"[..]));
        assert_eq!(kept("a/mod.DAT.10-1").as_deref(), Some(&b"replaced by an older repair"[..]));
        assert_eq!((kept("b.DAT"), kept("c.DAT.10-1"), kept("d.DAT")), (None, None, None));

        // keep_copy itself: a different file never replaces a kept copy; the same bytes add nothing.
        fs::create_dir_all(root.join(".xi-updater/backups/14-1")).unwrap();
        for (n, bytes) in [b"version 2".as_slice(), b"version 3", b"kept earlier", b"version 2"].into_iter().enumerate() {
            fs::write(root.join(format!(".xi-updater/backups/14-1/{n}.bin")), bytes).unwrap();
            keep_copy(&root, "a/mod.DAT", &format!(".xi-updater/backups/14-1/{n}.bin"), "14-1").unwrap();
        }
        assert_eq!(kept("a/mod.DAT").as_deref(), Some(&b"kept earlier"[..]));
        assert_eq!(kept("a/mod.DAT.14-1").as_deref(), Some(&b"version 2"[..]));
        assert_eq!(kept("a/mod.DAT.14-1-2").as_deref(), Some(&b"version 3"[..]));
        assert_eq!(fs::read_dir(root.join(".xi-updater/kept/a")).unwrap().count(), 4);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_rolled_back_update_never_loses_an_earlier_kept_copy() {
        let root = fixture("kept-rollback");
        fs::create_dir_all(root.join("ROM/1")).unwrap();
        fs::create_dir_all(root.join(".xi-updater/kept/ROM/1")).unwrap();
        fs::write(root.join(".xi-updater/kept/ROM/1/texture.DAT"), b"mod v1").unwrap();
        // The file is now damaged (a failed mod tool): Repair replaces it and keeps a copy.
        fs::write(root.join("ROM/1/texture.DAT"), b"half-written").unwrap();
        let cache = root.join(".xi-updater/staged/new.bin");
        fs::write(&cache, b"official").unwrap();
        let ready = vec![("ROM/1/texture.DAT".to_owned(), cache, FileSig::of(b"official"), Some(FileSig::of(b"half-written")))];
        let keep: HashSet<String> = ["ROM/1/texture.DAT".to_owned()].into();
        let calls = std::cell::Cell::new(0);
        // The second liveness check (before patch.cfg is written) finds a game: roll back.
        let guard = |_: &Path| { calls.set(calls.get() + 1); if calls.get() == 2 { Err("fixture game started".to_owned()) } else { Ok(()) } };
        let error = apply_keeping(&root, &ready, &keep, &|_, _| false, &Operation::new(),
            || Ok(Some((b"list".to_vec(), b"record".to_vec()))), guard).unwrap_err();
        assert!(error.contains("fixture game started") && error.contains("Transaction recovered"), "{error}");
        assert_eq!(fs::read(root.join("ROM/1/texture.DAT")).unwrap(), b"half-written");
        assert!(!root.join("patch.ver").exists());
        assert_eq!(fs::read(root.join(".xi-updater/kept/ROM/1/texture.DAT")).unwrap(), b"mod v1", "the earlier copy survives");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fresh_playonline_install_does_not_call_its_installer_files_your_copies() {
        let root = fixture("fresh-viewer");
        fs::create_dir_all(root.join("viewer/com")).unwrap();
        fs::write(root.join("viewer/com/app.dll"), b"installer build").unwrap();
        let m = Manifest { files: vec![FileHistory { path: "viewer/com/app.dll".into(), optional: false, versions: vec![manifest::Version {
            stamp: "20261001_0".into(), sig: FileSig::of(b"official build"), direct: "20261001_0/Direct/app.dll.slc".into(),
            direct_len: 20, indirect: None }] }] };
        let op = Operation::new();
        let (report, plan) = scan_with_workers_for(Product::Viewer, &root, &m, Mode::Repair, &op, 1, None).unwrap();
        assert_eq!((report.modified, report.kept, plan.len(), plan[0].keep), (1, 0, 1, false));
        // An installed Viewer's unrecognized file is a modification: its copy is kept.
        let (report, plan) = scan_with_workers_for(Product::Viewer, &root, &m, Mode::Repair, &op, 1, Some("20260901_0")).unwrap();
        assert_eq!((report.modified, report.kept, plan[0].keep), (1, 1, true));
        fs::remove_dir_all(root).unwrap();
    }

    /// A minimal PE file standing in for Square Enix's signed pol.exe.
    fn pe_file(characteristics: u16, tail: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x100];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&0x14cu16.to_le_bytes());
        bytes[0x96..0x98].copy_from_slice(&characteristics.to_le_bytes());
        bytes.extend_from_slice(tail);
        bytes
    }

    #[test]
    fn a_pol_exe_differing_only_by_the_local_4gb_flag_is_current_and_never_repaired() {
        let root = fixture("pol-laa");
        let official = pe_file(0x0102, b"signed official body");
        let m = Manifest { files: vec![FileHistory { path: "pol.exe".into(), optional: false, versions: vec![manifest::Version {
            stamp: "20261022_0".into(), sig: FileSig::of(&official), direct: "20261022_0/Direct/pol.exe.slc".into(),
            direct_len: official.len() as u64, indirect: None }] }] };
        let op = Operation::new();
        let scan = |mode| scan_with_workers_for(Product::Viewer, &root, &m, mode, &op, 1, Some("20261022_0")).unwrap();
        // Official bytes, and the same bytes with only IMAGE_FILE_LARGE_ADDRESS_AWARE set: current.
        for bytes in [official.clone(), pe_file(0x0122, b"signed official body")] {
            fs::write(root.join("pol.exe"), &bytes).unwrap();
            for mode in [Mode::Check, Mode::Repair, Mode::Update] {
                let (report, plan) = scan(mode);
                assert_eq!((report.current, report.modified, plan.len()), (1, 0, 0), "{mode:?}");
            }
        }
        // Any other difference, with or without the flag, is a modification Repair replaces.
        for bytes in [pe_file(0x0122, b"signed officiaL body"), pe_file(0x0103, b"signed official body"), pe_file(0x0122, b"signed official body!")] {
            fs::write(root.join("pol.exe"), &bytes).unwrap();
            let (report, plan) = scan(Mode::Repair);
            assert_eq!((report.current, report.modified, plan.len()), (0, 1, 1));
        }
        // Only pol.exe gets this: the same flag edit on another file is a modification.
        let other = Manifest { files: vec![FileHistory { path: "polhook.dll".into(), ..m.files[0].clone() }] };
        fs::write(root.join("polhook.dll"), pe_file(0x0122, b"signed official body")).unwrap();
        let (report, _) = scan_with_workers_for(Product::Viewer, &root, &other, Mode::Check, &op, 1, Some("20261022_0")).unwrap();
        assert_eq!(report.modified, 1);
        assert_eq!(laa_flag_byte(&pe_file(0x0122, b"")), Some(0x96));
        assert_eq!(laa_flag_byte(&pe_file(0x0102, b"")), None);
        assert_eq!(laa_flag_byte(b"MZ"), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_commit_leaves_a_full_check_only_when_it_touched_the_install() {
        let root = fixture("failed-commit");
        let file = |path: &str| FileHistory { path: path.into(), optional: false, versions: vec![manifest::Version {
            stamp: "30261006_0".into(), sig: FileSig::of(format!("official {path}").as_bytes()),
            direct: format!("30261006_0/Direct/{path}.slc"), direct_len: 9, indirect: None }] };
        let m = Manifest { files: vec![file("a.DAT"), file("b.DAT")] };
        let planned: Vec<Planned> = (0..2).map(|index| Planned { index, action: Action::Direct, original: None, keep: false }).collect();
        let prepared = |planned: Vec<Planned>| {
            let report = Report { version: "30261006_0".into(), total: 2, ..Default::default() };
            PreparedUpdate { product: Product::Ffxi, root: root.clone(), _lock: None, mode: Mode::Update,
                report, manifest: m.clone(), text: Vec::new(), planned, seed: Some(1), server: SERVER.into(), full_scan: false }
        };
        // The staged bytes are not the official ones: refused before any file is replaced, so
        // the next update keeps its version plan.
        let ready = stage_fixture(&root, &m, &planned, &|_| b"damaged".to_vec());
        assert!(commit_with_guard(prepared(planned.clone()), ready, &Operation::new(), |_| Ok(())).is_err());
        assert!(!full_check_due(&root), "nothing in the install changed");
        // A failure after the first replacement rolls back and leaves the full check.
        let ready = stage_fixture(&root, &m, &planned, &|path| format!("official {path}").into_bytes());
        let calls = std::cell::Cell::new(0);
        // Calls: the commit's own guard, file a, then file b (fails); recovery passes.
        let guard = |_: &Path| { calls.set(calls.get() + 1);
            if calls.get() == 3 { Err("fixture game started".to_owned()) } else { Ok(()) } };
        let error = commit_with_guard(prepared(planned), ready, &Operation::new(), guard).unwrap_err();
        assert!(error.contains("Transaction recovered"), "{error}");
        assert!(full_check_due(&root));
        assert!(!root.join("a.DAT").exists() && !root.join("patch.ver").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checksum_cancellation_is_observed_between_reads() {
        struct CancellingReader { op: Operation, reads: usize }
        impl Read for CancellingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                assert_eq!(self.reads, 1, "a cancelled checksum must not start another read");
                buffer.fill(42);
                self.op.cancel();
                Ok(buffer.len())
            }
        }
        let op = Operation::new();
        let error = read_signature(CancellingReader { op: op.clone(), reads: 0 }, &op, &mut [0; 16]).unwrap_err();
        assert!(op.cancelled());
        assert!(error.contains("Cancelled"));
    }

    #[test]
    fn parallel_check_error_stops_workers_without_masking_error_or_reporting_success() {
        let root = fixture("parallel-error");
        fs::create_dir(root.join("bad.DAT")).unwrap();
        let mut m = Manifest::default();
        for index in 0..64 {
            m.files.push(FileHistory { optional: false, path: if index == 5 { "bad.DAT".into() } else { format!("missing{index}.DAT") },
                versions: vec![manifest::Version { stamp: "30260904_1".into(), sig: FileSig::of(b"official"),
                    direct: "30260904_1/Direct/f.DAT.slc".into(), direct_len: 20, indirect: None }] });
        }
        for workers in [4, 12] {
            let op = Operation::new();
            let error = scan_with_workers(&root, &m, Mode::Check, &op, workers).unwrap_err();
            assert!(error.contains("bad.DAT") && error.contains("not a regular file"), "{error}");
            assert!(!op.cancelled(), "an IO failure must not become a user cancellation");
            assert!(op.snapshot().done < op.snapshot().total);
            op.cancel();
            assert!(scan_with_workers(&root, &m, Mode::Check, &op, workers).unwrap_err().contains("Cancelled"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_repair_replaces_corrupt_applied_record_only_after_file_verification() {
        let root = fixture("corrupt-applied");
        let op = Operation::new();
        let corrupt = b"damaged applied record";
        fs::write(root.join("patch.ver"), corrupt).unwrap();
        fs::write(root.join("official.dat"), b"official bytes").unwrap();
        assert!(crate::patch::applied_seed(&root).is_err());
        let seed = crate::patch::repair_seed(&root).unwrap();
        let verified = FileSig::of(b"official bytes");
        let encoded = crate::patch::encode_applied_record("30260904_1", seed).unwrap();
        assert!(fixture_apply(&root, &[], &op, || {
            assert_eq!(
                signature(&root.join("official.dat"), &op).unwrap(),
                verified
            );
            assert_eq!(fs::read(root.join("patch.ver")).unwrap(), corrupt);
            Ok(Some((b"end\n".to_vec(), encoded.clone())))
        })
        .unwrap());
        assert_eq!(crate::patch::applied_seed(&root).unwrap(), seed);
        assert_eq!(fs::read(root.join("patch.ver")).unwrap(), encoded);
        let backup_dir = fs::read_dir(root.join(".xi-updater/backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let record: Journal =
            serde_json::from_slice(&fs::read(backup_dir.join("records.json")).unwrap()).unwrap();
        let backup = record
            .entries
            .iter()
            .find(|e| e.path == "patch.ver")
            .unwrap()
            .backup
            .as_ref()
            .unwrap();
        assert_eq!(fs::read(root.join(backup)).unwrap(), corrupt);
        fs::remove_dir_all(root).unwrap();
    }
}
