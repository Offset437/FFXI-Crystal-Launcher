#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("Crystal Launcher is Windows-only");

mod backend;
mod account_import;
// The launcher <-> loader wire contract, shared with crystal-loader source.
#[path = "../crystal-loader/src/b64.rs"]
#[allow(dead_code)]
mod b64;
#[path = "../crystal-loader/src/wire.rs"]
#[allow(dead_code)]
mod wire;
mod account_import_ui;
mod account_order;
mod account_row;
mod discovery;
mod dns;
mod effect;
mod controller_nav;
mod dgvoodoo;
mod feature_check;
#[allow(dead_code)] // the Slint screens bind to most of this API
mod features;
mod reshade;
mod friends;
mod social;
mod game_config;
mod ffxiah;
mod footprint;
mod fsutil;
mod gamepad;
mod i18n;
mod installer;
mod launch_log;
mod launch_timing;
mod launch_tools;
mod open_with_game;
mod launcher_close;
mod prereqs;
mod settings_page;
mod official_versions;
mod operation;
mod palette;
mod patch;
mod patch_watch;
mod play_flag;
mod play_update;
mod pol_less;
mod portraits;
mod preview;
mod privacy;
mod services;
mod self_update;
mod session_worker;
mod startup_prefetch;
mod startup_task;
mod stock_socket;
mod tick_gate;
mod totp;
mod tray;
mod runtime;
mod slint_host;
mod updater;
mod viewer;
mod win;
mod window_chrome;

use backend::{account_key, Account, AccountMode, Config, LaunchResult};
use i18n::{t, tf};

use effect::{Edge, Effect, Screenshot, Size, WindowId, WindowOp};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Clone, Copy)]
pub(crate) enum Field {
    Label,
    Server,
    PolId,
    PolPassword,
    SeId,
    SePassword,
    PolHandleNames,
    TotpKey,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Home,
    Friends,
    Maintenance,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceKind {
    Check,
    Update,
    Repair,
    Download,
    Extract,
    Setup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MainAction {
    Play,
    Maintenance,
    Offline,
    Working,
}
impl MaintenanceKind {
    pub fn blocks_launch(self) -> bool {
        matches!(self, Self::Update | Self::Repair | Self::Setup)
    }
    pub fn cancellable(self) -> bool {
        !matches!(self, Self::Extract | Self::Setup)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Check => t("Checking files"),
            Self::Update => t("Updating PlayOnline and FFXI"),
            Self::Repair => t("Repairing files"),
            Self::Download => t("Downloading official installer"),
            Self::Extract => t("Extracting official installer"),
            Self::Setup => t("Official setup is running"),
        }
    }
}

/// What the Downloads page's "Last result" line reports, kept apart from its
/// (translated) text so the page never has to match words in a language.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum ResultTone {
    #[default]
    Done,
    /// Nothing has been checked yet.
    Unverified,
    /// The operation failed or was cancelled.
    Stopped,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub install_path: String,
    #[serde(default)]
    pub light_theme: bool,
    #[serde(default)]
    pub install_selected: bool,
    #[serde(default = "default_installer_path")]
    pub installer_path: String,
    #[serde(default)]
    pub installer_selected: bool,
    #[serde(default)]
    selected_accounts: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_order: Vec<String>,
    /// Public setup names; account credentials stay in the unchanged DPAPI vault.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub account_pol_handles: HashMap<String, String>,
    // The default launcher (Settings → System → Launch tools), for accounts whose own
    // launch_with is None. Home's Launch with edits the shown account's own choice.
    #[serde(default)]
    pub launch_with: crate::backend::LaunchWith,
    // Settings → System → Launch tools. Empty means detect / default.
    #[serde(default)]
    pub ashita_path: String,
    #[serde(default)]
    pub windower_path: String,
    #[serde(default)]
    pub ashita_boot_ini: String,
    #[serde(default)]
    pub windower_profile: String,
    #[serde(default)]
    pub loader_path: String,
    /// Non-secret per-account installation overrides; the encrypted vault stays compatible.
    #[serde(default)]
    pub account_installs: HashMap<String, String>,
    #[serde(default)]
    pub account_overlays: HashMap<String, Vec<String>>,
    /// Per-account PlayOnline Auto-Away minutes (0 = off); absent = 10.
    #[serde(default)]
    pub account_auto_away: HashMap<String, u8>,
    /// Files before 2026-10-13: the per-account "Sign in when Crystal opens" choice. Read only to
    /// migrate it (`migrate_startup_signin`) and never written again.
    #[serde(default, rename = "account_startup_signin", skip_serializing)]
    legacy_startup_signin: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub account_auto_login: HashMap<String, pol_less::AutoLogin>,
    /// Accounts whose last successful sign-in took a one-time code (learned, never the code):
    /// a "Wait and play" released after a restart asks for a code instead of signing in
    /// without one, and Crystal never signs them in by itself.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub account_otp: BTreeSet<String>,
    /// Accounts whose last successful sign-in took no one-time code (learned the same way; never
    /// in `account_otp` too). Only these are signed in by Crystal itself (`presigns`). An account
    /// in neither set has not been seen signing in yet.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub account_no_otp: BTreeSet<String>,
    /// Sign known accounts in when the launcher opens.
    #[serde(default = "default_true")]
    pub auto_sign_in: bool,
    /// Retain native POL after a clean game exit; older hosts can sign back in.
    /// Older settings leave this off so closing a game also closes its host.
    #[serde(default)]
    pub sign_in_after_game: bool,
    /// The launcher's own UI language (Settings page). Absent = follow the
    /// Windows display language. The game's language is never changed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<i18n::Lang>,
    /// Screenshot privacy: names, IDs, mail and the user folder are shown
    /// blurred (`privacy.rs`). Display only. Absent = off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privacy: bool,
    /// Settings → Graphics. On by default (older files without it too): Crystal
    /// installs dgVoodoo and puts it next to the loader at each launch.
    #[serde(default = "default_true")]
    pub dgvoodoo: bool,
    #[serde(default)]
    pub reshade: reshade::Preferences,
    #[serde(default = "default_true")]
    pub launcher_auto_updates: bool,
    /// Settings → System: a downloaded launcher update installs by itself when no game runs
    /// and nobody is using the window (`self_update::auto`). On by default.
    #[serde(default = "default_true")]
    pub launcher_auto_restart: bool,
    #[serde(default = "default_true")]
    pub controller_navigation: bool,
    /// Settings → System: the window's close button keeps Crystal running in the
    /// notification area (sessions stay signed in). Off: closing quits, as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close_to_tray: bool,
    /// Settings → System: a logon scheduled task starts Crystal with `--background`
    /// (`startup_task`). Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_with_windows: bool,
    /// Settings → System → Open with the game: programs started once per Play batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_with_game: Vec<open_with_game::Program>,
}
fn default_true() -> bool {
    true
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            install_path: String::new(),
            install_selected: false,
            light_theme: false,
            installer_path: default_installer_path(),
            installer_selected: false,
            selected_accounts: None,
            account_order: Vec::new(),
            account_pol_handles: HashMap::new(),
            launch_with: crate::backend::LaunchWith::default(),
            ashita_path: String::new(),
            windower_path: String::new(),
            ashita_boot_ini: String::new(),
            windower_profile: String::new(),
            loader_path: String::new(),
            account_installs: HashMap::new(),
            account_overlays: HashMap::new(),
            account_auto_away: HashMap::new(),
            legacy_startup_signin: BTreeSet::new(),
            account_auto_login: HashMap::new(),
            account_otp: BTreeSet::new(),
            account_no_otp: BTreeSet::new(),
            auto_sign_in: true,
            sign_in_after_game: false,
            language: None,
            privacy: false,
            dgvoodoo: true,
            reshade: reshade::Preferences::default(),
            launcher_auto_updates: true,
            launcher_auto_restart: true,
            controller_navigation: true,
            close_to_tray: false,
            start_with_windows: false,
            open_with_game: Vec::new(),
        }
    }
}

pub(crate) struct AccountRow {
    pub key: String,
    pub account: Account,
    pub selected: bool,
    pub otp: String,
    pub busy: bool,
    signing_out: bool,
    pub shutting_down: bool,
    /// Background sign-in can accept Play without starting another login.
    preparing_session: bool,
    /// The current sign-in thread's completion flag, set just before its Complete is sent:
    /// launcher close stops waiting for a sign-in that ended before any worker was seen.
    sign_in_done: Option<Arc<AtomicBool>>,
    queued_play: Option<Instant>,
    /// A coordinated Play owns this row even if an older startup sign-in finishes.
    batch_pending: bool,
    /// This window is awaiting its batch callback; ownership may not be published yet.
    batch_requested: bool,
    launch_generation: u64,
    pub status: String,
    /// The note shown after `status` while `status` is still `note_on` (`status_text`).
    note: Option<RowNote>,
    note_on: String,
    pub pid: Option<u32>,
    /// `pid`'s creation time, read when the session's result arrived (`HostId`).
    host_created: Option<u64>,
    alive: Option<Arc<AtomicBool>>,
    game_running: Option<Arc<AtomicBool>>,
    /// Launch diagnostics: the client's stderr log, how it ended, and whether that was a problem.
    pub log: Option<PathBuf>,
    outcome: Option<Arc<std::sync::Mutex<Option<launch_log::ExitReport>>>>,
    pub problem: bool,
    /// How long the running launch took, e.g. "Signed in 2.8s · game started 9.4s".
    pub timing: String,
    /// A Play from an already authenticated session, measured separately from sign-in.
    play_timing: Option<launch_timing::Timeline>,
    /// How the last client ended, while `status` still holds its (English) summary.
    exit: Option<launch_log::ExitReport>,
    /// What started the sign-in in flight (pol-spec 07d §4.2), for its failure handling.
    launch_trigger: Option<launch_timing::LaunchTrigger>,
    /// A sign-in that failed on the network starts again once DNS answers (07d §5.4-5.5).
    network_wait: Option<NetworkWait>,
    /// The sign-in in flight (or last) took the row's one-time code, which is now spent: no
    /// automatic second sign-in may follow it, since it would go without a code and be refused
    /// (07d §5.4) until the user types a new one.
    otp_spent: bool,
    /// The row's session came from an earlier window's worker whose record does not say whether
    /// its sign-in took a code (workers before the field existed write none), so `otp_spent` is
    /// a guess: nothing that session does teaches that the account signs in without one.
    otp_unknown: bool,
    /// The sign-in in flight (or last) is one Crystal started by itself without a code
    /// (`LaunchTrigger::is_presign`), also when a Play has since queued on it: only such a
    /// sign-in's refusal marks the account as one that may need a code (owner decision
    /// 2026-10-13). A user's own Play or Connect never changes what Crystal knows that way.
    presign_login: bool,
    /// PlayOnline refused a code made from the account's authenticator key (no code typed): the
    /// key is likely wrong. Until a sign-in without a typed code works or the key is edited, the
    /// row asks for a typed code and Crystal signs nothing in by itself, so a wrong key never
    /// costs a second refused login.
    authenticator_refused: bool,
    /// The running session's game was seen running: its end is a game end.
    game_seen: bool,
    /// The user signed out or edited the account during the running session: its end signs
    /// nothing back in.
    resign_declined: bool,
    /// An explicit sign-out, or a game closed without automatic sign-in: keep
    /// this account free of both signed-in and parked hosts until the next Play.
    signed_out: bool,
    /// A sign-in after the game closed, waiting for its session to end fully.
    resign: Option<Resign>,
}

/// A session whose game ran has ended: its row's key, how the game ended, and its host.
type GameEnd = (String, Option<launch_log::ExitKind>, Option<HostId>);

/// An optional fresh sign-in after the game and its previous POL host close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Resign {
    /// The row's launch generation at the game's end: a launch since (Play, Connect) wins.
    generation: u64,
    /// When the game's end was seen.
    ended: Instant,
    /// Since when the row has been free (host exited, sign-out done, worker record closed).
    free_since: Option<Instant>,
    /// An unclean end: the connectivity probe must see the network first.
    network: bool,
    /// The ended session's host (loader). Its worker's record closing is not enough: a
    /// pipe failure ends the session while a hung loader may still hold its polcore session.
    host: Option<HostId>,
}

/// Cooldown after both the previous worker and host have ended. Keep the
/// three-second minimum for automatic re-sign-in; explicit Play starts through
/// its ordinary launch path. Native authentication has its own pacing.
const RESIGN_DELAY: Duration = Duration::from_secs(3);

/// What a row does when its connectivity probe succeeds (pol-spec 07d §5.4-5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NetworkWait {
    /// Re-issue the Play queued behind a failed pre-sign-in, keeping its click time.
    Reissue(Instant),
    /// The one retry of a startup sign-in.
    StartupRetry,
    /// A game ended uncleanly: its sign-in after the game waits for the network first.
    AfterGame,
    /// The one retry of a sign-in after a game.
    AfterGameRetry,
}

/// A Play clicked before this window's first status read finished (owner, 2026-10-13: Play
/// at 05:07:40 UTC, 19 s into Crystal's start, went through during All Worlds Maintenance).
struct HeldPlay {
    keys: Vec<String>,
    clicked: Instant,
    trigger: Option<launch_timing::LaunchTrigger>,
    since: Instant,
    /// What each held row and the status line said before the hold, restored at release.
    rows: Vec<(String, String)>,
    status: String,
}

/// The longest a Play waits for the first status read. A read that fails or takes longer
/// establishes nothing, and the Play proceeds as without one.
const STATUS_HOLD_LIMIT: Duration = Duration::from_secs(10);

/// A Play that found FFXI down (owner decision 2026-10-13, "Wait and play"): instead of being
/// refused, it starts as soon as FFXI is back (`release_waiting_plays`). Accounts clicked together
/// wait, and start, together. Waits live in this window only and are never saved: a Crystal
/// restart drops them, and the user presses Play again.
struct WaitingPlay {
    keys: Vec<String>,
    /// Accounts whose one-time code was typed for this Play: they sign in with a code.
    otp: Vec<String>,
    /// When the wait began.
    armed: Instant,
    /// The codes typed for this Play, taken off the rows. A release within OTP_KEEP of `armed`
    /// (the immediate flag read, when the official status lags behind a flag already at 1)
    /// still uses them; after that they are stale and dropped (`expire_wait_codes`), and the
    /// release asks the user for a new one.
    codes: Vec<(String, Zeroizing<String>)>,
}

/// How long a one-time code typed for a Play that then waits stays usable for that Play.
const OTP_KEEP: Duration = Duration::from_secs(20);

/// How long a startup sign-in waits for a still-running session of its account to end
/// before it is skipped (07d §5.5: the previous game's host may take seconds to close).
const STARTUP_WAIT_LIMIT: Duration = Duration::from_secs(60);
impl Drop for AccountRow {
    fn drop(&mut self) {
        self.otp.zeroize();
    }
}
/// Whether `row`'s next sign-in waits for a code typed on it: the account is known to take
/// one-time codes, or its last typed code was spent, and no authenticator key fills them in;
/// or PlayOnline refused its key's code (another would be refused too).
fn wants_typed_code(settings: &Settings, row: &AccountRow) -> bool {
    row.authenticator_refused
        || !row.authenticator_ready() && (row.otp_spent || settings.account_otp.contains(&row.key))
}

impl AccountRow {
    /// The account's one-time codes come from its saved authenticator key, which PlayOnline has
    /// not refused: none of its sign-ins waits for a typed code.
    pub(crate) fn authenticator_ready(&self) -> bool {
        self.account.mode == AccountMode::Retail && !self.account.totp_key.is_empty() && !self.authenticator_refused
    }
    pub(crate) fn is_playing(&self) -> bool {
        self.pid.is_some() && self.game_running.as_ref().is_none_or(|g| g.load(Ordering::Acquire))
    }
    pub(crate) fn can_play(&self) -> bool {
        !self.shutting_down && !self.signing_out && !self.batch_pending && !self.is_playing() && self.queued_play.is_none() && (!self.busy || self.preparing_session)
    }
    fn queued_signin_play(&self) -> bool {
        self.busy && self.preparing_session && self.queued_play.is_some() && !self.batch_pending && !self.is_playing()
    }
    pub(crate) fn game_start_pending(&self) -> bool {
        !self.signing_out && (self.batch_pending || self.queued_play.is_some() || self.busy && !self.preparing_session)
    }
    pub(crate) fn can_sign_out(&self) -> bool {
        self.pid.is_some() && !self.shutting_down && !self.signing_out && !self.busy && !self.is_playing()
            && !self.batch_pending && self.queued_play.is_none()
    }
    /// The status line to show. A client's exit report and the row's note are worded in the
    /// current language here; any later status replaces both.
    pub(crate) fn status_text(&self) -> std::borrow::Cow<'_, str> {
        if self.shutting_down { return t("Shutting down FFXI…").into(); }
        if self.signing_out { return t("Signing out of PlayOnline…").into(); }
        let status: std::borrow::Cow<'_, str> = match &self.exit {
            Some(report) if report.summary == self.status => report.message().into(),
            _ => self.status.as_str().into(),
        };
        match self.shown_note() {
            Some(note) => format!("{status} · {}", note.text()).into(),
            None => status,
        }
    }
    /// The note after the status, while the status it was added to still shows.
    fn shown_note(&self) -> Option<&RowNote> {
        self.note.as_ref().filter(|_| self.note_on == self.status)
    }
    /// Put `note` after the current status (`None`: none).
    fn set_note(&mut self, note: Option<RowNote>) {
        self.note_on.clone_from(&self.status);
        self.note = note;
    }
    /// Show how the client ended.
    fn set_exit(&mut self, report: launch_log::ExitReport) {
        self.problem = report.is_problem();
        self.status = report.summary.clone();
        self.exit = Some(report);
    }
    /// The user acted on the account (sign-out, edit): no sign-in after its game, pending or
    /// still to come from the running session.
    fn decline_resign(&mut self) {
        self.resign_declined = self.pid.is_some() || self.busy;
        self.drop_resign();
    }
    /// Forget a pending sign-in after a game, or its one network retry, and its promise on the row.
    fn drop_resign(&mut self) {
        self.resign = None;
        if self.network_wait == Some(NetworkWait::AfterGame) { self.network_wait = None; }
        if self.network_wait == Some(NetworkWait::AfterGameRetry) {
            self.network_wait = None;
            if self.shown_note() == Some(&RowNote::NetworkRetry) { self.note = None; }
        }
        if self.shown_note() == Some(&RowNote::ResignAfterGame) { self.note = None; }
    }
}

/// A short line after a row's status, kept apart from it so it follows the language and is
/// removed by what it is, not by matching words.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowNote {
    /// A Play waits for the sign-in in flight.
    QueuedPlay,
    /// The row signs back in after its game closed.
    ResignAfterGame,
    /// A sign-in that failed on the network starts again once DNS answers.
    NetworkRetry,
    /// That retry gave up.
    NetworkGone,
    /// The one-time code was spent by a sign-in that did not finish.
    OtpSpent,
    /// A sign-in Crystal started by itself without a code was refused.
    MayNeedOtp,
    /// No sign-in after the game during a maintenance or outage (with the service's note).
    ResignRefused(String),
}

impl RowNote {
    fn text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::QueuedPlay => t("FFXI will start when sign-in finishes.").into(),
            Self::ResignAfterGame => t("Signing back in after the game closed.").into(),
            Self::NetworkRetry => t("Crystal will try again when the network is back.").into(),
            Self::NetworkGone => t("The network did not come back. Sign in again once it is.").into(),
            Self::OtpSpent => t("That one-time code was used. Enter a new one, then press Play.").into(),
            Self::MayNeedOtp => t("This account may need a one-time password now: Crystal signs it in only when you press Play.").into(),
            Self::ResignRefused(note) => note.as_str().into(),
        }
    }
}

/// A session's host (loader) process: its PID and, when it could be read while the host ran,
/// its creation time, so a PID Windows has since reused is never taken for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostId {
    pid: u32,
    created: Option<u64>,
}

/// `pid`'s creation time, read while it is known to run (the session's start).
#[cfg(not(test))]
fn process_created(pid: u32) -> Option<u64> {
    win::process::open_with_created(pid, win::process::PROCESS_QUERY_LIMITED_INFORMATION).map(|(_, created)| created)
}
#[cfg(test)]
fn process_created(_pid: u32) -> Option<u64> { None }

/// The loader a signed-in session runs: its host process's own file (a ReShade session runs a
/// copy in its own host folder, where the game reads dgVoodoo's files), else the configured one.
fn session_loader(host: Option<HostId>, loader_setting: &str) -> Option<PathBuf> {
    let configured = pol_less::tools::loader_path(loader_setting);
    let running = host.and_then(process_image).filter(|image| {
        let name = |path: &Path| path.file_name().map(|name| name.to_ascii_lowercase());
        configured.as_deref().is_some_and(|configured| name(image) == name(configured))
    });
    running.or(configured)
}

/// The host's image file, unless its PID now names a later process (creation time differs).
#[cfg(not(test))]
fn process_image(host: HostId) -> Option<PathBuf> {
    let process = win::process::open(host.pid, win::process::PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
    let same = host.created.is_none_or(|created| win::process::created(&process) == Some(created));
    if !same { return None; }
    win::process::image_path_of(&process).ok()
}
#[cfg(test)]
fn process_image(_host: HostId) -> Option<PathBuf> { None }

/// Whether the ended session's host process still runs. One this process may not open (an
/// elevated host) counts as running, unless it was opened before (then the PID is another
/// process's now): a sign-in after the game then waits for its limit.
#[cfg(not(test))]
fn host_running(host: HostId) -> bool {
    use std::os::windows::io::AsRawHandle;
    use win::process::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    use windows_sys::Win32::{Foundation::{ERROR_ACCESS_DENIED, WAIT_TIMEOUT}, System::Threading::WaitForSingleObject};
    let process = match win::process::open(host.pid, PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(process) => process,
        Err(error) => return host.created.is_none() && error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32),
    };
    let waiting = unsafe { WaitForSingleObject(process.as_raw_handle() as _, 0) };
    waiting == WAIT_TIMEOUT && host.created.is_none_or(|created| win::process::created(&process) == Some(created))
}
#[cfg(test)]
thread_local! { static RUNNING_HOSTS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) }; }
#[cfg(test)]
fn host_running(host: HostId) -> bool { RUNNING_HOSTS.with(|hosts| hosts.borrow().contains(&host.pid)) }
pub(crate) struct Editor {
    pub key: Option<String>,
    pub account: Account,
    pub game_install: String,
    pub dat_overlays: Vec<String>,
    pub auto_login: pol_less::AutoLogin,
    pub flags: EditorFlags,
}

/// The account sheet's switches that are not account fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EditorFlags {
    /// "Uses a one-time password": saved as what Crystal knows about the login
    /// (`Settings::account_otp`), so it is never signed in by itself.
    pub uses_otp: bool,
    /// New accounts: sign in to PlayOnline in the background once saved.
    pub sign_in_now: bool,
    pub show_id: bool,
    pub show_password: bool,
    /// A saved account's password field is read-only until Change.
    pub change_password: bool,
    pub show_totp_key: bool,
    /// A saved account's authenticator key, when it has one, is read-only until Change.
    pub change_totp_key: bool,
}

/// Which of `EditorFlags` an `EditorFlag` message sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditorFlag {
    UsesOtp,
    SignInNow,
    ShowId,
    ShowPassword,
    ChangePassword,
    ShowTotpKey,
    ChangeTotpKey,
}

impl Editor {
    /// A fresh sheet for a new account: background sign-in on by default.
    fn blank() -> Self {
        Editor {
            key: None,
            account: blank_account(),
            game_install: String::new(),
            dat_overlays: Vec::new(),
            auto_login: pol_less::AutoLogin::default(),
            flags: EditorFlags { sign_in_now: true, ..Default::default() },
        }
    }
}

/// The Viewer refreshes its last-input stamp from window input (app 10060039)
/// for Auto-Away; Crystal forwards window input at most this often.
const ACTIVITY_INTERVAL: Duration = Duration::from_secs(5);
static LAST_ACTIVITY: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// How long a Friends request may go without a result before the page says so. Shorter than
/// the loader's mail deadlines (MAIL_FRAMES per attempt, up to 3 attempts for a send): a result
/// after it still counts (`App::friends_expired`).
const FRIENDS_REQUEST_LIMIT: Duration = Duration::from_secs(45);

fn activity_due(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_some_and(|at| now.saturating_duration_since(at) < ACTIVITY_INTERVAL) {
        return false;
    }
    *last = Some(now);
    true
}

/// `--preview-idle`: the preview's sample data with the live window's idle behaviour (its
/// own tick rate, the controller thread, no capture), for measuring the idle footprint
/// without accounts, sign-ins or network traffic.
pub(crate) fn preview_idle() -> bool {
    preview::has("--preview-idle")
}

/// Keyboard or pointer input reached the window: a `Message::UserInput`, at most once per
/// ACTIVITY_INTERVAL (every front end reports input through this one throttle).
pub(crate) fn user_activity() -> Option<Message> {
    activity_due(&mut LAST_ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()), Instant::now()).then_some(Message::UserInput)
}

#[derive(Clone)]
enum WorkerEvent {
    /// Ignore a startup/reconnect result superseded by a coordinated Play.
    Launch(String, u64, Box<WorkerEvent>),
    Progress(String, String),
    Complete(String, Result<LaunchResult, String>),
    /// A news update, tagged with its request and the language its feeds were read in.
    Services(u64, i18n::Lang, services::Update),
    Friends(u64, String, bool, Option<friends::Snapshot>),
    /// A signed-in session's Play: W20's report (absent when it never ran) and the outcome.
    StartGame(String, Option<patch::PatchReport>, Result<String, String>),
    /// A row's connectivity probe (07d §5.5): the patch server resolves again (`true`), or
    /// it gave up after `session_worker::NETWORK_PROBE_LIMIT` (`false`). Sent `for_launch`.
    NetworkBack(String, bool),
    /// A line for the status bar that outlives the account's progress lines.
    Notice(String),
    WindowerProfileProgress(String),
    Viewer(u64, viewer::Status),
    /// An interrupted update found at a Play was finished or undone (`Ok`), or could not
    /// be: the held launch then starts, or its rows say why.
    InterruptedUpdate(Box<Recovered>),
    /// A Play-driven update's thread (its job generation), proposal 09 phase 2.
    PlayUpdate(u64, play_update::Event),
    /// A maintenance version check of a game folder from this account's session (patch_watch):
    /// the W20's report, whether it said "current", and its phases; `Err` when none was sent.
    PatchCheck(PathBuf, String, Result<(patch::PatchReport, bool, Option<String>), String>),
    /// Window layouts, hotkeys, recovery, notifications (`features::Runtime`).
    Features(features::Event),
}
/// A launch waiting for something local (an interrupted update's recovery) before it starts.
#[derive(Clone, Debug)]
struct HeldLaunch {
    keys: Vec<String>,
    social_only: bool,
    clicked: Instant,
    trigger: Option<launch_timing::LaunchTrigger>,
}
/// The outcome of finishing interrupted updates for a held launch.
#[derive(Clone)]
struct Recovered {
    held: HeldLaunch,
    /// The folders whose journals were finished or undone.
    folders: Vec<PathBuf>,
    /// Every parked host was closed first (PlayOnline's folder was among them, or a retry).
    parks_closed: bool,
    result: Result<(), String>,
    /// After a success, the Settings game folder's and PlayOnline's local state again.
    game: Option<patch::PatchReport>,
    viewer: Option<viewer::Status>,
}
impl WorkerEvent {
    fn for_launch(self, key: &str, generation: u64) -> Self {
        Self::Launch(key.to_owned(), generation, Box::new(self))
    }
}

/// Sends one launch's events, tagged with its row and launch generation (`for_launch`): a
/// newer launch of the row makes them stale.
#[derive(Clone)]
struct LaunchTx {
    tx: tick_gate::EventSender,
    key: String,
    generation: u64,
    /// A sign-in's `AccountRow::sign_in_done`, set before its Complete goes out.
    done: Option<Arc<AtomicBool>>,
}
impl LaunchTx {
    fn new(tx: tick_gate::EventSender, key: String, generation: u64) -> Self {
        Self { tx, key, generation, done: None }
    }
    fn send(&self, event: WorkerEvent) {
        let _ = self.tx.send(event.for_launch(&self.key, self.generation));
    }
    fn progress(&self, line: &str) {
        self.send(WorkerEvent::Progress(self.key.clone(), line.to_owned()));
    }
    fn complete(&self, result: Result<LaunchResult, String>) {
        if let Some(done) = &self.done { done.store(true, Ordering::Release); }
        self.send(WorkerEvent::Complete(self.key.clone(), result));
    }
}

/// The window's timers and listeners for the current state (`App::schedule`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Schedule {
    /// The logic Tick's rate (`tick_gate` drops the ones that would change nothing).
    pub tick: Duration,
    /// Poll the controller thread.
    pub gamepad: bool,
    /// An account row is being dragged: raw pointer input goes to `account_order`.
    pub account_drag: bool,
    /// A held controller direction repeats (`Message::ControllerTick` every 50 ms).
    pub controller_repeat: bool,
}

#[derive(Clone)]
pub(crate) enum Message {
    Tick,
    ControllerTick,
    ControllerBack,
    ControllerPage(i8),
    ControllerPlay,
    ControllerNavigation(bool),
    SelfUpdate(self_update::Msg),
    WindowResized(Size),
    MinimizeWindow,
    MaximizeWindow,
    // native chrome, title double-click and edge resizing.
    WindowOpened(WindowId),
    WindowClosed(WindowId),
    /// The notification-area icon (or a second Crystal start) asked for something.
    Tray(tray::Command),
    /// After hiding: give the freed renderer's pages back to Windows.
    TrimMemory,
    /// Settings → System: closing the window keeps Crystal in the notification area.
    CloseToTray(bool),
    /// Settings → System: start Crystal in the background when Windows starts.
    StartWithWindows(bool),
    /// The scheduled task was registered (`true`) or removed, or why not.
    StartupTaskSet(bool, Result<(), String>),
    NativeWindow(isize),
    WindowMaximized(bool),
    TitlePressed,
    TitleMoved,
    TitleReleased,
    ResizeWindow(Edge),
    Navigate(Page),
    FriendsAccount(String),
    FriendsConnect,
    FriendsPlay,
    FriendsSignOut,
    ShutdownSelected,
    ShutdownAll,
    GameShutdownFinished(String, u64, Result<(), String>),
    AccountSignOut(String),
    AccountSignedOut(String, u64, Result<(), String>),
    FriendsStatus(session_worker::NativeControl),
    FriendsStatusGroups(bool),
    FriendsAutoAway(u8),
    /// Keyboard or mouse input reached the window (throttled at the source).
    UserInput,
    /// The window gained (`true`) or lost the keyboard focus (minimising loses it).
    WindowFocus(bool),
    FriendsQuery(String),
    /// Open a conversation, the Mailbox or the block list (None: nothing).
    FriendsSelect(Option<friends::Pane>),
    /// Fold or unfold the Online (0) / Offline (1) section.
    FriendsSection(usize),
    FriendsStatusOptions,
    FriendsAddSubject(bool),
    FriendsSocial(social::Request),
    FriendsRemove(friends::Friend),
    FriendsRemoveConfirm,
    FriendsSubject(String),
    FriendsMessageEdit(String),
    FriendsSend,
    FriendsDraftClose,
    ToggleDetails,
    ToggleAccount(String, bool),
    AccountDragStart(String),
    AccountDragOver(String),
    AccountDragFinish,
    AccountDragCancel,
    EditWindowerProfiles,
    WindowerProfilesClosed(Result<(), String>),
    Launch(String),
    MainAction,
    LaunchAll,
    /// Sign this account in to PlayOnline in the background (no game), with its typed code.
    SignIn(String),
    /// Home's Character select: automatic character login on (true) or off for Play's accounts.
    SetCharacterSelect(bool),
    OtpChanged(String, String),
    NewAccount,
    EditAccount(String),
    EditorChanged(Field, String),
    EditorRegion(u8),
    // the editor's Launch section (None = use the default).
    EditorLaunchWith(Option<crate::backend::LaunchWith>),
    EditorAutoLogin(bool),
    EditorFlag(EditorFlag, bool),
    EditorCharacterSlot(u8),
    AccountImport(account_import_ui::Msg),
    EditorBootIni(Option<String>),
    EditorProfile(Option<String>),
    SaveAccount,
    RequestDeleteAccount(String),
    CancelDeleteAccount,
    DeleteAccount(String),
    CloseEditor,
    ToggleSettings,
    SetLightTheme(bool),
    SetLanguage(i18n::Lang),
    SetPrivacy(bool),
    /// Settings → System: "Sign accounts in when Crystal opens".
    AutoSignIn(bool),
    SignInAfterGame(bool),
    SetLaunchWith(crate::backend::LaunchWith),
    WikiQueryChanged(String),
    AhQueryChanged(String),
    SearchWiki,
    SearchAh,
    AhResults(u64, Result<ffxiah::Results, String>),
    CloseAh,
    /// Open the portrait picker for this account (its key), or close it.
    PortraitPicker(Option<String>),
    /// The installed portraits, listed off the UI thread.
    PortraitsListed(std::sync::Arc<[u16]>),
    PortraitPage(usize),
    PortraitPick(u16),
    AhServerChanged(&'static str),
    NewsFilter(crate::services::NewsTab),
    InstallPathChanged(String),
    SelectInstallPath,
    FindInstallation,
    InstallationFound(u64, bool, Option<PathBuf>),
    InstallerPathChanged(String),
    AccountInstallChanged(String),
    PickedAccountFolder(String, Option<PathBuf>),
    PickedAccountOverlay(String, Option<PathBuf>),
    SelectInstallerPath,
    /// Open the native folder (or loader file) picker for a path field.
    Browse(PickTarget),
    Picked(PickTarget, Option<PathBuf>),
    StartMaintenance(MaintenanceKind),
    MaintenanceFinished(MaintenanceKind, Result<String, String>, Option<patch::PatchReport>),
    CancelMaintenance,
    /// Show or hide Downloads → Troubleshooting (Check files, Repair files).
    ToggleTroubleshooting,
    SaveSettings,
    Refresh,
    OpenUrl(String),
    OpenLog(String),
    OpenLogsFolder(String),
    AccountsLoaded(Result<Vec<Account>, String>),
    AccountsSaved(Result<Vec<Account>, String>),
    /// The finished news refresh, its request generation and language.
    ServicesLoaded(u64, i18n::Lang, Result<services::ServiceSnapshot, String>),
    ServiceStatusLoaded(u64, i18n::Lang, Result<services::ServiceSnapshot, String>),
    /// A read of FFXI's play flag, made while a Play waits.
    PlayFlagLoaded(play_flag::Reading),
    /// Cancel these accounts' "Wait and play".
    CancelWait(Vec<String>),
    /// Pause the Play-driven update with this generation (its downloads are kept).
    CancelPlayUpdate(u64),
    /// A Play's PlayOnline update may sign the idle accounts out (the row asked once).
    ConfirmViewerUpdate,
    PatchLoaded(u64, patch::PatchReport),
    ViewerLoaded(u64, viewer::Status),
    CloseRequested,
    ConfirmClose,
    CancelClose,
    PreviewReady(Screenshot),
    /// The launcher features' screens (`features::ui`).
    Feature(features::ui::Msg),
    // The Settings page features own their state and messages in their modules.
    SettingsTab(settings_page::Tab),
    GameConfig(game_config::Msg),
    Prereqs(prereqs::Msg),
    Gamepad(gamepad::Msg),
    OpenSettingsTab(settings_page::Tab), // banner Fix → Settings tab
    LaunchTools(launch_tools::Msg),
    LaunchToolsScanned(launch_tools::Scan),
    OpenWithGame(open_with_game::Msg),
    /// The programs a game start opened: what failed.
    OpenWithGameDone(open_with_game::Report),
    Dgvoodoo(dgvoodoo::Msg),
    Reshade(reshade::Msg),
}
/// A path field that has a native Browse dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PickTarget {
    GameFolder,
    AccountGameFolder,
    AccountDatFolder,
    InstallerFolder,
    AshitaFolder,
    WindowerFolder,
    /// A file, not a folder: the loader's pol.exe.
    LoaderFile,
    /// A file: the program at this index in Open with the game.
    ProgramFile(usize),
}

// Debug for logs and tests, but input messages can contain account passwords.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LauncherMessage (payload redacted)")
    }
}

pub(crate) struct App {
    pub viewport: Size,
    pub maximized: bool,
    pub title_held: bool, // title pressed, drag not yet started
    pub page: Page,
    pub show_details: bool,
    pub accounts: Vec<AccountRow>,
    pub account_drag: account_order::Drag,
    pub editor: Option<Editor>,
    pub account_import: account_import_ui::State,
    pub services: Option<services::ServiceSnapshot>,
    pub news_loading: bool,
    /// The language of the news refresh in flight (meaningful while `news_loading`).
    news_lang: i18n::Lang,
    news_generation: u64,
    pub news_error: String,
    startup_warning: Option<String>,
    /// settings.json could not be read at start: nothing Crystal saves by itself replaces it
    /// (the warning above is shown once; this stays until the user saves settings).
    settings_damaged: bool,
    pub patch: Option<patch::PatchReport>,
    pub patch_loading: bool,
    pub installation_loading: bool,
    installation_generation: u64,
    pub viewer: Option<viewer::Status>,
    pub viewer_loading: bool,
    viewer_generation: u64,
    pub settings: Settings,
    pub show_settings: bool,
    pub status: String,
    pub vault_ready: bool,
    /// Why the saved accounts could not be read (the rail says so in place of its first-run state).
    vault_error: Option<String>,
    startup_signin_pending: bool,
    /// Startup sign-ins whose row was still busy with a previous session, and since when
    /// (07d §5.5): each starts once its row is free, or is skipped after STARTUP_WAIT_LIMIT.
    startup_waiting: Vec<(String, Instant)>,
    /// When the startup sign-ins were issued (the parks start after them, 07d §5.7).
    startup_issued: Option<Instant>,
    /// The `launcher start` history line was written (07d §4.4).
    launcher_start_logged: bool,
    /// This window's first official status read has finished (either way): until it has, a
    /// Play has only a missing or cached status to go on, so it is held (`hold_for_status`).
    status_read_done: bool,
    /// A hold reached STATUS_HOLD_LIMIT without a read: later Plays are not held either.
    status_hold_spent: bool,
    /// Plays clicked before the first status read, waiting for it.
    held_plays: Vec<HeldPlay>,
    /// Plays waiting for FFXI to come back ("Wait and play").
    waiting_plays: Vec<WaitingPlay>,
    /// The last read of FFXI's play flag, and when the next is due (only while a Play waits or
    /// FFXI is in maintenance).
    play_flag: play_flag::Known,
    flag_schedule: play_flag::Schedule,
    /// Play is the update (proposal 09 phase 2): one update job per game folder.
    pub(crate) play_updates: play_update::Jobs,
    /// The update as soon as it is out: maintenance's version checks (patch_watch.rs).
    patch_watch: patch_watch::Watch,
    /// Tests: record `start_launch` calls instead of starting workers.
    #[cfg(test)]
    launches: Option<Vec<(Vec<String>, bool, Instant, Option<launch_timing::LaunchTrigger>)>>,
    /// Tests: recorded starts that went without a W20 (`skip_w20`).
    #[cfg(test)]
    w20_skipped: Vec<String>,
    /// Parked warm hosts for cold Plays, one per account, tool and profile (07d §1.5).
    parks: pol_less::park::Pool<session_worker::ParkWorker>,
    /// Each wanted park key's last fingerprint; re-read every FINGERPRINT_EVERY.
    park_prints: std::collections::BTreeMap<pol_less::park::ParkKey, pol_less::park::Fingerprint>,
    /// When the fingerprints were read; `None` re-reads them at the next tend (after a
    /// settings save, a profile-editor close or an installation change).
    park_checked: Option<Instant>,
    /// The last pool tend (at most once a second).
    park_tended: Option<Instant>,
    pub confirm_close: bool,
    closing: bool,
    pub pending_delete: Option<String>,
    pub wiki_query: String,
    pub ah_query: String,
    pub ah_lookup: ffxiah::Lookup,
    /// The portrait picker, while open.
    pub portrait_picker: Option<portraits::Picker>,
    /// Each account's own portrait as its PlayOnline session last reported it (this run only).
    pub own_portraits: HashMap<String, u16>,
    pub ah_server: &'static str,
    pub news_filter: crate::services::NewsTab,
    pub maintenance: Option<MaintenanceKind>,
    pub maintenance_progress: operation::Progress,
    pub maintenance_result: String,
    pub installer_error: Option<String>,
    pub maintenance_tone: ResultTone,
    pub maintenance_cancelled: bool,
    /// What the running Update covers: (PlayOnline, FINAL FANTASY XI).
    update_scope: (bool, bool),
    /// Downloads → Troubleshooting is expanded.
    pub troubleshooting_open: bool,
    /// An interrupted update is being finished before a launch: `Some(true)` when that
    /// includes PlayOnline's folder (whose files parked hosts hold).
    recovering: Option<bool>,
    /// The game folder `patch` describes when it is not the Settings one: an account's own
    /// folder whose W20 refused a Play. The main button's Update updates that folder.
    patch_folder: Option<PathBuf>,
    /// The official versions real version checks last told (`official_versions`), read again
    /// whenever a check's outcome reaches the window.
    official: official_versions::Store,
    /// Plays a version check refused for an update (account key, click), and those an
    /// Update started from them: started again when that Update succeeds (proposal 09).
    refused_plays: Vec<(String, Instant)>,
    reissue_after_update: Vec<(String, Instant)>,
    active_operation: Option<operation::Operation>,
    config: Config,
    sender: tick_gate::EventSender,
    receiver: mpsc::Receiver<WorkerEvent>,
    /// The launcher window; `None` while Crystal runs in the background (notification area
    /// icon only, no surface or renderer).
    window: Option<WindowId>,
    /// The last park tend found every wanted park parked (or none wanted): nothing for the
    /// next Tick to start (`tick_gate`).
    parks_settled: bool,
    /// Start with Windows: the scheduled task's registration is being changed.
    startup_task_busy: bool,
    vault_saving: bool,
    patch_generation: u64,
    refreshed: Instant,
    preview: bool,
    preview_capture_sent: bool,
    pub settings_tab: settings_page::Tab,
    pub game_config: game_config::State,
    pub prereqs: prereqs::State,
    pub gamepad: gamepad::State,
    pub launch_tools: launch_tools::State,
    open_gate: open_with_game::BatchGate,
    pub windower_editor_open: bool,
    pub friends: friends::State,
    /// The Friends request whose time limit passed without a result, still matched against
    /// later reads: the loader's own deadlines are longer (a send retries for minutes).
    friends_expired: Option<String>,
    friends_generation: u64,
    friends_checked: Instant,
    pub dgvoodoo: dgvoodoo::State,
    pub reshade: reshade::State,
    pub self_update: self_update::State,
    /// The automatic restart that installs a downloaded update (`self_update::auto`).
    pub(crate) auto_restart: self_update::auto::AutoRestart,
    pub controller_nav: controller_nav::State,
    /// The launcher features beside Play (`features`); `None` in previews and tests.
    pub(crate) features: Option<features::Runtime>,
    /// The signed-in accounts the features last heard of (notifications follow them).
    features_sessions: Vec<String>,
    /// The features' screens' own state (`features::ui`).
    pub(crate) features_view: features::ui::State,
}

impl App {
    /// The daemon's boot: the state, then its window, unless Crystal starts in the background
    /// (`--background`, Start with Windows), where only the notification-area icon shows.
    fn new() -> (Self, Effect<Message>) {
        let (mut app, task) = Self::boot();
        // `--preview-idle` measures the background too: its close button hides the window.
        if preview_idle() { app.settings.close_to_tray = true; }
        let live = !app.preview || preview_idle();
        let background = live && std::env::args().any(|a| a == startup_task::BACKGROUND_ARG);
        if live { tray::start(); }
        // `--preview-idle` too, so the idle footprint is measured with the features' threads.
        if live { app.start_features(); }
        if background {
            tray::set_icon(true);
            launch_log::append_history(&app.config.history_path(), "Crystal", &format!(
                "background start: pid {}, no window", std::process::id()), None);
            // Once the startup sign-ins and checks have run.
            return (app, Effect::batch([task, App::trim_later(Duration::from_secs(30))]));
        }
        let open = app.open_window();
        (app, Effect::batch([task, open]))
    }

    /// The one field list: inert values, no disk or network reads. `boot` sets what a real
    /// start differs in; tests use it as it is (sample data, `preview` as the no-I/O switch).
    fn base(config: Config, settings: Settings, sender: tick_gate::EventSender, receiver: mpsc::Receiver<WorkerEvent>, dgvoodoo: dgvoodoo::State) -> Self {
        Self {
            viewport: Size::new(1200.0, 820.0),
            maximized: false,
            title_held: false,
            page: Page::Home,
            show_details: false,
            accounts: Vec::new(),
            account_drag: account_order::Drag::default(),
            windower_editor_open: false,
            editor: None,
            account_import: account_import_ui::State::default(),
            services: None,
            news_loading: false,
            news_lang: i18n::Lang::En,
            news_generation: 0,
            news_error: String::new(),
            startup_warning: None,
            settings_damaged: false,
            patch: None,
            patch_loading: false,
            installation_loading: false,
            installation_generation: 0,
            viewer: None,
            viewer_loading: false,
            viewer_generation: 0,
            settings,
            show_settings: false,
            status: String::new(),
            vault_ready: true,
            vault_error: None,
            startup_signin_pending: false,
            startup_waiting: Vec::new(),
            startup_issued: None,
            launcher_start_logged: false,
            status_read_done: true,
            status_hold_spent: false,
            held_plays: Vec::new(),
            waiting_plays: Vec::new(),
            play_flag: play_flag::Known::default(),
            flag_schedule: play_flag::Schedule::default(),
            play_updates: play_update::Jobs::default(),
            patch_watch: patch_watch::Watch::default(),
            #[cfg(test)]
            launches: None,
            #[cfg(test)]
            w20_skipped: Vec::new(),
            parks: Default::default(),
            park_prints: Default::default(),
            park_checked: None,
            park_tended: None,
            confirm_close: false,
            closing: false,
            pending_delete: None,
            wiki_query: String::new(),
            ah_query: String::new(),
            ah_lookup: ffxiah::Lookup::default(),
            portrait_picker: None,
            own_portraits: HashMap::new(),
            ah_server: crate::ffxiah::DEFAULT_AH_SERVER,
            news_filter: crate::services::NewsTab::Latest,
            maintenance: None,
            maintenance_progress: operation::Progress::default(),
            maintenance_result: String::new(),
            installer_error: None,
            maintenance_tone: ResultTone::default(),
            maintenance_cancelled: false,
            update_scope: (true, true),
            troubleshooting_open: false,
            recovering: None,
            patch_folder: None,
            official: official_versions::Store::default(),
            refused_plays: Vec::new(),
            reissue_after_update: Vec::new(),
            active_operation: None,
            config,
            sender,
            receiver,
            window: None,
            parks_settled: true,
            startup_task_busy: false,
            vault_saving: false,
            patch_generation: 0,
            refreshed: Instant::now(),
            preview: true,
            preview_capture_sent: false,
            settings_tab: settings_page::Tab::default(),
            game_config: game_config::State::default(),
            prereqs: prereqs::State::default(),
            gamepad: gamepad::State::default(),
            launch_tools: launch_tools::State::default(),
            open_gate: Default::default(),
            friends: friends::State::default(),
            friends_expired: None,
            friends_generation: 0,
            friends_checked: Instant::now(),
            dgvoodoo,
            reshade: reshade::State::default(),
            self_update: self_update::State::default(),
            auto_restart: self_update::auto::AutoRestart::default(),
            controller_nav: controller_nav::State::default(),
            features: None,
            features_sessions: Vec::new(),
            features_view: features::ui::State::default(),
        }
    }

    fn boot() -> (Self, Effect<Message>) {
        let config = Config::default();
        let preview = preview::flags().is_some();
        let (settings, startup_warning) = if preview {
            (Settings::default(), None)
        } else {
            match load_settings_and_language(&config) {
                Ok(settings) => (settings, None),
                Err(error) => (
                    Settings::default(),
                    Some(tf("{error} The original settings file was kept; defaults are being used.", &[("error", &error)])),
                ),
            }
        };
        // Previews are English unless --preview-ja, whatever Windows is set to.
        i18n::set(if preview {
            if preview::has("--preview-ja") { i18n::Lang::Ja } else { i18n::Lang::En }
        } else {
            settings.language.unwrap_or_else(i18n::system_default)
        });
        privacy::set(if preview { preview::has("--preview-private") } else { settings.privacy });
        let (sender, receiver) = mpsc::channel();
        // With dgVoodoo on and not yet installed, its download starts in the background now,
        // so the first Play doesn't wait for it.
        let dgvoodoo = dgvoodoo::State::new(settings.dgvoodoo);
        let mut app = Self::base(config.clone(), settings, tick_gate::EventSender::new(sender), receiver, dgvoodoo);
        app.viewport = initial_window_size();
        app.preview = preview;
        app.settings_damaged = startup_warning.is_some();
        app.startup_warning = startup_warning;
        app.services = if preview { None } else { services::cached(&news_cache_path(&config, i18n::current()), i18n::current()) };
        app.news_lang = i18n::current();
        app.status = t("Loading saved accounts…").into();
        app.vault_ready = false;
        app.startup_signin_pending = !preview;
        app.status_read_done = preview;
        app.maintenance_result = t("Files have not been checked. Version availability alone does not verify file integrity.").into();
        app.maintenance_tone = ResultTone::Unverified;
        if !preview { app.official = official_versions::load(&official_versions::path_for(&config)); }
        app.self_update = self_update::initial(&config);
        let tools = app.launch_tools.refresh(&app.settings);
        // The game's settings are read once, as soon as the app runs (a few registry values).
        let game_config = Effect::done(Message::GameConfig(game_config::Msg::Load));
        if preview {
            let (app, task) = preview::boot(app);
            return (app, Effect::batch([tools, game_config, task]));
        }
        let accounts = Effect::perform(
            async move {
                tokio::task::spawn_blocking(move || backend::load_or_import_accounts(&config))
                    .await
                    .unwrap_or_else(|_| Err(t("Could not load saved accounts.").into()))
            },
            Message::AccountsLoaded,
        );
        let checks = app.refresh();
        let installation = app.find_installation(true);
        let viewer = app.start_viewer_check();
        let launcher = if app.settings.launcher_auto_updates && !matches!(app.self_update, self_update::State::Error(_)) { Effect::done(Message::SelfUpdate(self_update::Msg::Check)) } else { Effect::none() };
        // Start with Windows on: its task still starts this exe (it may have moved).
        let startup = if app.settings.start_with_windows { app.startup_task_busy = true; app.startup_task(None) } else { Effect::none() };
        (app, Effect::batch([accounts, checks, viewer, launcher, installation, startup, tools, game_config]))
    }

    /// What the window's timers and listeners should be after this update, in no toolkit's
    /// terms; the host (`slint_host`) runs them to match.
    pub(crate) fn schedule(&self) -> Schedule {
        // Every tick rebuilds and redraws the whole view, so poll fast only while
        // work is in flight and idle along at 1 Hz otherwise.
        let busy = self.preview && !preview_idle()
            || self.maintenance.is_some()
            || self.active_operation.is_some()
            || self.vault_saving
            || self.news_loading
            || self.patch_loading
            || self.installation_loading
            || self.viewer_loading
            || self.friends.pending.is_some()
            || !self.held_plays.is_empty()
            || !self.play_updates.jobs.is_empty()
            || self.accounts.iter().any(|a| a.busy || a.batch_pending);
        let tick = Duration::from_millis(if busy { 100 } else { 1000 });
        // Ticks that would change nothing are not sent (each message redraws the window).
        tick_gate::set_quiet(!busy && self.tick_quiet(), self.watched_flags(), self.next_timed_work());
        Schedule {
            tick,
            // The controller thread polls only while there is a window to drive.
            gamepad: self.window.is_some(),
            account_drag: self.account_drag.source.is_some(),
            controller_repeat: self.controller_nav.repeating(),
        }
    }

    /// The Start with Windows task is being registered or removed (its switch waits).
    pub(crate) fn startup_task_busy(&self) -> bool { self.startup_task_busy }

    pub fn launchable_count(&self) -> usize {
        self.launch_keys(false).len()
    }
    pub fn selected_count(&self) -> usize {
        self.launch_keys(true).len()
    }
    fn launch_keys(&self, selected_only: bool) -> Vec<String> {
        if self.launch_blocked() {
            return Vec::new();
        }
        // Row keys are unique (`replace_accounts`).
        self.accounts
            .iter()
            .filter(|r| r.account.mode == AccountMode::Retail && r.can_play() && (!selected_only || r.selected))
            .map(|r| r.key.clone())
            .collect()
    }
    /// Whether `key`'s Play waits for FFXI to come back.
    pub(crate) fn is_waiting(&self, key: &str) -> bool {
        self.waiting_plays.iter().any(|wait| wait.keys.iter().any(|waiting| waiting == key))
    }
    /// The Play-driven update `key` takes part in: its Play asked for it, or waits for it
    /// (`true`: it is one of the job's waiters; `false`: its Play waits for the folder).
    pub(crate) fn play_update_of(&self, key: &str) -> Option<(&play_update::Job, bool)> {
        if self.play_updates.jobs.is_empty() { return None; }
        if let Some(job) = self.play_updates.for_key(key) { return Some((job, true)); }
        let row = self.accounts.iter().find(|row| row.key == key && row.queued_play.is_some())?;
        let folder = self.play_folder(row)?;
        self.play_updates.for_folder(&folder).map(|job| (job, false))
    }
    /// The selected accounts "Wait and play" would set waiting: launchable, not waiting yet.
    pub(crate) fn wait_keys(&self) -> Vec<String> {
        self.launch_keys(true).into_iter().filter(|key| !self.is_waiting(key)).collect()
    }
    fn has_sessions(&self) -> bool {
        self.accounts.iter().any(|r| r.shutting_down || r.signing_out || r.busy || r.batch_pending || r.pid.is_some()
            || !self.preview && session_worker::find(&self.config, &r.account).is_some())
    }

    fn update_sessions(&self) -> Option<Vec<session_worker::BackgroundSession>> {
        let mut sessions = Vec::new();
        for row in &self.accounts {
            if row.shutting_down || row.signing_out || row.batch_pending || row.queued_play.is_some() || row.is_playing()
                || row.busy && !row.preparing_session { return None; }
            let session = (!self.preview).then(|| session_worker::find(&self.config, &row.account)).flatten();
            if let Some(session) = session {
                if !session.can_prepare_update() { return None; }
                sessions.push(session);
            } else if row.pid.is_some() || row.busy { return None; }
        }
        Some(sessions)
    }

    pub(crate) fn sessions_block_update(&self) -> bool { self.update_sessions().is_none() }
    pub fn launch_blocked(&self) -> bool {
        self.closing || self.installation_loading || self.files_changing() || self.recovering.is_some() || self.windower_editor_open
    }

    /// Game, PlayOnline, ReShade or Crystal files are being written: what a parked pol.exe
    /// (polcore.dll loaded) must not hold open. A read-only installation scan is not, and
    /// neither is finishing an interrupted update in a game folder only.
    fn files_changing(&self) -> bool {
        self.reshade.busy
            || self.recovering == Some(true)
            || self.maintenance.is_some_and(MaintenanceKind::blocks_launch)
            || matches!(self.self_update, self_update::State::Applying)
    }

    /// What the running file operation is called: an Update names what it updates.
    pub(crate) fn maintenance_label(&self) -> &'static str {
        match (self.maintenance, self.update_scope) {
            (Some(MaintenanceKind::Update), (true, false)) => t("Updating PlayOnline"),
            (Some(MaintenanceKind::Update), (false, true)) => t("Updating FINAL FANTASY XI"),
            (Some(kind), _) => kind.label(),
            (None, _) => t("Working"),
        }
    }

    /// What an Update now covers: PlayOnline when its last sign-in check or local state
    /// says it needs one, FINAL FANTASY XI when its version check did, or when nothing
    /// says so (the Play button's Update). A game update never asks for PlayOnline's file
    /// list, as stock never does (capture 2026-10-13 00:49).
    fn update_scope(&self) -> (bool, bool) {
        let viewer = self.viewer.as_ref().is_some_and(viewer::Status::requires_update);
        let game = self.patch.as_ref().is_some_and(|report| report.state == patch::PatchState::Required);
        (viewer, game || !viewer)
    }

    /// The Play gate's verdict and the note that explains it: the official status (Round 4),
    /// over which a fresh read of FFXI's play flag wins both ways (`play_flag::decide`).
    fn service_status(&self) -> (services::Availability, String) {
        if self.preview {
            return self.services.as_ref().map_or((services::Availability::Unknown, String::new()),
                |snapshot| (snapshot.availability, snapshot.availability_note.clone()));
        }
        let now = chrono::Utc::now();
        let official = self.services.as_ref().map_or((services::Availability::Unknown, String::new()), |snapshot| snapshot.recent_status(now));
        play_flag::decide(&self.play_flag, official, now)
    }

    pub fn service_availability(&self) -> services::Availability {
        self.service_status().0
    }

    /// The note that explains `service_availability`, as a waiting Play shows it.
    pub fn service_note(&self) -> String {
        self.service_status().1
    }

    fn service_down(&self) -> bool {
        matches!(self.service_availability(), services::Availability::Maintenance | services::Availability::Outage)
    }

    pub fn main_action(&self) -> MainAction {
        if self.maintenance.is_some() || self.reshade.busy || matches!(self.self_update, self_update::State::Applying) { return MainAction::Working; }
        // An update FFXI or PlayOnline needs no longer turns the button into Update: Play is
        // the update (proposal 09 phase 2).
        match self.service_availability() {
            services::Availability::Maintenance => MainAction::Maintenance,
            services::Availability::Outage => MainAction::Offline,
            _ => MainAction::Play,
        }
    }

    fn run_main_action(&mut self) -> Effect<Message> {
        if self.reshade.busy {
            self.page = Page::Settings;
            self.settings_tab = settings_page::Tab::Graphics;
            return Effect::none();
        }
        // Resolve again on click: a status refresh may have changed the action.
        let action = self.main_action();
        match action {
            MainAction::Play => self.play(self.launch_keys(true)),
            MainAction::Working => {
                self.page = Page::Maintenance;
                self.show_settings = false;
                self.show_details = false;
            }
            // "Wait and play": the selected accounts wait for FFXI (`wait_for_service`). With
            // nothing left to set waiting, the button explains the downtime.
            MainAction::Maintenance | MainAction::Offline => {
                let keys = self.wait_keys();
                if self.preview || keys.is_empty() { self.show_details = true; } else { self.play(keys); }
            }
        }
        Effect::none()
    }

    /// Open the native dialog off the UI thread, starting from what the field holds.
    fn browse(&mut self, target: PickTarget) -> Effect<Message> {
        let blocked = match target {
            PickTarget::GameFolder | PickTarget::InstallerFolder => self.maintenance.is_some(),
            _ => false,
        };
        if self.preview || blocked {
            return Effect::none();
        }
        if target == PickTarget::AccountGameFolder || target == PickTarget::AccountDatFolder {
            let Some(editor) = &self.editor else { return Effect::none(); };
            if self.vault_saving { return Effect::none(); }
            if target == PickTarget::AccountDatFolder && editor.dat_overlays.len() >= 8 { return Effect::none(); }
            let current = if target == PickTarget::AccountDatFolder {
                editor.dat_overlays.last().unwrap_or(&editor.game_install)
            } else { &editor.game_install };
            let start = dialog_start(current, false);
            let key = account_key(&editor.account);
            return Effect::perform(pick_path(target, start), move |picked| {
                if target == PickTarget::AccountDatFolder { Message::PickedAccountOverlay(key.clone(), picked) }
                else { Message::PickedAccountFolder(key.clone(), picked) }
            });
        }
        let current = match target {
            PickTarget::GameFolder => &self.settings.install_path,
            PickTarget::InstallerFolder => &self.settings.installer_path,
            PickTarget::AshitaFolder => &self.settings.ashita_path,
            PickTarget::WindowerFolder => &self.settings.windower_path,
            PickTarget::LoaderFile => &self.settings.loader_path,
            PickTarget::ProgramFile(i) => self.settings.open_with_game.get(i).map_or(&EMPTY, |p| &p.path),
            PickTarget::AccountGameFolder | PickTarget::AccountDatFolder => unreachable!(),
        };
        static EMPTY: String = String::new();
        let start = dialog_start(current, matches!(target, PickTarget::LoaderFile | PickTarget::ProgramFile(_)));
        Effect::perform(pick_path(target, start), move |picked| {
            Message::Picked(target, picked)
        })
    }

    /// A dialog answer: put the chosen path in its field. The game and installer
    /// folders then go through the same validation and confirmation as before.
    fn picked(&mut self, target: PickTarget, picked: Option<PathBuf>) -> Effect<Message> {
        let Some(picked) = picked else { return Effect::none() };
        let text = backend::plain_windows_path(&picked).to_string_lossy().into_owned();
        match target {
            PickTarget::GameFolder | PickTarget::InstallerFolder => {
                if self.maintenance.is_some() {
                    return Effect::none();
                }
                let installer = target == PickTarget::InstallerFolder;
                if installer {
                    self.settings.installer_path = text;
                    self.settings.installer_selected = false;
                } else {
                    let _ = self.update(Message::InstallPathChanged(text));
                }
                self.select_path(installer);
            }
            PickTarget::AshitaFolder => return self.update(Message::LaunchTools(launch_tools::Msg::AshitaPath(text))),
            PickTarget::WindowerFolder => return self.update(Message::LaunchTools(launch_tools::Msg::WindowerPath(text))),
            PickTarget::LoaderFile => return self.update(Message::LaunchTools(launch_tools::Msg::LoaderPath(text))),
            PickTarget::ProgramFile(i) => {
                let _ = self.update(Message::OpenWithGame(open_with_game::Msg::Path(i, text)));
            }
            PickTarget::AccountGameFolder | PickTarget::AccountDatFolder => {}
        }
        Effect::none()
    }

    fn select_path(&mut self, installer: bool) {
        if self.preview || self.maintenance.is_some() || self.reshade.busy {
            return;
        }
        if installer { self.installer_error = None; }
        let raw = if installer {
            &self.settings.installer_path
        } else {
            &self.settings.install_path
        };
        let path = match selected_folder(raw, !installer) {
            Ok(path) => path,
            Err(error) => {
                if installer { self.installer_error = Some(error.clone()); }
                self.status = error;
                return;
            }
        };
        // Installer archives have their own validated names and staging files. Their
        // folder may contain (or be inside) the selected game installation.
        let mut settings = self.settings.clone();
        if installer {
            settings.installer_path = path.to_string_lossy().into_owned();
            settings.installer_selected = true;
        } else {
            settings.install_path = path.to_string_lossy().into_owned();
            settings.install_selected = true;
        }
        match save_settings(&self.config, &settings) {
            Ok(()) => {
                self.settings = settings;
                self.status = if installer {
                    tf("Selected installer download folder: {path}", &[("path", &path.display())])
                } else {
                    tf("Selected game installation folder: {path}", &[("path", &path.display())])
                };
                if !installer {
                    self.installation_generation = self.installation_generation.wrapping_add(1);
                    self.installation_loading = false;
                    self.patch = None;
                    self.patch_folder = None;
                    self.maintenance_result =
                        t("Game folder selected. Play checks its version before FINAL FANTASY XI starts.")
                            .into();
                    self.maintenance_tone = ResultTone::Unverified;
                }
            }
            Err(error) => {
                if installer { self.installer_error = Some(error.clone()); }
                self.status = error;
            }
        }
    }

    fn start_maintenance(&mut self, kind: MaintenanceKind) -> Effect<Message> {
        self.start_maintenance_scoped(kind, false)
    }

    /// `viewer_only`: a Play's PlayOnline update, PlayOnline's alone (`run_combined(Some(viewer), None, ..)`): the
    /// game's own update follows that Play in its session. Passed, never stored, so an Update
    /// refused here cannot leave a later Downloads Update PlayOnline-only.
    fn start_maintenance_scoped(&mut self, kind: MaintenanceKind, viewer_only: bool) -> Effect<Message> {
        if self.preview || self.maintenance.is_some() {
            return Effect::none();
        }
        let game_operation = matches!(
            kind,
            MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair
        );
        if game_operation && self.installation_loading { return Effect::none(); }
        // A Play-driven update holds its folder's updater lock and its accounts' sessions.
        if kind.blocks_launch() && !self.play_updates.jobs.is_empty() {
            self.status = t("Wait for the FINAL FANTASY XI update to finish.").into();
            if !game_operation { self.installer_error = Some(self.status.clone()); }
            return Effect::none();
        }
        let account_folder = self.update_account_folder(kind);
        if !game_operation {
            // Pressing an installer step selects its current folder. The old
            // extra confirmation left all three buttons disabled on first use.
            self.settings.installer_selected = false;
            self.select_path(true);
            if !self.settings.installer_selected { return Effect::none(); }
        } else if !self.settings.install_selected && account_folder.is_none() {
            self.status =
                t("Select the target folder explicitly before starting this operation.").into();
            return Effect::none();
        }
        let idle_sessions = if matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) {
            self.update_sessions()
        } else { Some(Vec::new()) };
        if kind.blocks_launch() && (idle_sessions.is_none()
            || !matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) && self.has_sessions()) {
            self.status = t("Close all games and sign out PlayOnline sessions before updating, repairing, or running official setup. Existing sessions were kept.").into();
            if !game_operation { self.installer_error = Some(self.status.clone()); }
            return Effect::none();
        }
        if kind.blocks_launch() && !self.vault_ready {
            self.status = t("Wait for saved accounts and running sessions to be checked before changing game files.").into();
            if !game_operation { self.installer_error = Some(self.status.clone()); }
            return Effect::none();
        }
        // A parked pol.exe has polcore.dll loaded: close every park before files change (07d §1.5),
        // off the interface thread, as the operation's first step.
        let parks = if kind.blocks_launch() { self.parks.drain(None) } else { Vec::new() };
        // The Plays refused for this update start again when it succeeds (those released from a
        // wait too, whoever started it).
        if kind == MaintenanceKind::Update {
            self.reissue_after_update = std::mem::take(&mut self.refused_plays);
            // The row's "Sign out and update" asked for this Update.
            self.play_updates.viewer_confirm = None;
        }
        let install = account_folder.unwrap_or_else(|| PathBuf::from(&self.settings.install_path));
        let dir = PathBuf::from(&self.settings.installer_path);
        let op = operation::Operation::default();
        // The update or repair reads and writes the game folder: its I/O, not ours.
        startup_prefetch::cancel();
        if matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) { op.begin_update(); }
        let idle_sessions = idle_sessions.unwrap_or_default();
        if !idle_sessions.is_empty() {
            op.progress(crate::operation::Stage::SignOut, 0, idle_sessions.len() as u64,
                t("Signing out idle PlayOnline sessions before updating"));
        }
        // Check and Repair cover both components; an Update what needs it.
        let (update_viewer, update_game) = if kind != MaintenanceKind::Update { (true, true) }
            else if viewer_only { (true, false) } else { self.update_scope() };
        self.update_scope = (update_viewer, update_game);
        self.active_operation = Some(op.clone());
        self.maintenance = Some(kind);
        self.maintenance_cancelled = false;
        self.maintenance_progress = op.snapshot();
        self.maintenance_result.clear();
        self.maintenance_tone = ResultTone::Done;
        self.status = self.maintenance_label().into();
        if game_operation && update_viewer { self.viewer_generation += 1; self.viewer_loading = true; }
        let viewer_generation = self.viewer_generation;
        let previous_viewer_version = self.viewer.as_ref().map(|v| v.version.remote_version.clone())
            .filter(|version| patch::valid_viewer_version(version));
        let sender = self.sender.clone();
        let (official_path, preview) = (official_versions::path_for(&self.config), self.preview);
        let backup_config = self.config.clone();
        Effect::perform(
            async move {
                if let Err(error) = close_parks_now(parks).await {
                    return (Err(error), None);
                }
                if !idle_sessions.is_empty() {
                    if let Err(error) = session_worker::sign_out_for_update(idle_sessions, &op).await {
                        return (Err(error), None);
                    }
                }
                let result = match kind {
                MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair => {
                    let mode = match kind {
                        MaintenanceKind::Check => updater::Mode::Check,
                        MaintenanceKind::Update => updater::Mode::Update,
                        _ => updater::Mode::Repair,
                    };
                    // An Update asked the official server for its version; the interface shows
                    // that answer instead of sending a request of its own. Check is offline.
                    let folder = install.clone();
                    // USER data first, before any file changes (features::backups).
                    if mode != updater::Mode::Check && update_game && !preview {
                        let (config, install) = (backup_config.clone(), install.clone());
                        let kind = if mode == updater::Mode::Update { features::backups::Maintenance::Update } else { features::backups::Maintenance::Repair };
                        let _ = tokio::task::spawn_blocking(move || features::runtime::backup_before_maintenance_logged(&config, &install, kind)).await;
                    }
                    let pol_folder = if update_viewer { patch::install_folder(updater::Product::Viewer) } else { None };
                    let result = match (pol_folder.clone(), update_viewer) {
                        (None, true) => Err(updater::CombinedFailure::from(t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").to_owned())),
                        (pol, _) => updater::run_combined(pol, update_game.then_some(install), mode, op).await,
                    };
                    // Refresh POL even after a partial failure: its verified
                    // transaction may have completed before the game stopped.
                    if let Some(pol) = pol_folder {
                        let remote = if mode == updater::Mode::Check { previous_viewer_version } else { match &result {
                            Ok(report) => report.viewer.as_ref().map(|viewer| viewer.version.clone()),
                            Err(error) => error.viewer_version.clone().or(previous_viewer_version) } };
                        let _ = tokio::task::spawn_blocking(move || {
                            // An Update or Repair asked the server (Check sent nothing): remember it.
                            if let Some(version) = remote.as_deref().filter(|_| mode != updater::Mode::Check && !preview) {
                                official_versions::learn_playonline_at(&official_path, version, official_versions::now());
                            }
                            let _ = sender.send(WorkerEvent::Viewer(viewer_generation, viewer::after_check(pol, remote.as_deref())));
                        }).await;
                    } else if update_viewer {
                        let _ = sender.send(WorkerEvent::Viewer(viewer_generation, viewer::unavailable(
                            t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").into())));
                    }
                    return match result {
                        Ok(report) => {
                            let summary = report.summary();
                            let shown = match report.game {
                                Some(game) => tokio::task::spawn_blocking(move || patch::report_after_operation(&folder, &game, mode)).await.ok(),
                                None => None,
                            };
                            (Ok(summary), shown)
                        }
                        Err(error) => (Err(error.message), None),
                    };
                }
                MaintenanceKind::Download => installer::download(dir, op).await,
                MaintenanceKind::Extract | MaintenanceKind::Setup => {
                    tokio::task::spawn_blocking(move || {
                        op.check_cancelled()?;
                        // No stage: the page names the step from `kind`.
                        op.progress(crate::operation::Stage::None, 0, 0, dir.display().to_string());
                        if kind == MaintenanceKind::Extract {
                            installer::extract(&dir)?;
                            Ok(t("Extractor finished. Choose Run setup. If you extracted elsewhere, browse to that FFXIFullSetup_US folder first.").into())
                        } else {
                            installer::install(&dir)?;
                            Ok(t("Setup closed. Crystal will look for the installed FFXI folder. PlayOnline Viewer installs only when selected in the wizard. Then press Update to install the official updates.").into())
                        }
                    }).await.unwrap_or_else(|_| Err(t("Installer worker could not complete.").into()))
                }
                };
                (result, None)
            },
            move |(result, shown)| Message::MaintenanceFinished(kind, result, shown),
        )
    }

    /// Starts reading the news feeds in the UI language showing now, unless a
    /// read for that language is already running. A read for the other language
    /// (the user just switched) is left to finish unheard: its updates carry its
    /// language and are dropped.
    fn start_news(&mut self) -> Effect<Message> {
        let lang = i18n::current();
        if self.news_loading && self.news_lang == lang {
            return Effect::none();
        }
        self.news_loading = true;
        self.news_lang = lang;
        self.news_generation = self.news_generation.wrapping_add(1);
        let generation = self.news_generation;
        self.refreshed = Instant::now();
        let sender = self.sender.clone();
        let cache = news_cache_path(&self.config, lang);
        Effect::perform(
            services::refresh_with_progress(lang, Some(cache), move |update| {
                let _ = sender.send(WorkerEvent::Services(generation, lang, update));
            }),
            move |result| Message::ServicesLoaded(generation, lang, result),
        )
    }

    /// How often the official status is read again: each minute while FFXI is down.
    fn status_interval(&self) -> Duration {
        Duration::from_secs(if self.service_down() { 60 } else { 300 })
    }

    fn poll_service_status(&mut self) -> Effect<Message> {
        let interval = self.status_interval();
        if self.preview || self.news_loading || self.refreshed.elapsed() < interval {
            return Effect::none();
        }
        let lang = i18n::current();
        self.news_loading = true;
        self.news_lang = lang;
        self.news_generation = self.news_generation.wrapping_add(1);
        let generation = self.news_generation;
        self.refreshed = Instant::now();
        Effect::perform(services::refresh_status(lang), move |result| Message::ServiceStatusLoaded(generation, lang, result))
    }

    fn refresh(&mut self) -> Effect<Message> {
        if self.preview {
            return Effect::none();
        }
        self.refreshed = Instant::now();
        Effect::batch([self.start_news(), self.read_installed_version()])
    }

    fn find_installation(&mut self, automatic: bool) -> Effect<Message> {
        if self.preview || self.maintenance.is_some() || self.installation_loading {
            return Effect::none();
        }
        self.installation_generation = self.installation_generation.wrapping_add(1);
        let generation = self.installation_generation;
        self.installation_loading = true;
        let preferred = automatic.then(|| PathBuf::from(&self.settings.install_path));
        if !automatic { self.status = t("Finding FINAL FANTASY XI…").into(); }
        Effect::perform(async move {
            tokio::task::spawn_blocking(move || patch::find_game_installation(preferred.as_deref()))
                .await.ok().flatten()
        }, move |folder| Message::InstallationFound(generation, automatic, folder))
    }

    fn read_installed_version(&mut self) -> Effect<Message> {
        if self.preview { return Effect::none(); }
        self.patch_loading = true;
        self.patch_generation += 1;
        let generation = self.patch_generation;
        let install = PathBuf::from(&self.settings.install_path);
        // Reads the installed record only. The server is asked when Play is pressed.
        Effect::perform(
            async move {
                tokio::task::spawn_blocking(move || patch::local_report(&install))
                    .await
                    .unwrap_or_else(|e| patch::PatchReport {
                        state: patch::PatchState::Unknown,
                        local_version: t("Unavailable").into(),
                        remote_version: t("Not checked").into(),
                        detail: tf("Could not read local patch metadata: {error}", &[("error", &e)]),
                        files_verified: None,
                    })
            },
            move |r| Message::PatchLoaded(generation, r),
        )
    }

    fn cancel_account_drag(&mut self) {
        let drag = std::mem::take(&mut self.account_drag);
        if drag.source.is_some() {
            account_order::apply(&mut self.accounts, &drag.original, |row| row.key.as_str());
        }
    }

    fn finish_account_drag(&mut self) {
        let drag = std::mem::take(&mut self.account_drag);
        if drag.source.is_none() { return; }
        let order: Vec<_> = self.accounts.iter().map(|row| row.key.clone()).collect();
        if order == drag.original { return; }
        let mut settings = self.settings.clone();
        settings.account_order = order;
        if !self.preview {
            if let Err(error) = save_settings(&self.config, &settings) {
                account_order::apply(&mut self.accounts, &drag.original, |row| row.key.as_str());
                self.status = tf("Account order could not be saved: {error}", &[("error", &error)]);
                return;
            }
        }
        self.settings = settings;
        self.status = t("Account order saved.").into();
        self.features_order_changed();
    }

    fn replace_accounts(&mut self, values: Vec<Account>) {
        self.cancel_account_drag();
        let mut old: HashMap<String, AccountRow> = std::mem::take(&mut self.accounts)
            .into_iter()
            .map(|r| (r.key.clone(), r))
            .collect();
        let mut seen = BTreeSet::new();
        for mut account in values {
            let key = account_key(&account);
            if let Some(names) = self.settings.account_pol_handles.get(&key) {
                account.pol_handle_names = names.clone();
            }
            if !seen.insert(key.clone()) {
                continue;
            }
            let mut row = if let Some(mut row) = old.remove(&key) {
                // A changed (or removed) authenticator key is tried afresh.
                if row.account.totp_key != account.totp_key { row.authenticator_refused = false; }
                row.account = account;
                row
            } else {
                AccountRow {
                    selected: self
                        .settings
                        .selected_accounts
                        .as_ref()
                        .is_none_or(|s| s.contains(&key)),
                    key,
                    status: if account.mode == AccountMode::Retail { t("Ready to launch") } else { t("Private-server client temporarily unavailable") }.into(),
                    account,
                    otp: String::new(),
                    busy: false,
                    signing_out: false,
                    shutting_down: false,
                    preparing_session: false,
                    sign_in_done: None,
                    queued_play: None,
                    batch_pending: false,
                    batch_requested: false,
                    launch_generation: 0,
                    note: None,
                    note_on: String::new(),
                    pid: None,
                    host_created: None,
                    alive: None,
                    game_running: None,
                    log: None,
                    outcome: None,
                    problem: false,
                    timing: String::new(),
                    play_timing: None,
                    exit: None,
                    launch_trigger: None,
                    network_wait: None,
                    otp_spent: false,
                    otp_unknown: false,
                    presign_login: false,
                    authenticator_refused: false,
                    game_seen: false,
                    resign_declined: false,
                    signed_out: false,
                    resign: None,
                }
            };
            if !row.batch_requested {
                row.batch_pending = session_worker::batch_pending(&self.config, &row.account);
            }
            let reconnect = if row.pid.is_none() && !row.busy && !row.shutting_down {
                session_worker::find(&self.config, &row.account)
            } else {
                None
            };
            let key = row.key.clone();
            if let Some(session) = reconnect {
                row.busy = true;
                row.preparing_session = session.preparing_social();
                row.sign_in_done = None;
                // A code spent in an earlier window is still spent (no sign-in after its game).
                // An older worker's record does not say; its "no code" teaches nothing.
                let used = session.otp_used();
                (row.otp_spent, row.otp_unknown) = (used.unwrap_or(false), used.is_none());
                // Whatever that window started it as, this one did not sign it in.
                row.presign_login = false;
                // A reconnect is no Play: nothing an earlier batch left classifies its end.
                row.launch_trigger = None;
                row.launch_generation = row.launch_generation.wrapping_add(1);
                let generation = row.launch_generation;
                row.status = t("Reconnecting to the background session…").into();
                let tx = LaunchTx::new(self.sender.clone(), key.clone(), generation);
                let spawned = std::thread::Builder::new().name("session-reconnect".into()).spawn(move || {
                    let result = session.reconnect(|message| tx.progress(message));
                    tx.complete(result);
                });
                if spawned.is_err() {
                    row.busy = false;
                    row.status = t("Could not reconnect to the background session.").into();
                }
            }
            self.accounts.push(row);
        }
        account_order::apply(&mut self.accounts, &self.settings.account_order, |row| row.key.as_str());
        self.reset_missing_friends_account();
    }

    fn save_accounts(&mut self, values: Vec<Account>) -> Effect<Message> {
        if !self.vault_ready || self.vault_saving || self.preview {
            return Effect::none();
        }
        self.vault_saving = true;
        self.status = t("Saving account…").into();
        let config = self.config.clone();
        Effect::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    backend::save_accounts(&config, &values)?;
                    Ok(values)
                })
                .await
                .unwrap_or_else(|_| Err(t("Could not save accounts.").into()))
            },
            Message::AccountsSaved,
        )
    }

    /// A Play clicked now.
    fn play(&mut self, keys: Vec<String>) {
        self.start_launch(keys, false, Instant::now(), None);
    }

    /// A sign-in only (Connect, a new account's "Sign in now") clicked now.
    fn connect(&mut self, keys: Vec<String>) {
        self.start_launch(keys, true, Instant::now(), None);
    }

    /// Local Viewer readiness for the UI. Stock sends no W2U (:54000) without a Connect, so
    /// launcher start and setup never query the server; each sign-in asks once (07c §3).
    fn start_viewer_check(&mut self) -> Effect<Message> {
        if self.preview || self.viewer_loading { return Effect::none(); }
        self.viewer_loading = true;
        self.viewer_generation += 1;
        let generation = self.viewer_generation;
        Effect::perform(async move {
            tokio::task::spawn_blocking(viewer::local_status).await
                .unwrap_or_else(|e| viewer::unavailable(e.to_string()))
        }, move |report| Message::ViewerLoaded(generation, report))
    }

    fn set_viewer_status(&mut self, generation: u64, report: viewer::Status) {
        if generation == self.viewer_generation { self.viewer_loading = false; self.viewer = Some(report); }
        self.reload_official();
    }

    fn coordinated_keys(&self, keys: &[String]) -> Vec<String> {
        let mut seen = BTreeSet::new();
        keys.iter().filter(|key| seen.insert((*key).clone())).filter(|key| {
            self.accounts.iter().any(|row| row.key == **key && row.can_play()
                && row.account.mode == AccountMode::Retail && self.settings.launch_plan(&row.account).is_some())
        }).cloned().collect()
    }

    fn batch_sessions(&self, keys: Vec<String>) -> Vec<(String, Option<session_worker::BackgroundSession>)> {
        keys.into_iter().map(|key| {
            let session = self.accounts.iter().find(|row| row.key == key).and_then(|row| {
                let config = self.settings.config_for(&self.config, &row.account);
                session_worker::find(&config, &row.account)
            });
            (key, session)
        }).collect()
    }

    fn start_together(&mut self, keys: Vec<(String, Option<session_worker::BackgroundSession>)>, clicked: Instant, trigger: launch_timing::LaunchTrigger) {
        self.prefetch_startup_files(&keys.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>());
        let mut jobs = Vec::new();
        let mut generations = HashMap::new();
        for (key, session) in keys {
            let Some(row) = self.accounts.iter_mut().find(|row| row.key == key
                && (row.can_play() || row.queued_signin_play() && row.queued_play == Some(clicked))) else { continue; };
            let Some(mut via) = self.launch_tools.launch_plan(&self.settings, &row.account) else { continue; };
            via.trigger = trigger;
            row.launch_trigger = Some(trigger);
            row.network_wait = None;
            let config = self.settings.config_for(&self.config, &row.account);
            // The pre-sign-in already used the row's code unless a new one was typed since (or an
            // authenticator key makes a fresh one for any second sign-in).
            let presign_otp_spent = row.otp_spent && row.otp.is_empty() && !row.authenticator_ready();
            row.otp_spent = presign_otp_spent || !row.otp.is_empty();
            // An existing session keeps what is known of its own sign-in (a pre-sign-in's refusal
            // still arrives here); a new one is this Play's own sign-in.
            let job = match session {
                Some(session) => session_worker::BatchAccount::existing(key.clone(), config, row.account.clone(),
                    std::mem::take(&mut row.otp), via, &session).with_otp_spent(presign_otp_spent),
                None if row.busy || row.pid.is_some() => {
                    row.queued_play = None;
                    row.problem = true;
                    row.status = t("The background session ended before Play could start. Sign in again.").into();
                    continue;
                }
                None => {
                    (row.otp_unknown, row.presign_login) = (false, false);
                    session_worker::BatchAccount::new(key.clone(), config, row.account.clone(), std::mem::take(&mut row.otp), via)
                }
            };
            jobs.push(job);
            row.signed_out = false;
            row.game_seen = false;
            row.resign_declined = false;
            row.launch_generation = row.launch_generation.wrapping_add(1);
            generations.insert(key, row.launch_generation);
            row.batch_pending = true;
            row.batch_requested = true;
            row.busy = true;
            row.preparing_session = false;
            row.sign_in_done = None;
            row.queued_play = None;
            row.problem = false;
            row.exit = None;
            row.timing.clear();
            row.play_timing = Some(launch_timing::Timeline::begin_at(clicked));
            row.status = t("Preparing accounts to start FFXI together…").into();
        }
        let count = jobs.len();
        if count == 0 { return; }
        // A batch signs its accounts in itself (cold): their parked hosts close (07d §1.5).
        let batch_keys: Vec<String> = jobs.iter().map(|job| job.key.clone()).collect();
        self.close_parks_for(&batch_keys);
        // The batch owner is another headless Crystal process. The interface only
        // receives its progress; its lifetime does not control the game handoff.
        let sender = self.sender.clone();
        let worker_generations = generations.clone();
        let spawned = std::thread::Builder::new().name("xi-launch-together".into()).spawn(move || {
            let mut completed = BTreeSet::new();
            let result = session_worker::launch_together(jobs, clicked, |key, message| {
                if let Some(generation) = worker_generations.get(key) {
                    let _ = sender.send(WorkerEvent::Progress(key.to_owned(), message.to_owned()).for_launch(key, *generation));
                }
            }, |key, result| {
                completed.insert(key.to_owned());
                if let Some(generation) = worker_generations.get(key) {
                    let _ = sender.send(WorkerEvent::Complete(key.to_owned(), result).for_launch(key, *generation));
                }
            });
            if let Err(error) = result {
                for (key, generation) in worker_generations {
                    if !completed.contains(&key) {
                        let _ = sender.send(WorkerEvent::Complete(key.clone(), Err(error.clone())).for_launch(&key, generation));
                    }
                }
            }
        });
        if spawned.is_err() {
            for key in generations.keys() {
                if let Some(row) = self.accounts.iter_mut().find(|row| &row.key == key) {
                    row.batch_pending = false;
                    row.batch_requested = false;
                    row.busy = false;
                    row.play_timing = None;
                    row.problem = true;
                    row.status = t("Could not start the coordinated launch worker.").into();
                }
            }
            self.status = t("Could not start the coordinated launch worker.").into();
        } else {
            self.status = tf("Preparing {count} accounts in parallel; FFXI will start when every account is ready.", &[("count", &count)]);
        }
    }

    /// `trigger`: what started this launch, for its log (07d §4.2); `None` names it by the
    /// path it takes (Play, Connect, pre-signed, queued or batch Play).
    /// Every Play path (single, pre-signed, queued behind a pre-sign-in, batch, a reissue)
    /// comes through here and is held until this window's first status read has finished.
    /// A sign-in only (startup, Connect) is never gated: PlayOnline is up during FFXI
    /// maintenance and a stock user can sign in then.
    fn start_launch(&mut self, keys: Vec<String>, social_only: bool, clicked: Instant, trigger: Option<launch_timing::LaunchTrigger>) {
        self.prefetch_startup_files(&keys);
        // A new Play replaces a refused one an Update would have started again.
        if !social_only {
            self.refused_plays.retain(|(key, _)| !keys.contains(key));
            // and the "Sign out and update" that refused Play left on its row.
            if self.play_updates.viewer_confirm.as_ref().is_some_and(|(key, _)| keys.contains(key)) { self.play_updates.viewer_confirm = None; }
        }
        // A Play on a session with a maintenance check out waits for its answer
        // (`start_queued_plays`): one W20 at a time.
        let (held, keys): (Vec<String>, Vec<String>) = keys.into_iter().partition(|key| !social_only && self.patch_watch.in_flight(key));
        for row in self.accounts.iter_mut().filter(|row| held.contains(&row.key)) {
            row.queued_play = Some(clicked);
            row.status = t("Waiting for the update…").into();
        }
        if keys.is_empty() { return; }
        if !social_only && self.hold_for_status(&keys, clicked, trigger) { return; }
        self.start_launch_now(keys, social_only, clicked, trigger, false);
    }

    /// A Play of `key` was refused because PlayOnline needs an update: the Update that Play
    /// starts (`begin_viewer_update`) starts it again when it succeeds.
    fn note_refused_play(&mut self, key: &str, clicked: Instant) {
        self.refused_plays.retain(|(known, _)| known != key);
        self.refused_plays.push((key.to_owned(), clicked));
    }

    /// Whether `key`'s sign-ins take a one-time code, as its last successful one did (or as a
    /// code-less one the login refused suggests). Kept in Settings (only this yes or no, never a
    /// code), so a later window knows it before any sign-in there: a "Wait and play" released
    /// then asks for a code instead of signing in without one, and only an account known to
    /// need none is signed in by Crystal itself (`Settings::presigns`).
    fn note_otp_account(&mut self, key: &str, uses: bool) {
        if self.settings.note_sign_in(key, uses) && !self.preview {
            // Unsaved, it still holds for this window.
            let _ = save_settings(&self.config, &self.settings);
        }
    }

    /// The game folder `key`'s launches use when it is not the Settings one (the account's
    /// own folder), for the Update its W20 refusal asks for.
    fn account_game_folder(&self, key: &str) -> Option<PathBuf> {
        let row = self.accounts.iter().find(|row| row.key == key)?;
        let folder = self.settings.config_for(&self.config, &row.account).game_install?;
        let plain = |path: &Path| backend::plain_windows_path(path).to_string_lossy().trim_end_matches(['\\', '/']).to_ascii_lowercase();
        let settings = PathBuf::from(&self.settings.install_path);
        (!self.settings.install_selected || plain(&folder) != plain(&settings)).then_some(folder)
    }

    /// The game folder an operation of `kind` changes when it is not the Settings one: an
    /// Update the W20 of an account with its own folder asked for updates that folder.
    fn update_account_folder(&self, kind: MaintenanceKind) -> Option<PathBuf> {
        self.patch_folder.clone().filter(|_| kind == MaintenanceKind::Update && self.update_scope().1)
    }

    /// A W20 verdict for `key`'s game folder becomes the window's FFXI report.
    fn set_patch_from_row(&mut self, key: &str, report: patch::PatchReport) {
        self.patch_folder = self.account_game_folder(key);
        self.show_patch(report);
        self.reload_official();
    }

    /// `report` becomes the window's FFXI report; a read still in flight no longer replaces it.
    fn show_patch(&mut self, report: patch::PatchReport) {
        self.patch_generation += 1;
        self.patch_loading = false;
        self.patch = Some(report);
    }

    /// `status` becomes the window's PlayOnline report; a read still in flight no longer replaces it.
    fn show_viewer(&mut self, status: viewer::Status) {
        self.viewer_generation += 1;
        self.viewer_loading = false;
        self.viewer = Some(status);
    }

    /// The game folder `row`'s Play uses (its own, else the Settings one), when it is one.
    fn play_folder(&self, row: &AccountRow) -> Option<PathBuf> {
        pol_less::install_directory(&self.settings.config_for(&self.config, &row.account)).ok()
    }

    /// `row`'s game folder is being updated for a Play (its own or another account's).
    fn folder_updating(&self, row: &AccountRow) -> bool {
        !self.play_updates.jobs.is_empty()
            && self.play_folder(row).is_some_and(|folder| self.play_updates.for_folder(&folder).is_some())
    }

    /// Every signed-in account of `folder`, for an update's wait before installing.
    fn update_members(&self, folder: &Path) -> Vec<play_update::Member> {
        self.accounts.iter().filter(|row| row.pid.is_some())
            .filter(|row| self.play_folder(row).is_some_and(|own| play_update::same_folder(&own, folder)))
            .map(|row| {
                let config = self.settings.config_for(&self.config, &row.account);
                play_update::Member { key: row.key.clone(), label: row.account.label.clone(), host: row.pid,
                    session: (!self.preview).then(|| session_worker::find(&config, &row.account)).flatten(),
                    playing: row.is_playing() }
            }).collect()
    }

    /// Play is the update (proposal 09 phase 2): the W20 of `key`'s Play said its game folder
    /// needs an update (`report`). Its session stays signed in at the main menu while the
    /// folder updates (one job per folder: another account's refusal joins it), then the game
    /// starts with no second version check. `false` when no in-session update applies (no
    /// session, no official version, a file operation running): the row keeps the refusal.
    /// `batch`: a batch's refusal, which every account of the batch was told; one whose own
    /// folder is already that version starts on its own instead. `probe`: maintenance's version
    /// check asked (patch_watch): the same update runs with no Play waiting on it, so no game
    /// starts after it.
    fn begin_play_update(&mut self, key: &str, report: &patch::PatchReport, clicked: Instant, batch: bool, probe: bool) -> bool {
        if self.preview || self.closing { return false; }
        let Some(row) = self.accounts.iter().find(|row| row.key == key && row.pid.is_some()) else { return false };
        let Some(folder) = self.play_folder(row) else { return false };
        let Some(official) = patch::in_session_update(&folder, report) else {
            // A batch account whose own folder is current was refused for another's update.
            let current = !probe && batch && report.state == patch::PatchState::Required && !row.is_playing()
                && patch::installed_for_request(&folder) == report.remote_version;
            if current { self.start_launch_now(vec![key.to_owned()], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay), false); }
            return current;
        };
        let directory = self.settings.config_for(&self.config, &row.account).account_directory(&row.account);
        // The W20 ran in a session worker or the batch coordinator: its FIN spaces the update's
        // first connection (one in this window noted its own).
        let closed = if self.preview { None } else { patch::take_update_close(&directory) };
        let members = self.update_members(&folder);
        if let Some(job) = self.play_updates.for_folder_mut(&folder) {
            if !probe && !job.has(key) { job.waiters.push(play_update::Waiter { key: key.to_owned(), clicked, fallback: false }); }
            *job.members.lock().unwrap_or_else(|e| e.into_inner()) = members;
            if let Some(at) = closed { updater::note_patch_closed(updater::Product::Ffxi, at); }
        } else {
            // A Downloads operation holds the folder (or PlayOnline's): the refusal stands.
            if self.maintenance.is_some() || self.recovering.is_some() { return false; }
            let job = self.play_updates.begin(folder.clone(), key, official.clone(), clicked, members, probe);
            let (generation, op, shared) = (job.generation, job.op.clone(), job.members.clone());
            // The update reads and writes the game folder: its I/O, not the start-up pre-read's.
            startup_prefetch::cancel();
            #[cfg(test)]
            { let _ = (op, shared); self.play_updates.spawned.push((folder.clone(), official.clone(), closed)); }
            #[cfg(not(test))]
            if let Err(error) = play_update::spawn(generation, folder.clone(), official.clone(), closed, op, shared, self.sender.clone()) {
                self.play_updates.take(generation);
                self.status = error;
                return false;
            }
            #[cfg(test)]
            let _ = generation;
            if probe {
                launch_log::append_history(&self.config.history_path(), "Crystal",
                    &format!("maintenance update: started, official {official}"), None);
            }
        }
        if probe {
            self.status = t("Downloading the FFXI update").into();
            return true;
        }
        if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) {
            row.busy = true;
            row.preparing_session = false;
            row.problem = false;
            row.status = t("Updating FINAL FANTASY XI…").into();
        }
        self.status = tf("{account}: FINAL FANTASY XI needs an update to {version}. Updating it now; you stay signed in.",
            &[("account", &self.accounts.iter().find(|row| row.key == key).map_or("", |row| row.account.label.as_str())), ("version", &official)]);
        true
    }

    /// A Play of `key` while its folder updates for another account: it waits at the main
    /// menu (a cold Play signs in first) and `start_queued_plays` releases it afterwards.
    fn wait_for_folder_update(&mut self, key: &str, clicked: Instant) {
        let cold = self.accounts.iter().any(|row| row.key == key && row.pid.is_none() && !row.busy);
        if cold { self.start_launch_now(vec![key.to_owned()], true, clicked, None, false); }
        if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key && (row.pid.is_some() || row.busy)) {
            row.queued_play = Some(clicked);
            row.problem = false;
            row.status = t("Waiting for the update…").into();
        }
    }

    /// What a Play-driven update's thread reported.
    fn play_update_event(&mut self, generation: u64, event: play_update::Event) {
        use play_update::Event;
        let Some(job) = self.play_updates.get_mut(generation) else { return };
        match event {
            Event::Waiting(on) => job.blocked = Some(on),
            Event::Installing => { job.blocked = None; job.installing = true; }
            Event::SignedOut(key) => {
                match job.waiters.iter_mut().find(|waiter| waiter.key == key) {
                    Some(waiter) => waiter.fallback = true,
                    None => job.signed_out.push(key),
                }
            }
            Event::Finished(result) => {
                if let Some(job) = self.play_updates.take(generation) { self.finish_play_update(job, result); }
            }
        }
    }

    fn finish_play_update(&mut self, job: play_update::Job, result: Result<play_update::Done, String>) {
        // Installed, or cancelled: nothing more until the next flip. Failed (a network drop, a
        // full disk): checked again on the schedule, resuming from the verified downloads.
        if result.is_err() && !job.cancelled { self.patch_watch.checked(&job.folder, false, Instant::now(), patch_watch::jitter()); }
        else { self.patch_watch.updated(&job.folder, result.is_ok()); }
        if job.probe {
            let line = match &result {
                Ok(done) => format!("maintenance update: installed {}: {}", job.official, done.summary),
                Err(error) => format!("maintenance update: stopped: {error}"),
            };
            launch_log::append_history(&self.config.history_path(), "Crystal", &line, None);
        }
        match result {
            Ok(done) => {
                if let Some(report) = done.report {
                    self.patch_folder = self.account_game_folder(&job.owner);
                    self.show_patch(report);
                }
                self.learn_from_update(&job.folder, Some(&job.official), None);
                self.status = if self.patch_watch.installed(&job.folder) { t("Update installed · waiting for servers").into() }
                    else { tf("FINAL FANTASY XI updated: {summary}", &[("summary", &done.summary)]) };
                for key in &job.signed_out {
                    if let Some(row) = self.accounts.iter_mut().find(|row| row.key == *key && !row.busy) {
                        row.status = t("Signed out of PlayOnline so the update could install.").into();
                    }
                    // Held at the main menu again for the worlds' opening (maintenance_sign_ins).
                    self.patch_watch.forget_signed(key);
                }
                for waiter in job.waiters {
                    let Some(row) = self.accounts.iter_mut().find(|row| row.key == waiter.key) else { continue };
                    row.busy = false;
                    let signed_in = row.pid.is_some() && !waiter.fallback;
                    if !signed_in && wants_typed_code(&self.settings, row) && row.otp.is_empty() {
                        // Its host was signed out for the install: a new sign-in needs a new code.
                        row.status = t("FINAL FANTASY XI is updated. Enter a new one-time code, then press Play.").into();
                        row.problem = true;
                        continue;
                    }
                    // Still signed in: the game starts through the signed-in path with no second
                    // W20 (`skip_w20`). Signed out (the fallback): one new sign-in, as a Play.
                    // The skip is this start's only: one that waits, pauses or is refused here
                    // runs its own W20 when it goes ahead later.
                    self.start_launch_now(vec![waiter.key], false, waiter.clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay), signed_in);
                }
            }
            Err(error) => {
                // Cancel, a network drop, or anything else: the verified downloads stay, and the
                // next Play continues from them.
                let paused = t("Update paused. Press Play to continue.");
                for waiter in &job.waiters {
                    if let Some(row) = self.accounts.iter_mut().find(|row| row.key == waiter.key) {
                        row.busy = false;
                        row.problem = !job.cancelled;
                        row.play_timing = None;
                        row.status = paused.into();
                    }
                }
                // Plays that waited for it would only ask for it again: they wait for the user.
                // Not after maintenance's update, which no one asked for: their own W20 runs.
                for row in self.accounts.iter_mut().filter(|row| !job.probe && row.queued_play.is_some() && !job.has(&row.key)) {
                    let folder = pol_less::install_directory(&self.settings.config_for(&self.config, &row.account)).ok();
                    if folder.is_some_and(|folder| play_update::same_folder(&folder, &job.folder)) {
                        row.queued_play = None;
                        row.status = paused.into();
                    }
                }
                self.status = if job.cancelled { paused.into() } else { tf("Update paused: {error}", &[("error", &error)]) };
            }
        }
    }

    /// Cancel pauses a Play-driven update: what was downloaded stays verified for the next Play.
    fn cancel_play_update(&mut self, generation: u64) {
        let Some(job) = self.play_updates.get_mut(generation) else { return };
        job.op.cancel();
        job.cancelled = true;
        self.status = t("Pausing the update…").into();
    }

    /// A Play refused because PlayOnline is out of date: PlayOnline updates now (its own
    /// update, `run_combined(Some(viewer), None, ..)`), then the same Play starts again (`reissue_after_update`).
    /// Idle accounts must be signed out for it (their hosts load PlayOnline's files): the row
    /// asks once. A running game can't be closed for it: the row says so.
    fn begin_viewer_update(&mut self, key: &str) {
        if self.preview || self.maintenance.is_some() || !self.play_updates.jobs.is_empty() { return; }
        let shown = match self.update_sessions() {
            None => Some(t("PlayOnline needs an update. Close FINAL FANTASY XI on every account, then press Play.").to_owned()),
            Some(sessions) if !sessions.is_empty() => {
                self.play_updates.viewer_confirm = Some((key.to_owned(), sessions.len()));
                Some(tf("PlayOnline must sign out {count} accounts to update.", &[("count", &sessions.len())]))
            }
            Some(_) => {
                let task = self.start_maintenance_scoped(MaintenanceKind::Update, true);
                self.play_updates.tasks.push(task);
                (self.maintenance != Some(MaintenanceKind::Update)).then(|| self.status.clone())
            }
        };
        if let (Some(shown), Some(row)) = (shown, self.accounts.iter_mut().find(|row| row.key == key)) {
            row.status = shown;
            row.problem = true;
        }
    }

    /// The game folder the window's FFXI report describes.
    pub(crate) fn shown_game_folder(&self) -> PathBuf {
        self.patch_folder.clone().unwrap_or_else(|| PathBuf::from(&self.settings.install_path))
    }

    /// The official versions real checks have told, read again: the W20 and W2U run on
    /// other threads and in session workers, which write them where `official_versions` says.
    fn reload_official(&mut self) {
        if !self.preview { self.official = official_versions::load(&official_versions::path_for(&self.config)); }
    }

    /// An Update or Repair asked the server for its version (the updater's own request):
    /// remembered like a Play's check. Nothing is sent for this.
    fn learn_from_update(&mut self, folder: &Path, game: Option<&str>, playonline: Option<&str>) {
        if self.preview { return; }
        let path = official_versions::path_for(&self.config);
        let now = official_versions::now();
        if let Some(version) = game { official_versions::learn_game_at(&path, folder, version, now); }
        if let Some(version) = playonline { official_versions::learn_playonline_at(&path, version, now); }
        self.reload_official();
    }

    /// The remembered official version for the FFXI folder on screen.
    pub(crate) fn learned_game(&self) -> Option<&official_versions::Learned> {
        self.official.game(&self.shown_game_folder())
    }

    /// Holds a Play clicked before the first status read (in flight, or not yet started)
    /// instead of gating it on a missing or cached status. Off the interface thread: the read
    /// is async, and `release_held_plays` runs when it lands or at STATUS_HOLD_LIMIT.
    fn hold_for_status(&mut self, incoming: &[String], clicked: Instant, trigger: Option<launch_timing::LaunchTrigger>) -> bool {
        if self.preview || self.status_read_done || self.status_hold_spent { return false; }
        let held: BTreeSet<&String> = self.held_plays.iter().flat_map(|play| &play.keys).collect();
        let keys: Vec<String> = incoming.iter().filter(|key| !held.contains(key)).cloned().collect();
        // Nothing new to hold: a repeat click on held keys stays held; nothing at all goes on to
        // the normal refusal instead of a "checking" line nothing will clear.
        if keys.is_empty() { return incoming.iter().any(|key| held.contains(key)); }
        let checking = t("Checking whether FINAL FANTASY XI is up…");
        let mut rows = Vec::new();
        for row in self.accounts.iter_mut().filter(|row| keys.contains(&row.key)) {
            rows.push((row.key.clone(), std::mem::replace(&mut row.status, checking.into())));
            row.problem = false;
        }
        let before = std::mem::replace(&mut self.status, checking.into());
        let status = if before == checking { self.held_plays.first().map(|play| play.status.clone()).unwrap_or_default() } else { before };
        self.held_plays.push(HeldPlay { keys, clicked, trigger, since: Instant::now(), rows, status });
        true
    }

    /// Held Plays go through the normal gate once the first status read has finished; one held
    /// for STATUS_HOLD_LIMIT proceeds as without a read (Unknown never blocks). An account save
    /// in progress keeps them held (`start_launch_now` would drop them), and an account deleted
    /// meanwhile is left out.
    fn release_held_plays(&mut self, now: Instant) {
        if self.held_plays.is_empty() || self.vault_saving { return; }
        let read = self.status_read_done;
        let (due, waiting): (Vec<HeldPlay>, Vec<HeldPlay>) = std::mem::take(&mut self.held_plays).into_iter()
            .partition(|play| read || now.saturating_duration_since(play.since) >= STATUS_HOLD_LIMIT);
        self.held_plays = waiting;
        if !read && !due.is_empty() { self.status_hold_spent = true; }
        let checking = t("Checking whether FINAL FANTASY XI is up…");
        for play in due {
            // Rows that still say they are checking get back what they said; the launch (or its
            // refusal) replaces that.
            for (key, before) in play.rows {
                if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key && row.status == checking) { row.status = before; }
            }
            if self.status == checking { self.status = play.status; }
            let keys: Vec<String> = play.keys.into_iter().filter(|key| self.accounts.iter().any(|row| row.key == *key)).collect();
            if !keys.is_empty() { self.start_launch_now(keys, false, play.clicked, play.trigger, false); }
        }
    }

    /// PlayOnline's folder and, for a Play, each launching account's game folder that holds
    /// an interrupted update's journal (one metadata read per folder). A sign-in only
    /// (startup, Connect) never touches a game folder, so its journals wait for a Play.
    fn interrupted_updates(&self, keys: &[String], social_only: bool) -> Vec<PathBuf> {
        let interrupted = |folder: &Path| std::fs::symlink_metadata(folder.join(".xi-updater").join("transaction.json")).is_ok();
        let mut folders: Vec<PathBuf> = self.viewer.as_ref().and_then(|status| status.folder.clone())
            .filter(|folder| interrupted(folder)).into_iter().collect();
        if social_only { return folders; }
        for row in self.accounts.iter().filter(|row| keys.contains(&row.key)) {
            if let Some(folder) = self.settings.config_for(&self.config, &row.account).game_install {
                if interrupted(&folder) && !folders.contains(&folder) { folders.push(folder); }
            }
        }
        folders
    }

    /// Finish or undo the interrupted updates off the UI thread, then start the held launch
    /// (`WorkerEvent::InterruptedUpdate`). Parked hosts hold PlayOnline's files, so they close
    /// first when PlayOnline's folder is among `folders` or `close_parks` asks (a retry after
    /// a park kept the liveness check from passing); a game folder alone leaves other
    /// accounts their parks.
    fn finish_interrupted_updates(&mut self, folders: Vec<PathBuf>, held: HeldLaunch, close_parks: bool) {
        let note = t("Finishing an interrupted update…");
        for row in self.accounts.iter_mut().filter(|row| held.keys.contains(&row.key) && !row.busy) {
            row.status = note.into();
            row.problem = false;
        }
        self.status = note.into();
        let pol_folder = self.viewer.as_ref().and_then(|status| status.folder.clone());
        let parks_closed = close_parks || pol_folder.as_ref().is_some_and(|pol| folders.contains(pol));
        self.recovering = Some(parks_closed);
        let parks = if parks_closed { self.parks.drain(None) } else { Vec::new() };
        let sender = self.sender.clone();
        let keys = held.keys.clone();
        let install = (!self.preview && self.settings.install_selected && !self.settings.install_path.trim().is_empty())
            .then(|| PathBuf::from(&self.settings.install_path));
        let spawned = std::thread::Builder::new().name("finish-update".into()).spawn(move || {
            let result = pol_less::park::close_workers_and_wait(parks, pol_less::park::EXIT_WAIT)
                .and_then(|()| folders.iter().try_for_each(|folder| updater::recover_interrupted(folder).map(|_| ())));
            // The window's reports said "interrupted": read them again.
            let (game, viewer) = if result.is_ok() {
                (install.map(|install| patch::local_report(&install)), Some(viewer::local_status()))
            } else { (None, None) };
            let _ = sender.send(WorkerEvent::InterruptedUpdate(Box::new(Recovered { held, folders, parks_closed, result, game, viewer })));
        });
        if spawned.is_err() {
            self.recovering = None;
            self.refuse_interrupted(&keys, t("Could not start the worker that finishes it.").into());
        }
    }

    fn refuse_interrupted(&mut self, keys: &[String], error: String) {
        let shown = tf("Could not finish an interrupted update: {error}", &[("error", &error)]);
        for row in self.accounts.iter_mut().filter(|row| keys.contains(&row.key) && !row.busy) {
            row.status = shown.clone();
            row.problem = true;
            row.queued_play = None;
        }
        self.status = shown;
    }

    fn interrupted_update_finished(&mut self, done: Recovered) {
        let Recovered { held, folders, parks_closed, result, game, viewer } = done;
        self.recovering = None;
        // Only a report that said "interrupted" is replaced: a server verdict stays.
        let interrupted = |state: patch::PatchState| state == patch::PatchState::Interrupted;
        if let Some(report) = game.filter(|_| self.patch_folder.is_none() && self.patch.as_ref().is_none_or(|p| interrupted(p.state))) {
            self.show_patch(report);
        }
        if let Some(status) = viewer.filter(|_| self.viewer.as_ref().is_none_or(|v| interrupted(v.version.state))) {
            self.show_viewer(status);
        }
        match result {
            // Never a loop: a journal that came back is reported, not finished again.
            Ok(()) if !self.interrupted_updates(&held.keys, held.social_only).is_empty() =>
                self.refuse_interrupted(&held.keys, t("Another update started meanwhile.").into()),
            Ok(()) => {
                let keys: Vec<String> = held.keys.into_iter().filter(|key| self.accounts.iter().any(|row| row.key == *key)).collect();
                if !keys.is_empty() { self.start_launch_now(keys, held.social_only, held.clicked, held.trigger, false); }
            }
            // A parked host (an elevated one cannot be inspected) may be what kept the
            // liveness check from passing: once more with the parks closed.
            Err(_) if !parks_closed && !self.parks.is_empty() => self.finish_interrupted_updates(folders, held, true),
            Err(error) => self.refuse_interrupted(&held.keys, error),
        }
    }

    /// This window's first status read has finished, whatever it found.
    fn note_status_read(&mut self) {
        self.status_read_done = true;
        self.release_held_plays(Instant::now());
    }

    /// A Play that finds FFXI down for maintenance (or every world down) waits for it instead of
    /// being refused (owner decision 2026-10-13, "Wait and play"): at a click, at a held Play's
    /// release and at a queued Play's release alike. The wait itself sends nothing but one read
    /// of the play flag a minute (`read_play_flag`); only while that flag's `$patch_enable` is 1
    /// does a waiting account that needs no code sign in, for the version check
    /// (`tend_patch_watch`). An account already waiting is not added again. A
    /// one-time code typed for the Play goes with it (`WaitingPlay::codes`).
    fn wait_for_service(&mut self, keys: &[String]) {
        let headline = self.wait_headline();
        let mut wait = WaitingPlay { keys: Vec::new(), otp: Vec::new(), armed: Instant::now(), codes: Vec::new() };
        for row in self.accounts.iter_mut().filter(|row| keys.contains(&row.key)) {
            row.queued_play = None;
            if !row.can_play() || row.account.mode != AccountMode::Retail || wait.keys.contains(&row.key)
                || self.waiting_plays.iter().any(|waiting| waiting.keys.contains(&row.key)) { continue; }
            if !row.otp.is_empty() {
                wait.codes.push((row.key.clone(), Zeroizing::new(std::mem::take(&mut row.otp))));
                wait.otp.push(row.key.clone());
            }
            if !row.busy {
                row.status = headline.into();
                row.problem = false;
            }
            wait.keys.push(row.key.clone());
        }
        if wait.keys.is_empty() { return; }
        self.waiting_plays.push(wait);
        self.status = headline.into();
    }

    /// Codes typed for waiting Plays are stale OTP_KEEP after the wait began: dropped (zeroed).
    fn expire_wait_codes(&mut self) {
        for wait in self.waiting_plays.iter_mut().filter(|wait| wait.armed.elapsed() > OTP_KEEP) { wait.codes.clear(); }
    }

    /// These accounts no longer wait; codes kept for them are dropped. Once nothing waits, the
    /// status line stops promising a start.
    fn unwait(&mut self, keys: &[String]) {
        for wait in &mut self.waiting_plays {
            wait.keys.retain(|key| !keys.contains(key));
            wait.otp.retain(|key| !keys.contains(key));
            wait.codes.retain(|(key, _)| !keys.contains(key));
        }
        self.waiting_plays.retain(|wait| !wait.keys.is_empty());
        let headlines = [t("FFXI is down for maintenance. Starting as soon as it's back…"), t("FFXI's worlds are down. Starting as soon as they're back…")];
        if self.waiting_plays.is_empty() && headlines.contains(&self.status.as_str()) {
            let note = self.service_note();
            self.status = if self.service_down() && !note.is_empty() { note }
                else { t("Ready. Select accounts or use Launch all. Games continue when you close Crystal.").into() };
        }
    }

    /// What a waiting Play says: maintenance, or every world down.
    pub(crate) fn wait_headline(&self) -> &'static str {
        if self.service_availability() == services::Availability::Outage {
            t("FFXI's worlds are down. Starting as soon as they're back…")
        } else {
            t("FFXI is down for maintenance. Starting as soon as it's back…")
        }
    }

    /// The line under a waiting row's headline: when the play flag was last read, and the
    /// server's own status line, or the official status the gate falls back on.
    pub(crate) fn wait_detail(&self) -> String {
        let Some(checked) = self.play_flag.checked() else { return String::new() };
        let time = checked.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
        let mut parts = vec![tf("Last checked: {time}", &[("time", &time)])];
        // The server's own line, or what the gate goes by (the official status, or an outage
        // the flag does not end).
        match self.play_flag.fresh(chrono::Utc::now()) {
            Some(play_flag::Flag::Down(Some(status))) => parts.push(status.clone()),
            _ => parts.push(self.service_note()),
        }
        parts.retain(|part| !part.is_empty());
        parts.join(" · ")
    }

    /// Cancel drops these accounts' waits; their rows say what an idle row says.
    fn cancel_wait(&mut self, keys: &[String]) {
        let keys: Vec<String> = keys.iter().filter(|key| self.is_waiting(key)).cloned().collect();
        self.unwait(&keys);
        for row in self.accounts.iter_mut().filter(|row| keys.contains(&row.key) && !row.busy) {
            row.problem = false;
            row.status = if row.pid.is_some() { t("Signed in to PlayOnline · FFXI is closed") } else { t("Ready to launch") }.into();
        }
    }

    /// One read of FFXI's play flag when one is due: only while a Play waits, one at a time,
    /// every minute, and at once when the first wait starts or the PC wakes (`play_flag::Schedule`).
    fn read_play_flag(&mut self) -> Effect<Message> {
        let now = Instant::now();
        if self.preview || !self.flag_schedule.due(self.flag_wanted(), now) { return Effect::none(); }
        self.flag_schedule.begin(now);
        Effect::perform(play_flag::read(), Message::PlayFlagLoaded)
    }

    /// The play flag is read while a Play waits or FFXI is in maintenance: the official status
    /// says so, or a fresh flag of 0 does. While the official status still shows maintenance
    /// after a flag of 1, the reads keep that 1 fresh.
    fn flag_wanted(&self) -> bool {
        !self.waiting_plays.is_empty() || self.patch_watch.active()
            || self.service_availability() == services::Availability::Maintenance
            || self.services.as_ref().is_some_and(|snapshot|
                snapshot.recent_status(chrono::Utc::now()).0 == services::Availability::Maintenance)
    }

    /// The update as soon as it is out (patch_watch.rs): while FFXI is in maintenance and the
    /// play flag's `$patch_enable` is 1, accounts Crystal may sign in by itself (and those whose
    /// Play waits, without a code) sign in and wait at the main menu, and each game folder's
    /// version check runs from one of them on the watch's schedule. Nothing outside maintenance,
    /// and only an affirmative end (a fresh flag of 1, or an official "up" that holds) ends it.
    fn tend_patch_watch(&mut self) {
        if self.preview { return; }
        let now = chrono::Utc::now();
        let seen = match (self.play_flag.fresh(now), self.service_availability()) {
            (Some(play_flag::Flag::Up), _) => patch_watch::Seen::Open,
            (_, services::Availability::Maintenance) => patch_watch::Seen::Closed,
            (_, services::Availability::Operational | services::Availability::PartialOutage) => patch_watch::Seen::Up,
            _ => patch_watch::Seen::Unknown,
        };
        let before = self.patch_watch.last_patch();
        self.patch_watch.observe(seen, self.play_flag.patch(now), Instant::now());
        if let (Some(now), true) = (self.patch_watch.last_patch(), self.patch_watch.last_patch() != before) {
            let was = before.map_or("unknown", |was| if was { "1" } else { "0" });
            launch_log::append_history(&self.config.history_path(), "Crystal",
                &format!("play flag: $patch_enable {was} → {}", u8::from(now)), None);
        }
        if !self.patch_watch.ready() { return; }
        self.maintenance_sign_ins();
        self.start_patch_checks();
    }

    /// Maintenance sign-ins, held at the main menu for the version check: once per account per
    /// trigger (the watch's cycle), through the startup sign-in's path (a refused login stops
    /// Crystal signing that account in; no network retry). Never an account that takes a code.
    fn maintenance_sign_ins(&mut self) {
        if !self.vault_ready || self.vault_saving || self.launch_blocked() || self.confirm_close || self.startup_signin_pending { return; }
        let candidates: Vec<String> = self.accounts.iter().filter(|row| {
            if self.patch_watch.signed(&row.key) { return false; }
            // Known to need no code, as Crystal's own sign-ins (`Settings::presigns`), or its
            // codes come from its authenticator key.
            let waits_without_code = self.waiting_plays.iter().any(|wait| wait.keys.contains(&row.key) && !wait.otp.contains(&row.key))
                && row.account.mode == AccountMode::Retail && row.otp.is_empty()
                && (row.authenticator_ready() || !row.otp_spent
                    && self.settings.account_no_otp.contains(&row.key) && !self.settings.account_otp.contains(&row.key));
            (self.presigned_row(row) || waits_without_code)
                && !self.startup_waiting.iter().any(|(key, _)| *key == row.key) && self.startup_row_free(&row.key)
        }).map(|row| row.key.clone()).collect();
        let keys: Vec<String> = candidates.into_iter().filter(|key| self.patch_watch.sign_in_once(key)).collect();
        if keys.is_empty() { return; }
        launch_log::append_history(&self.config.history_path(), "Crystal", &format!("maintenance sign-in: {} accounts", keys.len()), None);
        self.start_launch(keys, true, Instant::now(), Some(launch_timing::LaunchTrigger::MaintenanceSignIn));
    }

    /// The version checks that are due: one per game folder, from an account signed in and idle
    /// at the main menu that plays from it. Not while that folder updates or files change.
    fn start_patch_checks(&mut self) {
        if self.maintenance.is_some() || self.recovering.is_some() || self.launch_blocked() { return; }
        let now = Instant::now();
        let ready: Vec<(String, PathBuf)> = self.accounts.iter()
            .filter(|row| row.account.mode == AccountMode::Retail && account_row::signed_in(row) && row.queued_play.is_none()
                && !self.patch_watch.in_flight(&row.key))
            .filter_map(|row| Some((row.key.clone(), self.play_folder(row)?)))
            .collect();
        for (key, folder) in ready {
            if self.patch_watch.due(&folder, now) && self.play_updates.for_folder(&folder).is_none() {
                self.spawn_patch_check(key, folder);
            }
        }
    }

    /// One version check (W20 only) of `folder` from `key`'s session at its idle main menu.
    fn spawn_patch_check(&mut self, key: String, folder: PathBuf) {
        let Some(row) = self.accounts.iter().find(|row| row.key == key) else { return };
        let (config, account, log) = (self.settings.config_for(&self.config, &row.account), row.account.clone(), row.log.clone());
        self.patch_watch.begin_check(&folder, &key);
        self.status = t("Checking for the FFXI update").into();
        #[cfg(test)]
        { let _ = (config, account, log); self.patch_watch.checks.push((folder, key)); }
        #[cfg(not(test))]
        {
            let sender = self.sender.clone();
            let install = folder.clone();
            // Still ready after the wait for the main menu (maintenance, `$patch_enable` 1).
            let gate = self.patch_watch.gate();
            let thread_key = key.clone();
            let spawned = std::thread::Builder::new().name("xi-patch-check".into()).spawn(move || {
                let key = thread_key;
                let still_ready = || gate.load(std::sync::atomic::Ordering::SeqCst);
                let result = session_worker::check_at_main_menu(&config, &account, still_ready, || patch::check_before_launch_timed(&install))
                    .map(|(check, timing)| {
                        if let Some(log) = &log {
                            launch_log::append_launcher_lines(log, &launch_timing::check_lines("FFXI check (W20) in maintenance, no game start", &timing));
                        }
                        let current = matches!(check, patch::LaunchCheck::Start(_));
                        let (patch::LaunchCheck::Start(report) | patch::LaunchCheck::Update(report) | patch::LaunchCheck::Unverified(report)) = check;
                        (report, current, timing.phases())
                    });
                let _ = sender.send(WorkerEvent::PatchCheck(install, key, result));
            });
            if spawned.is_err() {
                self.patch_watch.answered(&key);
                self.patch_watch.checked(&folder, false, Instant::now(), patch_watch::jitter());
            }
        }
    }

    /// A maintenance version check answered: "update required" starts the update Play's W20
    /// would (no Play waits on it); anything else checks again on the watch's schedule.
    fn patch_checked(&mut self, folder: PathBuf, key: String, result: Result<(patch::PatchReport, bool, Option<String>), String>) {
        if self.preview { return; }
        self.patch_watch.answered(&key);
        let now = Instant::now();
        let checking = self.status == t("Checking for the FFXI update");
        let history = self.config.history_path();
        let mut required = false;
        let updating = match result {
            Err(error) => {
                launch_log::append_history(&history, "Crystal", &format!("maintenance update check (W20): not sent: {error}"), None);
                false
            }
            Ok((report, current, phases)) => {
                let verdict = if current { format!("current, {}", report.remote_version) }
                    else if report.state == patch::PatchState::Required {
                        format!("update required, installed {}, official {}", report.local_version, report.remote_version)
                    } else { format!("no verdict: {}", report.detail) };
                let phases = phases.map(|phases| format!(" ({phases})")).unwrap_or_default();
                launch_log::append_history(&history, "Crystal", &format!("maintenance update check (W20): {verdict}{phases}"), None);
                self.set_patch_from_row(&key, report.clone());
                // An answer that comes once the update may no longer be checked for (maintenance
                // over, `$patch_enable` 0) starts nothing: the next Play's own W20 does.
                required = !current && report.state == patch::PatchState::Required && self.patch_watch.ready();
                required && self.begin_play_update(&key, &report, now, false, true)
            }
        };
        self.patch_watch.checked(&folder, updating, now, patch_watch::jitter());
        if required && !updating {
            // Told to update with no update possible: no more W20s until the next flip.
            launch_log::append_history(&history, "Crystal", "maintenance update: not started; no more checks until $patch_enable flips", None);
            self.patch_watch.updated(&folder, false);
        }
        if checking && !updating { self.status = self.service_note(); }
    }

    /// FFXI is known to be up: a fresh play flag of 1, or without one an official verdict of up.
    /// Unknown (a stale or failed status read, no read since the PC woke) is not.
    fn service_up(&self) -> bool {
        matches!(self.service_availability(), services::Availability::Operational | services::Availability::PartialOutage)
    }

    /// Waiting Plays start once FFXI is known to be back (`service_up`), each group through the
    /// normal start path (`start_launch`: the multi-account gate and stagger, a sign-in where
    /// there is no session). `from_read`: a flag read has just landed. Between reads only a fresh
    /// play flag of 1 releases, so a reading that has just gone stale, or a PC that has just
    /// woken, waits for the read that is due instead of the official status deciding alone. An
    /// account that signs in with a one-time code starts only with a code typed within OTP_KEEP;
    /// otherwise it waits for the user (an older code is stale, and no code would be refused).
    fn release_waiting_plays(&mut self, from_read: bool) {
        if self.waiting_plays.is_empty() || self.preview { return; }
        let flag_up = self.play_flag.fresh(chrono::Utc::now()) == Some(&play_flag::Flag::Up);
        if !(from_read || flag_up) || !self.service_up() { return; }
        let otp_note = t("FFXI is back — enter your one-time password to start");
        for wait in std::mem::take(&mut self.waiting_plays) {
            // Checked for each group: starting one (an interrupted update it finishes first) can
            // pause launches, and the next group then keeps waiting.
            // A session with a maintenance check out waits for its answer: one W20 at a time.
            if !self.vault_ready || self.vault_saving || self.launch_blocked() || wait.keys.iter().any(|key| self.patch_watch.in_flight(key)) {
                self.waiting_plays.push(wait);
                continue;
            }
            let codes_fresh = wait.armed.elapsed() <= OTP_KEEP;
            let mut keys = Vec::new();
            for row in self.accounts.iter_mut().filter(|row| wait.keys.contains(&row.key)) {
                if row.pid.is_some() || row.busy {
                    keys.push(row.key.clone());
                    continue;
                }
                if codes_fresh && row.otp.is_empty() {
                    if let Some((_, code)) = wait.codes.iter().find(|(key, _)| *key == row.key) { row.otp = code.as_str().to_owned(); }
                }
                let otp = wait.otp.contains(&row.key) || row.otp_spent || !row.otp.is_empty()
                    || self.settings.account_otp.contains(&row.key) || row.authenticator_refused;
                if otp && !(codes_fresh && !row.otp.is_empty()) {
                    row.otp.zeroize();
                    // An authenticator key makes a fresh code in place of a stale typed one.
                    if row.authenticator_ready() {
                        keys.push(row.key.clone());
                        continue;
                    }
                    row.status = otp_note.into();
                    row.problem = true;
                    self.status = otp_note.into();
                } else {
                    keys.push(row.key.clone());
                }
            }
            if keys.is_empty() { continue; }
            // An update one of them needs follows its Play in its session (phase 2).
            self.start_launch(keys, false, Instant::now(), None);
        }
    }

    /// `skip_w20`: a signed-in start right after its session installed the update its W20 asked
    /// for (stock checks nothing between that update and GameStart). A start this defers
    /// (a wait, a pause, a held launch) runs its own W20 later.
    fn start_launch_now(&mut self, keys: Vec<String>, social_only: bool, clicked: Instant, trigger: Option<launch_timing::LaunchTrigger>,
        skip_w20: bool) {
        use launch_timing::LaunchTrigger;
        // A Play the downtime gate below sets waiting is not recorded: it waits, as it would.
        #[cfg(test)]
        if self.launches.is_some() && !self.windower_editor_open && (social_only || self.preview || !self.service_down()) {
            // A recorded start skips the W20 as the signed-in start would.
            if skip_w20 { self.w20_skipped.extend(keys.iter().cloned()); }
            if let Some(launches) = &mut self.launches { launches.push((keys, social_only, clicked, trigger)); }
            return;
        }
        if self.launch_blocked() {
            self.status = if self.windower_editor_open {
                t("Close Windower's profile editor before starting another session.")
            } else {
                t("Game launches are paused while files are being changed or official setup is running.")
            }.into();
            return;
        }
        if self.preview || !self.vault_ready || self.vault_saving {
            return;
        }
        if !social_only && self.service_down() {
            self.wait_for_service(&keys);
            return;
        }
        // A Play that goes ahead now replaces a wait for the same accounts (one whose release
        // was held back while launches were paused), so the wait cannot start them a second time.
        if !social_only { self.unwait(&keys); }
        // An interrupted update in a folder this launch uses is finished (or undone) first,
        // instead of refusing the launch; the launch then goes on by itself.
        let interrupted = self.interrupted_updates(&keys, social_only);
        if !interrupted.is_empty() {
            self.finish_interrupted_updates(interrupted, HeldLaunch { keys, social_only, clicked, trigger }, false);
            return;
        }
        // A Play whose game folder is updating for another account waits for that update before
        // its own version check: signed in at the main menu (a cold Play signs in first), then
        // released by `start_queued_plays` once the update is done.
        let keys = if social_only { keys } else {
            let (waiting, keys): (Vec<String>, Vec<String>) = keys.into_iter().partition(|key|
                self.accounts.iter().find(|row| row.key == *key && row.can_play())
                    .and_then(|row| self.play_folder(row)).is_some_and(|folder| self.play_updates.for_folder(&folder).is_some()));
            for key in waiting { self.wait_for_folder_update(&key, clicked); }
            if keys.is_empty() { return; }
            keys
        };
        if !social_only {
            let together = self.coordinated_keys(&keys);
            if together.len() > 1 {
                let sessions = self.batch_sessions(together.clone());
                let unpublished = sessions.iter().any(|(key, session)| session.is_none()
                    && self.accounts.iter().any(|row| row.key == *key && (row.busy || row.pid.is_some())));
                if !unpublished {
                    self.start_together(sessions, clicked, trigger.unwrap_or(LaunchTrigger::BatchPlay));
                    return;
                }
            }
            // A batch (and a Play queued behind a batch-capable pre-sign-in) is released to the
            // game by the separate batch-coordinator process when the sign-in reaches the main
            // menu. That process cannot see this window's status reads, so the gate above (after
            // the first-read hold) is the guard for it; a maintenance that begins between the
            // click and that release is not re-checked. A Play this window queues itself is
            // re-checked at its release (`start_queued_plays`).
            if let [key] = together.as_slice() {
                if self.accounts.iter().any(|row| row.key == *key && row.busy && row.preparing_session) {
                    let sessions = self.batch_sessions(together);
                    if sessions[0].1.as_ref().is_some_and(session_worker::BackgroundSession::supports_batch_start) {
                        self.start_together(sessions, clicked, trigger.unwrap_or(LaunchTrigger::QueuedPlay));
                        return;
                    }
                }
            }
        }
        let count = keys.len();
        if count == 0 {
            self.status = t("Choose an account that is ready to launch.").into();
            return;
        }
        // Launch timing counts from the Play click, not from when each worker thread gets going.
        // Which tools this batch starts, for the status line.
        let mut tools: Vec<&'static str> = Vec::new();
        let mut all_native = true;
        // One worker per account: every sign-in starts now and runs in parallel; only the
        // Ashita/Windower start inside each worker waits its turn (pol_less::TurnGate: the
        // machine-wide windower.lock and ashita.lock in session_worker).
        for key in keys {
            let Some(row) = self
                .accounts
                .iter_mut()
                .find(|r| r.key == key && r.can_play())
            else {
                continue;
            };
            if row.busy {
                if !social_only {
                    row.queued_play = Some(clicked);
                    row.status = t("FFXI will start when sign-in finishes.").into();
                    // The pre-sign-in skips the Viewer pauses still to come (07d §4.3).
                    if !self.preview {
                        let (config, account) = (self.settings.config_for(&self.config, &row.account), row.account.clone());
                        let _ = std::thread::Builder::new().name("play-pending".into())
                            .spawn(move || { let _ = session_worker::note_play_pending(&config, &account); });
                    }
                }
                continue;
            }
            let account = row.account.clone();
            if account.mode != AccountMode::Retail {
                row.status = t("Private-server client temporarily unavailable; saved account kept.").into();
                continue;
            }
            // Signing in again must never turn an existing Friends session into a game launch.
            if social_only && row.pid.is_some() { continue; }
            let config = self.settings.config_for(&self.config, &account);
            if let Some(plan) = self.settings.launch_plan(&account) {
                if let Some(session) = session_worker::find(&config, &account) {
                    if let Err(error) = session.ensure_launch_plan(&plan) {
                        row.status = error;
                        row.problem = true;
                        continue;
                    }
                }
            }
            row.launch_generation = row.launch_generation.wrapping_add(1);
            let generation = row.launch_generation;
            row.signed_out = false;
            row.game_seen = false;
            row.resign_declined = false;
            if row.pid.is_some() {
                // The stock FFXI version check runs immediately before the game starts, here
                // as well as in a Play that signs in: off the interface thread, once the
                // session's Viewer main menu is up and idle (07c §5.3), then the request.
                let install = match pol_less::install_directory(&config) {
                    Ok(install) => install,
                    Err(error) => { row.status = error; row.problem = true; continue; }
                };
                row.busy = true;
                row.problem = false;
                row.timing.clear();
                row.play_timing = Some(launch_timing::Timeline::begin_at(clicked));
                row.status = t("Checking the game version…").into();
                let tx = LaunchTx::new(self.sender.clone(), key.clone(), generation);
                // the signed-in session's loader starts the game; put the wrapper
                // beside it (or take Crystal's copy away) before the start.
                let dgvoodoo = self.settings.dgvoodoo;
                let loader_setting = self.settings.loader_path.clone();
                let host = row.pid.map(|pid| HostId { pid, created: row.host_created });
                // The trigger and W20 lines go to the session's own log (07d §4.2, §5.3;
                // 08 §4 item 6: they were dropped on this thread).
                let log = row.log.clone();
                let trigger = trigger.unwrap_or(LaunchTrigger::PreSignedPlay);
                // This session's update just finished (phase 2): stock starts the game with no
                // second version check, and no patch-host lookup either.
                let spawned = std::thread::Builder::new().name("xi-start-game".into()).spawn(move || {
                    if !skip_w20 { crate::dns::warm(crate::dns::GAME_PATCH_HOST); }
                    if let Some(log) = &log {
                        launch_log::append_launcher_lines(log, &pol_less::launch_clock_lines(&launch_timing::Timeline::begin_at(clicked), trigger));
                    }
                    if let Some(loader) = session_loader(host, &loader_setting) {
                        let _ = dgvoodoo::sync_for_launch(&loader, dgvoodoo);
                    }
                    let mut report = None;
                    let result = session_worker::start_signed_in(&config, &account, &mut |line| tx.progress(line), || {
                        if skip_w20 {
                            if let Some(log) = &log {
                                launch_log::append_launcher_lines(log, &["FFXI check (W20): skipped; this session just installed the update the Play's W20 asked for.".to_owned()]);
                            }
                            return Ok(t("FINAL FANTASY XI was updated in this session; starting it.").into());
                        }
                        let (check, timing) = patch::check_before_launch_timed(&install);
                        if let Some(log) = &log {
                            launch_log::append_launcher_lines(log, &launch_timing::check_lines("FFXI check (W20)", &timing));
                        }
                        report = Some(match &check {
                            patch::LaunchCheck::Start(r) | patch::LaunchCheck::Update(r) | patch::LaunchCheck::Unverified(r) => r.clone(),
                        });
                        patch::gate(check)
                    });
                    tx.send(WorkerEvent::StartGame(tx.key.clone(), report, result));
                });
                if spawned.is_err() {
                    row.busy = false;
                    row.problem = true;
                    row.play_timing = None;
                    row.status = t("Could not start the game version check.").into();
                }
                continue;
            }
            // the account's own launcher, boot config and profile,
            // over the Launch tools defaults. All retail routes use the same session worker.
            let mut via = self.launch_tools.launch_plan(&self.settings, &account);
            let trigger = trigger.unwrap_or(if social_only { LaunchTrigger::Connect } else { LaunchTrigger::Play });
            // Named before a parked host renames it (ParkedConnect).
            let presign_login = social_only && trigger.is_presign();
            // Re-read the host's dependencies at Play: an edit can precede the next periodic
            // retirement. A mismatch closes off-thread and this Play signs in cold.
            let parked = via.as_ref().and_then(|plan| {
                let key = pol_less::park::ParkKey::for_plan(&account, plan);
                match self.parks.take_for_play(&row.key, &key, || pol_less::park::fingerprint(&config, plan)) {
                    Ok(worker) => worker,
                    Err(worker) => { close_parks_later(vec![worker]); None }
                }
            });
            let deliveries = self.parks.deliveries.clone();
            // A sign-in-only launch here is a pre-sign-in (startup or the explicit sign-in): its
            // Viewer pauses cost nothing at Play, so the loader restores them (07c §4, §5.4).
            if let Some(plan) = &mut via { plan.social_only = social_only; plan.viewer_gaps = social_only; plan.trigger = trigger; }
            // Cold if the parked host turns out to be gone: named as the click (07d §1.6).
            let cold = via.clone();
            let trigger = match (&parked, social_only) {
                (Some(_), false) => LaunchTrigger::ParkedPlay,
                (Some(_), true) => LaunchTrigger::ParkedConnect,
                (None, _) => trigger,
            };
            if let Some(plan) = &mut via { plan.trigger = trigger; }
            row.launch_trigger = Some(trigger);
            row.network_wait = None;
            let tool = via.as_ref().map_or(t("the native client"), |plan| plan.tool.name());
            all_native &= via.is_none();
            if !tools.contains(&tool) {
                tools.push(tool);
            }
            let viewer_generation = self.viewer_generation;
            // Wiped wherever the sign-in thread drops it (a gate refusal, a spawn failure).
            let mut otp = Zeroizing::new(std::mem::take(&mut row.otp));
            row.otp_spent = !otp.is_empty();
            (row.otp_unknown, row.presign_login) = (false, presign_login);
            row.busy = true;
            row.preparing_session = social_only;
            row.problem = false;
            row.timing.clear();
            row.play_timing = None;
            row.status = t("Starting sign-in…").into();
            let done = Arc::new(AtomicBool::new(false));
            row.sign_in_done = Some(done.clone());
            let tx = LaunchTx { done: Some(done), ..LaunchTx::new(self.sender.clone(), key.clone(), generation) };
            // installed here first when it isn't yet; the session worker then puts
            // it next to the loader it runs. A failed install never stops the launch.
            let dgvoodoo = self.settings.dgvoodoo && via.is_some();
            let reshade = self.settings.reshade.enabled;
            let result = std::thread::Builder::new()
                .name("xi-account-login".into())
                .spawn(move || {
                    // Local readiness only; the sign-in's own W2U runs at Connect (07c §3).
                    // Only a blocking local state replaces the Viewer row: a passing one has
                    // no server verdict and would erase the updater's.
                    let viewer = viewer::local_status();
                    if let Err(error) = viewer::gate(&viewer) {
                        let _ = tx.tx.send(WorkerEvent::Viewer(viewer_generation, viewer));
                        tx.complete(Err(error));
                        return;
                    }
                    if dgvoodoo {
                        let installing = tx.clone();
                        let installed = dgvoodoo::ensure_installed_blocking(move |line| installing.progress(&line));
                        // ReShade cannot run without it: the launch stops here with one reason,
                        // before any sign-in work.
                        if let (Err(error), true) = (&installed, reshade) {
                            let error = format!("{} {error}", t("dgVoodoo is not installed yet."));
                            tx.complete(Err(error));
                            return;
                        }
                        if let Err(error) = installed {
                            let _ = tx.tx.send(WorkerEvent::Notice(tf(
                                "dgVoodoo could not be installed, so this launch uses the game's own DirectX 8. {error}",
                                &[("error", &error)],
                            )));
                        }
                    }
                    let mut progress = |line: &str| tx.progress(line);
                    let result = match (parked, via) {
                        (Some(worker), Some(plan)) => {
                            // Parked deliveries from this window go ≥ 1.5 s (+ jitter) apart (07a
                            // parity condition 3): N member logins never start within milliseconds.
                            // The slot is taken once the host has parked, where the credentials leave.
                            match worker.play(&config, account.clone(), std::mem::take(&mut *otp), plan, clicked, &deliveries, &mut progress) {
                                Ok(result) => result,
                                Err(otp) => session_worker::launch(&config, account, otp, cold, clicked, &mut progress),
                            }
                        }
                        (_, via) => session_worker::launch(&config, account, std::mem::take(&mut *otp), via, clicked, &mut progress),
                    };
                    tx.complete(result);
                });
            if result.is_err() {
                row.busy = false;
                row.preparing_session = false;
                row.sign_in_done = None;
                // The code never left: none was spent (it was wiped with the thread's closure).
                row.otp_spent = false;
                row.status = t("Could not start the sign-in worker.").into();
            }
        }
        self.status = launch_status(social_only, all_native, count, &tools);
    }

    /// A capable worker can check the version during the rest of sign-in. Older
    /// loaders keep the signed-in path, with the same version and native idle gates.
    /// Each queued Play is released here, by this window, so availability is checked again at
    /// its release: one that a maintenance or outage begun since its click finds leaves its
    /// sign-in and waits for FFXI instead (`wait_for_service`), never left queued on it.
    fn start_queued_plays(&mut self) {
        if self.preview || !self.vault_ready || self.vault_saving || self.launch_blocked() { return; }
        // A Play waiting for its folder's update stays queued until that update is done.
        let preparing: Vec<_> = self.accounts.iter().filter_map(|row| {
            if !row.queued_signin_play() || row.account.mode != AccountMode::Retail
                || self.settings.launch_plan(&row.account).is_none() || self.folder_updating(row) { return None; }
            let config = self.settings.config_for(&self.config, &row.account);
            let session = session_worker::find(&config, &row.account)?;
            if !session.supports_batch_start() { return None; }
            Some((row.key.clone(), row.queued_play?, session))
        }).collect();
        if self.service_down() {
            let keys: Vec<String> = preparing.into_iter().map(|(key, _, _)| key)
                .chain(self.accounts.iter().filter(|row| row.queued_play.is_some() && !row.busy && row.pid.is_some() && !row.is_playing())
                    .map(|row| row.key.clone()))
                .collect();
            if !keys.is_empty() { self.wait_for_service(&keys); }
            return;
        }
        for (key, clicked, session) in preparing { self.start_together(vec![(key, Some(session))], clicked, launch_timing::LaunchTrigger::QueuedPlay); }
        let held: Vec<String> = self.accounts.iter()
            .filter(|row| row.queued_play.is_some() && (self.patch_watch.in_flight(&row.key) || self.folder_updating(row)))
            .map(|row| row.key.clone()).collect();
        let queued: Vec<_> = self.accounts.iter_mut().filter_map(|row| {
            if row.busy || row.pid.is_none() || row.is_playing() || held.contains(&row.key) { return None; }
            row.queued_play.take().map(|clicked| (row.key.clone(), clicked))
        }).collect();
        for (key, clicked) in queued { self.start_launch(vec![key], false, clicked, None); }
    }

    /// Consume the startup action once the vault and the installation are ready. Account
    /// refreshes and client exits never retry it. A row still busy with a previous session
    /// (its host closing, as at 18:59:55 in 08 §1 row 9) keeps its key pending until that
    /// session ends (`take_waiting_startups`); one already signed in and idle needs nothing.
    fn take_startup_signins(&mut self) -> Vec<String> {
        if !self.startup_signin_pending || !self.vault_ready || self.installation_loading || self.windower_editor_open { return Vec::new(); }
        self.startup_signin_pending = false;
        // The parks start PARK_START_DELAY after this, startup sign-ins or not (07d §5.7).
        let now = Instant::now();
        self.startup_issued = Some(now);
        if self.preview || self.vault_saving || self.launch_blocked() { return Vec::new(); }
        let keys: Vec<String> = self.accounts.iter().filter(|row| self.presigned_row(row)
            && !(row.pid.is_some() && !row.busy && !row.is_playing())).map(|row| row.key.clone()).collect();
        let mut start = Vec::new();
        for key in keys {
            if self.startup_row_free(&key) { start.push(key); continue; }
            if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) {
                row.status = t("Startup sign-in waits for the previous session to end.").into();
            }
            self.startup_waiting.push((key, now));
        }
        start
    }

    /// One `launch-history.log` line per window start, at the first account load (07d §4.4),
    /// so measurements can tell launcher restarts apart.
    fn log_launcher_start(&mut self) {
        if self.launcher_start_logged || self.preview { return; }
        self.launcher_start_logged = true;
        let startup = self.accounts.iter().filter(|row| self.presigned_row(row)).count();
        let parks = self.park_keys().len();
        let exe = std::env::current_exe().ok().and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned())).unwrap_or_default();
        launch_log::append_history(&self.config.history_path(), "Crystal", &format!("launcher start: pid {}, {exe}, v{}, startup sign-in {startup}, parks {parks}",
            std::process::id(), env!("CARGO_PKG_VERSION")), None);
        let keys: Vec<String> = self.accounts.iter().map(|row| row.key.clone()).collect();
        self.prefetch_startup_files(&keys);
    }

    /// Warm the file cache with the startup files of these rows' game folders
    /// (`startup_prefetch`): at Crystal's start and at every sign-in or Play. Not while any
    /// game runs (its folder is warm, and its I/O comes first) or files are being changed.
    fn prefetch_startup_files(&self, keys: &[String]) {
        // Not in unit tests: the pre-read is a detached, process-wide thread that appends to
        // the fixture's history log after the test has moved on (and removed its folder).
        if cfg!(test) { return; }
        if self.preview || self.launch_blocked() || self.maintenance.is_some() || self.accounts.iter().any(|row| row.is_playing()) { return; }
        let mut games: Vec<PathBuf> = Vec::new();
        for row in self.accounts.iter().filter(|row| keys.contains(&row.key)) {
            let game = self.settings.config_for(&self.config, &row.account).game_directory();
            if !games.contains(&game) { games.push(game); }
        }
        // One pre-read runs at a time; a second folder waits for the next sign-in.
        if let Some(game) = games.first() {
            let _ = startup_prefetch::start(game, &self.config.startup_prefetch_path(), &self.config.history_path());
        }
    }

    /// The accounts a parked warm host would serve (07d §1.5 "Who"): retail with a tool plan,
    /// no session of any kind, and no pre-sign-in coming (`presign_coming`). That is OTP
    /// accounts, ones not yet seen signing in, every account while the sign-in switch is off,
    /// and a pre-signed one Crystal will not sign in by itself now (a sign-in after its game
    /// refused during a maintenance, a sign-out, a startup sign-in that failed or was skipped):
    /// its next Play starts warm instead of cold. A pre-sign-in that comes later takes the park.
    fn park_keys(&self) -> Vec<(String, pol_less::park::ParkKey, pol_less::Plan)> {
        self.accounts.iter()
            .filter(|row| row.account.mode == AccountMode::Retail && !row.signed_out && !self.presign_coming(row)
                && row.pid.is_none() && !row.busy && row.can_play())
            .filter_map(|row| {
                let plan = self.launch_tools.launch_plan(&self.settings, &row.account)?;
                Some((row.key.clone(), pol_less::park::ParkKey::for_plan(&row.account, &plan), plan))
            })
            .collect()
    }

    /// Crystal will sign this pre-signed row in by itself: the startup sign-ins are still to be
    /// taken, or one waits for a closing session, a sign-in after its game is pending, or either
    /// waits for the network to retry.
    fn presign_coming(&self, row: &AccountRow) -> bool {
        self.presigned_row(row) && (self.startup_signin_pending || row.resign.is_some()
            || self.startup_waiting.iter().any(|(key, _)| *key == row.key)
            || matches!(row.network_wait, Some(NetworkWait::StartupRetry | NetworkWait::AfterGame | NetworkWait::AfterGameRetry)))
    }

    /// The row has no session of any kind: no host, no worker, nothing queued or batched.
    fn startup_row_free(&self, key: &str) -> bool {
        self.accounts.iter().any(|row| row.key == key && row.pid.is_none() && !row.busy && row.can_play()
            && (self.preview || session_worker::find(&self.settings.config_for(&self.config, &row.account), &row.account).is_none()))
    }

    /// Pending startup sign-ins whose previous session has ended (07d §5.5 step 2); one still
    /// waiting after STARTUP_WAIT_LIMIT is dropped with a row status.
    fn take_waiting_startups(&mut self, now: Instant) -> Vec<String> {
        if self.startup_waiting.is_empty() || self.preview || !self.vault_ready || self.vault_saving || self.launch_blocked() { return Vec::new(); }
        let mut start = Vec::new();
        for (key, since) in std::mem::take(&mut self.startup_waiting) {
            // The switch turned off, or the account learned to need a code, meanwhile.
            if !self.presigned(&key) { continue; }
            // A row that was reconnecting to its previous session is now signed in and idle:
            // that session is what the startup sign-in would give; nothing to do or say.
            if self.accounts.iter().any(|row| row.key == key && row.pid.is_some() && !row.busy && !row.is_playing()) { continue; }
            if self.startup_row_free(&key) {
                start.push(key);
            } else if now.saturating_duration_since(since) >= STARTUP_WAIT_LIMIT {
                // A game still running keeps its own status ("continue in game").
                if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key && !(row.pid.is_some() && !row.busy)) {
                    row.status = t("Startup sign-in skipped: a session is still running.").into();
                }
            } else {
                self.startup_waiting.push((key, since));
            }
        }
        start
    }

    /// Start startup sign-ins (or their one network retry) as pre-sign-ins.
    fn start_startup(&mut self, keys: Vec<String>, trigger: launch_timing::LaunchTrigger) {
        if !keys.is_empty() { self.start_launch(keys, true, Instant::now(), Some(trigger)); }
    }

    fn start_ready_startup_signins(&mut self) {
        let keys = self.take_startup_signins();
        self.start_startup(keys, launch_timing::LaunchTrigger::StartupSignIn);
    }

    /// Whether Crystal may sign `key` in by itself now (owner decision 2026-10-13): a retail
    /// account it knows signs in without a one-time code, with the switch on (`Settings::presigns`),
    /// whose row's last sign-in took no code and that has none typed. A code typed on the row is
    /// the user's, for their own Play: Crystal never signs in with a stored or stale one.
    fn presigned(&self, key: &str) -> bool {
        self.accounts.iter().any(|row| row.key == key && self.presigned_row(row))
    }

    /// An account with an authenticator key counts as one that needs no typed code: the switch
    /// alone decides, until PlayOnline refuses one of its codes (then never, until the key is
    /// edited or a sign-in without a typed code works).
    fn presigned_row(&self, row: &AccountRow) -> bool {
        let known = if row.account.totp_key.is_empty() {
            self.settings.presigns(&row.key) && !row.otp_spent
        } else {
            self.settings.auto_sign_in && !row.authenticator_refused
        };
        row.account.mode == AccountMode::Retail && !row.signed_out && known && row.otp.is_empty()
    }

    /// Whether `key` may sign back in after its game by itself (see `Resign`): a pre-signed
    /// account (`presigned`). An OTP account's code is spent; a login without one would be
    /// refused. Never during a known maintenance or outage: the end was the service's, and a
    /// login then is one no user makes (the pre-sign-in path skips start_launch's downtime gate).
    fn resign_allowed(&self, key: &str) -> bool {
        self.resign_eligible(key) && !self.service_down()
    }

    /// `resign_allowed` but for the service's state.
    fn resign_eligible(&self, key: &str) -> bool {
        self.settings.sign_in_after_game && self.presigned(key)
    }

    /// The row of a sign-in after a game that the maintenance or outage refused says so.
    fn note_resign_refused(&mut self, key: &str) {
        let note = tf("No sign-in after the game while the worlds are down. {note}", &[("note", &self.service_note())]);
        if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) { row.set_note(Some(RowNote::ResignRefused(note))); }
    }

    /// A session whose game ran has ended. A pre-signed account signs back in once that
    /// session has fully ended (`take_resigns`). A normal quit or FFXI's usual -14 waits only for
    /// RESIGN_DELAY; a crash or error first has the connectivity probe check the network, so a
    /// connection loss waits for it to return (07d §5.5). A worker that vanished without a report
    /// is left alone: its host may still be running. `host` is the ended session's loader.
    fn note_game_end(&mut self, key: &str, kind: Option<launch_log::ExitKind>, host: Option<HostId>) {
        use launch_log::ExitKind;
        if !self.settings.sign_in_after_game {
            self.startup_waiting.retain(|(waiting, _)| waiting != key);
            if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) {
                row.signed_out = true;
                row.drop_resign();
            }
            return;
        }
        if self.preview || kind == Some(ExitKind::WorkerEnded) { return; }
        if !self.resign_allowed(key) {
            if self.resign_eligible(key) {
                // A startup sign-in waiting for this session would otherwise run on the next
                // Tick: the very sign-in after the game this refuses.
                self.startup_waiting.retain(|(waiting, _)| waiting != key);
                self.note_resign_refused(key);
            }
            return;
        }
        let clean = matches!(kind, Some(ExitKind::Normal | ExitKind::Code { code: -14 }));
        // A startup sign-in still waiting for this session (Crystal opened during the game)
        // becomes this delayed sign-in after the game, not an immediate startup sign-in.
        self.startup_waiting.retain(|(waiting, _)| waiting != key);
        let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) else { return };
        row.resign = Some(Resign { generation: row.launch_generation, ended: Instant::now(), free_since: None, network: !clean, host });
        row.set_note(Some(RowNote::ResignAfterGame));
        if !clean {
            row.network_wait = Some(NetworkWait::AfterGame);
            let generation = row.launch_generation;
            self.spawn_network_probe(key.to_owned(), generation);
        }
    }

    /// Sign-ins after a game that start now: each once its row has been free (no host, no
    /// worker, nothing queued) for RESIGN_DELAY, reusing the startup sign-in's wait for a
    /// closing session (07d §5.5). One the user overtook (a launch since, a sign-out, an edit),
    /// no longer pre-signed (the switch turned off, or the account learned to need a code), or
    /// whose row stayed busy for STARTUP_WAIT_LIMIT is dropped.
    fn take_resigns(&mut self, now: Instant) -> Vec<String> {
        let keys: Vec<String> = self.accounts.iter().filter(|row| row.resign.is_some()).map(|row| row.key.clone()).collect();
        // Only while Crystal stays open: not while it asks to close.
        let held = self.preview || !self.vault_ready || self.vault_saving || self.launch_blocked() || self.confirm_close;
        let mut start = Vec::new();
        let mut refused = Vec::new();
        for key in keys {
            let allowed = self.resign_allowed(&key);
            let down = !allowed && self.resign_eligible(&key);
            let free = !held && self.startup_row_free(&key);
            let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) else { continue };
            let Some(mut resign) = row.resign else { continue };
            let waited = now.saturating_duration_since(resign.ended);
            if down && row.launch_generation == resign.generation {
                row.drop_resign();
                refused.push(key);
                continue;
            }
            // A connection loss waits for the probe (up to NETWORK_PROBE_LIMIT), whose
            // NetworkBack ends that wait either way and restarts `ended`; the slack only covers
            // a probe that never reports.
            if !allowed || row.launch_generation != resign.generation
                || resign.network && waited >= session_worker::NETWORK_PROBE_LIMIT + STARTUP_WAIT_LIMIT
                || !resign.network && resign.free_since.is_none() && waited >= STARTUP_WAIT_LIMIT {
                row.drop_resign();
                continue;
            }
            if resign.network { continue; }
            // Fully ended: the host process exited too, not only its worker record.
            let free = free && !resign.host.is_some_and(host_running);
            resign.free_since = free.then(|| resign.free_since.unwrap_or(now));
            row.resign = Some(resign);
            if resign.free_since.is_some_and(|since| now.saturating_duration_since(since) >= RESIGN_DELAY) {
                row.resign = None;
                start.push(key);
            }
        }
        for key in refused { self.note_resign_refused(&key); }
        start
    }

    /// Start sign-ins after a game (or their one network retry) as pre-sign-ins, together and
    /// so staggered like the startup sign-ins. A Play meanwhile queues behind them (07d §5.4).
    fn start_resigns(&mut self, keys: Vec<String>, trigger: launch_timing::LaunchTrigger) {
        if keys.is_empty() { return; }
        self.start_startup(keys.clone(), trigger);
        for row in self.accounts.iter_mut().filter(|row| row.busy && keys.contains(&row.key)) {
            row.set_note(Some(RowNote::ResignAfterGame));
        }
    }

    /// The parked warm hosts (07d §1.5, §5.7), on each Tick: their reports, then closing
    /// what is no longer wanted or changed, then at most one new park.
    fn tend_parks(&mut self, now: Instant) {
        use pol_less::park::{self, Note};
        if self.preview { return; }
        for (_, row, note) in self.parks.poll() {
            match note {
                Note::Parked(_) => {}
                // An older loader: expected, and not parked again until something changes.
                Note::Failed(why) if why == park::UNSUPPORTED => {}
                Note::Failed(why) => self.park_note(&row, &why),
                Note::Ended => self.park_note(&row, t("The parked game host ended.")),
            }
        }
        // polcore.dll is loaded in a parked pol.exe: no parks while files change (07d §1.5).
        // A read-only installation scan (opening Downloads, Crystal start) only holds new parks
        // back below; closing for it sent a Play right after it down the cold path (live
        // 2026-10-12 23:17:55).
        if self.files_changing() {
            let closing = self.parks.drain(None);
            if !closing.is_empty() {
                launch_log::append_history(&self.config.history_path(), "Crystal", "warm hosts closed: game or Crystal files are changing", None);
            }
            close_parks_later(closing);
            // Re-tended at the Tick rate once the files settle.
            self.parks_settled = false;
            return;
        }
        // The profile editor needs the profile guard the Windower parks hold.
        if self.windower_editor_open { self.close_parks_in_background(Some(pol_less::Tool::Windower.name())); }
        if self.park_tended.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1)) { return; }
        self.park_tended = Some(now);
        // Nor while an interrupted update is being finished: no new host may join it.
        if !self.vault_ready || self.installation_loading || self.startup_signin_pending || self.recovering.is_some() { return; }
        // The startup sign-ins take the Windower turn first (§5.7).
        let Some(issued) = self.startup_issued else { return };
        if now.saturating_duration_since(issued) < park::PARK_START_DELAY { self.parks_settled = false; return; }
        let refresh = self.park_checked.is_none_or(|at| now.saturating_duration_since(at) >= park::FINGERPRINT_EVERY);
        if refresh { self.park_checked = Some(now); }
        let candidates: Vec<_> = self.park_keys().into_iter()
            .filter(|(_, _, plan)| !(self.windower_editor_open && plan.tool == pol_less::Tool::Windower))
            .filter_map(|(row, key, plan)| {
                let account = self.accounts.iter().find(|r| r.key == row)?.account.clone();
                let config = self.settings.config_for(&self.config, &account);
                session_worker::find(&config, &account).is_none().then_some((row, key, plan, account, config))
            })
            .collect();
        let mut wanted = Vec::new();
        for (row, key, plan, account, config) in candidates {
            if refresh || !self.park_prints.contains_key(&key) {
                self.park_prints.insert(key.clone(), park::fingerprint(&config, &plan));
            }
            let fingerprint = self.park_prints[&key].clone();
            wanted.push(park::Wanted { key, row, fingerprint, payload: (config, account, plan) });
        }
        self.park_prints.retain(|key, _| wanted.iter().any(|want| want.key == *key));
        let retired = self.parks.retire(&wanted);
        let mut closing = Vec::new();
        for (key, names, worker) in retired {
            let label = self.accounts.iter().find(|row| row.account.mode == AccountMode::Retail && account_key(&row.account) == key.account)
                .map_or_else(String::new, |row| row.account.label.clone());
            let note = if names.is_empty() { "warm host closed: account no longer idle or eligible".to_owned() } else { format!("re-park: {} changed", names.join(", ")) };
            launch_log::append_history(&self.config.history_path(), &label, &note, None);
            closing.push(worker);
        }
        close_parks_later(closing);
        if let Some(next) = self.parks.next(&wanted, now) {
            let (config, account, plan) = &next.payload;
            let (key, row, fingerprint) = (next.key.clone(), next.row.clone(), next.fingerprint.clone());
            match session_worker::spawn_park(config, account, plan) {
                Ok(worker) => self.parks.insert(key, row, fingerprint, worker),
                Err(error) => self.park_note(&row, &error),
            }
        }
        self.parks_settled = self.parks.settled(&wanted);
    }

    /// A non-blocking row note for a park that failed; Play still works, cold.
    fn park_note(&mut self, row: &str, why: &str) {
        if let Some(row) = self.accounts.iter_mut().find(|r| r.key == row && !r.busy && r.pid.is_none()) {
            row.status = tf("Warm start unavailable: {error}", &[("error", &why)]);
        }
    }

    /// Close parked hosts (`tool`: only that tool's) off the UI thread.
    fn close_parks_in_background(&mut self, tool: Option<&'static str>) {
        close_parks_later(self.parks.drain(tool));
    }

    fn controller_settings_targets(&self) -> Vec<(Config, Account)> {
        self.accounts.iter().filter(|row| row.account.mode == AccountMode::Retail && row.is_playing()
            && row.alive.as_ref().is_none_or(|alive| alive.load(Ordering::Acquire)))
            .map(|row| {
                let config = self.settings.config_for(&self.config, &row.account);
                (config, row.account.without_secrets())
            }).collect()
    }

    fn apply_controller_settings(&self, revision: u64, values: std::collections::BTreeMap<String, String>) -> Effect<Message> {
        let targets: Vec<_> = self.controller_settings_targets().into_iter().map(|(config, account)|
            session_worker::find(&config, &account).ok_or_else(||
                t("The game session ended before controller settings could be sent.").to_owned())).collect();
        if targets.is_empty() {
            return Effect::done(Message::Gamepad(gamepad::Msg::LiveResult(revision,
                Ok(t("Saved. Changes apply when you launch FFXI.").into()))));
        }
        Effect::perform(async move {
            tokio::task::spawn_blocking(move || {
                let count = targets.len();
                let results: Vec<_> = std::thread::scope(|scope| {
                    targets.into_iter().map(|session| {
                        let values = &values;
                        scope.spawn(move || session.and_then(|session|
                            session_worker::request_controller_settings_for_session(&session, values.clone())))
                    }).collect::<Vec<_>>().into_iter().map(|thread| thread.join()
                        .unwrap_or_else(|_| Err(t("Could not apply controller settings.").into()))).collect()
                });
                if let Some(error) = results.iter().find_map(|result| result.as_ref().err()) {
                    let applied = results.iter().filter(|result| result.is_ok()).count();
                    Err(tf("Saved. Applied to {applied} of {count} running games. {error}",
                        &[("applied", &applied), ("count", &count), ("error", error)]))
                } else {
                    Ok(tf("Saved. Applied to running games ({count}).", &[("count", &count)]))
                }
            }).await.unwrap_or_else(|_| Err(t("Saved. Could not confirm changes in running games.").into()))
        }, move |result| Message::Gamepad(gamepad::Msg::LiveResult(revision, result)))
    }

    /// Close every parked host now (exit, file changes): each worker ends Windower first,
    /// then its host; past CLOSE_WAIT it is terminated (07d §1.5 "Kill").
    fn close_parks(&mut self) {
        pol_less::park::close_workers(self.parks.drain(None));
    }

    pub(crate) fn shutdown_count(&self, selected: bool) -> usize {
        self.accounts.iter().filter(|row| (!selected || row.selected) && row.account.mode == AccountMode::Retail
            && row.is_playing() && !row.shutting_down && !row.signing_out && !row.busy).count()
    }

    fn shutdown_games(&mut self, selected: bool) -> Effect<Message> {
        if self.closing { return Effect::none(); }
        let mut tasks = Vec::new();
        for row in &mut self.accounts {
            if (selected && !row.selected) || row.account.mode != AccountMode::Retail
                || !row.is_playing() || row.shutting_down || row.signing_out || row.busy { continue; }
            let key = row.key.clone();
            let generation = row.launch_generation;
            let config = self.settings.config_for(&self.config, &row.account);
            let session = (!self.preview).then(|| session_worker::find(&config, &row.account)).flatten();
            row.game_seen = true;
            if !self.settings.sign_in_after_game {
                row.decline_resign();
                row.signed_out = true;
            }
            row.network_wait = None;
            row.shutting_down = true;
            row.problem = false;
            row.status = t("Shutting down FFXI…").into();
            if self.preview {
                tasks.push(Effect::done(Message::GameShutdownFinished(key, generation, Ok(()))));
            } else {
                tasks.push(Effect::perform(async move {
                    tokio::task::spawn_blocking(move || match session {
                        Some(session) => session_worker::game_shutdown::request(&session),
                        None => Err(t("The selected game session ended before shutdown could be requested.").into()),
                    }).await.unwrap_or_else(|_| Err(t("Could not request game shutdown.").into()))
                }, move |result| Message::GameShutdownFinished(key.clone(), generation, result)));
            }
        }
        Effect::batch(tasks)
    }

    fn sign_out_account(&mut self, key: String) -> Effect<Message> {
        self.startup_waiting.retain(|(waiting, _)| *waiting != key);
        let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) else { return Effect::none() };
        row.decline_resign();
        row.signed_out = true;
        row.network_wait = None;
        if !row.can_sign_out() { return Effect::none(); }
        let generation = row.launch_generation;
        let config = self.settings.config_for(&self.config, &row.account);
        let session = (!self.preview).then(|| session_worker::find(&config, &row.account)).flatten();
        row.busy = true;
        row.signing_out = true;
        row.preparing_session = false;
        row.problem = false;
        row.status = t("Signing out of PlayOnline…").into();
        if self.preview { return Effect::done(Message::AccountSignedOut(key, generation, Ok(()))); }
        Effect::perform(async move {
            tokio::task::spawn_blocking(move || match session {
                Some(session) => session.sign_out(),
                None => Ok(()),
            }).await.unwrap_or_else(|error| Err(error.to_string()))
        }, move |result| Message::AccountSignedOut(key.clone(), generation, result))
    }

    fn begin_close(&mut self) -> Effect<Message> {
        self.confirm_close = false;
        self.closing = true;
        startup_prefetch::cancel();
        self.startup_signin_pending = false;
        self.startup_waiting.clear();
        let mut sessions = Vec::new();
        for row in &mut self.accounts {
            row.decline_resign();
            row.signed_out = true;
            row.network_wait = None;
            if !self.preview {
                let config = self.settings.config_for(&self.config, &row.account);
                if let Some(session) = session_worker::IdleSessionClose::for_account(&config, &row.account, row.busy && row.preparing_session) {
                    sessions.push(match &row.sign_in_done {
                        Some(done) => session.with_sign_in_done(done.clone()),
                        None => session,
                    });
                }
            }
        }
        let parks = self.parks.drain(None);
        let history = self.config.history_path();
        if !self.preview {
            launch_log::append_history(&history, "Crystal", &format!(
                "launcher close: pid {}, idle sessions {}, parks {}", std::process::id(), sessions.len(), parks.len()), None);
        }
        if !self.preview && (!sessions.is_empty() || !parks.is_empty()) {
            let started = launcher_close::start(move || {
                let started = Instant::now();
                pol_less::park::close_workers(parks);
                let result = session_worker::close_idle_sessions(sessions);
                launch_log::append_history(&history, "Crystal", &format!(
                    "launcher close cleanup: pid {}, {} ms, {}", std::process::id(), started.elapsed().as_millis(),
                    if result.is_ok() { "completed" } else { "failed" }), None);
                result
            });
            if let Err(error) = started {
                self.closing = false;
                self.status = tf("Could not start PlayOnline cleanup: {error}", &[("error", &error.to_string())]);
                return Effect::none();
            }
        }
        tray::set_icon(false);
        // Destroy the window and release its renderer now. main() keeps the
        // cleanup owner alive until sign-out and final profile saves finish.
        match self.window.take() {
            Some(id) => Effect::window(id, WindowOp::Close).chain(Effect::Exit),
            None => {
                tray::wake_ui_thread_soon();
                Effect::Exit
            }
        }
    }

    /// Open the window (at start, or back from the background) at its last size.
    fn open_window(&mut self) -> Effect<Message> {
        let id = WindowId::unique();
        self.window = Some(id);
        Effect::window(id, WindowOp::Open { size: self.viewport, maximized: self.maximized })
    }

    /// Close the window but keep everything else running: sessions stay signed in, parks,
    /// waits and timers continue, and the renderer (with its GPU device) is released.
    fn hide_window(&mut self) -> Effect<Message> {
        let Some(id) = self.window.take() else { return Effect::none() };
        self.auto_restart.focused = false;
        self.cancel_account_drag();
        self.title_held = false;
        window_chrome::detach();
        tray::set_icon(true);
        if !self.preview {
            launch_log::append_history(&self.config.history_path(), "Crystal", &format!(
                "background: window closed, pid {}, {}", std::process::id(),
                footprint::memory(None).map_or_else(|| "memory unknown".to_owned(), footprint::Memory::text)), None);
        }
        Effect::batch([Effect::window(id, WindowOp::Close), Self::trim_later(Duration::from_secs(3))])
    }

    /// Back from the background, or forward when the window is open.
    fn show_window(&mut self) -> Effect<Message> {
        tray::set_icon(false);
        match self.window {
            Some(id) => Effect::window(id, WindowOp::Minimize(false)).chain(Effect::window(id, WindowOp::Focus)),
            None => self.open_window(),
        }
    }

    /// Register (`Some(true)`) or remove the Start with Windows task off the UI thread, or
    /// (`None`, at start with the switch on) check it still starts this exe.
    fn startup_task(&self, on: Option<bool>) -> Effect<Message> {
        let scratch = self.config.experiment.join("iced-launcher");
        Effect::perform(async move {
            tokio::task::spawn_blocking(move || {
                let exe = std::env::current_exe().map_err(|_| t("Could not locate Crystal's program file.").to_owned())?;
                let user = startup_task::current_user().ok_or_else(|| t("Could not identify the Windows user.").to_owned())?;
                let mut run = startup_task_runner();
                match on {
                    Some(on) => startup_task::set(on, &exe, &user, &scratch, &mut run),
                    None => startup_task::ensure(&exe, &user, &scratch, &mut run).map(|_| ()),
                }
            }).await.unwrap_or_else(|_| Err(t("Could not change Crystal's startup task.").into()))
        }, move |result| Message::StartupTaskSet(on.unwrap_or(true), result))
    }

    /// The freed renderer's pages go back to Windows once the window is gone.
    fn trim_later(after: Duration) -> Effect<Message> {
        Effect::perform(async move { tokio::time::sleep(after).await }, |_| Message::TrimMemory)
    }

    /// The window's close button: to the notification area when Settings say so, else quit.
    fn request_quit(&mut self) -> Effect<Message> {
        if matches!(self.self_update, self_update::State::Applying) {
            self.status = t("Waiting for the launcher update helper…").into();
            return Effect::none();
        }
        if self.reshade.busy {
            self.page = Page::Settings;
            self.settings_tab = settings_page::Tab::Graphics;
            self.status = t("Finish ReShade setup before closing Crystal.").into();
            return Effect::none();
        }
        if self.prereqs.busy() {
            self.page = Page::Settings;
            self.settings_tab = settings_page::Tab::System;
            self.status = t("Wait for Windows component setup to finish, or cancel its download, before closing.").into();
            return Effect::none();
        }
        if self.maintenance.is_some() {
            self.page = Page::Maintenance;
            self.show_settings = false;
            self.show_details = false;
            self.status = t("Wait for the operation to finish, or request Cancel, before closing the launcher.").into();
            return Effect::none();
        }
        if self.vault_saving {
            self.confirm_close = true;
            Effect::none()
        } else {
            self.begin_close()
        }
    }

    /// The Tick has no timed work: nothing waits to start, retry or expire (`tick_gate`).
    /// Busy states are checked by the caller.
    fn tick_quiet(&self) -> bool {
        self.page != Page::Friends
            && !self.startup_signin_pending && self.startup_waiting.is_empty()
            && self.waiting_plays.is_empty() && self.play_updates.jobs.is_empty()
            && self.recovering.is_none() && !self.closing && self.parks_settled
            && self.accounts.iter().all(|row| row.resign.is_none() && row.network_wait.is_none()
                && row.queued_play.is_none() && !row.shutting_down && !row.signing_out && !row.batch_requested)
    }

    /// When a quiet window's next periodic Tick work is due: the official status read,
    /// (with parked hosts) their fingerprint check, and in maintenance the play flag's read
    /// and the next version check.
    fn next_timed_work(&self) -> Option<Instant> {
        let status = (!self.preview).then(|| self.refreshed + self.status_interval());
        let parks = (!self.parks.is_empty()).then(|| self.park_checked.map_or_else(Instant::now, |at| at + pol_less::park::FINGERPRINT_EVERY));
        let flag = (!self.preview && self.flag_wanted()).then(|| self.flag_schedule.next()).flatten();
        // A downloaded update's restart countdown, postponement or input grace (none otherwise).
        let restart = (!self.preview).then(|| { let now = Instant::now(); self.auto_restart.next_due(self.auto_conditions(now), now) }).flatten();
        status.into_iter().chain(parks).chain(flag).chain(self.patch_watch.next_due()).chain(restart).min()
    }

    /// The session flags whose change a quiet window must see at once: a session or game
    /// that ends (or a game that starts inside a signed-in session).
    fn watched_flags(&self) -> Vec<(Arc<AtomicBool>, bool)> {
        let mut flags = Vec::new();
        for row in &self.accounts {
            for flag in [&row.alive, &row.game_running].into_iter().flatten() {
                flags.push((flag.clone(), flag.load(Ordering::Acquire)));
            }
        }
        flags
    }

    /// Close parked hosts for `keys` off the UI thread (a batch signs them in cold).
    fn close_parks_for(&mut self, keys: &[String]) {
        let mut closing = Vec::new();
        for row in self.accounts.iter().filter(|row| keys.contains(&row.key)) {
            if let Some(plan) = self.settings.launch_plan(&row.account) {
                closing.extend(self.parks.take(&row.key, &pol_less::park::ParkKey::for_plan(&row.account, &plan)));
            }
        }
        close_parks_later(closing);
    }

    /// One connectivity probe for a row whose sign-in failed on the network (07d §5.5):
    /// DNS only, off the UI thread. Unit tests deliver `NetworkBack` themselves.
    fn spawn_network_probe(&self, key: String, generation: u64) {
        if self.preview || cfg!(test) { return; }
        let tx = LaunchTx::new(self.sender.clone(), key, generation);
        let _ = std::thread::Builder::new().name("network-probe".into()).spawn(move || {
            let back = session_worker::wait_for_network(Instant::now() + session_worker::NETWORK_PROBE_LIMIT);
            tx.send(WorkerEvent::NetworkBack(tx.key.clone(), back));
        });
    }

    /// What `account` launches with (its own choice, else the default).
    pub fn launcher_for(&self, account: &Account) -> crate::backend::LaunchWith {
        self.settings.launch_with_for(account)
    }

    /// The accounts Home's Play acts on: the selected ones, else the first.
    pub(crate) fn play_target_keys(&self) -> Vec<String> {
        let selected: Vec<String> = self.accounts.iter().filter(|r| r.selected).map(|r| r.key.clone()).collect();
        if selected.is_empty() { self.accounts.first().map(|r| r.key.clone()).into_iter().collect() } else { selected }
    }

    /// Set these retail accounts' own launcher (Home's Launch with) and save the vault.
    fn set_account_launcher(&mut self, keys: &[String], choice: crate::backend::LaunchWith) -> Effect<Message> {
        let changes = |r: &AccountRow| keys.contains(&r.key) && r.account.mode == AccountMode::Retail && r.account.launch_with != Some(choice);
        if choice == crate::backend::LaunchWith::Native || !self.accounts.iter().any(changes) {
            return Effect::none();
        }
        if self.preview {
            for row in &mut self.accounts {
                if changes(row) { row.account.launch_with = Some(choice); }
            }
            return Effect::none();
        }
        if !self.vault_ready || self.vault_saving {
            self.status = t("Wait for the accounts to finish saving, then choose again.").into();
            return Effect::none();
        }
        let values = self
            .accounts
            .iter()
            .map(|r| {
                let mut account = r.account.clone();
                if changes(r) {
                    account.launch_with = Some(choice);
                }
                account
            })
            .collect();
        self.save_accounts(values)
    }

    /// Open an account's latest log (or its logs folder) in the default app.
    fn open_log(&mut self, key: &str, folder: bool) {
        if self.preview {
            return;
        }
        let Some(row) = self.accounts.iter().find(|r| r.key == key) else {
            return;
        };
        let logs = self.config.logs_directory(&row.account);
        let target = if folder { Some(logs.clone()) } else { row.log.clone() };
        let opened = match target {
            Some(target) => open_log_path(&logs, &target),
            None => Err(t("There is no log for this account yet.").into()),
        };
        if let Err(error) = opened {
            self.status = error;
        }
    }

    fn search_ah(&mut self) -> Effect<Message> {
        let generation = self.ah_lookup.begin(&self.ah_query, self.ah_server);
        if self.preview {
            self.ah_lookup.finish(generation, Ok(ffxiah::Results::default()));
            return Effect::none();
        }
        let query = self.ah_lookup.query.clone();
        let world = self.ah_lookup.world;
        Effect::perform(ffxiah::search(query, world), move |result| Message::AhResults(generation, result))
    }

    fn reset_friends(&mut self, account: Option<String>) {
        self.friends_generation = self.friends_generation.wrapping_add(1);
        self.friends = friends::State { account, ..Default::default() };
        self.friends_expired = None;
    }

    fn reset_missing_friends_account(&mut self) {
        if self.friends.account.as_ref().is_some_and(|key| {
            !self.accounts.iter().any(|row| &row.key == key && row.account.mode == AccountMode::Retail)
        }) {
            self.reset_friends(None);
        }
    }

    fn load_friends(&mut self) {
        self.reset_missing_friends_account();
        if self.preview || self.friends.loading { return; }
        if self.friends.account.is_none() {
            self.friends.account = self.accounts.iter().find(|a| a.pid.is_some() && a.account.mode == AccountMode::Retail)
                .or_else(|| self.accounts.iter().find(|a| a.account.mode == AccountMode::Retail)).map(|a| a.key.clone());
        }
        let (Some((account, config)), Some(key)) = (self.friends_target(), self.friends.account.clone()) else { return };
        let sender = self.sender.clone();
        self.friends.loading = true;
        self.friends_generation = self.friends_generation.wrapping_add(1);
        let generation = self.friends_generation;
        self.friends_checked = Instant::now();
        if std::thread::Builder::new().name("friends-view".into()).spawn(move || {
            let (connected, snapshot) = session_worker::friends_snapshot(&config, &account);
            let _ = sender.send(WorkerEvent::Friends(generation, key, connected, snapshot));
        }).is_err() {
            self.friends.loading = false;
            self.friends.error = t("Could not read the friends session.").into();
        }
    }

    /// Ask for the next message the open conversation needs, if any.
    fn drive_sync(&mut self) -> Effect<Message> {
        if self.preview {
            return Effect::none();
        }
        if let Some(request) = self.friends.next_sync() {
            let task = self.update(Message::FriendsSocial(request));
            if self.friends.pending.is_none() {
                // A momentary collision with the session worker's own pass over the request
                // files: the next poll asks again. Any other refusal stops until the user acts.
                let transient = [t("Another PlayOnline request is being submitted."), t("Wait for the current PlayOnline request to finish.")];
                if transient.contains(&self.friends.error.as_str()) { self.friends.error.clear(); } else { self.friends.mail.stalled = true; }
            }
            return task;
        }
        // Once Sent lists the message just sent, it shows from there.
        if self.friends.pending.is_none() && self.friends.outgoing.as_ref().is_some_and(|o| o.sent) {
            self.friends.outgoing = None;
        }
        Effect::none()
    }

    /// A read of the Friends account's session: the page's state, and the end of the request
    /// in flight when the worker reports it done (or the session is gone).
    fn friends_read(&mut self, connected: bool, snapshot: Option<friends::Snapshot>) {
        self.friends.connected = connected;
        let mut finished = None;
        if let Some((id, _)) = &self.friends.pending {
            let completed = snapshot.as_ref().is_some_and(|s| s.social.id.as_ref() == Some(id) && !s.social.busy);
            if completed || !connected {
                finished = completed.then(|| snapshot.clone()).flatten();
                if !completed {
                    self.friends.mail.reading = None;
                    self.friends.outgoing = None;
                }
                self.friends.pending = None;
            }
        } else if let Some(id) = &self.friends_expired {
            // A request reported late, after its time limit: what it did counts after all.
            let completed = snapshot.as_ref().is_some_and(|s| s.social.id.as_ref() == Some(id) && !s.social.busy);
            if completed || !connected {
                if completed {
                    finished = snapshot.clone();
                    self.friends.mail.stalled = false;
                    if self.friends.error == t("PlayOnline has not reported completion. Check messages or the friend list before repeating this request.") { self.friends.error.clear(); }
                } else {
                    self.friends.outgoing = None;
                }
                self.friends_expired = None;
            }
        }
        if let (Some(key), Some(portrait)) = (&self.friends.account, snapshot.as_ref().and_then(|s| s.own_portrait)) {
            if self.own_portraits.get(key) != Some(&portrait) { self.own_portraits.insert(key.clone(), portrait); }
        }
        self.friends.snapshot = snapshot;
        if let Some(done) = finished {
            self.friends.completed(&done);
        }
        // Off the page a read only follows the request in flight; the next
        // conversation request waits until the page is open again.
        if self.page == Page::Friends {
            let _ = self.drive_sync();
        }
    }

    /// A Friends request the session accepted is now the one in flight.
    fn friends_request_started(&mut self, id: String, reading: Option<String>) {
        self.friends.pending = Some((id, Instant::now()));
        // The expired request can no longer be matched (the session now reports this one): a
        // draft it was sending is dropped, not left marked as sending.
        if self.friends_expired.take().is_some() && self.friends.outgoing.as_ref().is_some_and(|o| !o.sent) {
            self.friends.outgoing = None;
        }
        self.friends.mail.reading = reading;
        self.friends.error.clear();
    }

    /// The request in flight has had no result for FRIENDS_REQUEST_LIMIT. A late window (a Tick
    /// held up, or the page left so nothing read the session) may simply not have looked: read
    /// the session once now, and report a missing completion only if it is still missing.
    fn expire_friends_request(&mut self) {
        if !self.friends.pending.as_ref().is_some_and(|(_, started)| started.elapsed() >= FRIENDS_REQUEST_LIMIT) { return; }
        if !self.preview {
            if let Some((account, config)) = self.friends_target() {
                let (connected, snapshot) = session_worker::friends_snapshot(&config, &account);
                self.friends_read(connected, snapshot);
            }
        }
        // A request the read just started is not the one that expired.
        if self.friends.pending.as_ref().is_some_and(|(_, started)| started.elapsed() >= FRIENDS_REQUEST_LIMIT) {
            // The loader may still finish it (a send retries for minutes): a later read that
            // reports it done is reconciled (`friends_read`), and a send's draft stays matched.
            self.friends_expired = self.friends.pending.take().map(|(id, _)| id);
            self.friends.mail.stalled = true;
            self.friends.mail.reading = None;
            self.friends.error = t("PlayOnline has not reported completion. Check messages or the friend list before repeating this request.").into();
        }
    }

    /// Each Tick: what the session flags say now. Rows whose game started are noted, and rows
    /// whose session ended are cleared; returns the game ends (key, how it ended, its host) and
    /// whether a game was newly seen.
    fn observe_sessions(&mut self) -> (Vec<GameEnd>, bool) {
        let mut game_ends = Vec::new();
        let mut game_newly_seen = false;
        // For `features`: games seen starting, and games ended (key, report, closed by Crystal).
        let (mut started, mut ended) = (Vec::new(), Vec::new());
        for row in &mut self.accounts {
            if !row.batch_requested {
                row.batch_pending = session_worker::batch_pending(&self.config, &row.account);
            }
            if row.pid.is_some() && row.alive.as_ref().is_some_and(|a| a.load(Ordering::Acquire)) {
                // A sign-out or edit before this game (a pre-signed Play since) declines
                // nothing about it.
                if row.is_playing() && !std::mem::replace(&mut row.game_seen, true) {
                    game_newly_seen = true;
                    started.push(row.key.clone());
                    row.resign_declined = false;
                    row.signed_out = false;
                    if !self.preview {
                        startup_prefetch::game_started(&self.settings.config_for(&self.config, &row.account).game_directory(),
                            &self.config.startup_prefetch_path());
                    }
                }
                if row.is_playing() && !row.batch_pending {
                    if let Some(mut timing) = row.play_timing.take() {
                        timing.stage("game start observed by launcher");
                        timing.mark(launch_timing::Milestone::GameStarted);
                        row.timing = timing.row_summary();
                        row.status = t("Client started · continue in game").into();
                        if let Some(log) = &row.log { timing.append_to(&launch_timing::sibling_path(log)); }
                    }
                }
                if row.busy && row.is_playing() && !row.batch_pending { row.busy = false; }
                // A start the version check refused keeps its message on the row.
                if !row.busy && !row.batch_pending && row.queued_play.is_none() && !row.is_playing() && !row.problem { row.status = t("Signed in to PlayOnline · FFXI is closed").into(); }
            }
            if row
                .alive
                .as_ref()
                .is_some_and(|a| !a.load(Ordering::Acquire))
            {
                let host = row.pid.take().map(|pid| HostId { pid, created: row.host_created.take() });
                row.alive = None;
                row.game_running = None;
                // An account whose host was signed out for its update still waits for it.
                row.busy = row.batch_requested || self.play_updates.for_key(&row.key).is_some();
                row.preparing_session = false;
                row.queued_play = None;
                row.timing.clear();
                row.play_timing = None;
                let report = row
                    .outcome
                    .take()
                    .and_then(|slot| slot.lock().ok().and_then(|mut r| r.take()));
                if row.game_seen { ended.push((row.key.clone(), report.clone(), row.shutting_down || row.signing_out)); }
                if row.game_seen && !self.settings.sign_in_after_game { row.signed_out = true; }
                if std::mem::take(&mut row.game_seen) && !std::mem::take(&mut row.resign_declined) {
                    game_ends.push((row.key.clone(), report.as_ref().map(|r| r.kind.clone()), host));
                }
                row.resign_declined = false;
                match report {
                    Some(report) => row.set_exit(report),
                    None => {
                        row.problem = false;
                        row.status = t("Client closed").into();
                    }
                }
            }
        }
        self.features_observed(started, ended);
        (game_ends, game_newly_seen)
    }

    fn worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Viewer(generation, report) => self.set_viewer_status(generation, report),
            WorkerEvent::InterruptedUpdate(done) => self.interrupted_update_finished(*done),
            WorkerEvent::PlayUpdate(generation, event) => self.play_update_event(generation, event),
            WorkerEvent::PatchCheck(folder, key, result) => self.patch_checked(folder, key, result),
            WorkerEvent::Launch(key, generation, event) => {
                if self.accounts.iter().any(|row| row.key == key && row.launch_generation == generation) {
                    self.worker_event(*event);
                }
            }
            WorkerEvent::Friends(generation, key, connected, snapshot) => {
                if generation == self.friends_generation && self.friends.account.as_ref() == Some(&key) {
                    self.friends.loading = false;
                    self.friends_read(connected, snapshot);
                }
            }
            // A language switch can supersede a read even after switching back.
            WorkerEvent::Services(generation, lang, update) => {
                if generation == self.news_generation && lang == i18n::current() {
                    let status = matches!(update, services::Update::Status(_));
                    services::apply(&mut self.services, update);
                    if status { self.note_status_read(); }
                }
            }
            WorkerEvent::StartGame(key, report, result) => {
                // The W20 refused this Play for an update: it runs now, in this signed-in session.
                let update = report.clone().filter(|report| result.is_err() && report.state == patch::PatchState::Required);
                let clicked = self.accounts.iter().find(|r| r.key == key)
                    .and_then(|row| row.play_timing.as_ref().map(launch_timing::Timeline::started)).unwrap_or_else(Instant::now);
                // No report: the session never reached its idle main menu, so W20 never ran.
                if let Some(report) = report { self.set_patch_from_row(&key, report); }
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    match result {
                        Ok(note) => {
                            if let Some(timing) = &mut row.play_timing { timing.mark(launch_timing::Milestone::GameChecked); }
                            row.status = t("Starting FFXI from the signed-in PlayOnline session…").into();
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &note)]);
                        }
                        Err(error) => {
                            row.busy = false;
                            row.problem = true;
                            row.play_timing = None;
                            row.status = error.clone();
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &error)]);
                        }
                    }
                }
                if let Some(report) = update { self.begin_play_update(&key, &report, clicked, false, false); }
            }
            WorkerEvent::Notice(message) => self.status = message,
            WorkerEvent::Features(event) => self.features_event(event),
            WorkerEvent::WindowerProfileProgress(message) => {
                if self.windower_editor_open { self.status = message; }
            }
            WorkerEvent::Progress(key, status) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    row.status = status;
                    row.set_note(if row.queued_play.is_some() {
                        Some(RowNote::QueuedPlay)
                    } else if matches!(row.launch_trigger, Some(launch_timing::LaunchTrigger::SignInAfterGame | launch_timing::LaunchTrigger::SignInAfterGameRetry)) {
                        Some(RowNote::ResignAfterGame)
                    } else { None });
                }
            }
            WorkerEvent::NetworkBack(key, back) => {
                let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) else { return };
                let Some(wait) = row.network_wait.take() else { return };
                // An unclean game end: the sign-in after it now waits for its delay (Tick), or is
                // dropped when the network did not come back.
                if wait == NetworkWait::AfterGame {
                    if let Some(resign) = row.resign.as_mut().filter(|_| back) {
                        resign.network = false;
                        resign.ended = Instant::now();
                        return;
                    }
                    row.drop_resign();
                    if !back && !row.busy && row.pid.is_none() { row.set_note(Some(RowNote::NetworkGone)); }
                    return;
                }
                // No retry is coming after all: the row stops promising one.
                if row.shown_note() == Some(&RowNote::NetworkRetry) && (!back || row.busy || row.pid.is_some() || !row.can_play()) {
                    row.set_note((!back).then_some(RowNote::NetworkGone));
                }
                // The user may have started something else meanwhile; that wins.
                if !back || row.busy || row.pid.is_some() || !row.can_play() { return; }
                match wait {
                    NetworkWait::Reissue(clicked) => self.start_launch(vec![key], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay)),
                    // Unless the switch turned off (or the account learned to need a code) meanwhile.
                    NetworkWait::StartupRetry if self.presigned(&key) => self.start_startup(vec![key], launch_timing::LaunchTrigger::StartupRetry),
                    NetworkWait::StartupRetry => {}
                    NetworkWait::AfterGameRetry if self.resign_allowed(&key) => self.start_resigns(vec![key], launch_timing::LaunchTrigger::SignInAfterGameRetry),
                    NetworkWait::AfterGameRetry if self.resign_eligible(&key) => self.note_resign_refused(&key),
                    NetworkWait::AfterGameRetry | NetworkWait::AfterGame => {}
                }
            }
            WorkerEvent::Complete(key, result) => {
                let mut follow_up = None;
                let mut viewer_outdated = None;
                // A W20 verdict for this row's folder, and a Play an Update should start again.
                let mut game_report = None;
                // A PlayOnline refusal's Play, which its PlayOnline update starts again.
                let mut refused_play = None;
                // A W20 refusal whose update runs in the account's session: (report, click, batch).
                let mut play_update = None;
                // Whether the account's sign-in took a one-time code, learned from a session.
                let mut uses_otp = None;
                // A sign-in that reached a verdict on the account (no network or host failure that
                // is retried): a Play waiting on it waits no longer.
                let mut verdict = false;
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    let batch = row.batch_requested;
                    row.busy = false;
                    row.preparing_session = false;
                    row.batch_requested = false;
                    row.batch_pending = session_worker::batch_pending(&self.config, &row.account);
                    let trigger = row.launch_trigger.take();
                    match result {
                        Ok(result) => {
                            // A reconnect (no trigger) to an older worker's session reads "no code"
                            // from a record that never said: only a sign-in this window started
                            // teaches that an account needs none. Neither does a Play that reused
                            // such a session (a batch, or a Play queued on it): it signed nothing in.
                            uses_otp = (row.otp_spent || trigger.is_some() && !row.otp_unknown).then_some(row.otp_spent);
                            if trigger.is_some() && !row.otp_spent && !row.account.totp_key.is_empty() {
                                // Its own sign-in without a typed code: the authenticator's code
                                // worked. The account does take codes (kept, should the key go).
                                row.authenticator_refused = false;
                                uses_otp = Some(true);
                            }
                            // The signed-in session stays up; the stock check refused the game start.
                            let refused = patch::parse_update_required(&result.detail);
                            if row.pid != Some(result.pid) { row.host_created = process_created(result.pid); }
                            row.pid = Some(result.pid);
                            row.alive = Some(result.alive);
                            row.game_running = Some(result.game_running);
                            // A batch reports each prepared account Ready, then Failed for all
                            // when the batch failed (session_batch::coordinate), such as for an
                            // update its W20 asked for: what started it classifies that failure
                            // too. A game that started needs nothing more.
                            if batch && !row.is_playing() { row.launch_trigger = trigger; }
                            row.log = Some(result.log);
                            row.outcome = Some(result.outcome);
                            row.problem = refused.is_some();
                            row.status = if let Some(report) = refused {
                                // Only a Play runs the W20: its update runs in this session.
                                let clicked = row.play_timing.as_ref().map_or_else(Instant::now, launch_timing::Timeline::started);
                                play_update = Some((report.clone(), clicked, false));
                                game_report = Some(report);
                                result.detail.clone()
                            } else if !row.is_playing() {
                                t("Signed in to PlayOnline · FFXI is closed").into()
                            } else if result.timing.is_empty() {
                                result.detail.clone()
                            } else {
                                t("Client started · continue in game").into()
                            };
                            row.timing = result.timing;
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &result.detail)]);
                        }
                        Err(error) => {
                            // No session came of it, so nothing for a sign-out or edit to decline.
                            row.resign_declined = false;
                            let queued = row.queued_play.take();
                            // The row shows the reason; the log path stays behind View log.
                            let (message, log) = launch_log::split_log(&error);
                            let update = patch::parse_update_required(message);
                            // 07d §5.4-5.5: a sign-in that never reached a verdict on the account
                            // (no network, or its host went away before authentication) starts
                            // once more: a queued Play as a cold Play, a startup sign-in after the
                            // network is back. A refused password, OTP or update never costs a
                            // second login, and neither does a spent one-time code: that second
                            // login would go without one and be refused.
                            let network = update.is_none() && launch_log::network_failure(message);
                            let transient = update.is_none() && launch_log::transient_failure(message)
                                && !session_worker::reached_authentication(&self.settings.config_for(&self.config, &row.account), &row.account);
                            verdict = !network && !transient;
                            // An authenticator key makes a fresh code for the retry.
                            let otp_spent = row.otp_spent && row.otp.is_empty() && !row.authenticator_ready();
                            let wanted = match (queued, trigger) {
                                (Some(clicked), _) if network => Some((NetworkWait::Reissue(clicked), true)),
                                (Some(clicked), _) if transient => Some((NetworkWait::Reissue(clicked), false)),
                                (None, Some(launch_timing::LaunchTrigger::StartupSignIn)) if network => Some((NetworkWait::StartupRetry, true)),
                                // One retry per game end; the retry's own failure ends it.
                                (None, Some(launch_timing::LaunchTrigger::SignInAfterGame)) if network => Some((NetworkWait::AfterGameRetry, true)),
                                _ => None,
                            };
                            match wanted {
                                Some(_) if otp_spent => {}
                                Some((wait, true)) => row.network_wait = Some(wait),
                                Some((wait, false)) => follow_up = Some((wait, false)),
                                None => {}
                            }
                            // The sign-in's W2U said PlayOnline is out of date: so does the
                            // window's PlayOnline status, and the main button becomes Update.
                            viewer_outdated = viewer::outdated_versions(message);
                            let play = queued.is_some() || matches!(trigger, Some(launch_timing::LaunchTrigger::Play
                                | launch_timing::LaunchTrigger::ParkedPlay | launch_timing::LaunchTrigger::PreSignedPlay
                                | launch_timing::LaunchTrigger::QueuedPlay | launch_timing::LaunchTrigger::BatchPlay
                                | launch_timing::LaunchTrigger::ReissuedPlay));
                            // PlayOnline's update starts from this Play and starts it again; the
                            // game's (a batch's W20) runs in the session the batch kept signed in.
                            if play && viewer_outdated.is_some() {
                                refused_play = Some(queued.unwrap_or_else(Instant::now));
                            }
                            if let Some(report) = update.as_ref().filter(|_| play || batch) {
                                let clicked = row.play_timing.as_ref().map_or_else(|| queued.unwrap_or_else(Instant::now), launch_timing::Timeline::started);
                                play_update = Some((report.clone(), clicked, batch));
                            }
                            game_report = update;
                            // Owner decision 2026-10-13: a sign-in Crystal started by itself without
                            // a code that the member login refused may be an account that now uses
                            // one. It counts as one from now on: Crystal no longer signs it in by
                            // itself, and nothing above retries it. Its next sign-in that works
                            // without a code undoes this. Only the row's own refusal counts: a
                            // batch reports another account's failure to every account in it, and
                            // one that got its session (Ready) was not refused.
                            let refused = row.presign_login && !row.otp_spent && row.pid.is_none()
                                && launch_log::sign_in_refused(message);
                            let stops_presigning = refused && self.settings.presigns(&key);
                            if refused { uses_otp = Some(true); }
                            // The row's own member login refused a code its authenticator key made
                            // (no code was typed): the key is likely wrong. Nothing signs it in
                            // again by itself; the row asks for a typed code (`needs_code`).
                            let key_refused = row.authenticator_ready() && !row.otp_spent && row.pid.is_none()
                                && launch_log::sign_in_refused(message);
                            if key_refused { row.authenticator_refused = true; }
                            row.status = if key_refused { t("Authenticator code refused. Check the key.").into() } else { message.to_owned() };
                            row.set_note(if row.network_wait.is_some() {
                                Some(RowNote::NetworkRetry)
                            } else if wanted.is_some() && otp_spent {
                                Some(RowNote::OtpSpent)
                            } else if key_refused {
                                None
                            } else if stops_presigning {
                                Some(RowNote::MayNeedOtp)
                            } else { None });
                            row.problem = true;
                            if log.is_some() {
                                row.log = log;
                            }
                            if let Some(wait) = row.network_wait { follow_up = Some((wait, true)); }
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &error)]);
                        }
                    }
                }
                if let Some(report) = game_report { self.set_patch_from_row(&key, report); }
                // The session worker's W20 and W2U wrote what the server said.
                self.reload_official();
                if let Some(clicked) = refused_play { self.note_refused_play(&key, clicked); }
                if let Some(uses) = uses_otp { self.note_otp_account(&key, uses); }
                // A waiting Play whose sign-in (startup, Connect) was refused keeps the refusal on
                // its row instead of waiting to sign in again into the same verdict: a refused
                // password, account or PlayOnline version never costs a second login.
                if verdict && self.is_waiting(&key) { self.unwait(std::slice::from_ref(&key)); }
                if let Some((local, remote)) = viewer_outdated {
                    let outdated = viewer::outdated(self.viewer.take(), local, remote);
                    self.show_viewer(outdated);
                    if refused_play.is_some() { self.begin_viewer_update(&key); }
                }
                if let Some((report, clicked, batch)) = play_update { self.begin_play_update(&key, &report, clicked, batch, false); }
                match follow_up {
                    Some((_, true)) => {
                        let generation = self.accounts.iter().find(|r| r.key == key).map_or(0, |row| row.launch_generation);
                        self.spawn_network_probe(key, generation);
                    }
                    Some((NetworkWait::Reissue(clicked), false)) => self.start_launch(vec![key], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay)),
                    _ => {}
                }
                self.start_queued_plays();
            }
        }
    }

    pub(crate) fn can_restart_launcher(&self) -> bool {
        !self.closing && self.maintenance.is_none() && self.active_operation.is_none() && !self.prereqs.busy()
            && !self.dgvoodoo.busy() && !self.reshade.busy && self.friends.pending.is_none()
            && self.vault_ready && !self.startup_signin_pending && !self.vault_saving
            && !self.startup_task_busy && !self.windower_editor_open
            && self.held_plays.is_empty()
            // A waiting Play, or the update a Play runs in its session, is pending launch work
            // (neither is saved across a restart).
            && self.waiting_plays.is_empty() && self.play_updates.jobs.is_empty()
            && !self.accounts.iter().any(|a| a.shutting_down || a.signing_out || a.busy || a.batch_pending)
    }

    fn poll_controller(&mut self, capturing: bool) {
        let allowed = self.settings.controller_navigation && window_chrome::is_foreground()
            && !capturing && !self.gamepad.capturing() && !matches!(self.self_update, self_update::State::Applying);
        if self.controller_nav.poll(self.gamepad.controller_pad(), allowed, Instant::now())
            && activity_due(&mut LAST_ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()), Instant::now()) {
            let _ = self.update(Message::UserInput);
        }
    }

    /// A dialog is over the page (the account sheet counts separately: `controller_modal`).
    fn dialog_open(&self) -> bool {
        self.confirm_close || self.pending_delete.is_some() || self.account_import.open
            || self.show_settings || self.show_details || self.ah_lookup.open || self.portrait_picker.is_some()
    }

    pub(crate) fn controller_modal(&self) -> bool {
        self.dialog_open() || self.editor.is_some()
    }

    /// The open account sheet, unless the vault is being saved (edits wait for that).
    fn editor_mut(&mut self) -> Option<&mut Editor> {
        if self.vault_saving { None } else { self.editor.as_mut() }
    }

    /// The Friends page's account and its launch config, as owned values.
    fn friends_target(&self) -> Option<(Account, Config)> {
        let row = self.accounts.iter().find(|row| self.friends.account.as_ref() == Some(&row.key))?;
        Some((row.account.clone(), self.settings.config_for(&self.config, &row.account)))
    }

    /// A command for the launcher window, when there is one.
    fn on_window(&self, command: WindowOp<Message>) -> Effect<Message> {
        self.window.map_or(Effect::none(), |id| Effect::window(id, command))
    }

    /// Save the settings as they are now (never in a preview); a failure shows on the status line.
    fn persist_settings(&mut self) {
        if self.preview { return; }
        if let Err(error) = save_settings(&self.config, &self.settings) { self.status = error; }
    }

    /// Handles `message`, then looks again at the automatic restart for a downloaded update
    /// (`self_update::auto`): every event that can change its answer (a game ending, work
    /// finishing, focus, input, the tray) arrives as a message, so it needs no polling.
    fn update(&mut self, message: Message) -> Effect<Message> {
        let effect = self.handle(message);
        if self.preview { return effect; }
        let auto = self.auto_restart_step(Instant::now());
        Effect::batch([effect, auto])
    }

    /// What the automatic restart sees now.
    fn auto_conditions(&self, now: Instant) -> self_update::auto::Conditions {
        self_update::auto::Conditions {
            enabled: self.settings.launcher_auto_restart,
            ready: matches!(self.self_update, self_update::State::Ready(_)),
            can_restart: self.can_restart_launcher(),
            game_running: self.accounts.iter().any(|row| row.is_playing() || row.game_start_pending()),
            dialog: self.controller_modal(),
            user_active: self.auto_restart.user_active(self.window.is_some(), now),
        }
    }

    /// Starts, stops or finishes the restart countdown; logs each change with the version.
    fn auto_restart_step(&mut self, now: Instant) -> Effect<Message> {
        use self_update::auto::Step;
        let conditions = self.auto_conditions(now);
        let step = self.auto_restart.step(conditions, now);
        let version = match &self.self_update { self_update::State::Ready(stage) => stage.version().to_owned(), _ => String::new() };
        let history = self.config.history_path();
        let log = |line: &str| if !self.preview { launch_log::append_history(&history, "Crystal", line, None); };
        match step {
            Step::Wait => Effect::none(),
            Step::Scheduled => {
                log(&format!("self-update: auto-restart scheduled for {version} in {} s", self_update::auto::COUNTDOWN.as_secs()));
                // Hidden in the notification area: the pill cannot be seen, so a balloon says it.
                if self.window.is_none() && !self.preview {
                    features::notify::balloon(
                        t("Crystal Launcher is restarting to update"),
                        &tf("Crystal Launcher {version} installs in {seconds} s. Open Crystal to postpone it.",
                            &[("version", &version), ("seconds", &self_update::auto::COUNTDOWN.as_secs())]));
                }
                Effect::none()
            }
            Step::Cancelled => {
                log(&format!("self-update: auto-restart for {version} waits (Crystal or a game is busy)"));
                Effect::none()
            }
            Step::Apply => {
                log(&format!("self-update: auto-restart applied {version}"));
                Effect::done(Message::SelfUpdate(self_update::Msg::Apply))
            }
        }
    }

    fn handle(&mut self, message: Message) -> Effect<Message> {
        if self.closing && !matches!(&message,
            Message::WindowResized(_) | Message::WindowMaximized(_)
            | Message::MinimizeWindow | Message::MaximizeWindow | Message::TitlePressed
            | Message::TitleMoved | Message::TitleReleased | Message::ResizeWindow(_)) {
            return Effect::none();
        }
        // The helper is preparing to replace this process. Do not start new
        // writes or work between its readiness check and our exit. Results of work already
        // running still land (they clear busy flags and touch nothing the update replaces), so a
        // helper that then fails leaves nothing stuck.
        if matches!(self.self_update, self_update::State::Applying)
            && !matches!(&message, Message::SelfUpdate(self_update::Msg::Applied(_)) | Message::CloseRequested
                | Message::StartupTaskSet(..) | Message::WindowerProfilesClosed(_) | Message::ServicesLoaded(..)
                | Message::ServiceStatusLoaded(..) | Message::PatchLoaded(..) | Message::ViewerLoaded(..) | Message::AhResults(..))
        {
            return Effect::none();
        }
        match message {
            Message::SelfUpdate(self_update::Msg::AutoCheck(on)) => {
                self.settings.launcher_auto_updates = on;
                self.persist_settings();
                if on && !self.preview { return Effect::done(Message::SelfUpdate(self_update::Msg::Check)); }
            }
            Message::SelfUpdate(self_update::Msg::AutoRestart(on)) => {
                self.settings.launcher_auto_restart = on;
                self.persist_settings();
            }
            Message::SelfUpdate(self_update::Msg::Postpone) => {
                self.auto_restart.postpone(Instant::now());
                if let (false, self_update::State::Ready(stage)) = (self.preview, &self.self_update) {
                    launch_log::append_history(&self.config.history_path(), "Crystal", &format!(
                        "self-update: auto-restart postponed for {} ({} h or the next start)", stage.version(),
                        self_update::auto::POSTPONE.as_secs() / 3600), None);
                }
            }
            // Signed-in idle sessions are separate worker processes: they stay signed in and the
            // new version reconnects to them at start (`session_worker::find`); accounts with no
            // session sign in through the normal startup sign-in. Parked hosts close first.
            Message::SelfUpdate(self_update::Msg::Applied(Ok(()))) => { self.close_parks(); tray::set_icon(false); tray::wake_ui_thread_soon(); return Effect::Exit; }
            Message::SelfUpdate(msg) => {
                if !self.preview {
                    let can_restart = self.can_restart_launcher();
                    let task = self_update::update(&mut self.self_update, msg, &self.config, can_restart).map(Message::SelfUpdate);
                    // The update replaces this exe: no parked host may outlive it (07d §1.5).
                    // They close off the interface thread before the helper starts.
                    if matches!(self.self_update, self_update::State::Applying) {
                        let parks = self.parks.drain(None);
                        return Effect::future(async move {
                            match close_parks_now(parks).await {
                                Ok(()) => task,
                                Err(error) => Effect::done(Message::SelfUpdate(self_update::Msg::Applied(Err(error)))),
                            }
                        });
                    }
                    return task;
                }
            }
            Message::ControllerTick => self.poll_controller(false),
            Message::ControllerNavigation(on) => {
                self.settings.controller_navigation = on;
                self.controller_nav.clear();
                self.persist_settings();
            }
            // Like the per-account choice it replaces: sessions already signed in stay, and a
            // sign-in in flight finishes; a startup sign-in still waiting for a closing session,
            // a sign-in after a game not yet started and a startup network retry are dropped at
            // their next check. Turned on, it signs nothing in now: from the next game's end, or
            // the next start. Parked warm hosts follow it (`park_keys`).
            Message::AutoSignIn(on) => {
                self.settings.auto_sign_in = on;
                self.park_checked = None;
                self.persist_settings();
            }
            Message::SignInAfterGame(on) => {
                self.settings.sign_in_after_game = on;
                if !on { for row in &mut self.accounts { row.drop_resign(); } }
                self.persist_settings();
            }
            Message::ControllerPlay => {
                if !self.controller_modal() { return self.update(Message::MainAction); }
            }
            Message::ControllerPage(step) => {
                if !self.controller_modal() {
                    // The top bar's order; Features (None) is a page over the one underneath.
                    let stops = [Some(Page::Home), Some(Page::Friends), Some(Page::Maintenance), None, Some(Page::Settings)];
                    let current = if self.features_view.open { None } else { Some(self.page) };
                    let index = stops.iter().position(|p| *p == current).unwrap_or(0);
                    self.controller_nav.clear();
                    return match stops[(index as i32 + step as i32).rem_euclid(stops.len() as i32) as usize] {
                        Some(page) => self.update(Message::Navigate(page)),
                        None => self.update(Message::Feature(features::ui::Msg::Open(true))),
                    };
                }
            }
            Message::ControllerBack => {
                if self.account_drag.source.is_some() { self.cancel_account_drag(); return Effect::none(); }
                if self.gamepad.capturing() { return self.update(Message::Gamepad(gamepad::Msg::CancelCapture)); }
                if self.confirm_close { return self.update(Message::CancelClose); }
                if self.pending_delete.is_some() { return self.update(Message::CancelDeleteAccount); }
                if self.portrait_picker.is_some() { return self.update(Message::PortraitPicker(None)); }
                if self.features_view.open { return self.update(Message::Feature(features::ui::Msg::Open(false))); }
                if self.editor.is_some() { return self.update(Message::CloseEditor); }
                if self.account_import.open { return self.import_update(account_import_ui::Msg::Close); }
                if self.show_settings { return self.update(Message::ToggleSettings); }
                if self.show_details { return self.update(Message::ToggleDetails); }
                if self.ah_lookup.open { return self.update(Message::CloseAh); }
                if self.page == Page::Friends && self.friends.remove.is_some() { return self.update(Message::FriendsDraftClose); }
                if self.page == Page::Friends && self.friends.pane.is_some() { return self.update(Message::FriendsSelect(None)); }
                if self.page != Page::Home { return self.update(Message::Navigate(Page::Home)); }
            }
            Message::FriendsAccount(key) => {
                self.reset_friends(Some(key));
                self.load_friends();
            }
            Message::FriendsQuery(query) => self.friends.query = query,
            Message::FriendsSelect(pane) => {
                let opened = self.friends.pane != pane;
                self.friends.select(pane.clone());
                // The Mailbox lists Inbox page 1 when it has nothing current, or, as it opens, when
                // the session last listed another folder or page (a conversation's sync ends on Sent).
                if pane == Some(friends::Pane::Mailbox) && self.friends.pending.is_none() && self.friends.snapshot.as_ref().is_some_and(|s| s.social.ready && !s.social.busy && s.game_running == Some(false)
                    && (s.social.inbox_unread.is_none() || s.social.inbox_changed || (opened && (s.social.folder != 0 || s.social.page != 0)))) {
                    return self.update(Message::FriendsSocial(social::Request::Inbox { folder:0, page:0 }));
                }
                return self.drive_sync();
            }
            Message::FriendsSection(index) => { if let Some(folded) = self.friends.collapsed.get_mut(index) { *folded = !*folded; } }
            Message::FriendsStatusOptions => self.friends.status_options = !self.friends.status_options,
            Message::FriendsAddSubject(open) => { if let Some(draft) = &mut self.friends.composer { draft.show_subject = open; } }
            Message::FriendsRemove(friend) => self.friends.remove = Some(friend),
            Message::FriendsRemoveConfirm => {
                let Some(friend) = self.friends.remove.clone() else { return Effect::none() };
                let Some(uid) = friend.uid else { self.friends.remove = None; return Effect::none() };
                // A request in flight (the bar pauses conversation sync, so it ends soon): the bar
                // stays open to confirm again once it has.
                if !self.preview && self.friends.pending.is_some() {
                    self.friends.error = t("Wait for the current PlayOnline request to finish.").into();
                    return Effect::none();
                }
                self.friends.error.clear();
                let task = self.update(Message::FriendsSocial(social::Request::Remove { alternate: friend.alternate, slot: friend.slot, uid }));
                // The bar closes once the request went out or was refused for good; a momentary
                // collision with the session worker leaves it open to confirm again.
                let collided = [t("Another PlayOnline request is being submitted."), t("Wait for the current PlayOnline request to finish.")]
                    .contains(&self.friends.error.as_str());
                if self.friends.pending.is_some() || !collided { self.friends.remove = None; }
                return task;
            }
            Message::FriendsDraftClose => {
                self.friends.remove = None;
                return self.drive_sync();
            }
            Message::FriendsSubject(subject) => { if let Some(draft) = &mut self.friends.composer { draft.subject = subject.chars().take(512).collect(); } }
            Message::FriendsMessageEdit(text) => { if let Some(draft) = &mut self.friends.composer { draft.text = text; } }
            Message::FriendsSend => {
                let ready = self.friends.connected && self.friends.pending.is_none()
                    && self.friends.snapshot.as_ref().is_some_and(|s| s.game_running == Some(false) && s.social.ready && !s.social.busy);
                if let (true, Some(request)) = (ready, self.friends.send_request()) {
                    let draft = if let social::Request::Send { uid, subject, message, .. } = &request {
                        Some(friends::Outgoing { uid: *uid, subject: subject.clone(), message: message.clone(), sent: false })
                    } else { None };
                    let task = self.update(Message::FriendsSocial(request));
                    // Set once the request went out (after FriendsSocial dropped any earlier draft).
                    if self.friends.pending.is_some() { self.friends.outgoing = draft; }
                    return task;
                }
            }
            Message::FriendsSocial(request) => {
                if !self.preview && self.friends.pending.is_none() {
                    if let Some((account, config)) = self.friends_target() {
                        if matches!(request, social::Request::Check {}) { self.friends.mail.stalled = false; }
                        let reading = if let social::Request::Read { header } = &request { Some(social::message_key(header)) } else { None };
                        match session_worker::request_social(&config, &account, request) {
                            Ok(id) => self.friends_request_started(id, reading),
                            Err(error) => self.friends.error = error,
                        }
                        self.load_friends();
                    }
                }
            }
            Message::FriendsConnect => {
                if let Some(key) = self.friends.account.clone() { self.connect(vec![key]); }
            }
            Message::FriendsPlay => {
                if let Some(key) = self.friends.account.clone() { self.play(vec![key]); }
            }
            Message::FriendsSignOut => {
                if let Some(key) = self.friends.account.clone() { return self.sign_out_account(key); }
            }
            Message::ShutdownSelected => return self.shutdown_games(true),
            Message::ShutdownAll => return self.shutdown_games(false),
            Message::GameShutdownFinished(key, generation, result) => {
                if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key && row.launch_generation == generation) {
                    row.shutting_down = false;
                    if let Err(error) = result {
                        row.problem = true;
                        row.status = error;
                    } else if row.game_seen {
                        row.problem = false;
                        row.status = if self.settings.sign_in_after_game {
                            t("FFXI closed. Returning to PlayOnline…")
                        } else { t("FFXI and PlayOnline closed.") }.into();
                        // The monitor publishes the exit report before `alive = false`.
                        // Let Tick consume both, whether it runs before or after this result,
                        // so automatic sign-in is scheduled once with the actual exit reason.
                        if self.preview {
                            row.alive = Some(Arc::new(AtomicBool::new(false)));
                        }
                    }
                }
            }
            Message::AccountSignOut(key) => return self.sign_out_account(key),
            Message::AccountSignedOut(key, generation, result) => {
                if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key && row.launch_generation == generation) {
                    row.signing_out = false;
                    row.busy = false;
                    row.problem = result.is_err();
                    row.status = result.as_ref().err().cloned().unwrap_or_else(|| t("Signed out of PlayOnline").into());
                    if result.is_ok() {
                        row.pid = None;
                        row.alive = None;
                        row.game_running = None;
                        row.outcome = None;
                        row.exit = None;
                        row.timing.clear();
                    }
                    if self.friends.account.as_ref() == Some(&key) {
                        self.friends.error = result.err().unwrap_or_default();
                        self.load_friends();
                    }
                }
            }
            Message::FriendsStatus(action) => {
                if !self.preview && matches!(action, session_worker::NativeControl::Visible | session_worker::NativeControl::Away | session_worker::NativeControl::Invisible) {
                    if let Some((account, config)) = self.friends_target() {
                        self.friends.error = session_worker::request_native_status(&config, &account, action, !self.friends.skip_groups).err().unwrap_or_default();
                    }
                }
            }
            Message::FriendsStatusGroups(groups) => self.friends.skip_groups = !groups,
            Message::FriendsAutoAway(minutes) => {
                if !self.preview && pol_less::AUTO_AWAY_CHOICES.contains(&minutes) {
                    if let Some((account, _)) = self.friends_target() {
                        self.settings.account_auto_away.insert(account_key(&account), minutes);
                        if let Err(error) = save_settings(&self.config, &self.settings) { self.friends.error = error; }
                    }
                }
            }
            Message::WindowFocus(focused) => self.auto_restart.focused = focused && self.window.is_some(),
            Message::UserInput => {
                self.auto_restart.last_input = Some(Instant::now());
                // Only signed-in sessions whose game is closed; the worker
                // forwards at most one native_activity per note.
                if !self.preview {
                    let idle: Vec<_> = self.accounts.iter()
                        .filter(|r| r.account.mode == AccountMode::Retail && r.pid.is_some() && !r.busy && !r.is_playing())
                        .map(|r| (self.settings.config_for(&self.config, &r.account), r.account.clone()))
                        .collect();
                    if !idle.is_empty() {
                        let _ = std::thread::Builder::new().name("native-activity".into()).spawn(move || {
                            for (config, account) in idle { let _ = session_worker::note_native_activity(&config, &account); }
                        });
                    }
                }
            }
            Message::SettingsTab(tab) => {
                self.settings_tab = tab;
                if tab == settings_page::Tab::Graphics { return reshade::update(self, reshade::Msg::Refresh); }
            }
            // open a Settings tab from elsewhere (the components banner).
            Message::OpenSettingsTab(tab) => {
                self.settings_tab = tab;
                return self.update(Message::Navigate(Page::Settings));
            }
            Message::GameConfig(m) => {
                return game_config::update(&mut self.game_config, m).map(Message::GameConfig)
            }
            Message::Prereqs(m) => return prereqs::update(&mut self.prereqs, m).map(Message::Prereqs),
            Message::Dgvoodoo(m) => return dgvoodoo::update(self, m),
            Message::Reshade(m) => return reshade::update(self, m),
            Message::Gamepad(m) => {
                let captured = self.gamepad.capturing();
                let task = gamepad::update(&mut self.gamepad, m, &self.config).map(Message::Gamepad);
                self.poll_controller(captured);
                if self.gamepad.take_persisted_change() {
                    self.park_checked = None;
                    self.close_parks_in_background(None);
                }
                if let Some((revision, values)) = self.gamepad.take_live_update() {
                    return Effect::batch([task, self.apply_controller_settings(revision, values)]);
                }
                return task;
            },
            Message::LaunchTools(m) => {
                self.park_checked = None;
                if launch_tools::apply(&mut self.settings, m) { self.persist_settings(); }
                return self.launch_tools.refresh(&self.settings);
            }
            Message::LaunchToolsScanned(scan) => return self.launch_tools.scanned(scan),
            Message::OpenWithGame(m) => {
                if open_with_game::apply(&mut self.settings, m) { self.persist_settings(); }
            }
            Message::OpenWithGameDone(report) => {
                for line in open_with_game::log_lines(&report) {
                    launch_log::append_history(&self.config.history_path(), "Crystal", &format!("open with game: {line}"), None);
                }
                if let Some(notice) = open_with_game::notice(&report) { self.status = notice; }
            }
            Message::EditWindowerProfiles => {
                if self.preview ||self.windower_editor_open { return Effect::none(); }
                self.windower_editor_open = true;
                // Idle signed-in owners retain the same profile guard as parks.
                // Let their native sign-out and final Windower save finish first.
                self.close_parks_in_background(Some(pol_less::Tool::Windower.name()));
                self.status = t("Edit profiles in Windower. Saved changes apply on the next fresh game launch.").into();
                let preferred = self.settings.windower_path.clone();
                let sessions = self.accounts.iter().filter_map(|row|
                    session_worker::find(&self.settings.config_for(&self.config, &row.account), &row.account)).collect();
                let sender = self.sender.clone();
                return Effect::perform(async move {
                    tokio::task::spawn_blocking(move || {
                        let folder = pol_less::tools::detect(pol_less::Tool::Windower, &preferred)
                            .ok_or_else(|| t("Windower was not found. Set its folder in Settings → System → Launch tools.").to_owned())?;
                        session_worker::sign_out_for_profile_editor(sessions, |line| {
                            let _ = sender.send(WorkerEvent::WindowerProfileProgress(line.into()));
                        })?;
                        launch_tools::edit_profiles(&folder, |line| {
                            let _ = sender.send(WorkerEvent::WindowerProfileProgress(line.into()));
                        })
                    }).await.unwrap_or_else(|_| Err(t("Could not open Windower's profile editor.").into()))
                }, Message::WindowerProfilesClosed);
            }
            Message::WindowerProfilesClosed(result) => {
                self.windower_editor_open = false;
                self.park_checked = None;
                if let Err(error) = result { self.status = error; }
                return self.launch_tools.refresh(&self.settings);
            }
            Message::Tick => {
                // A long gap since the last Tick is a resume from sleep: a waiting Play checks again.
                self.flag_schedule.tick(chrono::Utc::now());
                // No periodic patch-host lookups: stock looks pc001W2U.pol.com up only right
                // before a W20 (the W20's own lookup, or xi-start-game's warm-up at Play).
                if self.startup_signin_pending && self.vault_ready && !self.installation_loading {
                    self.start_ready_startup_signins();
                }
                let keys = self.take_waiting_startups(Instant::now());
                self.start_startup(keys, launch_timing::LaunchTrigger::StartupSignIn);
                // A request in flight is followed closely, so a conversation
                // fills message by message; otherwise every two seconds. It is
                // followed off the page too: its time limit runs on every page.
                let every = if self.friends.pending.is_some() { Duration::from_millis(250) } else { Duration::from_secs(2) };
                if (self.page == Page::Friends || self.friends.pending.is_some()) && self.friends_checked.elapsed() >= every {
                    self.load_friends();
                }
                if let Some(op) = &self.active_operation {
                    self.maintenance_progress = op.snapshot();
                }
                while let Ok(event) = self.receiver.try_recv() {
                    self.worker_event(event);
                }
                // After the results that already arrived: a completion waiting in the queue
                // is not a request PlayOnline never answered.
                self.expire_friends_request();
                self.release_held_plays(Instant::now());
                let (game_ends, game_newly_seen) = self.observe_sessions();
                for (key, kind, host) in game_ends { self.note_game_end(&key, kind, host); }
                // Once per Play batch, after the first game reports started; off the UI thread,
                // and never part of the launch itself.
                let game_active = self.accounts.iter().any(|row| row.game_seen);
                let open_programs = if self.open_gate.observe(game_active, game_newly_seen) && !self.preview
                    && self.settings.open_with_game.iter().any(|p| p.enabled) {
                    let programs = self.settings.open_with_game.clone();
                    Effect::perform(async move { tokio::task::spawn_blocking(move || open_with_game::run(programs)).await.unwrap_or_default() },
                        Message::OpenWithGameDone)
                } else { Effect::none() };
                let keys = self.take_resigns(Instant::now());
                self.start_resigns(keys, launch_timing::LaunchTrigger::SignInAfterGame);
                self.start_queued_plays();
                let accounts = &self.accounts;
                // A deleted account no longer waits.
                let deleted: Vec<String> = self.waiting_plays.iter().flat_map(|wait| &wait.keys)
                    .filter(|key| !accounts.iter().any(|row| row.key == **key)).cloned().collect();
                self.unwait(&deleted);
                self.expire_wait_codes();
                self.release_waiting_plays(false);
                self.tend_patch_watch();
                // Work a worker event started (a Play's PlayOnline update).
                let started = Effect::batch(std::mem::take(&mut self.play_updates.tasks));
                let flag = self.read_play_flag();
                self.tend_parks(Instant::now());
                if self.preview
                    && !preview_idle()
                    && !self.preview_capture_sent
                    && !self.news_loading
                    && self.refreshed.elapsed() > Duration::from_secs(
                        std::env::var("XI_PREVIEW_DELAY_SECS").ok()
                            .and_then(|value| value.parse::<u64>().ok()).unwrap_or(2).clamp(2, 30))
                {
                    self.preview_capture_sent = true;
                    return self.on_window(WindowOp::screenshot(Message::PreviewReady));
                }
                return Effect::batch([started, flag, open_programs, self.poll_service_status()]);
            }
            Message::PlayFlagLoaded(reading) => {
                self.flag_schedule.finish();
                self.play_flag.note(reading);
                self.release_waiting_plays(true);
                self.tend_patch_watch();
            }
            Message::CancelWait(keys) => self.cancel_wait(&keys),
            Message::CancelPlayUpdate(generation) => self.cancel_play_update(generation),
            Message::ConfirmViewerUpdate => {
                if self.play_updates.viewer_confirm.take().is_some() {
                    return self.start_maintenance_scoped(MaintenanceKind::Update, true);
                }
            }
            Message::AccountDragStart(key) => {
                // The rail shows on every page, so a reorder starts from any of them; a dialog
                // over the rail blocks it.
                if self.vault_ready && !self.vault_saving && !self.show_settings && self.portrait_picker.is_none()
                    && self.editor.is_none() && !self.account_import.open && !self.confirm_close && self.pending_delete.is_none()
                    && self.accounts.iter().any(|row| row.key == key) {
                    self.cancel_account_drag();
                    self.account_drag.original = self.accounts.iter().map(|row| row.key.clone()).collect();
                    self.account_drag.source = Some(key);
                }
            }
            Message::AccountDragOver(target) => {
                if let Some(source) = self.account_drag.source.as_ref() {
                    if let (Some(from), Some(to)) = (self.accounts.iter().position(|row| &row.key == source),
                        self.accounts.iter().position(|row| row.key == target)) {
                        if from != to {
                            let row = self.accounts.remove(from);
                            self.accounts.insert(to, row);
                        }
                    }
                }
            }
            Message::AccountDragFinish => self.finish_account_drag(),
            Message::AccountDragCancel => self.cancel_account_drag(),
            Message::ToggleAccount(key, value) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    row.selected = value;
                }
                self.settings.selected_accounts = Some(
                    self.accounts
                        .iter()
                        .filter(|r| r.selected)
                        .map(|r| r.key.clone())
                        .collect(),
                );
                self.persist_settings();
            }
            Message::WindowResized(size) => {
                self.viewport = size;
                // Maximise/restore arrives as a resize; keep the chrome's
                // restore glyph and resize edges in step with it.
                return self.on_window(WindowOp::is_maximized(Message::WindowMaximized));
            }
            Message::WindowMaximized(maximized) => self.maximized = maximized,
            Message::WindowOpened(id) => {
                if self.window == Some(id) { return self.on_window(WindowOp::native_handle(Message::NativeWindow)); }
            }
            Message::NativeWindow(hwnd) => {
                window_chrome::attach(hwnd, self.settings.light_theme)
            }
            Message::MinimizeWindow => return self.on_window(WindowOp::Minimize(true)),
            Message::MaximizeWindow => return self.on_window(WindowOp::ToggleMaximize),
            Message::TitlePressed => {
                self.title_held = !window_chrome::title_press_is_double();
                if !self.title_held { return self.on_window(WindowOp::ToggleMaximize); }
            }
            Message::TitleMoved => {
                if std::mem::take(&mut self.title_held) { return self.on_window(WindowOp::Drag); }
            }
            Message::TitleReleased => self.title_held = false,
            Message::ResizeWindow(edge) => return self.on_window(WindowOp::DragResize(edge)),
            Message::Navigate(page) => {
                self.cancel_account_drag();
                self.page = page;
                // Features is a page of its own: going anywhere leaves it.
                if self.features_view.open { self.features_view.open = false; self.features_view.touch(); }
                self.show_settings = false;
                self.show_details = false;
                if page == Page::Friends { self.load_friends(); }
                if page == Page::Settings && self.settings_tab == settings_page::Tab::Graphics {
                    return reshade::update(self, reshade::Msg::Refresh);
                }
                if page == Page::Maintenance && !self.settings.install_selected {
                    return self.find_installation(true);
                }
            }
            Message::ToggleDetails => self.show_details = !self.show_details,
            Message::Launch(key) => {
                if let Some(features) = &self.features { features.cancel_relaunch(&key); }
                if self.features_view.offers.remove(&key).is_some() { self.features_view.touch(); }
                self.play(vec![key])
            }
            Message::MainAction => return self.run_main_action(),
            Message::LaunchAll => self.play(self.launch_keys(false)),
            Message::SignIn(key) => {
                if self.accounts.iter().any(|row| row.key == key && row.account.mode == AccountMode::Retail && row.can_play() && row.pid.is_none()) {
                    self.connect(vec![key]);
                }
            }
            Message::SetCharacterSelect(on) => {
                let mut settings = self.settings.clone();
                for key in self.play_target_keys() {
                    let mut choice = settings.account_auto_login.get(&key).copied().unwrap_or_default().normalized();
                    choice.enabled = on;
                    if choice == pol_less::AutoLogin::default() { settings.account_auto_login.remove(&key); }
                    else { settings.account_auto_login.insert(key, choice); }
                }
                if !self.preview {
                    if let Err(error) = save_settings(&self.config, &settings) { self.status = error; return Effect::none(); }
                }
                self.settings = settings;
            }
            Message::OtpChanged(key, value) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key && !r.busy) {
                    row.otp.zeroize();
                    row.otp = value;
                }
            }
            Message::NewAccount => {
                self.cancel_account_drag();
                if self.vault_ready && !self.vault_saving {
                    self.status = t("Enter account details, then save.").into();
                    self.editor = Some(Editor::blank());
                }
            }
            Message::AccountImport(msg) => return self.import_update(msg),
            Message::EditAccount(key) => {
                self.cancel_account_drag();
                if self.vault_saving { return Effect::none(); }
                if let Some(row) = self
                    .accounts
                    .iter()
                    .find(|r| r.key == key)
                {
                    self.editor = Some(Editor {
                        game_install: self.settings.account_installs.get(&key).cloned().unwrap_or_default(),
                        dat_overlays: self.settings.account_overlays.get(&key).cloned().unwrap_or_default(),
                        auto_login: self.settings.account_auto_login.get(&key).copied().unwrap_or_default().normalized(),
                        flags: EditorFlags { uses_otp: self.settings.account_otp.contains(&key), ..Default::default() },
                        key: Some(key),
                        account: row.account.clone(),
                    });
                    self.status = t("Edit account details.").into();
                }
            }
            Message::EditorChanged(field, value) => {
                if matches!(field, Field::PolId | Field::Server) && self.editor.as_ref().and_then(|editor| editor.key.as_ref())
                    .is_some_and(|key| self.accounts.iter().any(|row| &row.key == key && (row.busy || row.pid.is_some()))) {
                    return Effect::none();
                }
                if let Some(editor) = self.editor_mut() {
                    let destination = match field {
                        Field::Label => &mut editor.account.label,
                        Field::Server => &mut editor.account.server,
                        Field::PolId => &mut editor.account.pol_id,
                        Field::PolPassword => &mut editor.account.pol_password,
                        Field::SeId => &mut editor.account.se_id,
                        Field::SePassword => &mut editor.account.se_password,
                        Field::PolHandleNames => &mut editor.account.pol_handle_names,
                        // Kept as typed until Save normalizes it (`totp::normalize`).
                        Field::TotpKey => &mut editor.account.totp_key,
                    };
                    destination.zeroize();
                    *destination = value;
                }
            }
            Message::EditorRegion(region) => {
                if let Some(editor) = self.editor_mut() { editor.account.region = region; }
            }
            Message::EditorAutoLogin(enabled) => {
                if let Some(editor) = self.editor_mut() { editor.auto_login.enabled = enabled; }
            }
            Message::EditorFlag(flag, on) => {
                if let Some(editor) = self.editor_mut() {
                    let flags = &mut editor.flags;
                    match flag {
                        EditorFlag::UsesOtp => flags.uses_otp = on,
                        EditorFlag::SignInNow => flags.sign_in_now = on,
                        EditorFlag::ShowId => flags.show_id = on,
                        EditorFlag::ShowPassword => flags.show_password = on,
                        EditorFlag::ChangePassword => flags.change_password = on,
                        EditorFlag::ShowTotpKey => flags.show_totp_key = on,
                        EditorFlag::ChangeTotpKey => flags.change_totp_key = on,
                    }
                }
            }
            Message::EditorCharacterSlot(slot) => {
                if let Some(editor) = self.editor_mut().filter(|_| (1..=16).contains(&slot)) { editor.auto_login.character_slot = slot; }
            }
            Message::SaveAccount => {
                // The authenticator key as typed (spaces, lowercase, padding) is saved in its
                // stored form; a key that is not base32 keeps the sheet open with the reason.
                if let Some(editor) = self.editor.as_mut() {
                    let key = &mut editor.account.totp_key;
                    if key.trim().is_empty() || editor.account.mode != AccountMode::Retail {
                        key.zeroize();
                    } else {
                        match totp::normalize(key) {
                            Ok(normalized) => { key.zeroize(); key.push_str(&normalized); }
                            Err(error) => { self.status = error.message().into(); return Effect::none(); }
                        }
                    }
                }
                if let Some(editor) = &self.editor {
                    if self.settings.launch_with_for(&editor.account) == crate::backend::LaunchWith::Native {
                        if editor.dat_overlays.len() > 8 { self.status = t("Choose at most eight DAT overlay folders.").into(); return Effect::none(); }
                        for path in &editor.dat_overlays {
                            if let Err(error) = selected_folder(path, true) { self.status = error; return Effect::none(); }
                        }
                    }
                    if !editor.game_install.trim().is_empty() {
                        let valid = selected_folder(&editor.game_install, true).and_then(|path| {
                            if path.join("FFXiMain.dll").is_file() && path.join("FFXi.dll").is_file() {
                                Ok(path)
                            } else {
                                Err(t("Choose the game folder containing FFXiMain.dll and FFXi.dll.").into())
                            }
                        });
                        if let Err(error) = valid { self.status = error; return Effect::none(); }
                    }
                    let key = account_key(&editor.account);
                    if editor.key.as_ref().is_some_and(|previous| *previous != key
                        && self.accounts.iter().any(|row| &row.key == previous && (row.busy || row.pid.is_some()))) {
                        self.status = t("Keep the account ID unchanged while its session is running.").into();
                        return Effect::none();
                    }
                    if self
                        .accounts
                        .iter()
                        .any(|r| r.key == key && Some(&r.key) != editor.key.as_ref())
                    {
                        self.status =
                            t("This login is already saved. Edit its existing account instead.")
                                .into();
                        return Effect::none();
                    }
                    let mut replaced = false;
                    let mut values: Vec<Account> = self.accounts.iter().map(|row| {
                        if Some(&row.key) == editor.key.as_ref() {
                            replaced = true;
                            editor.account.clone()
                        } else { row.account.clone() }
                    }).collect();
                    if !replaced { values.push(editor.account.clone()); }
                    return self.save_accounts(values);
                }
            }
            Message::RequestDeleteAccount(key) => {
                if self.vault_ready && !self.vault_saving && self.accounts.iter().any(|row| row.key == key && !row.busy && row.pid.is_none()) {
                    self.pending_delete = Some(key);
                }
            }
            Message::CancelDeleteAccount => self.pending_delete = None,
            Message::DeleteAccount(key) => {
                if self.pending_delete.as_deref() != Some(key.as_str()) {
                    return Effect::none();
                }
                self.pending_delete = None;
                if self
                    .accounts
                    .iter()
                    .any(|r| r.key == key && (r.busy || r.pid.is_some()))
                {
                    return Effect::none();
                }
                let values = self
                    .accounts
                    .iter()
                    .filter(|r| r.key != key)
                    .map(|r| r.account.clone())
                    .collect();
                return self.save_accounts(values);
            }
            Message::CloseEditor => {
                if !self.vault_saving {
                    self.editor = None;
                    if self.account_import.batch.is_some() { self.account_import.open = true; }
                }
            }
            Message::ToggleSettings => self.show_settings = !self.show_settings,
            Message::SetLightTheme(light) => {
                self.settings.light_theme = light;
                window_chrome::set_theme(light);
                self.persist_settings();
            }
            // Display only and at once: the next view draws pseudonyms.
            Message::SetPrivacy(on) => {
                self.settings.privacy = on;
                privacy::set(on);
                if let Some(features) = &self.features { features.set_privacy(on); }
                self.persist_settings();
            }
            // Applies at once: the next view reads the new language.
            Message::SetLanguage(lang) => {
                let before = i18n::current();
                self.settings.language = Some(lang);
                i18n::set(lang);
                self.retranslate(before);
                self.persist_settings();
                if lang != before {
                    // The official news is per language: drop the old language's
                    // headlines at once, show the new language's saved ones, and
                    // fetch fresh ones.
                    self.news_error.clear();
                    if !self.preview {
                        // The status (page reading, windows, read time) is the same in every
                        // language: keep it, or Play is ungated until the new read lands.
                        self.services = services::keep_status(self.services.as_ref(), services::cached(&news_cache_path(&self.config, lang), lang));
                        // A version-record read in flight words its placeholders in the old
                        // language (PatchLoaded matches them): read again, so it is dropped.
                        let patch = if self.patch_loading { self.read_installed_version() } else { Effect::none() };
                        return Effect::batch([self.start_news(), patch]);
                    }
                }
            }
            // Home's Launch with sets the own launcher of every account Play starts
            // (the selected ones, else the first).
            Message::SetLaunchWith(choice) => {
                let keys = self.play_target_keys();
                return self.set_account_launcher(&keys, choice);
            }
            Message::EditorLaunchWith(choice) => {
                if let Some(editor) = self.editor_mut().filter(|_| choice != Some(crate::backend::LaunchWith::Native)) { editor.account.launch_with = choice; }
            }
            Message::EditorBootIni(choice) => {
                if let Some(editor) = self.editor_mut() { editor.account.ashita_boot_ini = choice; }
            }
            Message::EditorProfile(choice) => {
                if let Some(editor) = self.editor_mut() { editor.account.windower_profile = choice; }
            }
            Message::WikiQueryChanged(value) => self.wiki_query = value,
            Message::AhQueryChanged(value) => self.ah_query = value,
            Message::AhServerChanged(value) => {
                self.ah_server = value;
                if self.ah_lookup.open { return self.search_ah(); }
            }
            Message::AhResults(generation, result) => self.ah_lookup.finish(generation, result),
            Message::CloseAh => self.ah_lookup.open = false,
            Message::PortraitPicker(None) => self.portrait_picker = None,
            Message::PortraitPicker(Some(key)) => {
                if !self.accounts.iter().any(|row| row.key == key && row.account.mode == AccountMode::Retail) { return Effect::none(); }
                self.portrait_picker = Some(portraits::Picker { account: key, ..Default::default() });
                // Every sheet is read once (a few hundred ms), off the UI thread; later opens reuse it.
                return Effect::perform(
                    async { tokio::task::spawn_blocking(portraits::available).await.unwrap_or_else(|_| std::sync::Arc::from([])) },
                    Message::PortraitsListed,
                );
            }
            Message::PortraitsListed(ids) => {
                if let Some(picker) = &mut self.portrait_picker {
                    // Privacy mode does not open on the page that would point at your own.
                    let current = if privacy::on() { None } else { self.own_portraits.get(&picker.account).copied() };
                    picker.listed(ids, current);
                }
            }
            Message::PortraitPage(page) => {
                if let Some(picker) = &mut self.portrait_picker {
                    if page < picker.pages() { picker.page = page; }
                }
            }
            Message::PortraitPick(id) => {
                // Nothing is written to PlayOnline until the portrait field is confirmed.
                if !portraits::PICKER_ENABLED || self.preview { return Effect::none(); }
                let Some(picker) = &mut self.portrait_picker else { return Effect::none() };
                if !picker.ids.as_ref().is_some_and(|ids| ids.contains(&id)) { return Effect::none(); }
                let Some(row) = self.accounts.iter().find(|row| row.key == picker.account) else { return Effect::none() };
                let config = self.settings.config_for(&self.config, &row.account);
                match session_worker::request_native_portrait(&config, &row.account, id) {
                    Ok(()) => self.portrait_picker = None,
                    Err(error) => picker.error = Some(error),
                }
            }
            Message::NewsFilter(value) => self.news_filter = value,
            Message::SearchAh => return self.search_ah(),
            Message::SearchWiki => {
                let url = crate::ffxiah::wiki_search_url(&self.wiki_query);
                if !self.preview {
                    if let Err(error) = open_url(url.as_str()) {
                        self.status = error;
                    }
                }
            }
            Message::InstallPathChanged(value) => {
                if self.maintenance.is_none() {
                    self.installation_generation = self.installation_generation.wrapping_add(1);
                    self.installation_loading = false;
                    self.settings.install_path = value;
                    self.settings.install_selected = false;
                    self.patch = None;
                    self.patch_folder = None;
                    self.patch_generation += 1;
                    self.patch_loading = false;
                    self.maintenance_result =
                        t("Installation path changed. Save settings to select this folder.").into();
                    self.maintenance_tone = ResultTone::Unverified;
                }
            }
            Message::SelectInstallPath => self.select_path(false),
            Message::FindInstallation => return self.find_installation(false),
            Message::InstallationFound(generation, automatic, folder) => {
                if generation != self.installation_generation { return Effect::none(); }
                self.park_checked = None;
                self.installation_loading = false;
                if self.preview || self.maintenance.is_some() { return Effect::none(); }
                let Some(folder) = folder else {
                    if !automatic { self.status = t("No installed FFXI folder was found. Run official setup or use Browse.").into(); }
                    self.start_ready_startup_signins();
                    return Effect::none();
                };
                let path = folder.to_string_lossy().into_owned();
                if self.settings.install_selected && self.settings.install_path == path {
                    if !automatic { self.status = tf("Found FINAL FANTASY XI: {path}", &[("path", &folder.display())]); }
                    self.start_ready_startup_signins();
                    return Effect::none();
                }
                let _ = self.update(Message::InstallPathChanged(path));
                // Recovery defaults must not replace an unreadable settings file
                // merely because startup found an installation.
                if automatic && self.settings_damaged {
                    self.settings.install_selected = true;
                } else {
                    self.select_path(false);
                }
                if self.settings.install_selected {
                    if !automatic { self.status = tf("Found FINAL FANTASY XI: {path}", &[("path", &folder.display())]); }
                    self.start_ready_startup_signins();
                    return self.read_installed_version();
                }
                self.start_ready_startup_signins();
            }
            Message::InstallerPathChanged(value) => {
                if self.maintenance.is_none() {
                    self.installer_error = None;
                    self.settings.installer_path = value;
                    self.settings.installer_selected = false;
                }
            }
            Message::SelectInstallerPath => self.select_path(true),
            Message::Browse(target) => return self.browse(target),
            Message::Picked(target, picked) => return self.picked(target, picked),
            Message::AccountInstallChanged(value) => {
                if let Some(editor) = self.editor_mut() { editor.game_install = value; }
            }
            Message::PickedAccountFolder(key, picked) => {
                if let Some(editor) = self.editor_mut() {
                    if account_key(&editor.account) == key {
                        if let Some(path) = picked { editor.game_install = backend::plain_windows_path(&path).to_string_lossy().into_owned(); }
                    }
                }
            }
            Message::PickedAccountOverlay(key, picked) => {
                if let Some(editor) = self.editor_mut() {
                    if account_key(&editor.account) == key && editor.dat_overlays.len() < 8 {
                        if let Some(path) = picked {
                            let path = backend::plain_windows_path(&path).to_string_lossy().into_owned();
                            if !editor.dat_overlays.iter().any(|p| p.eq_ignore_ascii_case(&path)) { editor.dat_overlays.push(path); }
                        }
                    }
                }
            }
            Message::StartMaintenance(kind) => return self.start_maintenance(kind),
            Message::ToggleTroubleshooting => self.troubleshooting_open = !self.troubleshooting_open,
            Message::CancelMaintenance => {
                if self.maintenance.is_some_and(MaintenanceKind::cancellable) {
                    if let Some(op) = &self.active_operation {
                        op.cancel();
                        self.maintenance_cancelled = true;
                        self.status = t("Cancellation requested; waiting for the current file operation to finish safely.").into();
                    }
                }
            }
            Message::MaintenanceFinished(kind, result, shown) => {
                if self.maintenance == Some(kind) {
                    let setup_finished = kind == MaintenanceKind::Setup && result.is_ok();
                    if matches!(kind, MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair) { self.viewer_loading = false; }
                    if matches!(kind, MaintenanceKind::Download | MaintenanceKind::Extract | MaintenanceKind::Setup) {
                        self.installer_error = result.as_ref().err().cloned();
                    }
                    if let Some(op) = self.active_operation.take() {
                        self.maintenance_progress = op.snapshot();
                    }
                    self.maintenance = None;
                    self.maintenance_tone = if result.is_err() { ResultTone::Stopped }
                        else if shown.as_ref().is_some_and(|report| report.files_verified == Some(false) || report.files_verified.is_none() && report.state != patch::PatchState::Current) { ResultTone::Unverified }
                        else { ResultTone::Done };
                    // An Update started from refused Plays starts them again once it succeeded
                    // (the update signed their sessions out: each is a new sign-in).
                    let reissue = if kind == MaintenanceKind::Update { std::mem::take(&mut self.reissue_after_update) } else { Vec::new() };
                    let updated = result.is_ok();
                    let failure = result.as_ref().err().cloned();
                    self.maintenance_result =
                        result.unwrap_or_else(|error| tf("Operation stopped: {error}", &[("error", &error)]));
                    self.status = self.maintenance_result.clone();
                    // No report came back: read the installed record again, so a Check never
                    // leaves the installed version at a placeholder.
                    let refresh = shown.is_none() && matches!(kind, MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair);
                    if let Some(report) = shown {
                        // An Update reports the folder it updated (an account's own one when
                        // its W20 asked); Check and Repair the Settings folder.
                        if kind != MaintenanceKind::Update { self.patch_folder = None; }
                        // An Update or Repair told the official version (Check's is a placeholder).
                        let official = report.remote_version.clone();
                        self.show_patch(report);
                        let folder = self.shown_game_folder();
                        self.learn_from_update(&folder, Some(&official), None);
                    }
                    for (key, clicked) in reissue.iter().filter(|_| updated).cloned() {
                        // Every restarted Play signs in again after the updater signed out its
                        // session. An OTP account needs a replacement code; one entered during
                        // the update stays on the row until the normal launch consumes it.
                        let settings = &self.settings;
                        if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key
                            && wants_typed_code(settings, row) && row.otp.is_empty() && !row.busy && row.pid.is_none()) {
                            row.status = t("That one-time code was used. Enter a new one, then press Play.").into();
                            row.problem = true;
                            continue;
                        }
                        if self.accounts.iter().any(|row| row.key == key && row.can_play() && !row.busy && row.pid.is_none()) {
                            self.start_launch(vec![key.clone()], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay));
                        }
                    }
                    // The Play that asked for a PlayOnline update it could not finish says why.
                    if let Some(error) = failure.filter(|_| kind == MaintenanceKind::Update) {
                        let shown = tf("PlayOnline could not be updated: {error}", &[("error", &error)]);
                        for (key, _) in &reissue {
                            if let Some(row) = self.accounts.iter_mut().find(|row| row.key == *key && !row.busy) {
                                row.status = shown.clone();
                                row.problem = true;
                            }
                        }
                    }
                    if refresh { return self.refresh(); }
                    if setup_finished {
                        return Effect::batch([self.find_installation(false), self.start_viewer_check()]);
                    }
                }
            }
            Message::SaveSettings => {
                if self.preview {
                    return Effect::none();
                }
                // Settings holds the only game-folder field: saving a typed folder selects it.
                // A folder that is not a game installation keeps the dialog open with the reason.
                if !self.settings.install_selected && !self.settings.install_path.trim().is_empty()
                    && self.maintenance.is_none() && !self.reshade.busy {
                    self.select_path(false);
                    if !self.settings.install_selected { return Effect::none(); }
                }
                match save_settings(&self.config, &self.settings) {
                    Ok(()) => {
                        self.settings_damaged = false;
                        self.park_checked = None;
                        self.show_settings = false;
                        self.status = t("Settings saved. Checking the installed version…").into();
                        return self.refresh();
                    }
                    Err(error) => self.status = error,
                }
            }
            Message::Refresh => {
                if !self.news_loading && !self.patch_loading {
                    return self.refresh();
                }
            }
            Message::AccountsLoaded(result) => {
                match result {
                    Ok(values) => {
                        self.replace_accounts(values);
                        self.vault_ready = true;
                        self.vault_error = None;
                        self.status = t("Ready. Select accounts or use Launch all. Games continue when you close Crystal.").into();
                        self.log_launcher_start();
                        self.start_ready_startup_signins();
                    }
                    Err(error) => {
                        self.vault_ready = false;
                        self.status = tf("Saved accounts could not be loaded: {error} The existing vault was kept.", &[("error", &error)]);
                        self.vault_error = Some(self.status.clone());
                    }
                }
                if let Some(warning) = self.startup_warning.take() {
                    self.status = tf("{warning} {status}", &[("warning", &warning), ("status", &self.status)]);
                }
            }
            Message::AccountsSaved(result) => {
                self.vault_saving = false;
                match result {
                    Ok(values) => {
                        let mut settings = self.settings.clone();
                        let saved: BTreeSet<String> = values.iter().map(account_key).collect();
                        settings.account_pol_handles.retain(|key, _| saved.contains(key));
                        for account in &values {
                            let key = account_key(account);
                            if account.pol_handle_names.trim().is_empty() { settings.account_pol_handles.remove(&key); }
                            else { settings.account_pol_handles.insert(key, account.pol_handle_names.clone()); }
                        }
                        // A login edit changes the account key. Keep its non-editor
                        // preferences with the account before removing obsolete keys.
                        if let Some(editor) = &self.editor {
                            let key = account_key(&editor.account);
                            if let Some(previous) = editor.key.as_ref().filter(|old| **old != key && saved.contains(&key)) {
                                if let Some(minutes) = settings.account_auto_away.remove(previous) {
                                    settings.account_auto_away.insert(key.clone(), minutes);
                                }
                                if let Some(choice) = settings.account_auto_login.remove(previous) {
                                    settings.account_auto_login.insert(key.clone(), choice);
                                }
                                if let Some(selected) = &mut settings.selected_accounts {
                                    if selected.remove(previous) { selected.insert(key); }
                                }
                                for ordered in &mut settings.account_order {
                                    if ordered == previous { *ordered = account_key(&editor.account); }
                                }
                            }
                        }
                        settings.account_installs.retain(|key, _| saved.contains(key));
                        settings.account_overlays.retain(|key, _| saved.contains(key));
                        settings.account_auto_away.retain(|key, _| saved.contains(key));
                        settings.account_auto_login.retain(|key, _| saved.contains(key));
                        // Learned from sign-ins of that login: a changed login learns it again.
                        settings.account_otp.retain(|key| saved.contains(key));
                        settings.account_no_otp.retain(|key| saved.contains(key));
                        settings.account_order.retain(|key| saved.contains(key));
                        if let Some(selected) = &mut settings.selected_accounts { selected.retain(|key| saved.contains(key)); }
                        if self.play_updates.viewer_confirm.as_ref().is_some_and(|(key, _)| !saved.contains(key)) { self.play_updates.viewer_confirm = None; }
                        if let Some(editor) = &self.editor {
                            let key = account_key(&editor.account);
                            if saved.contains(&key) {
                                let path = editor.game_install.trim();
                                if path.is_empty() { settings.account_installs.remove(&key); }
                                else { settings.account_installs.insert(key.clone(), plain_text_path(path)); }
                                if editor.dat_overlays.is_empty() { settings.account_overlays.remove(&key); }
                                else { settings.account_overlays.insert(key.clone(), editor.dat_overlays.iter().map(|p| plain_text_path(p)).collect()); }
                                // "Uses a one-time password": Crystal then never signs it in by
                                // itself; cleared, the next sign-in teaches it again.
                                if editor.flags.uses_otp {
                                    settings.account_no_otp.remove(&key);
                                    settings.account_otp.insert(key.clone());
                                } else {
                                    settings.account_otp.remove(&key);
                                }
                                if editor.auto_login != pol_less::AutoLogin::default() {
                                    settings.account_auto_login.insert(key, editor.auto_login.normalized());
                                } else { settings.account_auto_login.remove(&key); }
                            }
                        }
                        // An edited account signs nothing back in by itself (a removed one has no row).
                        let edited: Vec<String> = self.editor.iter().flat_map(|editor| [Some(account_key(&editor.account)), editor.key.clone()]).flatten().collect();
                        if (self.editor.is_some() || settings.account_installs != self.settings.account_installs || settings.account_overlays != self.settings.account_overlays
                            || settings.account_auto_away != self.settings.account_auto_away || settings.account_auto_login != self.settings.account_auto_login || settings.selected_accounts != self.settings.selected_accounts
                            || settings.account_order != self.settings.account_order
                            || settings.account_pol_handles != self.settings.account_pol_handles
                            || settings.account_otp != self.settings.account_otp
                            || settings.account_no_otp != self.settings.account_no_otp) && !self.preview {
                            if let Err(error) = save_settings(&self.config, &settings) {
                                if let Some(editor) = &mut self.editor {
                                    let key = account_key(&editor.account);
                                    if saved.contains(&key) {
                                        // The vault already saved this identity. A retry
                                        // must edit it instead of reporting a duplicate.
                                        editor.key = Some(key);
                                    } else {
                                        self.editor = None;
                                    }
                                }
                                // Keep the pending preferences for a retry or the next
                                // settings save; account storage is already committed.
                                self.settings = settings;
                                self.replace_accounts(values);
                                // The vault saved the edit: it declines a sign-in after the game too.
                                for row in self.accounts.iter_mut().filter(|row| edited.contains(&row.key)) { row.decline_resign(); }
                                self.status = tf("Accounts saved, but launcher settings were not saved: {error}", &[("error", &error)]);
                                return Effect::none();
                            }
                        }
                        self.settings = settings;
                        self.replace_accounts(values);
                        for row in self.accounts.iter_mut().filter(|row| edited.contains(&row.key)) { row.decline_resign(); }
                        // A new account's "Sign in now", under the
                        // Settings switch and never for a one-time-password account.
                        let sign_in = self.editor.as_ref()
                            .filter(|editor| editor.key.is_none() && editor.flags.sign_in_now && self.settings.auto_sign_in
                                && (!editor.flags.uses_otp || !editor.account.totp_key.is_empty()))
                            .map(|editor| account_key(&editor.account));
                        self.editor = None;
                        self.park_checked = None;
                        if let Some(key) = sign_in {
                            if self.accounts.iter().any(|row| row.key == key && row.account.mode == AccountMode::Retail && row.can_play() && row.pid.is_none()) {
                                self.connect(vec![key]);
                            }
                        }
                        if self.account_import.batch.is_some() { self.account_import.open = true; }
                        self.status = t("Accounts saved.").into();
                    }
                    Err(error) => self.status = error,
                }
            }
            Message::ServicesLoaded(generation, lang, _) | Message::ServiceStatusLoaded(generation, lang, _)
                if generation != self.news_generation || lang != i18n::current() => {
                // The newest request owns `news_loading` and the displayed data.
            }
            Message::ServicesLoaded(_, _, result) => {
                self.news_loading = false;
                // let the live feed paint before a preview capture; a
                // capture taken on the next tick replays the previous frame
                // (stale images, text whose paragraphs were already dropped).
                if self.preview {
                    self.refreshed = Instant::now();
                }
                match result {
                    Ok(mut snapshot) => {
                        // --preview-news-pictures (with --preview-live-news): only headlines that have a picture.
                        // The README shots show Home ready to play whatever the live status is that day.
                        if self.preview && preview::has("--preview-news-pictures") {
                            snapshot.news.retain(|item| item.image.is_some());
                            snapshot.availability = services::Availability::Operational;
                            snapshot.windows.clear();
                        }
                        services::apply(&mut self.services, services::Update::Headlines { items: snapshot.news.clone(), note: snapshot.news_note.clone() });
                        services::apply(&mut self.services, services::Update::Status(snapshot));
                        self.news_error.clear();
                    }
                    Err(error) => self.news_error = error,
                }
                self.note_status_read();
            }
            Message::ViewerLoaded(generation, report) => self.set_viewer_status(generation, report),
            Message::ServiceStatusLoaded(_, _, result) => {
                self.news_loading = false;
                match result {
                    Ok(snapshot) => services::apply(&mut self.services, services::Update::Status(snapshot)),
                    Err(error) => {
                        // The page is unread; an official window still covering now stays maintenance.
                        if let Some(snapshot) = &mut self.services {
                            snapshot.availability = services::Availability::Unknown;
                            snapshot.availability_note = tf("{note} Official world status unavailable: {error}.",
                                &[("note", &t("Official world status is unavailable. This does not establish a game outage. Check official notices.")), ("error", &error)]);
                        }
                    }
                }
                self.note_status_read();
            }
            Message::PatchLoaded(generation, mut result) => {
                self.reload_official();
                if generation == self.patch_generation {
                    self.patch_loading = false;
                    // A version-record read contains no new server or file-check evidence.
                    // Keep the last check while that record is unchanged and readable, and
                    // an account's own folder's verdict (this read is the Settings folder's).
                    let keep_checked = self.patch_folder.is_some() || result.state == patch::PatchState::Unknown && result.remote_version == t("Not checked") && self.patch.as_ref().is_some_and(|previous| {
                        previous.local_version == result.local_version &&
                            (previous.state != patch::PatchState::Unknown || previous.files_verified.is_some())
                    });
                    if result.remote_version == t("Not checked") {
                        if let Some(previous) = self.patch.as_ref().filter(|p| p.local_version == result.local_version) {
                            result.remote_version.clone_from(&previous.remote_version);
                            if result.state == patch::PatchState::Required && result.local_version == t("Not initialized") {
                                result.files_verified = previous.files_verified;
                            }
                        }
                    }
                    if !keep_checked { self.patch = Some(result); }
                }
            }
            Message::OpenUrl(url) => {
                if !self.preview {
                    if let Err(error) = open_url(&url) {
                        self.status = error;
                    }
                }
            }
            Message::OpenLog(key) => self.open_log(&key, false),
            Message::OpenLogsFolder(key) => self.open_log(&key, true),
            Message::CloseRequested => {
                if self.settings.close_to_tray && (!self.preview || preview_idle()) && !matches!(self.self_update, self_update::State::Applying) {
                    return self.hide_window();
                }
                return self.request_quit();
            }
            Message::Tray(tray::Command::Open) => return self.show_window(),
            Message::Tray(tray::Command::Quit) => {
                let quit = self.request_quit();
                // Something holds the quit (an update, a save): the window says what.
                if !self.closing && self.window.is_none() { return Effect::batch([quit, self.show_window()]); }
                return quit;
            }
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                    window_chrome::detach();
                    tray::set_icon(true);
                }
            }
            Message::TrimMemory => {
                if self.window.is_none() && (!self.preview || preview_idle()) {
                    let before = footprint::memory(None);
                    footprint::trim(None);
                    let after = footprint::memory(None);
                    let show = |m: Option<footprint::Memory>| m.map_or_else(|| "unknown".to_owned(), footprint::Memory::text);
                    launch_log::append_history(&self.config.history_path(), "Crystal", &format!(
                        "background: trimmed, pid {}, {} -> {}", std::process::id(), show(before), show(after)), None);
                }
            }
            Message::CloseToTray(on) => {
                self.settings.close_to_tray = on;
                self.persist_settings();
            }
            Message::StartWithWindows(on) => {
                if self.preview { self.settings.start_with_windows = on; return Effect::none(); }
                if self.startup_task_busy { return Effect::none(); }
                self.startup_task_busy = true;
                return self.startup_task(Some(on));
            }
            Message::StartupTaskSet(on, result) => {
                self.startup_task_busy = false;
                match result {
                    Ok(()) => {
                        self.settings.start_with_windows = on;
                        self.persist_settings();
                    }
                    Err(error) => self.status = error,
                }
            }
            Message::ConfirmClose => {
                if self.maintenance.is_none() && !self.prereqs.busy() && !self.reshade.busy {
                    return self.begin_close();
                }
                self.confirm_close = false;
            }
            Message::CancelClose => self.confirm_close = false,
            Message::Feature(msg) => return self.feature_update(msg),
            Message::PreviewReady(screenshot) => {
                // Each scene (and its light variant) gets its own file.
                let light = self.settings.light_theme;
                let filename = preview::capture_name(light, self.maintenance.is_some());
                // XI_PREVIEW_DIR lets parallel worktrees keep their previews apart.
                let dir = std::env::var_os("XI_PREVIEW_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.config.experiment.join("iced-launcher"));
                let mut path = if self.page == Page::Settings {
                    dir.join(format!(
                        "preview-settings-{}{}.png",
                        self.settings_tab.flag(),
                        if light { "-light" } else { "" }
                    ))
                } else {
                    dir.join(filename)
                };
                // --preview-private captures get their own -private files.
                if privacy::on() {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    path.set_file_name(format!("{stem}-private.png"));
                }
                // --preview-ja captures get their own -ja files.
                if i18n::current() == i18n::Lang::Ja {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    path.set_file_name(format!("{stem}-ja.png"));
                }
                self.status = match save_screenshot(&path, screenshot) {
                    Ok(()) => format!("Preview saved: {}", path.display()),
                    Err(e) => e,
                };
                return Effect::Exit;
            }
        }
        Effect::none()
    }

    /// After a language change, the stored lines that are fixed sentences
    /// follow it (texts with values or from other processes keep theirs until
    /// they next change).
    fn retranslate(&mut self, from: i18n::Lang) {
        let swap = |text: &mut String, keys: &[&'static str]| {
            if let Some(shown) = retranslated(text, from, keys) {
                *text = shown.into();
            }
        };
        swap(&mut self.maintenance_result, &RESULT_TEXTS);
        swap(&mut self.status, &STATUS_TEXTS);
        for row in &mut self.accounts {
            let noted = row.note_on == row.status;
            swap(&mut row.status, &ROW_TEXTS);
            if noted { row.note_on.clone_from(&row.status); }
        }
        if let Some(patch) = &mut self.patch {
            swap(&mut patch.local_version, &PATCH_TEXTS);
            swap(&mut patch.remote_version, &PATCH_TEXTS);
            swap(&mut patch.detail, &PATCH_TEXTS);
        }
        if let Some(viewer) = &mut self.viewer { viewer.retranslate(from); }
    }
}

fn blank_account() -> Account {
    Account {
        label: String::new(),
        mode: AccountMode::Retail,
        server: String::new(),
        pol_id: String::new(),
        pol_password: String::new(),
        se_id: String::new(),
        se_password: String::new(),
        region: 0,
        launch_with: None,
        ashita_boot_ini: None,
        windower_profile: None,
        pol_handle_names: String::new(),
        totp_key: String::new(),
    }
}

fn default_installer_path() -> String {
    Config::default()
        .experiment
        .join("official-installer")
        .to_string_lossy()
        .into_owned()
}

/// Where a Browse dialog opens: the field's folder (or the file's folder), else the
/// nearest existing parent, else the dialog's own default.
fn dialog_start(raw: &str, file: bool) -> Option<PathBuf> {
    let mut path = PathBuf::from(raw.trim());
    if raw.trim().is_empty() || !path.is_absolute() {
        return None;
    }
    if file {
        path.pop();
    }
    let mut dir = Some(path.as_path());
    while let Some(candidate) = dir {
        if candidate.is_dir() {
            return Some(backend::plain_windows_path(candidate));
        }
        dir = candidate.parent();
    }
    None
}

async fn pick_path(target: PickTarget, start: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new();
    if let Some(start) = start {
        dialog = dialog.set_directory(start);
    }
    let handle = match target {
        PickTarget::LoaderFile => {
            dialog
                .set_title(t("Choose the Crystal loader (pol.exe)"))
                .add_filter(t("Executable"), &["exe"])
                .pick_file()
                .await
        }
        PickTarget::ProgramFile(_) => {
            dialog
                .set_title(t("Choose a program to open with the game"))
                .add_filter(t("Executable"), &["exe"])
                .pick_file()
                .await
        }
        _ => {
            let title = match target {
                PickTarget::GameFolder | PickTarget::AccountGameFolder => t("Choose the FINAL FANTASY XI folder"),
                PickTarget::AccountDatFolder => t("Choose a DAT overlay folder containing ROM or sound folders"),
                PickTarget::InstallerFolder => t("Choose the download or extracted setup folder"),
                PickTarget::AshitaFolder => t("Choose the Ashita folder"),
                _ => t("Choose the Windower folder"),
            };
            dialog.set_title(title).pick_folder().await
        }
    };
    handle.map(|h| h.path().to_path_buf())
}

/// `\\?\C:\x` -> `C:\x` for a path kept as text; anything else is left alone.
fn plain_text_path(raw: &str) -> String {
    backend::plain_windows_path(std::path::Path::new(raw)).to_string_lossy().into_owned()
}

impl Settings {
    /// Owner decision 2026-10-13: Crystal signs `key` in by itself (when it opens, and again
    /// after its game closes) only while the switch is on and it knows the account signs in
    /// without a one-time code: its last successful sign-in took none. An OTP account, and one
    /// not yet seen signing in (its first Play teaches it), signs in only when Play is pressed.
    pub(crate) fn presigns(&self, key: &str) -> bool {
        self.auto_sign_in && self.account_no_otp.contains(key) && !self.account_otp.contains(key)
    }

    /// What `key`'s latest sign-in taught: it took a one-time code (`uses`), or it went without
    /// one. Only this yes or no is kept, never a code. Whether anything changed.
    fn note_sign_in(&mut self, key: &str, uses: bool) -> bool {
        let (add, remove) = if uses { (&mut self.account_otp, &mut self.account_no_otp) } else { (&mut self.account_no_otp, &mut self.account_otp) };
        let removed = remove.remove(key);
        add.insert(key.to_owned()) || removed
    }

    /// Files before 2026-10-13 kept "Sign in when Crystal opens" per account. An account that had
    /// it was already signing in at startup without a code, so it is known to sign in without one,
    /// unless its latest sign-in since took a code (`account_otp` is newer evidence).
    fn migrate_startup_signin(&mut self) {
        for key in std::mem::take(&mut self.legacy_startup_signin) {
            if !self.account_otp.contains(&key) { self.account_no_otp.insert(key); }
        }
    }

    fn config_for(&self, base: &Config, account: &Account) -> Config {
        let mut config = base.clone();
        if self.install_selected { config.game_install = Some(PathBuf::from(&self.install_path)); }
        if let Some(path) = self.account_installs.get(&account_key(account)).filter(|p| !p.trim().is_empty()) {
            config.game_install = Some(backend::plain_windows_path(PathBuf::from(path).as_path()));
        }
        config.dat_overlays = if self.launch_with_for(account) == crate::backend::LaunchWith::Native {
            self.account_overlays.get(&account_key(account)).into_iter().flatten()
                .map(|p| backend::plain_windows_path(PathBuf::from(p).as_path())).collect()
        } else { Vec::new() };
        config
    }

    /// Saved settings may hold `\\?\` paths from older builds' folder picker.
    fn plain_paths(&mut self) {
        for choice in self.account_auto_login.values_mut() { *choice = choice.normalized(); }
        for path in self.account_installs.values_mut() { *path = plain_text_path(path); }
        for overlays in self.account_overlays.values_mut() { for path in overlays { *path = plain_text_path(path); } }
        for field in [
            &mut self.install_path,
            &mut self.installer_path,
            &mut self.ashita_path,
            &mut self.windower_path,
            &mut self.loader_path,
        ] {
            let plain = plain_text_path(field);
            if plain != *field {
                *field = plain;
            }
        }
    }
}

fn selected_folder(raw: &str, must_exist: bool) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(
            t("Enter an absolute folder path without parent-directory (..) components.").into(),
        );
    }
    // Plain `C:\...`, not canonicalize's `\\?\C:\...`: this is stored and shown.
    match std::fs::canonicalize(&path).map(|p| backend::plain_windows_path(&p)) {
        Ok(path) if path.is_dir() => Ok(path),
        Ok(_) => Err(t("Select a folder, not a file.").into()),
        Err(_) if must_exist => {
            Err(t("Choose the existing FINAL FANTASY XI installation folder.").into())
        }
        Err(_) => {
            // Resolve the existing parent so a new download folder uses its actual location.
            let mut ancestor = path.as_path();
            let mut suffix = Vec::new();
            loop {
                if let Ok(mut resolved) =
                    std::fs::canonicalize(ancestor).map(|p| backend::plain_windows_path(&p))
                {
                    if !resolved.is_dir() {
                        return Err(t("The download folder's parent is not a directory.").into());
                    }
                    for part in suffix.into_iter().rev() {
                        resolved.push(part);
                    }
                    return Ok(resolved);
                }
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or(t("The folder path could not be resolved."))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or(t("The folder path could not be resolved."))?;
            }
        }
    }
}

fn initial_window_size() -> Size {
    // TEMP: XI_PREVIEW_SIZE=WxH for responsive checks.
    if let Some((w, h)) = std::env::var("XI_PREVIEW_SIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        return Size::new(w, h);
    }
    if preview::has("--preview-small") {
        Size::new(900.0, 650.0)
    } else if preview::has("--preview-wide") {
        // a large window, to check the layout reflows at 1.0.
        Size::new(1920.0, 1080.0)
    } else {
        Size::new(1440.0, 900.0)
    }
}

fn settings_path(config: &Config) -> PathBuf {
    config.experiment.join("iced-launcher/settings.json")
}
/// The headline cache for `lang`: one file per language, so one language's
/// headlines are never shown in the other.
fn news_cache_path(config: &Config, lang: i18n::Lang) -> PathBuf {
    services::cache_path(&config.experiment.join("iced-launcher/news-cache.json"), lang)
}
/// Loads the saved settings and sets the UI language from them: the Windows
/// display language until a choice is saved. The language is set before the
/// load, so a load error is already in it. The window and the background
/// session worker both start this way, so they always agree.
fn load_settings_and_language(config: &Config) -> Result<Settings, String> {
    i18n::set(i18n::system_default());
    let settings = load_settings(config)?;
    if let Some(language) = settings.language {
        i18n::set(language);
    }
    Ok(settings)
}

/// The status line for a launch of `count` accounts through `tools` (none: the native client).
fn launch_status(social_only: bool, all_native: bool, count: usize, tools: &[&str]) -> String {
    if social_only { return t("Signing in to PlayOnline…").into(); }
    match (all_native, count == 1) {
        (true, true) => t("Starting 1 account in parallel. Continue in each game window after sign-in.").into(),
        (true, false) => tf("Starting {count} accounts in parallel. Continue in each game window after sign-in.", &[("count", &count)]),
        (false, true) => tf("Starting 1 account through {tools}.", &[("tools", &join_and(tools))]),
        (false, false) => tf("Starting {count} accounts through {tools}.", &[("count", &count), ("tools", &join_and(tools))]),
    }
}

/// "A", "A and B", "A and B and C"; each join is a translatable sentence part.
fn join_and(names: &[&str]) -> String {
    let mut names = names.iter();
    let mut joined = names.next().map(|first| first.to_string()).unwrap_or_default();
    for next in names {
        joined = tf("{a} and {b}", &[("a", &joined), ("b", next)]);
    }
    joined
}

/// Fixed sentences the window keeps as text and re-words on a language change
/// (each is a `t("…")` key where it is set).
const RESULT_TEXTS: [&str; 3] = [
    "Files have not been checked. Version availability alone does not verify file integrity.",
    "Game folder selected. Play checks its version before FINAL FANTASY XI starts.",
    "Installation path changed. Save settings to select this folder.",
];
const STATUS_TEXTS: [&str; 13] = [
    "Checking whether FINAL FANTASY XI is up…",
    "FFXI is down for maintenance. Starting as soon as it's back…",
    "FFXI's worlds are down. Starting as soon as they're back…",
    "FFXI is back — enter your one-time password to start",
    "Loading saved accounts…",
    "Ready. Select accounts or use Launch all. Games continue when you close Crystal.",
    "Accounts saved.",
    "Enter account details, then save.",
    "Edit account details.",
    "Settings saved. Checking the installed version…",
    "Design preview — synthetic accounts. No account vault or game connection was accessed.",
    "Finding FINAL FANTASY XI…",
    "No installed FFXI folder was found. Run official setup or use Browse.",
];
const ROW_TEXTS: [&str; 13] = [
    "Checking whether FINAL FANTASY XI is up…",
    "FFXI is down for maintenance. Starting as soon as it's back…",
    "FFXI's worlds are down. Starting as soon as they're back…",
    "FFXI is back — enter your one-time password to start",
    "Ready to launch",
    "Private-server client temporarily unavailable",
    "Private-server client temporarily unavailable; saved account kept.",
    "Signed in to PlayOnline · FFXI is closed",
    "Client started · continue in game",
    "Client closed",
    "Starting sign-in…",
    "Checking the game version…",
    "Reconnecting to the background session…",
];
const PATCH_TEXTS: [&str; 7] = [
    "Unavailable",
    "Not checked",
    "Not initialized",
    "FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.",
    "The official patch server is asked when you press Play, before FINAL FANTASY XI starts.",
    "A newer official version is available.",
    "The launcher compares your installation with the official patch service.",
];

/// The current language's text for `text` when it is one of `keys` as it read
/// in `from`.
fn retranslated(text: &str, from: i18n::Lang, keys: &[&'static str]) -> Option<&'static str> {
    keys.iter()
        .find(|key| text == i18n::t_in(from, key))
        .map(|key| i18n::t_in(i18n::current(), key))
}

fn load_settings(config: &Config) -> Result<Settings, String> {
    let bytes = match read_settings_bytes(&settings_path(config))? {
        Some(bytes) => bytes,
        None => return Ok(Settings::default()),
    };
    if bytes.len() > 1024 * 1024 {
        return Err(t("Settings file is too large.").into());
    }
    let mut settings: Settings =
        serde_json::from_slice(&bytes).map_err(|_| t("Settings could not be read."))?;
    settings.plain_paths();
    settings.migrate_startup_signin();
    settings.launch_with = settings.launch_with.retail();
    Ok(settings)
}
fn save_settings(config: &Config, settings: &Settings) -> Result<(), String> {
    use std::io::Write;
    let path = settings_path(config);
    let data = serde_json::to_vec_pretty(settings).map_err(|_| t("Could not encode settings."))?;
    if data.len() > 1024 * 1024 { return Err(t("Launcher settings are too large to save.").into()); }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| t("Could not create the launcher settings folder."))?;
    if let Some(previous) = read_settings_bytes(&path)? {
        if serde_json::from_slice::<Settings>(&previous).is_err() {
            let backup = path.with_extension(format!("invalid-{}.json", backend::unique_id()?));
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(backup)
                .map_err(|_| t("Could not preserve the damaged settings file; it was kept unchanged."))?;
            file.write_all(&previous).and_then(|_| file.sync_all())
                .map_err(|_| t("Could not preserve the damaged settings file; it was kept unchanged."))?;
        }
    }
    fsutil::write_atomic(&path, &data).map_err(|failed| {
        if failed.replacing { t("Could not replace launcher settings.") } else { t("Could not write launcher settings.") }.into()
    })
}

fn read_settings_bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(t("Launcher settings are unavailable.").into()),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| t("Could not read launcher settings."))?;
    if bytes.len() > 1024 * 1024 { return Err(t("Launcher settings file is too large; it was kept unchanged.").into()); }
    Ok(Some(bytes))
}

fn allowed_link(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "www.playonline.com"
                    | "playonline.com"
                    | "sqex.to"
                    | "www.ffxiah.com"
                    | "ffxiah.com"
                    | "www.bg-wiki.com"
                    | "bg-wiki.com"
                    | "wiki.ffo.jp"
                    | "support.na.square-enix.com"
                    | "reshade.me"
            )
        )
}
fn open_url(url: &str) -> Result<(), String> {
    if !allowed_link(url) {
        return Err(t("This link is not an allowed FFXI website.").into());
    }
    shell_open(url).map_err(|_| t("Windows could not open the website.").into())
}
/// Open a client log, or the logs folder itself, in the default app. The target must resolve to
/// somewhere inside `logs_dir` (the account's own logs folder) and a file must be a `.log`.
fn open_log_path(logs_dir: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    let resolved = launch_log::resolve_log_target(logs_dir, target)?;
    // canonicalize yields \\?\C:\..., which ShellExecute handles but Notepad titles show verbatim.
    let plain = backend::plain_windows_path(&resolved);
    shell_open(&plain.to_string_lossy()).map_err(|_| t("Windows could not open the log.").into())
}
/// ShellExecute "open" on a URL or path the caller has already vetted.
fn shell_open(target: &str) -> Result<(), ()> {
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let target = win::wide(target);
        let verb = win::wide("open");
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if result as isize <= 32 {
            return Err(());
        }
    }
    Ok(())
}
fn save_screenshot(path: &std::path::Path, screenshot: Screenshot) -> Result<(), String> {
    imagecrate::save_buffer_with_format(
        path,
        &screenshot.rgba,
        screenshot.width,
        screenshot.height,
        imagecrate::ExtendedColorType::Rgba8,
        imagecrate::ImageFormat::Png,
    )
    .map_err(|e| e.to_string())
}

struct InstanceGuard(windows_sys::Win32::Foundation::HANDLE);
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
fn claim_instance() -> Result<Option<InstanceGuard>, String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
    };
    let name: Vec<u16> = "Local\\XI-Iced-Launcher-Test-1"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let failed = || t("Windows could not start the launcher instance. Close any other copy and try again.").to_owned();
    let background = std::env::args().any(|a| a == startup_task::BACKGROUND_ARG);
    // A closing Crystal holds the mutex while it signs idle sessions out (close_idle_sessions,
    // up to 30 s, plus the park close) but has already destroyed its tray window: wait for it.
    let deadline = Instant::now() + INSTANCE_CLOSE_WAIT;
    loop {
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() { return Err(failed()); }
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS { return Ok(Some(InstanceGuard(handle))); }
        unsafe { CloseHandle(handle) };
        // A background start (Start with Windows) finding Crystal already running changes nothing.
        if background { return Ok(None); }
        // The running Crystal (its hidden window exists from its start) opens or brings forward
        // its own window, whether or not it has one.
        match tray::ask_running_instance_to_open() {
            Some(tray::OpenAnswer::Opened) => return Ok(None),
            // Found but not reachable (a running Crystal at a higher integrity level refuses the
            // message): bring its window forward directly, if it has one.
            Some(tray::OpenAnswer::Unreachable) => {
                use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
                let title = win::wide("Crystal Launcher");
                let window = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
                if !window.is_null() {
                    unsafe {
                        ShowWindow(window, SW_RESTORE);
                        SetForegroundWindow(window);
                    }
                }
                return Ok(None);
            }
            // Closing (or its hidden window is already gone): wait for it to end.
            Some(tray::OpenAnswer::Closing) | None => {}
        }
        if Instant::now() >= deadline { return Err(failed()); }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The longest a new start waits for a closing Crystal to finish its cleanup (idle sign-outs
/// up to 30 s, then the park close).
const INSTANCE_CLOSE_WAIT: Duration = Duration::from_secs(45);

/// Task Scheduler for Start with Windows; unit tests never register a real task.
#[cfg(not(test))]
fn startup_task_runner() -> impl FnMut(&[String]) -> Result<(bool, String), String> { startup_task::schtasks }
#[cfg(test)]
fn startup_task_runner() -> impl FnMut(&[String]) -> Result<(bool, String), String> {
    |_: &[String]| Err("unit tests do not run schtasks".to_owned())
}

/// Close park workers on their own thread, so a re-park or a suspension never holds the UI.
/// If the thread cannot start, dropping them closes their stdin: they clean up themselves.
fn close_parks_later(workers: Vec<session_worker::ParkWorker>) {
    if workers.is_empty() { return; }
    let _ = std::thread::Builder::new().name("park-close".into()).spawn(move || pol_less::park::close_workers(workers));
}

/// Close park workers off the interface thread and wait for them and their hosts to exit (a
/// park still starting included, up to park::EXIT_WAIT): the first step of work that changes
/// the files they hold (an Update, a Repair, setup, a launcher update). `Err`: one still runs,
/// so that work must not write.
async fn close_parks_now(workers: Vec<session_worker::ParkWorker>) -> Result<(), String> {
    if workers.is_empty() { return Ok(()); }
    tokio::task::spawn_blocking(move || pol_less::park::close_workers_and_wait(workers, pol_less::park::EXIT_WAIT)).await
        .unwrap_or_else(|_| Err(t("Could not close the parked game hosts.").into()))
}

fn main() {
    if let Some(result) = self_update::cli(&std::env::args().collect::<Vec<_>>()) {
        if let Err(error) = result { eprintln!("{error}"); std::process::exit(1); }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--windower-profile-editor-worker") {
        let _ = load_settings_and_language(&Config::default());
        let args: Vec<_> = std::env::args_os().collect();
        if args.len() != 3 { std::process::exit(1); }
        std::process::exit(launch_tools::run_profile_editor_worker(std::path::Path::new(&args[2])));
    }
    if std::env::args().any(|a| a == "--batch-worker") {
        let _ = load_settings_and_language(&Config::default());
        std::process::exit(session_worker::run_batch());
    }
    if std::env::args().any(|a| a == "--session-worker") {
        // The worker writes progress and error text that the window shows, so it
        // speaks the window's language: the same saved setting, read the same way.
        let _ = load_settings_and_language(&Config::default());
        // A park worker (pol-spec 07d §1.4) parks a host first and reads its Play later.
        let code = if std::env::args().any(|a| a == pol_less::park::PARK_ARG) { session_worker::run_park() } else { session_worker::run() };
        pol_less::profile_access::finish_pending_launchers();
        std::process::exit(code);
    }
    let preview = preview::flags().is_some();
    // Before the single-instance check, so its error box is in the user's language too.
    if !preview {
        let _ = load_settings_and_language(&Config::default());
    }
    let _instance = if preview {
        None
    } else {
        match claim_instance() {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => return,
            Err(error) => {
                let text = win::wide(&error);
                let title = win::wide("Crystal Launcher");
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                        std::ptr::null_mut(),
                        text.as_ptr(),
                        title.as_ptr(),
                        windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                    );
                }
                return;
            }
        }
    };
    // A verified update downloaded by an earlier run (closed from the window or the tray before
    // it installed) installs now, before anything starts: its helper waits for this process to
    // exit, then starts the new version.
    if !preview && self_update::apply_at_start(&Config::default()) { return; }
    tray::note_ui_thread();
    if let Err(error) = slint_host::run() { eprintln!("{error}"); }
    tray::shutdown();
    if let Err(error) = launcher_close::finish() {
        launch_log::append_history(&Config::default().history_path(), "Crystal", &format!(
            "launcher close failed: pid {}, {error}", std::process::id()), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn app() -> App {
        let (sender, receiver) = mpsc::channel();
        let mut app = App::base(Config::default(), Settings::default(), tick_gate::EventSender::new(sender), receiver, dgvoodoo::State::new(false));
        app.load_preview();
        app
    }
    /// Audit 2026-11-09: launcher close reads the sign-in's flag (IdleSessionClose::with_sign_in_done);
    /// it is set before the Complete goes out, never by progress.
    #[test]
    fn a_sign_in_marks_itself_done_before_its_completion_is_sent() {
        let (sender, events) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let tx = LaunchTx { done: Some(done.clone()), ..LaunchTx::new(tick_gate::EventSender::new(sender), "a".into(), 1) };
        tx.progress("signing in");
        assert!(!done.load(Ordering::Acquire), "progress is no completion");
        tx.complete(Err("failed before its worker started".into()));
        assert!(done.load(Ordering::Acquire));
        assert_eq!(events.try_iter().count(), 2);
    }
    #[test]
    fn game_shutdown_selection_is_scoped_and_stays_signed_out_with_the_setting_off() {
        let mut app=app();
        for (i,row) in app.accounts.iter_mut().enumerate() {
            row.pid=Some(100+i as u32); row.busy=false; row.selected=i==0;
            row.game_running=Some(Arc::new(AtomicBool::new(true)));
            row.alive=Some(Arc::new(AtomicBool::new(true)));
        }
        assert_eq!(app.shutdown_count(true),1);
        assert_eq!(app.update(Message::ShutdownSelected).units(),1);
        assert!(app.accounts[0].shutting_down && !app.accounts[0].can_play());
        assert!(app.accounts[0].resign_declined && app.accounts[0].signed_out);
        let _=app.update(Message::Tick);
        assert!(app.accounts[0].resign_declined && app.accounts[0].signed_out,"the first game-observation tick must preserve explicit shutdown");
        app.accounts[0].pid=None;
        assert!(app.has_sessions() && app.sessions_block_update() && !app.can_restart_launcher(),"POL cleanup remains pending after the game window closes");
        app.accounts[0].pid=Some(100);
        assert!(!app.accounts[1].shutting_down);
        assert_eq!(app.update(Message::ShutdownSelected).units(),0,"a repeated click must not toggle the game countdown");
        assert_eq!(app.update(Message::ShutdownAll).units(),2);
        let key=app.accounts[0].key.clone(); let generation=app.accounts[0].launch_generation;
        let _=app.update(Message::GameShutdownFinished(key,generation,Err("Game refused shutdown".into())));
        assert!(app.accounts[0].pid.is_some() && app.accounts[0].problem && !app.accounts[0].shutting_down);
        assert!(app.accounts[1].shutting_down,"one failure cannot change another client");
        let key=app.accounts[1].key.clone(); let generation=app.accounts[1].launch_generation;
        let _=app.update(Message::GameShutdownFinished(key.clone(),generation,Ok(())));
        let _=app.update(Message::Tick);
        assert!(app.accounts[1].pid.is_none() && !app.accounts[1].problem && !app.accounts[1].shutting_down);
        app.accounts[1].launch_generation+=1; app.accounts[1].pid=Some(999);
        let _=app.update(Message::GameShutdownFinished(key,generation,Ok(())));
        assert_eq!(app.accounts[1].pid,Some(999),"a stale result cannot clear a fresh launch");
    }

    #[test]
    fn reshade_setup_blocks_launch_updates_and_launcher_restart() {
        let mut app = app();
        app.preview = false;
        app.reshade.busy = true;
        assert!(app.launch_blocked());
        assert!(!app.can_restart_launcher());
        assert_eq!(app.main_action(), MainAction::Working);
        let _ = app.run_main_action();
        assert!(app.page == Page::Settings && app.settings_tab == settings_page::Tab::Graphics);
        assert_eq!(app.update(Message::CloseRequested).units(), 0);
        assert_eq!(app.update(Message::ConfirmClose).units(), 0);
        assert_eq!(app.start_maintenance(MaintenanceKind::Update).units(), 0);
        let _ = dgvoodoo::update(&mut app, dgvoodoo::Msg::Toggle(false));
        assert!(app.settings.dgvoodoo);
    }

    /// Proposal 09 phase 1: an interrupted update no longer refuses Play; it is finished
    /// first and the launch goes on, or the rows say why it could not be.
    #[test]
    fn an_interrupted_update_is_finished_before_the_launch_goes_on() {
        let mut app = app();
        app.preview = false;
        let key = app.accounts[0].key.clone();
        let root = std::env::temp_dir().join(format!("crystal-interrupted-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join(".xi-updater")).unwrap();
        let journal = root.join(".xi-updater").join("transaction.json");
        std::fs::write(&journal, b"{}").unwrap();
        app.settings.install_path = root.to_string_lossy().into_owned();
        app.settings.install_selected = true;
        app.viewer = None;
        // The window's report is no update: Play stays the main button and finishes it.
        app.patch = Some(patch::local_report(&root));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Interrupted);
        assert_eq!(app.main_action(), MainAction::Play);
        assert_eq!(app.interrupted_updates(std::slice::from_ref(&key), false), vec![root.clone()]);
        // A sign-in only (startup, Connect) never touches a game folder.
        assert!(app.interrupted_updates(std::slice::from_ref(&key), true).is_empty());
        let clicked = Instant::now();
        let held = HeldLaunch { keys: vec![key.clone()], social_only: false, clicked, trigger: None };
        let recovered = |result: Result<(), String>, game: Option<patch::PatchReport>| WorkerEvent::InterruptedUpdate(Box::new(Recovered {
            held: held.clone(), folders: vec![root.clone()], parks_closed: false, result, game, viewer: None }));
        app.recovering = Some(false);
        assert!(app.launch_blocked(), "nothing starts while files are being restored");
        assert!(!app.files_changing(), "a game folder alone leaves other accounts' parked hosts alone");
        app.recovering = Some(true);
        assert!(app.files_changing(), "PlayOnline's folder closes them");
        // No parked host to blame: the refusal is final.
        app.worker_event(recovered(Err("fixture refusal".into()), None));
        assert!(app.recovering.is_none() && app.accounts[0].problem);
        assert!(app.accounts[0].status.contains("fixture refusal"), "{}", app.accounts[0].status);
        // Finished, but another journal appeared meanwhile: reported, never a loop.
        app.launches = Some(Vec::new());
        app.worker_event(recovered(Ok(()), None));
        assert!(app.launches.as_ref().unwrap().is_empty());
        std::fs::remove_file(&journal).unwrap();
        // Finished: the launch goes on, and the "interrupted" report is read again.
        app.worker_event(recovered(Ok(()), Some(patch::local_report(&root))));
        assert_eq!(app.launches.as_ref().unwrap().as_slice(), &[(vec![key.clone()], false, clicked, None)]);
        assert_ne!(app.patch.as_ref().unwrap().state, patch::PatchState::Interrupted);
        assert!(app.interrupted_updates(&[key], false).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn refused_report() -> patch::PatchReport {
        patch::PatchReport { state: patch::PatchState::Required, local_version: "30260904_1".into(),
            remote_version: "30261006_0".into(), detail: String::new(), files_verified: None }
    }

    /// A game folder at `version` (no record: a fresh install), isolated app state under it,
    /// and the Settings folder pointing there.
    fn play_update_app(label: &str, version: Option<&str>) -> (App, PathBuf) {
        let root = std::env::temp_dir().join(format!("crystal-play-update-{label}-{}", backend::unique_id().unwrap()));
        let game = root.join("FINAL FANTASY XI");
        std::fs::create_dir_all(&game).unwrap();
        for name in ["FFXi.dll", "FFXiMain.dll"] { std::fs::write(game.join(name), b"fixture").unwrap(); }
        if let Some(version) = version { std::fs::write(game.join("patch.ver"), patch::encode_applied_record(version, 1).unwrap()).unwrap(); }
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        app.viewer = None;
        app.config = Config { experiment: root.join("data"), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.settings.install_path = game.to_string_lossy().into_owned();
        app.settings.install_selected = true;
        (app, root)
    }

    /// A row signed in at the PlayOnline main menu (its host `pid`), its Play in flight.
    fn signed_in(app: &mut App, index: usize, pid: u32, clicked: Instant) -> String {
        let row = &mut app.accounts[index];
        row.pid = Some(pid);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(Arc::new(AtomicBool::new(false)));
        row.busy = true;
        row.play_timing = Some(launch_timing::Timeline::begin_at(clicked));
        row.key.clone()
    }

    fn refused(app: &mut App, key: &str) {
        let message = patch::gate(patch::LaunchCheck::Update(refused_report())).unwrap_err();
        app.worker_event(WorkerEvent::StartGame(key.to_owned(), Some(refused_report()), Err(message)));
    }

    fn generation(app: &App) -> u64 { app.play_updates.jobs[0].generation }

    /// A row signed in and idle at the PlayOnline main menu (its host `pid`).
    fn idle_signed_in(app: &mut App, index: usize, pid: u32) -> String {
        let key = signed_in(app, index, pid, Instant::now());
        let row = &mut app.accounts[index];
        (row.busy, row.play_timing) = (false, None);
        key
    }

    /// A play flag read made now, during maintenance, with `$patch_enable` as given.
    fn closed_flag(app: &mut App, patch: Option<bool>) {
        let _ = app.update(Message::PlayFlagLoaded(play_flag::Reading { at: chrono::Utc::now(),
            result: Ok(play_flag::Flag::Down(Some("The game is unavailable for play during maintenance.".into()))), patch }));
    }

    fn maintenance_signins(app: &App) -> Vec<Vec<String>> {
        app.launches.as_ref().unwrap().iter()
            .filter(|(_, social, _, trigger)| *social && *trigger == Some(launch_timing::LaunchTrigger::MaintenanceSignIn))
            .map(|(keys, ..)| keys.clone()).collect()
    }

    /// Owner, 1.1.0: "I want it to update as soon as update is available". In maintenance, at
    /// `$patch_enable`'s 0 → 1: the accounts Crystal may sign in by itself and a waiting Play
    /// without a code sign in (once per trigger; an OTP account never), one W20 runs for the
    /// folder from a session at the main menu, "update required" runs the in-session update
    /// with no Play waiting on it, and the outcome is installed, game not started.
    #[test]
    fn maintenance_checks_the_version_and_installs_the_update_without_starting_the_game() {
        i18n::set(i18n::Lang::En);
        let (mut app, root) = play_update_app("watch", Some("30260904_1"));
        let game = root.join("FINAL FANTASY XI");
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        assert_eq!(app.main_action(), MainAction::Maintenance);
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        app.settings.auto_sign_in = true;
        app.settings.account_no_otp.extend([keys[0].clone(), keys[2].clone()]);
        app.settings.account_otp.insert(keys[1].clone());
        // Plays that wait: an OTP account with its code, and an account that needs none.
        app.accounts[1].otp = "123456".into();
        app.start_launch(vec![keys[1].clone(), keys[2].clone()], false, Instant::now(), None);
        assert!(app.is_waiting(&keys[1]) && app.is_waiting(&keys[2]));
        // patch_enable 0: nothing signs in and nothing is checked.
        closed_flag(&mut app, Some(false));
        assert!(app.launches.as_ref().unwrap().is_empty() && app.patch_watch.checks.is_empty());
        // The flip: one maintenance sign-in, never the OTP account; not again in this cycle.
        closed_flag(&mut app, Some(true));
        assert_eq!(maintenance_signins(&app), vec![vec![keys[0].clone(), keys[2].clone()]]);
        closed_flag(&mut app, Some(true));
        let _ = app.update(Message::Tick);
        assert_eq!(maintenance_signins(&app).len(), 1, "once per trigger");
        // Signed in at the main menu: one W20 for the folder.
        idle_signed_in(&mut app, 0, 41);
        let _ = app.update(Message::Tick);
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 1, "one check per folder per trigger");
        assert!(play_update::same_folder(&app.patch_watch.checks[0].0, &game) && app.patch_watch.checks[0].1 == keys[0]);
        assert_eq!(play_update::watch_line(&app, &keys[0]).unwrap().headline, "Checking for the FFXI update");
        // Update required: the update a Play's W20 would start, with no Play waiting on it.
        app.worker_event(WorkerEvent::PatchCheck(app.patch_watch.checks[0].0.clone(), keys[0].clone(), Ok((refused_report(), false, None))));
        assert_eq!(app.play_updates.spawned.len(), 1);
        assert!(app.play_updates.jobs[0].probe && app.play_updates.jobs[0].waiters.is_empty() && !app.accounts[0].busy);
        assert_eq!(play_update::watch_line(&app, &keys[0]).unwrap().headline, "Downloading the FFXI update");
        let row = account_row::row_view(&app, &app.accounts[0]);
        assert!(row.detail.starts_with("Downloading the FFXI update") && matches!(row.action, Some((_, Message::CancelPlayUpdate(_), _))));
        let job = generation(&app);
        app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Finished(Ok(play_update::Done { summary: "Updated.".into(), report: None }))));
        assert!(app.play_updates.jobs.is_empty() && app.w20_skipped.is_empty());
        assert_eq!(app.launches.as_ref().unwrap().iter().filter(|(_, social, ..)| !social).count(), 0, "installed, game not started");
        assert_eq!(play_update::watch_line(&app, &keys[0]).unwrap().headline, "Update installed · waiting for servers");
        assert_eq!(app.status, "Update installed · waiting for servers");
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 1, "nothing more once installed");
        // The worlds open: the waiting Plays go through the normal start path (W20 → GameStart).
        let _ = app.update(Message::PlayFlagLoaded(play_flag::Reading { at: chrono::Utc::now(), result: Ok(play_flag::Flag::Up), patch: Some(true) }));
        assert!(!app.patch_watch.active() && play_update::watch_line(&app, &keys[0]).is_none());
        let plays: Vec<_> = app.launches.as_ref().unwrap().iter().filter(|(_, social, ..)| !social).collect();
        assert_eq!(plays.len(), 1, "the waiting Plays start together through the normal start path");
        assert_eq!(plays[0].0, vec![keys[1].clone(), keys[2].clone()]);
        let history = std::fs::read_to_string(app.config.history_path()).unwrap();
        for line in ["play flag: $patch_enable unknown → 0", "play flag: $patch_enable 0 → 1", "maintenance sign-in: 2 accounts",
            "maintenance update check (W20): update required, installed 30260904_1, official 30261006_0",
            "maintenance update: started, official 30261006_0", "maintenance update: installed 30261006_0: Updated."] {
            assert!(history.contains(line), "{line}\n{history}");
        }
        for row in &app.accounts {
            for secret in [&row.account.label, &row.account.pol_id, &row.account.se_id].into_iter().filter(|text| text.len() > 2) {
                assert!(!history.contains(secret.as_str()), "no account names in the history");
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The schedule in the window: nothing outside maintenance, at once at first sight with a
    /// session at the main menu, then 5 min ± 30 s after "current", none while
    /// `$patch_enable` is 0; a session maintenance ended signs in again once per trigger.
    #[test]
    fn maintenance_version_checks_follow_the_schedule_and_never_run_outside_maintenance() {
        i18n::set(i18n::Lang::En);
        let (mut app, root) = play_update_app("watch-schedule", Some("30261006_0"));
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        app.settings.auto_sign_in = true;
        app.settings.account_no_otp.extend([keys[0].clone(), keys[1].clone()]);
        let key = idle_signed_in(&mut app, 0, 51);
        // Outside maintenance: no flag read is wanted, and a 1 changes nothing.
        status_read(&mut app, services::Availability::Operational, None);
        assert!(!app.flag_wanted());
        let _ = app.update(Message::PlayFlagLoaded(play_flag::Reading { at: chrono::Utc::now(), result: Ok(play_flag::Flag::Up), patch: Some(true) }));
        let _ = app.update(Message::Tick);
        assert!(app.patch_watch.checks.is_empty() && !app.patch_watch.active() && maintenance_signins(&app).is_empty());
        // Maintenance first seen with patch_enable 1 and a session at the main menu: at once.
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        app.play_flag = play_flag::Known::default();
        assert!(app.flag_wanted());
        closed_flag(&mut app, Some(true));
        assert_eq!(app.patch_watch.checks.len(), 1);
        assert_eq!(maintenance_signins(&app), vec![vec![keys[1].clone()]], "the account not signed in signs in");
        let folder = app.patch_watch.checks[0].0.clone();
        let current = patch::PatchReport { state: patch::PatchState::Current, local_version: "30261006_0".into(),
            remote_version: "30261006_0".into(), detail: String::new(), files_verified: None };
        app.worker_event(WorkerEvent::PatchCheck(folder.clone(), key.clone(), Ok((current.clone(), true, Some("connect 180 ms".into())))));
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 1, "not before the next 5 minutes");
        let next = app.patch_watch.next_due().unwrap();
        let now = Instant::now();
        assert!(next > now + patch_watch::RECHECK - patch_watch::JITTER - Duration::from_secs(5) && next <= now + patch_watch::RECHECK + patch_watch::JITTER);
        assert!(app.next_timed_work().is_some_and(|at| at <= next), "a quiet window wakes for it");
        app.patch_watch.checked(&folder, false, now - patch_watch::RECHECK * 2, Duration::ZERO);
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 2, "due again");
        assert!(app.play_updates.spawned.is_empty(), "current never updates");
        // A check that sent nothing (the session left its main menu) waits for the schedule too.
        app.worker_event(WorkerEvent::PatchCheck(folder.clone(), key.clone(), Err("PlayOnline did not return to its main menu in time, so FFXI was not started.".into())));
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 2, "no tight loop");
        // patch_enable 0: no checks, however long.
        closed_flag(&mut app, Some(false));
        app.patch_watch.checked(&folder, false, now - patch_watch::RECHECK * 2, Duration::ZERO);
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 2);
        // Back to 1 (a flip): checked at once, and the account whose session maintenance ended
        // (it never signed in here) signs in once more.
        closed_flag(&mut app, Some(true));
        assert_eq!(app.patch_watch.checks.len(), 3);
        assert_eq!(maintenance_signins(&app).len(), 2);
        closed_flag(&mut app, Some(true));
        assert_eq!(maintenance_signins(&app).len(), 2, "once per trigger");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Review findings: an answer that comes once `$patch_enable` is no longer 1 starts no
    /// update; "update required" with no update possible stops the folder until the next flip;
    /// a waiting Play whose account's OTP use is unknown is not signed in for the check.
    #[test]
    fn a_late_or_unusable_update_required_starts_nothing_and_stops_further_checks() {
        i18n::set(i18n::Lang::En);
        let (mut app, root) = play_update_app("watch-late", Some("30260904_1"));
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        app.start_launch(vec![keys[2].clone()], false, Instant::now(), None);
        assert!(app.is_waiting(&keys[2]));
        idle_signed_in(&mut app, 0, 61);
        closed_flag(&mut app, Some(true));
        assert!(maintenance_signins(&app).is_empty(), "OTP use unknown: not signed in");
        assert_eq!(app.patch_watch.checks.len(), 1);
        let folder = app.patch_watch.checks[0].0.clone();
        // $patch_enable 0 before the answer: no update.
        closed_flag(&mut app, Some(false));
        app.worker_event(WorkerEvent::PatchCheck(folder.clone(), keys[0].clone(), Ok((refused_report(), false, None))));
        assert!(app.play_updates.spawned.is_empty() && app.play_updates.jobs.is_empty());
        // The flip checks at once; "required" with no update possible (the host is gone).
        closed_flag(&mut app, Some(true));
        assert_eq!(app.patch_watch.checks.len(), 2);
        app.accounts[0].pid = None;
        app.worker_event(WorkerEvent::PatchCheck(folder.clone(), keys[0].clone(), Ok((refused_report(), false, None))));
        assert!(app.play_updates.spawned.is_empty());
        let later = Instant::now() + patch_watch::RECHECK * 10;
        assert!(!app.patch_watch.due(&folder, later), "no more W20s with no download");
        closed_flag(&mut app, Some(false));
        closed_flag(&mut app, Some(true));
        assert!(app.patch_watch.due(&folder, Instant::now()), "until the next flip");
        let history = std::fs::read_to_string(app.config.history_path()).unwrap();
        assert!(history.contains("maintenance update: not started") && !history.contains("maintenance update: started"), "{history}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Review finding: one W20 at a time on a session. Two sessions of one folder give one
    /// check; a Play on the session with a check out (a wait maintenance's end releases, or a
    /// click) waits for its answer, then starts once.
    #[test]
    fn a_play_waits_for_the_check_out_on_its_session() {
        i18n::set(i18n::Lang::En);
        let (mut app, root) = play_update_app("watch-overlap", Some("30261006_0"));
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        closed_flag(&mut app, Some(false));
        idle_signed_in(&mut app, 0, 71);
        idle_signed_in(&mut app, 1, 72);
        app.start_launch(vec![keys[0].clone()], false, Instant::now(), None);
        assert!(app.is_waiting(&keys[0]));
        closed_flag(&mut app, Some(true));
        let _ = app.update(Message::Tick);
        assert_eq!(app.patch_watch.checks.len(), 1, "one check per folder");
        let (folder, key) = app.patch_watch.checks[0].clone();
        assert!(app.patch_watch.in_flight(&key));
        let plays = |app: &App| app.launches.as_ref().unwrap().iter().filter(|(keys, social, ..)| !social && keys.contains(&key)).count();
        // The worlds open while it is out: the wait is not released on that session yet.
        let _ = app.update(Message::PlayFlagLoaded(play_flag::Reading { at: chrono::Utc::now(), result: Ok(play_flag::Flag::Up), patch: Some(true) }));
        assert!(!app.patch_watch.active() && app.patch_watch.in_flight(&key));
        let _ = app.update(Message::Tick);
        assert_eq!(plays(&app), 0);
        assert!(key == keys[0] && app.is_waiting(&key));
        // A click there waits too.
        let _ = app.update(Message::CancelWait(vec![keys[0].clone()]));
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        let _ = app.update(Message::Tick);
        assert_eq!(plays(&app), 0, "no second W20 on the session");
        assert!(app.accounts.iter().any(|row| row.key == key && row.queued_play.is_some()));
        // The answer: then it starts, once.
        let current = patch::PatchReport { state: patch::PatchState::Current, local_version: "30261006_0".into(),
            remote_version: "30261006_0".into(), detail: String::new(), files_verified: None };
        app.worker_event(WorkerEvent::PatchCheck(folder, key.clone(), Ok((current, true, None))));
        let _ = app.update(Message::Tick);
        assert_eq!(plays(&app), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A Friends request past its time limit is reported as stalled, but a later read that finds
    /// it done (the loader's mail deadlines are longer) counts it: a send relists Sent.
    #[test]
    fn a_friends_result_after_its_time_limit_still_counts() {
        let mut app = app();
        app.friends.pending = Some(("req-1".into(), Instant::now() - FRIENDS_REQUEST_LIMIT - Duration::from_secs(1)));
        app.friends.mail.listed[2] = Some(Vec::new());
        app.expire_friends_request();
        assert!(app.friends.pending.is_none() && app.friends.mail.stalled && !app.friends.error.is_empty());
        assert_eq!(app.friends_expired.as_deref(), Some("req-1"));
        let mut snapshot = friends::Snapshot::default();
        snapshot.social.id = Some("req-1".into());
        snapshot.social.action = Some(crate::social::Action::Send);
        app.friends_read(true, Some(snapshot));
        assert!(app.friends_expired.is_none() && !app.friends.mail.stalled && app.friends.error.is_empty());
        assert!(app.friends.mail.listed[2].is_none(), "Sent is listed again");
    }

    /// An expired send's draft no longer shows as sending (Send stays usable) once another
    /// request replaces the expired one, which can then no longer be matched.
    #[test]
    fn a_new_friends_request_drops_an_expired_sends_draft() {
        let mut app = app();
        app.friends.pending = Some(("req-1".into(), Instant::now() - FRIENDS_REQUEST_LIMIT - Duration::from_secs(1)));
        app.friends.outgoing = Some(friends::Outgoing { uid: 7, subject: String::new(), message: "hi".into(), sent: false });
        app.expire_friends_request();
        assert!(app.friends.outgoing.is_some(), "kept while the late result may still match");
        app.friends_request_started("req-2".into(), None);
        assert!(app.friends_expired.is_none() && app.friends.outgoing.is_none());
        assert_eq!(app.friends.pending.as_ref().map(|(id, _)| id.as_str()), Some("req-2"));
        // A draft already sent is not touched.
        app.friends_expired = Some("req-2".into());
        app.friends.outgoing = Some(friends::Outgoing { uid: 7, subject: String::new(), message: "hi".into(), sent: true });
        app.friends_request_started("req-3".into(), None);
        assert!(app.friends.outgoing.as_ref().is_some_and(|o| o.sent));
    }

    /// Remove confirmed while a sync request is in flight: nothing goes out, the bar stays
    /// open with its reason, and conversation sync waits (so the reason is not cleared).
    #[test]
    fn remove_confirmed_during_a_sync_request_waits_with_the_bar_open() {
        let mut app = app();
        app.preview = false;
        app.friends.pending = Some(("sync-1".into(), Instant::now()));
        let friend = friends::Friend { name: "Ayame".into(), alternate: false, slot: 1, uid: Some(9), online: None, location: None, character: None, portrait: None };
        let _ = app.update(Message::FriendsRemove(friend));
        let _ = app.update(Message::FriendsRemoveConfirm);
        assert!(app.friends.remove.is_some());
        assert_eq!(app.friends.pending.as_ref().map(|(id, _)| id.as_str()), Some("sync-1"));
        assert_eq!(app.friends.error, t("Wait for the current PlayOnline request to finish."));
        app.friends.pending = None;
        assert!(app.friends.next_sync().is_none(), "sync waits for the open Remove");
        let _ = app.drive_sync();
        assert_eq!(app.friends.error, t("Wait for the current PlayOnline request to finish."));
    }

    /// A row's note follows the language and is removed by what it is: a language switch while
    /// it signs back in after its game no longer leaves the old language's note behind.
    #[test]
    fn a_row_note_follows_the_language_and_goes_after_a_switch() {
        let mut app = app();
        app.accounts[0].status = t("Ready to launch").into();
        app.accounts[0].set_note(Some(RowNote::ResignAfterGame));
        let _ = app.update(Message::SetLanguage(i18n::Lang::Ja));
        let shown = app.accounts[0].status_text().into_owned();
        assert_eq!(shown, format!("{} · {}", t("Ready to launch"), t("Signing back in after the game closed.")));
        app.accounts[0].drop_resign();
        assert_eq!(app.accounts[0].status_text(), t("Ready to launch"));
        // A status that replaces the one the note was added to ends the note too.
        app.accounts[0].set_note(Some(RowNote::NetworkGone));
        app.accounts[0].status = t("Client closed").into();
        assert_eq!(app.accounts[0].status_text(), t("Client closed"));
        i18n::set(i18n::Lang::En);
    }

    /// A Play with nothing launchable before the first status read holds nothing and leaves no
    /// "checking" line behind that nothing would clear.
    #[test]
    fn an_empty_play_before_the_first_status_read_is_not_held() {
        let mut app = app();
        app.preview = false;
        app.status_read_done = false;
        app.status = "Ready.".into();
        app.start_launch(Vec::new(), false, Instant::now(), None);
        assert!(app.held_plays.is_empty());
        assert_eq!(app.status, "Ready.");
    }

    /// The W20 skip after an in-session update is one-shot: a start that waits (here for a
    /// maintenance that began meanwhile) runs its own W20 when it is released later.
    #[test]
    fn a_play_update_skip_does_not_outlive_a_start_that_waits() {
        let (mut app, root) = play_update_app("skip-wait", Some("30260904_1"));
        let clicked = Instant::now();
        let key = signed_in(&mut app, 0, 101, clicked);
        refused(&mut app, &key);
        let job = generation(&app);
        status_read(&mut app, services::Availability::Maintenance, None);
        app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Finished(Ok(play_update::Done { summary: "Updated.".into(), report: None }))));
        assert!(app.is_waiting(&key), "the release waits for FFXI");
        assert!(app.w20_skipped.is_empty(), "no skip survives the wait");
        status_read(&mut app, services::Availability::Operational, None);
        app.play_flag.note(play_flag::Reading { at: chrono::Utc::now(), result: Ok(play_flag::Flag::Up), patch: None });
        app.release_waiting_plays(true);
        assert_eq!(app.launches.as_ref().unwrap().last().map(|launch| launch.0.clone()), Some(vec![key.clone()]));
        assert!(app.w20_skipped.is_empty(), "the released Play runs its own W20");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Proposal 09 phase 2: the W20 of a signed-in Play says "update". The account stays signed
    /// in while its folder updates, using that W20's verdict (no second version request), another
    /// account of the folder that the W20 refused joins the same job, and both start through the
    /// signed-in path with no version check once it is installed.
    #[test]
    fn a_refused_play_updates_in_its_session_then_starts_without_a_second_check() {
        let (mut app, root) = play_update_app("session", Some("30260904_1"));
        let game = root.join("FINAL FANTASY XI");
        let clicked = Instant::now() - Duration::from_secs(3);
        let first = signed_in(&mut app, 0, 101, clicked);
        let second = signed_in(&mut app, 1, 102, clicked);
        refused(&mut app, &first);
        // One job, for the account's own folder, on the W20's official version.
        assert_eq!(app.play_updates.spawned.len(), 1);
        let (folder, official, closed) = &app.play_updates.spawned[0];
        assert!(play_update::same_folder(folder, &game) && official == "30261006_0" && closed.is_none());
        assert_eq!(app.main_action(), MainAction::Play, "no Update button: Play is the update");
        assert!(app.refused_plays.is_empty(), "nothing waits for a Downloads update");
        let row = &app.accounts[0];
        assert!(row.busy && !row.problem && row.pid == Some(101), "still signed in: {}", row.status);
        let line = crate::play_update::line(&app, &first).unwrap();
        assert!(line.headline.starts_with(t("Updating FINAL FANTASY XI")), "{}", line.headline);
        assert_eq!(line.cancel, Some(generation(&app)));
        // The second account's W20 asked for the same update: it waits for this one.
        refused(&mut app, &second);
        assert_eq!(app.play_updates.spawned.len(), 1, "one job per folder");
        assert_eq!(app.play_updates.jobs[0].waiters.iter().map(|w| w.key.as_str()).collect::<Vec<_>>(), [first.as_str(), second.as_str()]);
        // A third account's Play now waits at its main menu, before its own version check.
        let third = app.accounts[2].key.clone();
        {
            let row = &mut app.accounts[2];
            (row.pid, row.alive, row.game_running) = (Some(103), Some(Arc::new(AtomicBool::new(true))), Some(Arc::new(AtomicBool::new(false))));
        }
        app.launches = None;
        let pressed = Instant::now();
        app.start_launch_now(vec![third.clone()], false, pressed, None, false);
        app.launches = Some(Vec::new());
        assert_eq!(app.accounts[2].queued_play, Some(pressed));
        assert!(matches!(app.play_update_of(&third), Some((_, false))));
        assert!(crate::play_update::line(&app, &third).unwrap().headline.starts_with("Waiting for the update"));
        app.start_queued_plays();
        assert!(app.launches.as_ref().unwrap().is_empty() && app.accounts[2].queued_play.is_some(), "held while the folder updates");
        // Downloaded; a game on another account still runs from the folder: it waits for it.
        let job = generation(&app);
        app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Waiting(Some("Alice".into()))));
        let line = crate::play_update::line(&app, &first).unwrap();
        assert!(line.headline.contains("Alice") && line.label == t("Update ready"), "{}", line.headline);
        app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Installing));
        // Installed: both start, signed in, without another W20.
        app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Finished(Ok(play_update::Done {
            summary: "3 files updated. Applied version 30261006_0.".into(), report: None }))));
        assert!(app.play_updates.jobs.is_empty());
        let reissued = Some(launch_timing::LaunchTrigger::ReissuedPlay);
        assert_eq!(app.launches.as_ref().unwrap().as_slice(), &[(vec![first.clone()], false, clicked, reissued), (vec![second.clone()], false, clicked, reissued)]);
        assert_eq!(app.w20_skipped, [first.clone(), second.clone()], "both start without a second W20");
        assert!(app.accounts.iter().take(2).all(|row| row.pid.is_some()), "nobody signed out");
        // The account that waited before its W20 is released now, with its own check.
        app.start_queued_plays();
        assert_eq!(app.launches.as_ref().unwrap().last().unwrap().0, vec![third.clone()]);
        assert!(!app.w20_skipped.contains(&third));
        // The signed-in start with the skip (its thread finds no live session here).
        app.launches = None;
        app.accounts[0].busy = false;
        app.settings.loader_path = root.join("no-loader.exe").to_string_lossy().into_owned();
        app.start_launch_now(vec![first.clone()], false, clicked, reissued, true);
        // Wait for that start thread to finish (it writes into `root`) before removing it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match app.receiver.recv_timeout(left) {
                Ok(WorkerEvent::Launch(key, _, event)) if key == first && matches!(*event, WorkerEvent::StartGame(..)) => break,
                Ok(_) => {}
                Err(error) => panic!("the signed-in start did not finish: {error}"),
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Cancel and a network drop pause the update: the rows say Play continues it (its verified
    /// downloads stay in the folder's cache), nothing starts, and a Play that waited stops waiting.
    #[test]
    fn a_cancelled_or_failed_play_update_is_paused_and_play_continues_it() {
        for cancel in [true, false] {
            let (mut app, root) = play_update_app("pause", None);
            let clicked = Instant::now();
            let key = signed_in(&mut app, 0, 101, clicked);
            refused(&mut app, &key);
            assert_eq!(app.play_updates.spawned.len(), 1, "a fresh install updates in its session too");
            let job = generation(&app);
            if cancel {
                let _ = app.update(Message::CancelPlayUpdate(job));
                assert!(app.play_updates.jobs[0].op.cancelled());
                assert_eq!(crate::play_update::line(&app, &key).unwrap().cancel, None);
            }
            let error = if cancel { t("Cancelled. Verified downloads are kept for resuming.").to_owned() } else { "Patch server timed out".to_owned() };
            app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Finished(Err(error))));
            let row = &app.accounts[0];
            assert!(!row.busy && row.status == t("Update paused. Press Play to continue.") && row.problem != cancel);
            assert!(row.pid.is_some() && row.can_play(), "still signed in; Play continues it");
            assert!(app.launches.as_ref().unwrap().is_empty() && app.play_updates.jobs.is_empty());
            assert!(cancel || app.status.contains("Patch server timed out"), "{}", app.status);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// A signed-in host that holds a game file (its install could not proceed) was signed out:
    /// that one account signs in again after the install (one new sign-in, still one click). An
    /// account with a one-time code asks for a new one instead.
    #[test]
    fn an_account_whose_host_held_a_file_signs_in_again_after_the_install() {
        for otp in [false, true] {
            let (mut app, root) = play_update_app("fallback", Some("30260904_1"));
            let clicked = Instant::now();
            let key = signed_in(&mut app, 0, 101, clicked);
            if otp { app.settings.account_otp.insert(key.clone()); }
            refused(&mut app, &key);
            let job = generation(&app);
            app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::SignedOut(key.clone())));
            assert!(app.play_updates.jobs[0].waiters[0].fallback);
            // Its host ended with the sign-out.
            app.accounts[0].alive.as_ref().unwrap().store(false, Ordering::Release);
            let _ = app.update(Message::Tick);
            assert!(app.accounts[0].pid.is_none() && crate::play_update::line(&app, &key).is_some(), "the row still shows the update");
            app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Finished(Ok(play_update::Done { summary: "Updated.".into(), report: None }))));
            assert!(!app.w20_skipped.contains(&key), "a new sign-in checks its version");
            if otp {
                assert!(app.launches.as_ref().unwrap().is_empty());
                assert!(app.accounts[0].problem && app.accounts[0].status == t("FINAL FANTASY XI is updated. Enter a new one-time code, then press Play."), "{}", app.accounts[0].status);
            } else {
                assert_eq!(app.launches.as_ref().unwrap().as_slice(), &[(vec![key.clone()], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay))]);
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// A batch reports one account's update refusal to every account. One whose own folder is
    /// another, already at that version, starts on its own; one without a session, or a report
    /// without a usable official version, keeps the refusal (nothing to update in-session).
    #[test]
    fn only_folders_the_refusal_applies_to_update_and_a_current_batch_account_starts() {
        let (mut app, root) = play_update_app("batch", Some("30260904_1"));
        let current = root.join("current");
        std::fs::create_dir_all(&current).unwrap();
        for name in ["FFXi.dll", "FFXiMain.dll"] { std::fs::write(current.join(name), b"fixture").unwrap(); }
        std::fs::write(current.join("patch.ver"), patch::encode_applied_record("30261006_0", 1).unwrap()).unwrap();
        app.settings.account_installs.insert(account_key(&app.accounts[1].account), current.to_string_lossy().into_owned());
        let clicked = Instant::now();
        let (first, second) = (signed_in(&mut app, 0, 101, clicked), signed_in(&mut app, 1, 102, clicked));
        let message = patch::gate(patch::LaunchCheck::Update(refused_report())).unwrap_err();
        for key in [&first, &second] {
            app.accounts.iter_mut().find(|row| row.key == **key).unwrap().batch_requested = true;
            app.worker_event(WorkerEvent::Complete((*key).clone(), Err(message.clone())));
        }
        assert_eq!(app.play_updates.spawned.len(), 1);
        assert!(app.play_updates.jobs[0].has(&first) && !app.play_updates.jobs[0].has(&second));
        assert_eq!(app.launches.as_ref().unwrap().as_slice(), &[(vec![second.clone()], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay))]);
        // No official version in the report (an unreadable record sends no W20): no job.
        let (mut app, other) = play_update_app("no-version", Some("30260904_1"));
        let key = signed_in(&mut app, 0, 101, clicked);
        let mut report = refused_report();
        report.remote_version = t("Unavailable").into();
        app.worker_event(WorkerEvent::StartGame(key.clone(), Some(report), Err("Update required".into())));
        assert!(app.play_updates.spawned.is_empty() && app.accounts[0].problem && !app.accounts[0].busy);
        std::fs::remove_dir_all(other).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    /// PlayOnline's own update is driven by the Play it refused: it starts by itself (PlayOnline
    /// only, `run_combined(Some(viewer), None, ..)`), and its success starts that Play again. A Connect (no Play) only
    /// shows the verdict; a game running elsewhere says why it can't run now.
    #[test]
    fn an_outdated_playonline_sign_in_updates_playonline_from_that_play_then_plays() {
        let (mut app, root) = play_update_app("viewer", None);
        app.patch = None;
        let key = app.accounts[0].key.clone();
        let outdated = viewer::ConnectVerdict::Outdated { local: "20110829_E".into(), remote: "20261001_0".into() }.message();
        // A sign-in only (Connect) refused by an outdated PlayOnline is no Play to start again.
        app.accounts[0].busy = true;
        app.accounts[0].launch_trigger = Some(launch_timing::LaunchTrigger::Connect);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(outdated.clone())));
        assert!(app.refused_plays.is_empty() && app.maintenance.is_none());
        let shown = app.viewer.as_ref().unwrap();
        assert!(shown.requires_update());
        assert_eq!((shown.version.local_version.as_str(), shown.version.remote_version.as_str()), ("20110829_E", "20261001_0"));
        assert_eq!(app.main_action(), MainAction::Play);
        // Another account in game: PlayOnline can't update now, and the row says so.
        app.accounts[1].pid = Some(55);
        app.accounts[1].game_running = Some(Arc::new(AtomicBool::new(true)));
        let clicked = Instant::now();
        app.accounts[0].busy = true;
        app.accounts[0].launch_trigger = Some(launch_timing::LaunchTrigger::Play);
        app.accounts[0].queued_play = Some(clicked);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(outdated.clone())));
        assert!(app.maintenance.is_none() && app.accounts[0].status.contains("Close FINAL FANTASY XI"), "{}", app.accounts[0].status);
        // Nothing else signed in: the same refusal updates PlayOnline by itself, PlayOnline only.
        (app.accounts[1].pid, app.accounts[1].game_running) = (None, None);
        app.accounts[0].busy = true;
        app.accounts[0].queued_play = Some(clicked);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(outdated)));
        assert!(app.maintenance == Some(MaintenanceKind::Update), "{}", app.status);
        assert_eq!(app.update_scope, (true, false));
        assert_eq!(app.maintenance_label(), t("Updating PlayOnline"));
        assert_eq!(app.play_updates.tasks.len(), 1, "the Tick returns its task");
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Update, Ok("Updated".into()), None));
        assert!(app.launches.as_ref().unwrap().iter().any(|(keys, social, at, trigger)|
            keys == std::slice::from_ref(&key) && !social && *at == clicked && *trigger == Some(launch_timing::LaunchTrigger::ReissuedPlay)));
        std::fs::remove_dir_all(root).unwrap();
    }


    #[test]
    fn reshade_toggle_preserves_a_running_sessions_renderer() {
        let mut app = app();
        app.settings.reshade.enabled = true;
        app.reshade.installed = true;
        app.preview = false;
        app.accounts[0].pid = Some(12345);
        let _ = reshade::update(&mut app, reshade::Msg::Toggle(false));
        assert!(app.settings.reshade.enabled);
        let _ = dgvoodoo::update(&mut app, dgvoodoo::Msg::Toggle(false));
        assert!(app.settings.dgvoodoo && app.settings.reshade.enabled);
    }

    #[test]
    fn reshade_ignores_install_checks_from_before_a_newer_check() {
        let mut app = app();
        app.preview = false;
        app.reshade.installed = true;
        let _ = reshade::update(&mut app, reshade::Msg::Refresh);
        let _ = reshade::update(&mut app, reshade::Msg::Checked(0, false, "stale.ini".into()));
        assert!(app.reshade.installed && app.reshade.preset.is_empty());
        let _ = reshade::update(&mut app, reshade::Msg::Checked(1, true, "current.ini".into()));
        assert_eq!(app.reshade.preset, "current.ini");
        let _ = reshade::update(&mut app, reshade::Msg::ChoosePreset);
        assert!(app.reshade.busy);
        let _ = reshade::update(&mut app, reshade::Msg::Picked(None));
        let _ = reshade::update(&mut app, reshade::Msg::Checked(1, false, "stale.ini".into()));
        assert!(app.reshade.installed && app.reshade.preset == "current.ini");
    }

    #[test]
    fn ffxiah_lookup_tracks_world_changes_and_blocks_background_controller_actions() {
        let mut app = app();
        app.ah_query = "copper".into();
        let _ = app.update(Message::SearchAh);
        assert!(app.ah_lookup.open && app.controller_modal());
        let previous = app.ah_lookup.generation;
        let _ = app.update(Message::AhServerChanged("Bismarck"));
        assert_eq!(app.ah_lookup.world, "Bismarck");
        assert!(app.ah_lookup.generation > previous);
        let states: Vec<_> = app.accounts.iter().map(|a| a.busy).collect();
        let page = app.page;
        let _ = app.update(Message::ControllerPlay);
        let _ = app.update(Message::ControllerPage(1));
        assert_eq!(app.accounts.iter().map(|a| a.busy).collect::<Vec<_>>(), states);
        assert!(app.page == page);
        let _ = app.update(Message::ControllerBack);
        assert!(!app.ah_lookup.open && !app.controller_modal());
    }

    #[test]
    fn controller_changes_target_running_retail_games_without_selection_or_passwords() {
        let mut app = app();
        for (index, row) in app.accounts.iter_mut().enumerate() {
            row.account.mode = if index == 2 { AccountMode::Private } else { AccountMode::Retail };
            row.pid = Some(100 + index as u32);
            row.alive = Some(Arc::new(AtomicBool::new(true)));
            row.game_running = Some(Arc::new(AtomicBool::new(index != 1)));
            row.selected = false;
        }
        let key = app.accounts[0].key.clone();
        app.accounts[0].account.pol_password = "kept locally".into();
        app.accounts[0].account.se_password = "kept locally too".into();
        app.settings.account_installs.insert(key, r"C:\test\FFXI".into());
        let targets = app.controller_settings_targets();
        assert_eq!(targets.len(), 1, "idle POL sessions and private clients are excluded");
        assert_eq!(account_key(&targets[0].1), account_key(&app.accounts[0].account));
        assert!(targets[0].1.pol_password.is_empty() && targets[0].1.se_password.is_empty());
        assert_eq!(targets[0].0.game_install, Some(PathBuf::from(r"C:\test\FFXI")));
        assert_eq!(app.accounts[0].account.pol_password, "kept locally");
        assert_eq!(app.accounts[0].account.se_password, "kept locally too");
        app.accounts[0].alive.as_ref().unwrap().store(false, Ordering::Release);
        assert!(app.controller_settings_targets().is_empty(), "a client already closed receives no changes");
    }

    #[test]
    fn privacy_mode_is_a_saved_setting_that_changes_no_data() {
        let old: Settings = serde_json::from_str(r#"{"install_path":"C:\\Games\\FFXI"}"#).unwrap();
        assert!(!old.privacy);
        assert!(!serde_json::to_string(&old).unwrap().contains("privacy"));
        let mut app = app();
        let accounts: Vec<_> = app.accounts.iter().map(|a| (a.account.label.clone(), a.account.pol_id.clone())).collect();
        let _ = app.update(Message::SetPrivacy(true));
        assert!(app.settings.privacy && privacy::on());
        assert!(serde_json::to_string(&app.settings).unwrap().contains(r#""privacy":true"#));
        assert_eq!(app.accounts.iter().map(|a| (a.account.label.clone(), a.account.pol_id.clone())).collect::<Vec<_>>(), accounts);
        let _ = app.update(Message::SetPrivacy(false));
        assert!(!privacy::on());
    }

    #[test]
    fn deleting_an_account_requires_confirmation_and_cancel_keeps_it() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.preview = false;
        assert_eq!(app.update(Message::DeleteAccount(key.clone())).units(), 0);
        assert!(!app.vault_saving);
        let _ = app.update(Message::RequestDeleteAccount(key.clone()));
        assert_eq!(app.pending_delete.as_deref(), Some(key.as_str()));
        let _ = app.update(Message::CancelDeleteAccount);
        assert!(app.pending_delete.is_none());
        assert_eq!(app.accounts.len(), 3);
        let _ = app.update(Message::RequestDeleteAccount(key.clone()));
        assert_eq!(app.update(Message::DeleteAccount(key)).units(), 1);
        assert!(app.vault_saving);
        assert!(app.pending_delete.is_none());
    }

    #[test]
    fn switching_language_swaps_the_news_and_drops_the_other_languages_late_results() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-news-language-{}", backend::unique_id().unwrap()));
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.preview = false;
        let en_cache = news_cache_path(&app.config, i18n::Lang::En);
        let ja_cache = news_cache_path(&app.config, i18n::Lang::Ja);
        assert_eq!(en_cache.file_name().unwrap(), "news-cache.json", "the English file keeps its name");
        assert_eq!(ja_cache.file_name().unwrap(), "news-cache.ja.json");
        std::fs::create_dir_all(en_cache.parent().unwrap()).unwrap();
        let saved = |title: &str, site: &str, lang: &str| format!(r#"{{"items":[{{"title":"{title}","url":"https://www.playonline.com/{site}/polnews/news1.shtml","date":"","summary":"","image_url":""}}],"saved_at":"2026-10-06 00:00:00 UTC","language":"{lang}"}}"#);
        std::fs::write(&en_cache, saved("English headline", "ff11us", "en")).unwrap();
        let headline = |app: &App| app.services.as_ref().map(|s| s.news[0].title.clone());
        let late = |title: &str| {
            let item = services::NewsItem { title: title.into(), url: "https://www.playonline.com/news/late.html".into(), ..Default::default() };
            services::ServiceSnapshot { news: vec![item], notices: Vec::new(), news_note: String::new(), availability: services::Availability::Unknown, availability_note: String::new(), windows: Vec::new(), windows_complete: false, ended_early: Vec::new(), checked_at: String::new() }
        };

        i18n::set(i18n::Lang::En);
        app.services = services::cached(&en_cache, i18n::Lang::En);
        assert_eq!(headline(&app).as_deref(), Some("English headline"));
        app.news_loading = true; // an English refresh is running
        app.news_lang = i18n::Lang::En;
        let first_en = app.news_generation;

        // No Japanese headlines saved yet: the English ones go, none take their place,
        // and a Japanese read starts although an English one is still running.
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::Ja)).units(), 1);
        assert_eq!(i18n::current(), i18n::Lang::Ja);
        assert!(app.services.is_none());
        assert!(app.news_loading && app.news_lang == i18n::Lang::Ja);
        let first_ja = app.news_generation;

        // The English read finishing now changes nothing.
        app.worker_event(WorkerEvent::Services(first_en, i18n::Lang::En, services::Update::Headlines { items: late("Late English").news, note: String::new() }));
        let _ = app.update(Message::ServicesLoaded(first_en, i18n::Lang::En, Ok(late("Late English"))));
        assert!(app.services.is_none(), "English results are not shown in Japanese");
        assert!(app.news_loading, "the Japanese read still owns the loading state");
        // A second request while the Japanese read runs does not start another.
        assert_eq!(app.start_news().units(), 0);

        // The Japanese read's own results show and end the loading state.
        app.worker_event(WorkerEvent::Services(first_ja, i18n::Lang::Ja, services::Update::Headlines { items: late("日本語の見出し").news, note: String::new() }));
        assert_eq!(headline(&app).as_deref(), Some("日本語の見出し"));
        let _ = app.update(Message::ServicesLoaded(first_ja, i18n::Lang::Ja, Ok(late("日本語の見出し"))));
        assert!(!app.news_loading);

        // Back to English: its saved headlines return (the cache file the Japanese
        // read would write is a different one), and a fresh read starts.
        std::fs::write(&ja_cache, saved("日本語の見出し", "ff11", "ja")).unwrap();
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::En)).units(), 1);
        assert_eq!(headline(&app).as_deref(), Some("English headline"));
        // The first English request still cannot publish after switching back.
        app.worker_event(WorkerEvent::Services(first_en, i18n::Lang::En, services::Update::Headlines { items: late("Stale English").news, note: String::new() }));
        let _ = app.update(Message::ServicesLoaded(first_en, i18n::Lang::En, Ok(late("Stale English"))));
        let _ = app.update(Message::ServiceStatusLoaded(first_en, i18n::Lang::En, Ok(late("Stale status"))));
        assert_eq!(headline(&app).as_deref(), Some("English headline"));
        assert!(app.news_loading, "the current English request still owns loading");
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::Ja)).units(), 1);
        assert_eq!(headline(&app).as_deref(), Some("日本語の見出し"));
        let _ = app.update(Message::ServicesLoaded(first_ja, i18n::Lang::Ja, Err("stale failure".into())));
        assert!(app.news_loading && app.news_error.is_empty());
        // Choosing the language already showing is not a change and fetches nothing.
        let _ = app.update(Message::ServicesLoaded(app.news_generation, i18n::Lang::Ja, Err("offline".into())));
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::Ja)).units(), 0);
        assert_eq!(headline(&app).as_deref(), Some("日本語の見出し"));
        i18n::set(i18n::Lang::En);
    }

    #[test]
    fn language_setting_is_optional_and_applies_at_once() {
        // A settings file from before the language setting still loads, follows
        // Windows, and is written back without a language until one is chosen.
        let old: Settings = serde_json::from_str(r#"{"install_path":"C:\\Games\\FFXI","light_theme":true}"#).unwrap();
        assert_eq!(old.language, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("language"));
        let mut app = app();
        i18n::set(i18n::Lang::En);
        let _ = app.update(Message::SetLanguage(i18n::Lang::Ja));
        assert_eq!(app.settings.language, Some(i18n::Lang::Ja));
        assert_eq!(i18n::current(), i18n::Lang::Ja);
        assert_eq!(i18n::t("Settings"), "設定");
        let saved = serde_json::to_string(&app.settings).unwrap();
        assert!(saved.contains(r#""language":"ja""#), "{saved}");
        let back: Settings = serde_json::from_str(&saved).unwrap();
        assert_eq!(back.language, Some(i18n::Lang::Ja));
        let _ = app.update(Message::SetLanguage(i18n::Lang::En));
        assert_eq!(i18n::t("Settings"), "Settings");
    }

    #[test]
    fn window_and_session_worker_take_their_language_from_the_same_saved_settings() {
        let root = std::env::temp_dir().join(format!("crystal-language-{}", backend::unique_id().unwrap()));
        let config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        // No settings file yet: the Windows display language.
        i18n::set(i18n::Lang::En);
        load_settings_and_language(&config).unwrap();
        assert_eq!(i18n::current(), i18n::system_default());
        // A saved choice wins, whatever Windows says.
        let settings = Settings { language: Some(i18n::Lang::Ja), ..Settings::default() };
        save_settings(&config, &settings).unwrap();
        i18n::set(i18n::Lang::En);
        assert!(load_settings_and_language(&config).is_ok());
        assert_eq!(i18n::current(), i18n::Lang::Ja);
        // A damaged file reports its error in the Windows language and falls back to it.
        std::fs::write(settings_path(&config), b"{\"install_path\":\"unfinished").unwrap();
        i18n::set(i18n::Lang::Ja);
        assert!(load_settings_and_language(&config).is_err());
        assert_eq!(i18n::current(), i18n::system_default());
        i18n::set(i18n::Lang::En);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launcher_names_join_as_translatable_pairs() {
        i18n::set(i18n::Lang::En);
        assert_eq!(join_and(&[]), "");
        assert_eq!(join_and(&["Ashita v4"]), "Ashita v4");
        assert_eq!(join_and(&["Ashita v4", "Windower 4"]), "Ashita v4 and Windower 4");
        assert_eq!(join_and(&["the native client", "Ashita v4", "Windower 4"]), "the native client and Ashita v4 and Windower 4");
    }

    #[test]
    fn the_last_result_carries_its_tone_apart_from_its_words() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Err("boom".into()), None));
        assert_eq!(app.maintenance_tone, ResultTone::Stopped);
        assert!(app.maintenance_result.starts_with("Operation stopped"));
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Ok("Checked.".into()), None));
        assert_eq!(app.maintenance_tone, ResultTone::Done);
    }

    #[test]
    fn unchecked_selections_read_as_unverified_not_done() {
        let mut app = app();
        app.preview = false;
        app.maintenance_tone = ResultTone::Done;
        let _ = app.update(Message::InstallPathChanged(r"C:\Elsewhere".into()));
        assert_eq!(app.maintenance_tone, ResultTone::Unverified);
        assert_eq!(app.maintenance_result, t("Installation path changed. Save settings to select this folder."));
    }

    #[test]
    fn choosing_an_install_folder_rejects_the_previous_folders_pending_report() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-picker-{}", backend::unique_id().unwrap()));
        let install = root.join("game");
        std::fs::create_dir_all(&install).unwrap();
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.preview = false;
        app.patch_loading = true;
        let old_generation = app.patch_generation;
        let old_report = app.patch.clone().unwrap();
        let _ = app.update(Message::Picked(PickTarget::GameFolder, Some(install.clone())));
        assert!(app.settings.install_selected);
        assert_eq!(app.settings.install_path, install.to_string_lossy());
        assert!(!app.patch_loading && app.patch.is_none());
        assert_ne!(app.patch_generation, old_generation);
        let _ = app.update(Message::PatchLoaded(old_generation, old_report));
        assert!(app.patch.is_none());
        assert_eq!(load_settings(&app.config).unwrap().install_path, install.to_string_lossy());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_installation_detection_saves_the_folder_and_rejects_old_version_results() {
        let root = std::env::temp_dir().join(format!("crystal-detect-{}", backend::unique_id().unwrap()));
        let install = root.join("FINAL FANTASY XI");
        std::fs::create_dir_all(&install).unwrap();
        let mut app = app();
        app.config.experiment = root.clone();
        app.preview = false;
        app.installation_loading = true;
        app.installation_generation = 7;
        app.patch_loading = true;
        let previous = app.patch.clone().unwrap();
        let previous_generation = app.patch_generation;
        assert_eq!(app.update(Message::InstallationFound(7, true, Some(install.clone()))).units(), 1);
        assert!(app.settings.install_selected && !app.installation_loading);
        assert_eq!(load_settings(&app.config).unwrap().install_path, install.to_string_lossy());
        assert!(app.patch.is_none() && app.patch_loading);
        let _ = app.update(Message::PatchLoaded(previous_generation, previous));
        assert!(app.patch.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_detection_cannot_replace_later_edits_or_run_in_a_preview() {
        let mut app = app();
        assert_eq!(app.update(Message::FindInstallation).units(), 0);
        assert!(!app.installation_loading);
        app.preview = false;
        let original = app.settings.install_path.clone();
        assert_eq!(app.find_installation(true).units(), 1);
        let generation = app.installation_generation;
        let _ = app.update(Message::InstallPathChanged(r"C:\Another install".into()));
        let _ = app.update(Message::InstallPathChanged(original.clone()));
        let _ = app.update(Message::InstallationFound(generation, true, Some(PathBuf::from(r"C:\Late install"))));
        assert_eq!(app.settings.install_path, original);
        assert!(!app.installation_loading && !app.settings.install_selected);
        app.maintenance = Some(MaintenanceKind::Update);
        assert_eq!(app.update(Message::FindInstallation).units(), 0);
    }

    #[test]
    fn installation_completion_starts_pending_signin_without_waiting_for_a_ui_tick() {
        let (mut app, key) = resign_app("startup-install-ready");
        app.startup_signin_pending = true;
        app.installation_loading = true;
        app.installation_generation = 2;
        let install = app.config.experiment.join("game");
        std::fs::create_dir_all(&install).unwrap();
        app.settings.install_selected = true;
        app.settings.install_path = install.to_string_lossy().into_owned();
        assert!(app.take_startup_signins().is_empty());
        let _ = app.update(Message::InstallationFound(2, true, Some(install.clone())));
        assert!(!app.startup_signin_pending);
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key]);
        let _ = app.update(Message::InstallationFound(2, true, Some(install)));
        assert_eq!(app.launches.as_ref().unwrap().len(), 1, "one automatic sign-in only");
        std::fs::remove_dir_all(&app.config.experiment).unwrap();
    }

    #[test]
    fn automatic_detection_keeps_a_damaged_settings_file_unchanged() {
        let root = std::env::temp_dir().join(format!("crystal-detect-damaged-{}", backend::unique_id().unwrap()));
        let install = root.join("game");
        std::fs::create_dir_all(&install).unwrap();
        let mut app = app();
        app.config.experiment = root.clone();
        let path = settings_path(&app.config);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let damaged = b"{\"install_path\":\"unfinished";
        std::fs::write(&path, damaged).unwrap();
        app.preview = false;
        app.startup_warning = Some("Settings could not be read".into());
        app.settings_damaged = true;
        // The vault loads first and shows (takes) the warning: the file stays protected.
        app.launcher_start_logged = true;
        let _ = app.update(Message::AccountsLoaded(Ok(Vec::new())));
        assert!(app.startup_warning.is_none() && app.settings_damaged);
        app.installation_loading = true;
        app.installation_generation = 5;
        assert_eq!(app.update(Message::InstallationFound(5, true, Some(install.clone()))).units(), 1);
        assert!(app.settings.install_selected);
        assert_eq!(app.settings.install_path, install.to_string_lossy());
        assert_eq!(std::fs::read(path).unwrap(), damaged);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_official_setup_rescans_installation_and_failed_setup_does_not() {
        let mut app = app();
        app.preview = false;
        app.maintenance = Some(MaintenanceKind::Setup);
        assert_eq!(app.update(Message::MaintenanceFinished(MaintenanceKind::Setup, Ok("Setup closed".into()), None)).units(), 2);
        assert!(app.installation_loading && app.viewer_loading);
        app.installation_loading = false;
        app.maintenance = Some(MaintenanceKind::Setup);
        assert_eq!(app.update(Message::MaintenanceFinished(MaintenanceKind::Setup, Err("Cancelled".into()), None)).units(), 0);
        assert!(!app.installation_loading);
    }

    #[test]
    fn fixed_sentences_follow_a_language_change() {
        // A table entry (Settings ↔ 設定) read back from Japanese into English.
        i18n::set(i18n::Lang::En);
        assert_eq!(retranslated("設定", i18n::Lang::Ja, &["Settings"]), Some("Settings"));
        assert_eq!(retranslated("Settings", i18n::Lang::Ja, &["Settings"]), None);
        assert_eq!(retranslated("Ready (3 accounts)", i18n::Lang::Ja, &["Settings"]), None);
        // The window's own lines, both ways.
        let mut app = app();
        app.maintenance_result = t(RESULT_TEXTS[0]).into();
        app.accounts[0].status = t("Client closed").into();
        let _ = app.update(Message::SetLanguage(i18n::Lang::Ja));
        assert_eq!(app.maintenance_result, i18n::t_in(i18n::Lang::Ja, RESULT_TEXTS[0]));
        assert_eq!(app.accounts[0].status, i18n::t_in(i18n::Lang::Ja, "Client closed"));
        let _ = app.update(Message::SetLanguage(i18n::Lang::En));
        assert_eq!(app.maintenance_result, RESULT_TEXTS[0]);
        assert_eq!(app.accounts[0].status, "Client closed");
    }

    #[test]
    fn an_exit_report_on_a_row_is_worded_when_shown() {
        i18n::set(i18n::Lang::En);
        let mut app = app();
        let report = launch_log::classify_loader_end(&launch_log::LoaderEnd::Closed, Duration::from_secs(1));
        app.accounts[0].set_exit(report.clone());
        assert!(app.accounts[0].problem);
        assert_eq!(app.accounts[0].status, report.summary, "stored in English");
        assert_eq!(app.accounts[0].status_text(), report.message());
        // A later status replaces it.
        app.accounts[0].status = "Starting sign-in…".into();
        assert_eq!(app.accounts[0].status_text(), "Starting sign-in…");
    }

    #[test]
    fn window_input_is_forwarded_at_most_every_few_seconds() {
        let start = Instant::now();
        let mut last = None;
        assert!(activity_due(&mut last, start));
        assert!(!activity_due(&mut last, start + Duration::from_secs(1)));
        assert!(!activity_due(&mut last, start + ACTIVITY_INTERVAL - Duration::from_millis(1)));
        assert!(activity_due(&mut last, start + ACTIVITY_INTERVAL));
        assert!(!activity_due(&mut last, start + ACTIVITY_INTERVAL + Duration::from_secs(4)));
    }

    #[test]
    fn status_changes_include_groups_unless_unchecked_and_refresh_is_gone() {
        let mut app = app();
        assert!(!app.friends.skip_groups, "Also set for my groups starts checked");
        let _ = app.update(Message::FriendsStatusGroups(false));
        assert!(app.friends.skip_groups);
        let _ = app.update(Message::FriendsStatusGroups(true));
        assert!(!app.friends.skip_groups);
        // Preview: settings are not changed or saved by the Auto-Away picker.
        let _ = app.update(Message::FriendsAutoAway(30));
        assert!(app.settings.account_auto_away.is_empty());
    }

    #[test]
    fn switching_friends_accounts_rejects_an_earlier_read_after_switching_back() {
        let mut app = app();
        let first = app.accounts[0].key.clone();
        let second = app.accounts[1].key.clone();
        let _ = app.update(Message::FriendsAccount(first.clone()));
        let old_generation = app.friends_generation;
        let _ = app.update(Message::FriendsAccount(second));
        let _ = app.update(Message::FriendsAccount(first.clone()));
        app.friends.loading = true;
        app.friends.connected = true;
        app.friends.pending = Some(("current request".into(), Instant::now()));
        app.friends.snapshot = Some(friends::Snapshot { fetched_at: 42, ..Default::default() });

        app.worker_event(WorkerEvent::Friends(old_generation, first.clone(), false, None));
        assert!(app.friends.loading && app.friends.connected && app.friends.pending.is_some());
        assert_eq!(app.friends.snapshot.as_ref().unwrap().fetched_at, 42);
        app.worker_event(WorkerEvent::Friends(app.friends_generation, first, true,
            Some(friends::Snapshot { fetched_at: 43, ..Default::default() })));
        assert!(!app.friends.loading);
        assert_eq!(app.friends.snapshot.as_ref().unwrap().fetched_at, 43);
    }

    #[test]
    fn removing_the_friends_account_clears_its_session_and_pending_results() {
        let mut app = app();
        app.load_friends_preview(false, false, true, false);
        let removed = app.friends.account.clone().unwrap();
        let old_generation = app.friends_generation;
        app.friends.loading = true;
        app.friends.pending = Some(("old request".into(), Instant::now()));
        let remaining = app.accounts.iter().filter(|row| row.key != removed).map(|row| row.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(remaining)));
        assert!(app.friends.account.is_none() && app.friends.snapshot.is_none());
        assert!(!app.friends.loading && !app.friends.connected);
        assert!(app.friends.pending.is_none() && app.friends.composer.is_none());
        app.worker_event(WorkerEvent::Friends(old_generation, removed, true, Some(friends::Snapshot::default())));
        assert!(app.friends.snapshot.is_none());
        let _ = app.update(Message::FriendsAccount(app.accounts[0].key.clone()));
        assert_eq!(app.friends.account.as_ref(), Some(&app.accounts[0].key));
    }

    #[test]
    fn back_closes_the_settings_dialog_and_friends_panes_before_leaving_the_page() {
        let mut app = app();
        app.show_settings = true;
        let _ = app.update(Message::ControllerBack);
        assert!(!app.show_settings);
        app.load_friends_preview(false, false, true, false);
        app.friends.remove = Some(app.friends.snapshot.as_ref().unwrap().entries[0].clone());
        let _ = app.update(Message::ControllerBack);
        assert!(app.friends.remove.is_none() && app.friends.composer.is_some());
        let _ = app.update(Message::ControllerBack);
        assert!(app.page == Page::Friends && app.friends.pane.is_none() && app.friends.composer.is_none());
        for pane in [friends::Pane::Mailbox, friends::Pane::Blocked] {
            app.friends.select(Some(pane));
            let _ = app.update(Message::ControllerBack);
            assert!(app.page == Page::Friends && app.friends.pane.is_none());
        }
        let _ = app.update(Message::ControllerBack);
        assert!(app.page == Page::Home);
    }

    #[test]
    fn refreshing_during_news_load_does_not_start_another_feed_stream() {
        let mut app = app();
        app.preview = false;
        assert_eq!(app.refresh().units(), 2);
        assert!(app.news_loading);
        assert_eq!(app.refresh().units(), 1);
        assert_eq!(app.patch_generation, 2);
    }

    #[test]
    fn main_button_tracks_update_downtime_and_running_operations() {
        let mut app = app();
        assert_eq!(app.main_action(), MainAction::Play);
        app.services.as_mut().unwrap().availability = services::Availability::Maintenance;
        assert_eq!(app.main_action(), MainAction::Maintenance);
        let _ = app.update(Message::MainAction);
        assert!(app.show_details);
        assert!(app.accounts.iter().all(|row| !row.busy && row.pid.is_none()));
        // A published patch installs from the Play that asks for it, not from a main-button Update.
        app.patch.as_mut().unwrap().state = patch::PatchState::Required;
        assert_eq!(app.main_action(), MainAction::Maintenance);
        app.services.as_mut().unwrap().availability = services::Availability::Operational;
        assert_eq!(app.main_action(), MainAction::Play);
        app.maintenance = Some(MaintenanceKind::Update);
        assert_eq!(app.main_action(), MainAction::Working);
        app.maintenance = None;
        app.patch.as_mut().unwrap().state = patch::PatchState::Current;
        app.services.as_mut().unwrap().availability = services::Availability::Outage;
        assert_eq!(app.main_action(), MainAction::Offline);
        for availability in [services::Availability::Operational, services::Availability::PartialOutage, services::Availability::Unknown] {
            app.services.as_mut().unwrap().availability = availability;
            assert_eq!(app.main_action(), MainAction::Play);
        }
    }

    #[test]
    fn main_update_starts_the_combined_updater_and_keeps_one_operation() {
        for viewer_needs_update in [false, true] {
            let mut app = app();
            app.preview = false;
            app.accounts.clear();
            app.settings.install_path = r"C:\fixture\FINAL FANTASY XI".into();
            app.settings.install_selected = true;
            app.show_settings = true;
            app.show_details = true;
            app.patch.as_mut().unwrap().state = if viewer_needs_update {
                patch::PatchState::Current
            } else {
                patch::PatchState::Required
            };
            if viewer_needs_update {
                app.viewer = Some(viewer_report(patch::PatchState::Required));
            }

            // Creating the task must schedule the shared updater without polling
            // it here: this test must not download or alter an installation.
            assert_eq!(app.start_maintenance(MaintenanceKind::Update).units(), 1);
            assert!(app.maintenance == Some(MaintenanceKind::Update));
            assert!(app.active_operation.is_some());
            assert_eq!(app.main_action(), MainAction::Working);
            assert!(app.launch_blocked());
            let generation = app.viewer_generation;

            app.page = Page::Home;
            assert_eq!(app.update(Message::MainAction).units(), 0);
            assert!(app.page == Page::Maintenance);
            assert!(!app.show_settings && !app.show_details);
            assert_eq!(app.viewer_generation, generation);
            assert!(app.maintenance == Some(MaintenanceKind::Update));
        }
    }

    #[test]
    fn main_update_keeps_folder_and_session_protections() {
        let mut app = app();
        app.preview = false;
        app.patch.as_mut().unwrap().state = patch::PatchState::Required;

        assert_eq!(app.start_maintenance(MaintenanceKind::Update).units(), 0);
        assert!(app.maintenance.is_none() && app.active_operation.is_none());
        assert_eq!(app.status, t("Select the target folder explicitly before starting this operation."));

        app.settings.install_selected = true;
        app.vault_ready = false;
        assert_eq!(app.start_maintenance(MaintenanceKind::Update).units(), 0);
        assert!(app.maintenance.is_none() && app.active_operation.is_none());
        assert_eq!(app.status, t("Wait for saved accounts and running sessions to be checked before changing game files."));

        app.vault_ready = true;
        app.accounts[0].pid = Some(42);
        assert_eq!(app.start_maintenance(MaintenanceKind::Update).units(), 0);
        assert!(app.maintenance.is_none() && app.active_operation.is_none());
        assert_eq!(app.accounts[0].pid, Some(42));
        assert_eq!(app.status, t("Close all games and sign out PlayOnline sessions before updating, repairing, or running official setup. Existing sessions were kept."));
    }

    #[test]
    fn local_refresh_preserves_a_known_update_until_the_installed_version_changes() {
        let mut app = app();
        app.patch.as_mut().unwrap().state = patch::PatchState::Required;
        let mut local = app.patch.as_ref().unwrap().clone();
        local.state = patch::PatchState::Unknown;
        local.remote_version = t("Not checked").into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local.clone()));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Required);
        local.local_version = "30260904_2".into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Unknown);
    }

    #[test]
    fn successful_file_check_survives_version_refresh_but_not_a_changed_record() {
        let mut app = app();
        let mut checked = app.patch.as_ref().unwrap().clone();
        checked.state = patch::PatchState::Current;
        checked.files_verified = Some(true);
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Ok("Files verified".into()), Some(checked.clone())));
        let mut local = checked;
        local.state = patch::PatchState::Unknown;
        local.files_verified = None;
        local.remote_version = t("Not checked").into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local.clone()));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Current);
        assert_eq!(app.patch.as_ref().unwrap().files_verified, Some(true));
        assert_eq!(app.maintenance_tone, ResultTone::Done);
        local.local_version = "30260904_2".into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(app.patch.as_ref().unwrap().files_verified, None);
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Unknown);
    }

    /// The Downloads page after Check files: no version check ran, so the latest version is
    /// the remembered one with its age, until nothing is known ("Press Play to check").
    #[test]
    fn check_files_shows_the_remembered_official_version_instead_of_not_checked() {
        let root = std::env::temp_dir().join(format!("crystal-latest-{}-{}", std::process::id(), official_versions::now()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = app();
        app.preview = false;
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.settings.install_path = r"C:\Games\FFXI".into();
        let mut local = app.patch.as_ref().unwrap().clone();
        local.state = patch::PatchState::Unknown;
        local.local_version = "30260904_1".into();
        local.remote_version = t("Not checked").into();
        app.patch = Some(local.clone());
        // Nothing learned yet: asks for a Play, never "Not checked".
        assert_eq!(crate::official_versions::latest_game_text(&app), t("Press Play to check"));
        assert_eq!(crate::official_versions::latest_playonline_text(&app), t("Press Play to check"));
        // A Play's W20 (any process) wrote the answer: the next outcome reaching the window reads it.
        let path = official_versions::path_for(&app.config);
        let now = official_versions::now();
        official_versions::learn_game_at(&path, Path::new(r"C:\Games\FFXI"), "30260905_0", now - 3 * 3600);
        official_versions::learn_playonline_at(&path, "20110829_E", now - 90);
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(crate::official_versions::latest_game_text(&app), "30260905_0 (checked 3 h ago)");
        assert_eq!(crate::official_versions::latest_playonline_text(&app), "20110829_E (checked 1 min ago)");
        // Installed 30260904_1 is older than the remembered 30260905_0.
        assert_eq!(official_versions::compare("30260904_1", app.learned_game()), Some(std::cmp::Ordering::Less));
        // An Update installs it: the updater's own answer is remembered and installed matches it.
        let mut updated = app.patch.as_ref().unwrap().clone();
        updated.state = patch::PatchState::Current;
        updated.local_version = "30260906_0".into();
        updated.remote_version = "30260906_0".into();
        app.maintenance = Some(MaintenanceKind::Update);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Update, Ok("Updated".into()), Some(updated)));
        assert_eq!(app.learned_game().map(|l| l.version.as_str()), Some("30260906_0"));
        assert_eq!(official_versions::compare("30260906_0", app.learned_game()), Some(std::cmp::Ordering::Equal));
        // A Check afterwards (offline: placeholder remote) leaves the memory alone.
        let mut checked = app.patch.as_ref().unwrap().clone();
        checked.state = patch::PatchState::Unknown;
        checked.remote_version = t("Not checked").into();
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Ok("Checked".into()), Some(checked)));
        assert_eq!(app.learned_game().map(|l| l.version.as_str()), Some("30260906_0"));
        assert!(crate::official_versions::latest_game_text(&app).starts_with("30260906_0 (checked "), "{}", crate::official_versions::latest_game_text(&app));
        // Another game folder has its own answer.
        app.patch_folder = Some(PathBuf::from(r"D:\Other"));
        assert_eq!(crate::official_versions::latest_game_text(&app), t("Press Play to check"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_import_picker_cannot_overwrite_a_reopened_import() {
        use account_import_ui::Msg;
        let mut app = app();
        let _ = app.update(Message::AccountImport(Msg::Open));
        let _ = app.update(Message::AccountImport(Msg::Sources(1, Vec::new())));
        let _ = app.update(Message::AccountImport(Msg::Browse(false)));
        assert!(app.account_import.busy);
        let _ = app.update(Message::AccountImport(Msg::Close));
        let _ = app.update(Message::AccountImport(Msg::Open));
        let _ = app.update(Message::AccountImport(Msg::Sources(4, Vec::new())));
        let _ = app.update(Message::AccountImport(Msg::Path(r"D:\Current\creds.dat".into())));
        let _ = app.update(Message::AccountImport(Msg::Picked(2, false, Some(PathBuf::from(r"D:\Old\creds.dat")))));
        assert_eq!(app.account_import.path, r"D:\Current\creds.dat");
        assert!(!app.account_import.busy);
    }

    #[test]
    fn back_cancels_visible_close_confirmation_before_a_pending_import() {
        let mut app = app();
        app.confirm_close = true;
        app.account_import.open = true;
        let _ = app.update(Message::ControllerBack);
        assert!(!app.confirm_close);
        assert!(app.account_import.open);
        let _ = app.update(Message::ControllerBack);
        assert!(!app.account_import.open);
    }

    #[test]
    fn unreadable_or_interrupted_update_replaces_a_previous_ready_verdict() {
        let mut app = app();
        app.patch.as_mut().unwrap().files_verified = Some(true);
        let mut invalid = app.patch.as_ref().unwrap().clone();
        invalid.state = patch::PatchState::Required;
        invalid.remote_version = t("Not checked").into();
        invalid.files_verified = None;
        invalid.detail = "Interrupted update".into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, invalid));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Required);
        assert_eq!(app.patch.as_ref().unwrap().files_verified, None);
    }

    #[test]
    fn a_fresh_install_stays_on_update_and_preserves_the_server_version_on_refresh() {
        let mut app = app();
        let fresh = patch::PatchReport { state: patch::PatchState::Required,
            local_version: t("Not initialized").into(), remote_version: "30260904_1".into(),
            detail: "Initial update required".into(), files_verified: None };
        app.maintenance = Some(MaintenanceKind::Update);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Update,
            Ok("0 known files updated; files preserved".into()), Some(fresh.clone())));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Required);
        assert_eq!(app.maintenance_tone, ResultTone::Unverified);
        let mut local = fresh;
        local.remote_version = t("Not checked").into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(app.patch.as_ref().unwrap().remote_version, "30260904_1");
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Required);
    }

    /// Every world down: a Play waits for them ("Wait and play"), and nothing signs in meanwhile.
    #[test]
    fn confirmed_downtime_sets_new_game_starts_waiting_without_touching_sessions() {
        let mut app = app();
        app.preview = false;
        let snapshot = app.services.as_mut().unwrap();
        snapshot.availability = services::Availability::Outage;
        snapshot.checked_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let keys = app.launch_keys(true);
        assert!(!keys.is_empty());
        app.start_launch(keys.clone(), false, Instant::now(), None);
        assert!(keys.iter().all(|key| app.is_waiting(key)));
        assert!(app.accounts.iter().all(|row| !row.busy && row.pid.is_none() && row.queued_play.is_none()));
        assert_eq!(app.status, "FFXI's worlds are down. Starting as soon as they're back…");
        assert!(app.receiver.try_recv().is_err(), "no sign-in worker");
    }

    #[test]
    fn status_polling_is_background_only_and_preserves_news_and_patch_verdict() {
        let mut app = app();
        app.preview = false;
        let original_news = app.services.as_ref().unwrap().news[0].title.clone();
        let generation = app.patch_generation;
        app.refreshed = Instant::now() - Duration::from_secs(301);
        assert_eq!(app.poll_service_status().units(), 1);
        assert!(app.news_loading);
        assert_eq!(app.poll_service_status().units(), 0);
        assert_eq!(app.patch_generation, generation, "Only public HTTP status is polled");
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Current);
        let mut snapshot = app.services.as_ref().unwrap().clone();
        snapshot.news.clear();
        snapshot.availability = services::Availability::Outage;
        snapshot.checked_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Ok(snapshot)));
        assert!(!app.news_loading);
        assert_eq!(app.services.as_ref().unwrap().news[0].title, original_news);
        assert_eq!(app.main_action(), MainAction::Offline);
        app.refreshed = Instant::now() - Duration::from_secs(61);
        assert_eq!(app.poll_service_status().units(), 1, "Downtime is followed more closely");
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Err("status unavailable".into())));
        assert_eq!(app.main_action(), MainAction::Play);
        assert_eq!(app.services.as_ref().unwrap().news[0].title, original_news);
    }

    #[test]
    fn damaged_settings_are_preserved_before_saving_and_warn_at_startup() {
        let root = std::env::temp_dir().join(format!("crystal-settings-{}", backend::unique_id().unwrap()));
        let config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let path = settings_path(&config);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let damaged = b"{\"install_path\":\"unfinished";
        std::fs::write(&path, damaged).unwrap();
        assert!(load_settings(&config).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        let settings = Settings { install_path: root.join("client").to_string_lossy().into_owned(), ..Settings::default() };
        save_settings(&config, &settings).unwrap();
        assert_eq!(load_settings(&config).unwrap().install_path, settings.install_path);
        let backup = std::fs::read_dir(path.parent().unwrap()).unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap().to_string_lossy().starts_with("settings.invalid-")).unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), damaged);
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        std::fs::write(&path, &oversized).unwrap();
        assert!(save_settings(&config, &settings).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), oversized);
        std::fs::remove_dir_all(root).unwrap();

        let mut app = app();
        app.startup_warning = Some("Damaged settings were kept unchanged.".into());
        let _ = app.update(Message::AccountsLoaded(Ok(vec![])));
        assert!(app.status.contains("Damaged settings"));
        assert!(app.startup_warning.is_none());
    }

    #[test]
    fn verbatim_prefixes_are_cleaned_from_saved_settings_and_picked_folders() {
        let mut settings = Settings {
            install_path: r"\\?\C:\Games\SquareEnix\FINAL FANTASY XI".into(),
            installer_path: r"\\?\C:\Downloads\installer".into(),
            ashita_path: r"\\?\D:\Ashita".into(),
            windower_path: r"\\?\UNC\srv\share\Windower".into(),
            loader_path: r"\\?\C:\crystal\pol.exe".into(),
            ..Settings::default()
        };
        settings.plain_paths();
        assert_eq!(settings.install_path, r"C:\Games\SquareEnix\FINAL FANTASY XI");
        assert_eq!(settings.installer_path, r"C:\Downloads\installer");
        assert_eq!(settings.ashita_path, r"D:\Ashita");
        assert_eq!(settings.windower_path, r"\\srv\share\Windower");
        assert_eq!(settings.loader_path, r"C:\crystal\pol.exe");
        // Plain and empty values are left alone.
        settings.ashita_path.clear();
        settings.plain_paths();
        assert!(settings.ashita_path.is_empty());

        let dir = std::env::temp_dir();
        let chosen = selected_folder(&dir.to_string_lossy(), true).unwrap();
        assert!(!chosen.to_string_lossy().starts_with(r"\\?\"), "{}", chosen.display());
        let missing = dir.join("xi-launcher-not-yet-there").join("deeper");
        let chosen = selected_folder(&missing.to_string_lossy(), false).unwrap();
        assert!(!chosen.to_string_lossy().starts_with(r"\\?\"), "{}", chosen.display());
        assert!(chosen.ends_with("deeper"));
    }

    #[test]
    fn dialogs_open_at_the_field_or_its_nearest_existing_parent() {
        let dir = std::env::temp_dir();
        assert_eq!(dialog_start("", false), None);
        assert_eq!(dialog_start("relative\\dir", false), None);
        let nested = dir.join("xi-launcher-missing-a").join("b");
        let start = dialog_start(&nested.to_string_lossy(), false).unwrap();
        assert!(dir.starts_with(&start) || start.starts_with(&dir) || start == dir);
        // A file field opens in the file's folder.
        let exe = dir.join("pol.exe");
        assert_eq!(dialog_start(&exe.to_string_lossy(), true), dialog_start(&dir.to_string_lossy(), false));
    }

    #[test]
    fn separate_completions_preserve_other_running_account() {
        let mut app = app();
        let first = app.accounts[0].key.clone();
        let second = app.accounts[1].key.clone();
        app.accounts[0].busy = true;
        app.accounts[1].busy = true;
        app.worker_event(WorkerEvent::Complete(
            second.clone(),
            Ok(LaunchResult {
                pid: 77,
                detail: "started".into(),
                alive: Arc::new(AtomicBool::new(true)),
                game_running: Arc::new(AtomicBool::new(true)),
                log: PathBuf::new(),
                outcome: Arc::new(std::sync::Mutex::new(None)),
                timing: "Signed in 2.8s · game started 9.4s".into(),
            }),
        ));
        assert!(app.accounts[0].busy);
        assert_eq!(app.accounts[1].pid, Some(77));
        assert_eq!(app.accounts[1].timing, "Signed in 2.8s · game started 9.4s");
        assert!(app.accounts[0].timing.is_empty());
        app.worker_event(WorkerEvent::Complete(first, Err("Rejected".into())));
        assert_eq!(app.accounts[1].pid, Some(77));
        assert_eq!(app.launch_keys(false).len(), 2);
    }
    #[test]
    fn ending_ffxi_keeps_the_pol_session_and_makes_play_available() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        let game = Arc::new(AtomicBool::new(true));
        app.worker_event(WorkerEvent::Complete(key, Ok(LaunchResult {
            pid: 123, detail: "fixture".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: game.clone(), log: PathBuf::new(), outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert!(app.accounts[0].is_playing());
        assert!(!app.accounts[0].can_play());
        game.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert_eq!(app.accounts[0].pid, Some(123));
        assert!(app.has_sessions());
        assert!(app.accounts[0].can_play());
        assert!(app.accounts[0].status.contains("Signed in to PlayOnline"));
    }
    #[test]
    fn file_changes_block_launches_but_scans_allow_them() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Check);
        assert_eq!(app.launchable_count(), 3);
        for kind in [
            MaintenanceKind::Update,
            MaintenanceKind::Repair,
            MaintenanceKind::Setup,
        ] {
            app.maintenance = Some(kind);
            assert_eq!(app.launchable_count(), 0);
            assert_eq!(app.selected_count(), 0);
        }
    }
    #[test]
    fn maintenance_requires_explicit_path_and_keeps_sessions() {
        let mut app = app();
        app.preview = false;
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none());
        assert!(app.status.contains("Select the target folder"));
        app.settings.install_selected = true;
        app.accounts[0].pid = Some(42);
        let _ = app.start_maintenance(MaintenanceKind::Repair);
        assert!(app.maintenance.is_none());
        assert_eq!(app.accounts[0].pid, Some(42));
        assert!(app.status.contains("Close all games"));
    }
    #[test]
    fn installer_buttons_select_their_folder_on_click_and_report_invalid_paths() {
        let root = std::env::temp_dir().join(format!("crystal-installer-click-{}", backend::unique_id().unwrap()));
        for kind in [MaintenanceKind::Download, MaintenanceKind::Extract, MaintenanceKind::Setup] {
            let mut app = app();
            app.preview = false;
            app.config.experiment = root.clone();
            app.settings.install_path = root.join("game").to_string_lossy().into_owned();
            app.settings.installer_path = root.join("installer").to_string_lossy().into_owned();
            assert!(!app.settings.installer_selected);
            // Task creation must not run the download, extractor or installer.
            assert_eq!(app.start_maintenance(kind).units(), 1);
            assert!(app.maintenance == Some(kind));
            assert!(app.settings.installer_selected);
        }
        let mut app = app();
        app.preview = false;
        app.settings.installer_path = "relative-installer".into();
        assert_eq!(app.start_maintenance(MaintenanceKind::Download).units(), 0);
        assert!(app.maintenance.is_none());
        assert!(app.status.contains("absolute folder path"));
        assert_eq!(app.installer_error.as_deref(), Some(app.status.as_str()));
        let _ = app.update(Message::InstallerPathChanged(root.join("installer").to_string_lossy().into_owned()));
        assert!(app.installer_error.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn viewer_report(state: patch::PatchState) -> viewer::Status {
        viewer::Status { folder: Some(PathBuf::from(r"C:\fixture\PlayOnlineViewer")),
            version: patch::PatchReport { state, local_version: "20070101_E".into(), remote_version: "20110829_E".into(),
                detail: "Viewer fixture".into(), files_verified: None }, core_error: None }
    }

    #[test]
    fn viewer_verdict_routes_to_downloads_without_replacing_the_game_verdict() {
        let mut app = app();
        app.viewer_generation = 2;
        let game = patch::PatchReport { state: patch::PatchState::Current, local_version: "30260904_1".into(),
            remote_version: "30260904_1".into(), detail: "Game fixture".into(), files_verified: None };
        app.patch = Some(game);
        let _ = app.update(Message::ViewerLoaded(1, viewer_report(patch::PatchState::Required)));
        assert!(app.viewer.is_none()); assert_eq!(app.main_action(), MainAction::Play);
        let _ = app.update(Message::ViewerLoaded(2, viewer_report(patch::PatchState::Required)));
        // Play updates PlayOnline when its sign-in finds it out of date: no Update button.
        assert!(app.viewer.as_ref().is_some_and(viewer::Status::requires_update));
        assert_eq!(app.main_action(), MainAction::Play);
        assert_eq!(app.patch.as_ref().unwrap().local_version, "30260904_1");
        app.viewer_generation = 3;
        let _ = app.update(Message::ViewerLoaded(3, viewer_report(patch::PatchState::Current)));
        app.worker_event(WorkerEvent::Viewer(2, viewer_report(patch::PatchState::Required)));
        assert_eq!(app.main_action(), MainAction::Play);
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Current);
    }

    #[test]
    fn unified_update_refuses_idle_social_owners_and_unfinished_session_discovery() {
        let mut app = app(); app.preview = false;
        app.settings.install_selected = true;
        app.accounts[0].pid = Some(42);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none()); assert_eq!(app.accounts[0].pid, Some(42));
        assert!(app.status.contains("sign out PlayOnline sessions"));
        app.accounts[0].pid = None; app.vault_ready = false;
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none()); assert!(app.status.contains("Wait for saved accounts"));
    }

    #[test]
    fn installer_steps_start_when_the_download_folder_contains_the_game_folder() {
        let root = std::env::temp_dir().join(format!("crystal-installer-parent-{}", backend::unique_id().unwrap()));
        let game = root.join("SquareEnix/FINAL FANTASY XI");
        std::fs::create_dir_all(&game).unwrap();
        for kind in [MaintenanceKind::Download, MaintenanceKind::Extract, MaintenanceKind::Setup] {
            let mut app = app();
            app.preview = false;
            app.config.experiment = root.clone();
            app.settings.install_path = game.to_string_lossy().into_owned();
            app.settings.installer_path = game.parent().unwrap().to_string_lossy().into_owned();
            app.settings.install_selected = true;
            // This is the user's actual parent/child layout. A click must schedule
            // the operation; checking a path must not execute external setup itself.
            assert_eq!(app.start_maintenance(kind).units(), 1);
            assert!(app.maintenance == Some(kind));
            assert!(app.settings.installer_selected && app.installer_error.is_none());
        }
        let mut app = app();
        app.preview = false;
        app.config.experiment = root.clone();
        app.settings.install_path = game.to_string_lossy().into_owned();
        app.settings.installer_path = game.parent().unwrap().to_string_lossy().into_owned();
        app.select_path(false);
        assert!(app.settings.install_selected, "selecting the game folder must also accept its installer parent");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installer_failure_is_visible_until_the_next_folder_or_attempt() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Download);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Download, Err("Metadata request failed".into()), None));
        assert_eq!(app.installer_error.as_deref(), Some("Metadata request failed"));
        let _ = app.update(Message::InstallerPathChanged(r"C:\Installer".into()));
        assert!(app.installer_error.is_none());
        app.maintenance = Some(MaintenanceKind::Download);
        app.installer_error = Some("Previous failure".into());
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Download, Ok("Downloaded".into()), None));
        assert!(app.installer_error.is_none());
    }

    #[test]
    fn disabled_private_client_cannot_launch_and_keeps_the_saved_profile() {
        let mut app = app();
        app.preview = false;
        app.accounts[0].account.mode = AccountMode::Private;
        let key = app.accounts[0].key.clone();
        assert!(!app.launch_keys(false).contains(&key));
        app.play(vec![key.clone()]);
        assert!(!app.accounts[0].busy);
        assert!(app.accounts[0].status.contains("temporarily unavailable"));
        assert_eq!(app.accounts[0].key, key);
        assert_eq!(app.accounts.len(), 3);
    }

    #[test]
    fn legacy_native_default_loads_as_an_available_retail_launcher() {
        let root = std::env::temp_dir().join(format!("crystal-retail-default-{}", backend::unique_id().unwrap()));
        let config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let settings = Settings { launch_with: crate::backend::LaunchWith::Native, ..Settings::default() };
        save_settings(&config, &settings).unwrap();
        assert_eq!(load_settings(&config).unwrap().launch_with, crate::backend::LaunchWith::Windower);
        let account = blank_account();
        assert_eq!(load_settings(&config).unwrap().launch_plan(&account).unwrap().tool, pol_less::Tool::Windower);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_close_request_keeps_launcher_open() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Update);
        let _ = app.update(Message::CloseRequested);
        assert!(app.page == Page::Maintenance);
        assert!(!app.confirm_close);
        assert!(app.status.contains("before closing"));
    }

    #[test]
    fn launcher_update_handshake_cannot_start_new_work_or_change_settings() {
        let mut app = app();
        app.self_update = self_update::State::Applying;
        let _ = app.update(Message::Navigate(Page::Friends));
        let _ = app.update(Message::NewAccount);
        let _ = app.update(Message::MainAction);
        let _ = app.update(Message::SelfUpdate(self_update::Msg::AutoCheck(false)));
        assert!(app.page == Page::Home);
        assert!(app.editor.is_none());
        assert!(app.maintenance.is_none());
        assert!(app.settings.launcher_auto_updates);
        assert!(app.launch_blocked());
        let _ = app.update(Message::CloseRequested);
        assert!(app.status.contains("update helper"));
    }

    /// The automatic restart for a downloaded update, as the app gathers its conditions.
    #[test]
    fn a_ready_update_restarts_by_itself_only_when_no_game_runs_and_nobody_uses_crystal() {
        use self_update::auto::{COUNTDOWN, IDLE_AFTER};
        let mut app = app();
        for row in &mut app.accounts {
            row.busy = false;
            row.batch_pending = false;
            row.queued_play = None;
            row.pid = None;
            row.game_running = None;
        }
        let start = Instant::now();
        let is_apply = |effect: &Effect<Message>| matches!(effect, Effect::Message(Message::SelfUpdate(self_update::Msg::Apply)));
        // Nothing downloaded: nothing happens.
        assert!(matches!(app.auto_restart_step(start), Effect::None));
        app.self_update = self_update::State::Ready(self_update::tests::fixture_stage("9.0.0"));
        // A game runs: it waits, with no timer.
        app.accounts[0].pid = Some(1234);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(true)));
        assert!(matches!(app.auto_restart_step(start), Effect::None));
        assert!(app.auto_restart.remaining(start).is_none());
        assert_eq!(app.auto_restart.next_due(app.auto_conditions(start), start), None);
        // The game ends (the session stays signed in at the main menu): scheduled.
        app.accounts[0].game_running.as_ref().unwrap().store(false, Ordering::Release);
        assert!(matches!(app.auto_restart_step(start), Effect::None));
        assert_eq!(app.auto_restart.remaining(start), Some(COUNTDOWN.as_secs()));
        assert!(is_apply(&app.auto_restart_step(start + COUNTDOWN)), "the countdown ran out");
        // Someone is using the window: it waits until they stop for a minute.
        let mut app = app_with_ready();
        app.window = Some(WindowId::unique());
        app.auto_restart.focused = true;
        app.auto_restart.last_input = Some(start);
        let _ = app.auto_restart_step(start);
        assert!(app.auto_restart.remaining(start).is_none());
        let idle = start + IDLE_AFTER;
        let _ = app.auto_restart_step(idle);
        assert_eq!(app.auto_restart.remaining(idle), Some(COUNTDOWN.as_secs()));
        // "Not now" postpones it.
        let _ = app.update(Message::SelfUpdate(self_update::Msg::Postpone));
        assert!(app.auto_restart.remaining(idle).is_none());
        assert!(matches!(app.auto_restart_step(idle + COUNTDOWN), Effect::None));
        assert!(app.auto_restart.postponed(Instant::now()));
        // The setting off: never by itself, saved off; the pill still works (its own test).
        let mut app = app_with_ready();
        let _ = app.update(Message::SelfUpdate(self_update::Msg::AutoRestart(false)));
        assert!(!app.settings.launcher_auto_restart);
        assert!(matches!(app.auto_restart_step(start), Effect::None));
        assert!(matches!(app.auto_restart_step(start + COUNTDOWN), Effect::None));
        assert!(app.auto_restart.remaining(start).is_none());
        assert!(app.can_restart_launcher());
        // An open dialog or a sign-in in flight waits too.
        let mut app = app_with_ready();
        app.show_settings = true;
        let _ = app.auto_restart_step(start);
        assert!(app.auto_restart.remaining(start).is_none());
        app.show_settings = false;
        app.accounts[0].busy = true;
        let _ = app.auto_restart_step(start);
        assert!(app.auto_restart.remaining(start).is_none());
        // Older settings files without the switch turn it on.
        let settings: Settings = serde_json::from_str(r#"{"install_path":""}"#).unwrap();
        assert!(settings.launcher_auto_restart);
    }

    fn app_with_ready() -> App {
        let mut app = app();
        for row in &mut app.accounts {
            row.busy = false;
            row.batch_pending = false;
            row.queued_play = None;
            row.pid = None;
            row.game_running = None;
        }
        app.self_update = self_update::State::Ready(self_update::tests::fixture_stage("9.0.0"));
        app
    }

    #[test]
    fn launcher_restart_waits_for_saves_and_requests_but_allows_running_games() {
        let mut app = app();
        for account in &mut app.accounts {
            account.busy = false;
            account.batch_pending = false;
            account.pid = Some(1234);
        }
        assert!(app.can_restart_launcher());
        app.vault_saving = true;
        assert!(!app.can_restart_launcher());
        app.vault_saving = false;
        app.friends.pending = Some(("pending-request".into(), Instant::now()));
        assert!(!app.can_restart_launcher());
        app.friends.pending = None;
        app.accounts[0].batch_pending = true;
        assert!(!app.can_restart_launcher());
        app.accounts[0].batch_pending = false;
        assert!(app.can_restart_launcher());
    }
    #[test]
    fn game_changes_wait_for_accounts_to_be_loaded_after_reopening() {
        let mut app = app();
        app.preview = false;
        app.accounts.clear();
        app.settings.install_selected = true;
        app.vault_ready = false;
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none());
        assert!(app.status.contains("running sessions"));
    }

    #[test]
    fn dependency_setup_keeps_the_window_open_and_account_saves_keep_the_editor() {
        let mut app = app();
        let component = prereqs::Component::DirectPlay;
        let _ = prereqs::update(&mut app.prereqs, prereqs::Msg::Ask(component));
        let _ = prereqs::update(&mut app.prereqs, prereqs::Msg::Install(component));
        assert!(app.prereqs.busy());
        let _ = app.update(Message::CloseRequested);
        assert!(app.page == Page::Settings);
        assert!(app.status.contains("Windows component"));
        let original = app.accounts[0].key.clone();
        let other = app.accounts[1].key.clone();
        let _ = app.update(Message::EditAccount(original.clone()));
        app.vault_saving = true;
        let _ = app.update(Message::EditAccount(other));
        assert_eq!(app.editor.as_ref().unwrap().key.as_ref(), Some(&original));
    }
    #[test]
    fn canonical_login_key_prevents_case_alias_duplicate() {
        let mut a = blank_account();
        a.pol_id = "abc12345".into();
        let mut b = a.clone();
        b.label = "another name".into();
        b.pol_id = "ABC12345".into();
        assert_eq!(account_key(&a), account_key(&b));
        a.mode = AccountMode::Private;
        b.mode = AccountMode::Private;
        a.server = "Example.COM".into();
        b.server = "example.com".into();
        b.pol_id = a.pol_id.clone();
        assert_eq!(account_key(&a), account_key(&b));
    }

    #[test]
    fn coordinated_selection_includes_warm_and_signing_in_accounts_once() {
        let mut app = app();
        let warm = app.accounts[0].key.clone();
        let signing_in = app.accounts[1].key.clone();
        let running = app.accounts[2].key.clone();
        app.accounts[0].pid = Some(77);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        app.accounts[1].busy = true;
        app.accounts[1].preparing_session = true;
        app.accounts[2].pid = Some(88);
        app.accounts[2].game_running = Some(Arc::new(AtomicBool::new(true)));
        assert_eq!(app.coordinated_keys(&[warm.clone(), signing_in.clone(), warm.clone(), running]), vec![warm.clone(), signing_in]);
        app.accounts[1].batch_pending = true;
        assert_eq!(app.coordinated_keys(&app.launch_keys(false)), vec![warm]);
    }

    #[test]
    fn coordinated_play_before_worker_publication_keeps_both_original_signins() {
        let mut app = app();
        app.preview = false;
        app.config.experiment = std::env::temp_dir().join(format!("crystal-unpublished-batch-{}", std::process::id()));
        assert!(!app.config.experiment.exists());
        let keys: Vec<_> = app.accounts[..2].iter_mut().map(|row| {
            row.busy = true;
            row.preparing_session = true;
            row.key.clone()
        }).collect();
        let clicked = Instant::now() - Duration::from_secs(2);
        app.start_launch(keys, false, clicked, None);
        app.start_queued_plays();
        for row in &app.accounts[..2] {
            assert_eq!(row.queued_play, Some(clicked));
            assert!(row.busy && row.preparing_session && !row.batch_pending);
            assert!(row.pid.is_none() && !row.problem);
        }
        assert!(app.receiver.try_recv().is_err(), "keep both startup workers; never start a second sign-in");
    }

    #[test]
    fn coordinated_play_ignores_late_startup_results_and_keeps_other_game() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.accounts[0].launch_generation = 2;
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = true;
        app.accounts[0].busy = true;
        app.accounts[0].status = "Waiting for the other account".into();
        app.accounts[1].pid = Some(88);
        app.accounts[1].game_running = Some(Arc::new(AtomicBool::new(true)));
        app.worker_event(WorkerEvent::Progress(key.clone(), "Old startup progress".into()).for_launch(&key, 1));
        app.worker_event(WorkerEvent::Complete(key.clone(), Err("Old startup result".into())).for_launch(&key, 1));
        let row = &app.accounts[0];
        assert!(row.batch_pending && row.busy && row.game_start_pending());
        assert!(!row.can_play());
        assert_eq!(row.status, "Waiting for the other account");
        app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
            pid: 77, detail: "started together".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(true)), log: PathBuf::new(),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })).for_launch(&key, 2));
        assert!(!app.accounts[0].batch_pending && !app.accounts[0].busy);
        assert_eq!(app.accounts[0].pid, Some(77));
        assert_eq!(app.accounts[1].pid, Some(88));
        assert!(app.accounts[1].is_playing());
    }

    #[test]
    fn failed_coordinated_play_keeps_prepared_session_and_error() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = true;
        app.accounts[0].busy = true;
        app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
            pid: 77, detail: "signed in".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::from("batch.log"),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        app.worker_event(WorkerEvent::Complete(key, Err("Another account needs an update".into())));
        let row = &app.accounts[0];
        assert!(!row.batch_pending && !row.busy && row.can_play());
        assert_eq!(row.pid, Some(77));
        assert_eq!(row.log, Some(PathBuf::from("batch.log")));
        assert!(row.problem);
        assert_eq!(row.status, "Another account needs an update");
        assert!(app.has_sessions(), "a prepared signed-in session still blocks file changes");
    }

    #[test]
    fn coordinated_play_stays_pending_when_an_old_host_exits() {
        let mut app = app();
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = true;
        app.accounts[0].busy = true;
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.update(Message::Tick);
        let row = &app.accounts[0];
        assert!(row.batch_pending && row.busy && row.game_start_pending());
        assert!(!row.can_play());
        assert!(app.has_sessions());
    }

    #[test]
    fn native_preparation_does_not_finish_batch_timing_before_release() {
        let mut app = app();
        let row = &mut app.accounts[0];
        row.pid = Some(77);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(Arc::new(AtomicBool::new(true)));
        row.batch_pending = true;
        row.batch_requested = true;
        row.busy = true;
        row.play_timing = Some(launch_timing::Timeline::begin_at(Instant::now()));
        row.status = "Waiting for the other account".into();
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_some());
        assert!(app.accounts[0].busy);
        assert_eq!(app.accounts[0].status, "Waiting for the other account");
        app.accounts[0].batch_requested = false;
        app.accounts[0].batch_pending = false;
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_none());
        assert!(!app.accounts[0].busy);
        assert!(!app.accounts[0].timing.is_empty());
    }

    #[test]
    fn reconnected_batch_stops_blocking_play_after_ownership_ends() {
        let mut app = app();
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(true)));
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = false;
        assert!(!app.accounts[0].can_play());
        let _ = app.update(Message::Tick);
        assert!(!app.accounts[0].batch_pending);
        assert!(app.accounts[0].can_play());
        assert_eq!(app.accounts[0].pid, Some(77));
    }

    #[test]
    fn background_signin_accepts_one_play_and_keeps_its_click_time() {
        let mut app = app();
        app.preview = false;
        let row = &mut app.accounts[0];
        row.busy = true;
        row.preparing_session = true;
        let key = row.key.clone();
        assert!(row.can_play(), "startup sign-in must not disable Play");
        assert!(!row.game_start_pending(), "background preparation is not a requested game launch");
        assert!(app.launch_keys(true).contains(&key));
        let clicked = Instant::now() - Duration::from_secs(2);
        app.start_launch(vec![key.clone()], false, clicked, None);
        let row = &app.accounts[0];
        assert_eq!(row.queued_play, Some(clicked));
        assert!(row.game_start_pending());
        assert!(!row.can_play(), "a second click must not queue another launch");
        assert!(row.pid.is_none());
        assert!(!app.launch_keys(true).contains(&key));
        app.play(vec![key.clone()]);
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked), "an unpublished or legacy worker must keep the original queue");
        assert!(!app.accounts[0].batch_pending);
        app.worker_event(WorkerEvent::Progress(key, "Signing in to PlayOnline…".into()));
        assert!(app.accounts[0].status_text().contains("FFXI will start when sign-in finishes."));
        assert!(app.receiver.try_recv().is_err(), "queued Play reuses the existing worker");
    }

    /// pol-spec 07d §5.4: a refused sign-in (password, OTP, any verdict on the account) drops
    /// the queued Play as before; nothing signs in a second time.
    #[test]
    fn failed_background_signin_clears_queued_play_without_retrying() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        for failure in ["Sign-in failed",
            "Game loader error during startup: Native PlayOnline member login failed (state -1036, POL result -1036). (Windower via crystal-loader).",
            &format!("{} installed 30260904_1, official server 30260905_0. Open Downloads.", patch::UPDATE_REQUIRED)] {
            let row = &mut app.accounts[0];
            row.busy = true;
            row.preparing_session = true;
            row.queued_play = Some(Instant::now());
            row.launch_trigger = Some(launch_timing::LaunchTrigger::StartupSignIn);
            let key = row.key.clone();
            app.worker_event(WorkerEvent::Complete(key, Err(failure.to_owned())));
            let row = &app.accounts[0];
            assert!(!row.busy && !row.preparing_session);
            assert!(row.queued_play.is_none() && row.pid.is_none() && row.network_wait.is_none());
            assert!(row.problem && row.can_play());
            assert_eq!(row.status, failure);
        }
        assert!(app.launches.as_ref().unwrap().is_empty(), "never a second login");
        assert!(app.receiver.try_recv().is_err());
    }

    /// 07d §5.4: a pre-sign-in whose host went away re-issues its queued Play at once, cold,
    /// with the original click; one that failed on the network waits for the probe first.
    /// Neither happens twice.
    #[test]
    fn failed_background_signin_reissues_its_queued_play_once_for_network_or_transient_failures() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        let key = app.accounts[0].key.clone();
        let clicked = Instant::now() - Duration::from_secs(3);
        let queue = |app: &mut App| {
            let row = &mut app.accounts[0];
            row.busy = true;
            row.preparing_session = true;
            row.queued_play = Some(clicked);
            row.launch_trigger = Some(launch_timing::LaunchTrigger::StartupSignIn);
        };
        queue(&mut app);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err("The background session worker ended before starting the game.".into())));
        assert_eq!(app.launches.as_ref().unwrap().as_slice(), &[(vec![key.clone()], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay))]);
        app.launches = Some(Vec::new());
        queue(&mut app);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err("Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).".into())));
        let row = &app.accounts[0];
        assert_eq!(row.network_wait, Some(NetworkWait::Reissue(clicked)));
        assert!(row.status_text().ends_with("Crystal will try again when the network is back."), "{}", row.status_text());
        assert!(app.launches.as_ref().unwrap().is_empty(), "not before the network is back");
        let generation = row.launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert_eq!(app.launches.as_ref().unwrap().as_slice(), &[(vec![key.clone()], false, clicked, Some(launch_timing::LaunchTrigger::ReissuedPlay))]);
        assert!(app.accounts[0].network_wait.is_none());
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert_eq!(app.launches.as_ref().unwrap().len(), 1, "at most once");
        // The re-issued Play is an ordinary Play: its own failure queues nothing more.
        app.accounts[0].busy = true;
        app.accounts[0].launch_trigger = Some(launch_timing::LaunchTrigger::ReissuedPlay);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err("The background session worker ended.".into())));
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
        assert!(app.accounts[0].network_wait.is_none());
    }

    /// Finding: a pre-sign-in that took the row's one-time code never re-issues its queued
    /// Play (or retries a startup sign-in) by itself: the code is spent, and a second login
    /// without one is refused at Square Enix. The row asks for a new code instead; a code typed
    /// since makes the re-issue safe again.
    #[test]
    fn a_failed_signin_that_spent_its_one_time_code_is_never_reissued() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        let key = app.accounts[0].key.clone();
        let clicked = Instant::now();
        let network = "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).";
        for (failure, trigger) in [(network, launch_timing::LaunchTrigger::Connect), ("The background session worker ended before starting the game.", launch_timing::LaunchTrigger::Connect),
            (network, launch_timing::LaunchTrigger::StartupSignIn)] {
            let row = &mut app.accounts[0];
            row.busy = true;
            row.preparing_session = true;
            row.queued_play = (trigger != launch_timing::LaunchTrigger::StartupSignIn).then_some(clicked);
            row.launch_trigger = Some(trigger);
            row.otp_spent = true;
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(failure.to_owned())));
            let row = &app.accounts[0];
            assert!(row.network_wait.is_none() && row.queued_play.is_none(), "{failure}");
            assert!(row.status_text().ends_with("That one-time code was used. Enter a new one, then press Play."), "{}", row.status_text());
        }
        assert!(app.launches.as_ref().unwrap().is_empty(), "no automatic login without a code");
        // A new code typed since: the re-issue may use it.
        let row = &mut app.accounts[0];
        (row.busy, row.preparing_session, row.queued_play, row.otp) = (true, true, Some(clicked), "123456".into());
        app.worker_event(WorkerEvent::Complete(key.clone(), Err("The background session worker ended before starting the game.".into())));
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
    }

    /// Finding: a host lost after the pre-sign-in reached authentication had its login taken;
    /// its queued Play is not re-issued as a second login by itself.
    #[test]
    fn a_transient_failure_after_authentication_is_not_reissued() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        let root = std::env::temp_dir().join(format!("crystal-reissue-auth-{}", crate::backend::unique_id().unwrap()));
        app.config = crate::backend::Config { experiment: root.clone(), repository: root.clone(), game_install: None, dat_overlays: Vec::new() };
        let key = app.accounts[0].key.clone();
        let row = &mut app.accounts[0];
        (row.busy, row.preparing_session, row.queued_play) = (true, true, Some(Instant::now()));
        let config = app.settings.config_for(&app.config, &app.accounts[0].account);
        session_worker::save_authenticated_record_for_test(&config, &app.accounts[0].account);
        app.worker_event(WorkerEvent::Complete(key, Err("Game exited during startup (Windower via crystal-loader).".into())));
        assert!(app.launches.as_ref().unwrap().is_empty(), "the login was already taken");
        assert!(app.accounts[0].queued_play.is_none() && app.accounts[0].network_wait.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Finding: once the probe gives up, the row stops promising a retry.
    #[test]
    fn a_probe_that_gives_up_takes_back_the_retry_promise() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        let key = app.accounts[0].key.clone();
        let network = "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).";
        let row = &mut app.accounts[0];
        (row.busy, row.preparing_session, row.launch_trigger) = (true, true, Some(launch_timing::LaunchTrigger::StartupSignIn));
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(network.into())));
        assert!(app.accounts[0].status_text().ends_with("Crystal will try again when the network is back."));
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), false).for_launch(&key, generation));
        let status = app.accounts[0].status_text();
        assert!(!status.contains("Crystal will try again") && status.starts_with(network)
            && status.ends_with("The network did not come back. Sign in again once it is."), "{status}");
    }

    /// 07d §5.5 step 3: a startup sign-in that failed on the network starts once more, as a
    /// startup retry, when the probe sees the network; a non-network failure never does, and
    /// neither does a probe that gave up or a row the user started meanwhile.
    #[test]
    fn a_startup_signin_that_failed_on_the_network_retries_once_when_it_returns() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        let key = app.accounts[0].key.clone();
        app.settings.account_no_otp.insert(key.clone());
        let fail = |app: &mut App, trigger, error: &str| {
            app.accounts[0].busy = true;
            app.accounts[0].preparing_session = true;
            app.accounts[0].launch_trigger = Some(trigger);
            app.worker_event(WorkerEvent::Complete(app.accounts[0].key.clone(), Err(error.to_owned())));
        };
        let network = "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).";
        fail(&mut app, launch_timing::LaunchTrigger::StartupSignIn, "Sign-in failed");
        assert!(app.accounts[0].network_wait.is_none());
        fail(&mut app, launch_timing::LaunchTrigger::Connect, network);
        assert!(app.accounts[0].network_wait.is_none(), "only the startup sign-in retries by itself");
        fail(&mut app, launch_timing::LaunchTrigger::StartupSignIn, network);
        assert_eq!(app.accounts[0].network_wait, Some(NetworkWait::StartupRetry));
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), false).for_launch(&key, generation));
        assert!(app.launches.as_ref().unwrap().is_empty() && app.accounts[0].network_wait.is_none(), "the probe gave up");
        fail(&mut app, launch_timing::LaunchTrigger::StartupSignIn, network);
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        let launches = app.launches.as_ref().unwrap();
        assert_eq!(launches.len(), 1);
        assert_eq!((&launches[0].0, launches[0].1, launches[0].3), (&vec![key.clone()], true, Some(launch_timing::LaunchTrigger::StartupRetry)));
        // The retry failing on the network again is not retried.
        fail(&mut app, launch_timing::LaunchTrigger::StartupRetry, network);
        assert!(app.accounts[0].network_wait.is_none());
        // A row the user started meanwhile keeps its own launch.
        fail(&mut app, launch_timing::LaunchTrigger::StartupSignIn, network);
        app.accounts[0].busy = true;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
    }

    /// 07d §1.5 "Who": retail accounts with a tool plan and no session of any kind; pre-signed
    /// accounts not while they are to sign in (here: when Crystal opens), OTP accounts and
    /// unknown ones too.
    #[test]
    fn park_eligibility_excludes_startup_and_busy_accounts_but_not_otp_ones() {
        let mut app = app();
        app.settings.account_no_otp.insert(app.accounts[1].key.clone());
        app.startup_signin_pending = true;
        app.accounts[2].otp = "123456".into();
        assert_eq!(app.park_keys().len(), 2);
        app.accounts[0].busy = true;
        let keys: Vec<String> = app.park_keys().into_iter().map(|(row, ..)| row).collect();
        assert_eq!(keys, vec![app.accounts[2].key.clone()]);
        let (_, key, plan) = app.park_keys().remove(0);
        assert_eq!((key.tool, key.profile.as_str(), plan.tool), ("Windower 4", "", pol_less::Tool::Windower));
        app.accounts[2].pid = Some(77);
        assert!(app.park_keys().is_empty(), "a running session is never parked beside");
        app.accounts[2].pid = None;
        app.accounts[2].account.mode = AccountMode::Private;
        assert!(app.park_keys().is_empty());
    }

    /// 07d §1.5 "Kill": parked hosts close while files change (polcore.dll is loaded in a parked
    /// pol.exe), before the profile editor and on exit; none park before the startup sign-ins.
    #[test]
    fn parks_close_while_files_change_and_on_exit() {
        let mut app = app();
        app.preview = false;
        app.config.experiment = std::env::temp_dir().join(format!("crystal-park-close-{}", backend::unique_id().unwrap()));
        let park = |app: &mut App| {
            let (account, row) = (app.accounts[0].account.clone(), app.accounts[0].key.clone());
            let plan = app.settings.launch_plan(&account).unwrap();
            // A stand-in worker that exits once its stdin closes.
            let worker = session_worker::spawn_test_park(std::process::Command::new("sort"), &app.config, &account, &plan).unwrap();
            app.parks.insert(pol_less::park::ParkKey::for_plan(&account, &plan), row, Default::default(), worker);
            assert!(!app.parks.is_empty());
        };
        park(&mut app);
        // A read-only installation scan keeps the parked host (a Play right after it stays warm).
        app.installation_loading = true;
        app.tend_parks(Instant::now());
        assert!(!app.parks.is_empty(), "an installation scan changes no files");
        app.installation_loading = false;
        // Files really changing close it.
        app.reshade.busy = true;
        app.park_tended = None;
        app.tend_parks(Instant::now());
        assert!(app.parks.is_empty(), "closed while files change");
        app.reshade.busy = false;
        park(&mut app);
        let _ = app.update(Message::EditWindowerProfiles);
        assert!(app.parks.is_empty(), "the profile editor needs the profile guard");
        app.windower_editor_open = false;
        park(&mut app);
        let _ = app.update(Message::CloseRequested);
        assert!(app.parks.is_empty(), "closed on exit");
        // Nothing parks before the startup sign-ins were issued, nor in previews.
        app.startup_issued = None;
        app.tend_parks(Instant::now() + Duration::from_secs(60));
        assert!(app.parks.is_empty());
    }

    #[test]
    fn profile_editor_holds_new_launches_and_preserves_pending_startup_signins() {
        let mut app = app();
        app.windower_editor_open = true;
        app.startup_signin_pending = true;
        app.launches = Some(Vec::new());
        assert!(app.launch_blocked());
        assert!(app.take_startup_signins().is_empty());
        assert!(app.startup_signin_pending);
        app.start_launch_now(vec![app.accounts[0].key.clone()], false, Instant::now(), None, false);
        assert!(app.launches.as_ref().unwrap().is_empty());
        assert!(app.status.contains("profile editor"));
        app.windower_editor_open = false;
        assert!(!app.launch_blocked());
    }

    /// 07d §5.5 steps 1-2 (08 §1 row 9): a startup key whose row is still busy with the
    /// previous session stays pending with a status, starts once the row is free, and is
    /// skipped with a status after STARTUP_WAIT_LIMIT. An idle signed-in session needs nothing.
    #[test]
    fn startup_signin_waits_for_a_closing_session_then_starts_or_is_skipped() {
        let mut app = app();
        app.preview = false;
        app.config.experiment = std::env::temp_dir().join(format!("crystal-startup-wait-{}", backend::unique_id().unwrap()));
        app.startup_signin_pending = true;
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        app.settings.account_no_otp = keys.iter().cloned().collect();
        app.accounts[0].busy = true; // the previous game's host is still closing
        app.accounts[1].pid = Some(77);
        app.accounts[1].game_running = Some(Arc::new(AtomicBool::new(false))); // signed in and idle
        app.accounts[2].account.mode = AccountMode::Private;
        assert!(app.take_startup_signins().is_empty());
        assert!(app.startup_issued.is_some());
        assert_eq!(app.startup_waiting.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(), vec![keys[0].clone()]);
        assert_eq!(app.accounts[0].status, "Startup sign-in waits for the previous session to end.");
        let now = Instant::now();
        assert!(app.take_waiting_startups(now).is_empty(), "still busy");
        app.accounts[0].busy = false;
        assert_eq!(app.take_waiting_startups(now), vec![keys[0].clone()]);
        assert!(app.startup_waiting.is_empty());
        // Skipped after the limit.
        app.accounts[0].busy = true;
        app.startup_waiting = vec![(keys[0].clone(), now)];
        assert!(app.take_waiting_startups(now + STARTUP_WAIT_LIMIT - Duration::from_secs(1)).is_empty());
        assert_eq!(app.startup_waiting.len(), 1);
        assert!(app.take_waiting_startups(now + STARTUP_WAIT_LIMIT).is_empty());
        assert!(app.startup_waiting.is_empty());
        assert_eq!(app.accounts[0].status, "Startup sign-in skipped: a session is still running.");
    }

    /// Owner decision 2026-10-12: a test app whose first account signs in when Crystal opens
    /// (2026-10-13: it is known to sign in without a one-time code; the switch is on by default).
    fn resign_app(name: &str) -> (App, String) {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        app.config.experiment = std::env::temp_dir().join(format!("crystal-{name}-{}", backend::unique_id().unwrap()));
        let key = app.accounts[0].key.clone();
        assert!(app.settings.auto_sign_in);
        app.settings.sign_in_after_game = true;
        app.settings.account_no_otp.insert(key.clone());
        (app, key)
    }

    /// Row `index` runs its game (seen on a Tick), then the session ends with `report`.
    fn game_ends(app: &mut App, index: usize, report: launch_log::ExitReport) {
        let alive = Arc::new(AtomicBool::new(true));
        let row = &mut app.accounts[index];
        row.pid = Some(77);
        row.alive = Some(alive.clone());
        row.game_running = Some(Arc::new(AtomicBool::new(true)));
        row.outcome = Some(Arc::new(std::sync::Mutex::new(Some(report))));
        let _ = app.update(Message::Tick);
        assert!(app.accounts[index].game_seen);
        alive.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(app.accounts[index].pid.is_none());
    }

    fn signed_out() -> launch_log::ExitReport { launch_log::classify_loader_end(&launch_log::LoaderEnd::SignedOut, Duration::from_secs(900)) }

    #[test]
    fn shutdown_buttons_sign_back_in_once_in_either_completion_order() {
        for selected in [true, false] {
            for completion_first in [true, false] {
                let (mut app, _) = resign_app("shutdown-resign-order");
                let mut ended = Vec::new();
                for (index, row) in app.accounts.iter_mut().enumerate() {
                    row.pid = Some(77 + index as u32);
                    row.busy = false;
                    row.selected = index == 0;
                    row.alive = Some(Arc::new(AtomicBool::new(true)));
                    row.game_running = Some(Arc::new(AtomicBool::new(true)));
                    let outcome = Arc::new(std::sync::Mutex::new(None));
                    row.outcome = Some(outcome.clone());
                    app.settings.account_no_otp.insert(row.key.clone());
                    if !selected || row.selected {
                        ended.push((row.key.clone(), row.launch_generation, row.alive.clone().unwrap(), outcome));
                    }
                }
                let _ = app.update(Message::Tick);
                let message = if selected { Message::ShutdownSelected } else { Message::ShutdownAll };
                assert_eq!(app.update(message).units(), ended.len());
                for (key, generation, _, _) in &ended {
                    let row = app.accounts.iter().find(|row| row.key == *key).unwrap();
                    assert!(row.shutting_down && row.game_seen);
                    assert!(!row.resign_declined && !row.signed_out, "shutdown must respect automatic sign-in");
                    if completion_first {
                        let _ = app.update(Message::GameShutdownFinished(key.clone(), *generation, Ok(())));
                        let row = app.accounts.iter().find(|row| row.key == *key).unwrap();
                        assert!(row.game_seen && row.pid.is_some() && row.outcome.is_some(), "the exit observer still owns the report");
                    }
                }
                // The monitor publishes the exit report before releasing `alive`.
                for (_, _, alive, outcome) in &ended {
                    *outcome.lock().unwrap() = Some(signed_out());
                    alive.store(false, Ordering::Release);
                }
                let _ = app.update(Message::Tick);
                for (key, generation, _, _) in &ended {
                    let row = app.accounts.iter().find(|row| row.key == *key).unwrap();
                    let pending = row.resign.expect("one pending sign-in after game shutdown");
                    assert!(!pending.network, "preserve the clean exit report");
                    if !completion_first {
                        let _ = app.update(Message::GameShutdownFinished(key.clone(), *generation, Ok(())));
                    }
                    let row = app.accounts.iter().find(|row| row.key == *key).unwrap();
                    assert_eq!(row.resign.unwrap().ended, pending.ended, "completion must not restart the delay");
                    assert!(row.status_text().ends_with(t("Signing back in after the game closed.")));
                    assert!(!row.shutting_down && row.pid.is_none());
                }
                let _ = app.update(Message::Tick);
                let keys = app.take_resigns(Instant::now() + RESIGN_DELAY);
                assert_eq!(keys, ended.iter().map(|(key, ..)| key.clone()).collect::<Vec<_>>());
                app.start_resigns(keys, launch_timing::LaunchTrigger::SignInAfterGame);
                assert_eq!(app.launches.as_ref().unwrap().len(), 1);
                assert!(app.take_resigns(Instant::now() + RESIGN_DELAY * 2).is_empty());
                assert!(app.accounts.iter().filter(|row| selected && !row.selected).all(|row| row.is_playing() && row.resign.is_none()));
            }
        }
    }

    #[test]
    fn explicit_signout_cancels_relogin_even_with_a_shutdown_result_still_pending() {
        for signout_before_end in [true, false] {
            let (mut app, key) = resign_app("shutdown-resign-cancel");
            let alive = Arc::new(AtomicBool::new(true));
            let outcome = Arc::new(std::sync::Mutex::new(None));
            let row = &mut app.accounts[0];
            row.pid = Some(77);
            row.alive = Some(alive.clone());
            row.game_running = Some(Arc::new(AtomicBool::new(true)));
            row.outcome = Some(outcome.clone());
            row.selected = true;
            let generation = row.launch_generation;
            let _ = app.update(Message::Tick);
            assert_eq!(app.update(Message::ShutdownSelected).units(), 1);
            if signout_before_end {
                let _ = app.update(Message::AccountSignOut(key.clone()));
            }
            *outcome.lock().unwrap() = Some(signed_out());
            alive.store(false, Ordering::Release);
            let _ = app.update(Message::Tick);
            if !signout_before_end {
                assert!(app.accounts[0].resign.is_some());
                let _ = app.update(Message::AccountSignOut(key.clone()));
            }
            let _ = app.update(Message::GameShutdownFinished(key, generation, Ok(())));
            let _ = app.update(Message::Tick);
            assert!(app.accounts[0].signed_out && app.accounts[0].resign.is_none());
            assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty());
            assert!(app.launches.as_ref().unwrap().is_empty());
        }
    }

    #[test]
    fn closing_a_game_stays_signed_out_by_default_even_with_startup_signin_enabled() {
        let (mut app, key) = resign_app("close-game-stays-out");
        app.settings.sign_in_after_game = false;
        let mut older = serde_json::to_value(Settings::default()).unwrap();
        older.as_object_mut().unwrap().remove("sign_in_after_game");
        assert!(!serde_json::from_value::<Settings>(older).unwrap().sign_in_after_game);
        app.startup_waiting.push((key.clone(), Instant::now()));
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].signed_out && app.accounts[0].resign.is_none());
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty());
        assert!(app.take_waiting_startups(Instant::now()).is_empty());
        assert!(!app.park_keys().iter().any(|(row, ..)| row == &key), "no replacement parked pol.exe");
        assert!(app.launches.as_ref().unwrap().is_empty());
        app.startup_signin_pending = true;
        assert!(app.take_startup_signins().is_empty(), "an explicit Play or next launcher window owns the next sign-in");
    }

    #[test]
    fn account_signout_is_available_without_opening_friends_and_blocks_recreation() {
        let mut app = app();
        app.page = Page::Home;
        let key = app.accounts[0].key.clone();
        app.settings.account_no_otp.insert(key.clone());
        app.accounts[0].pid = Some(77);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        assert!(app.accounts[0].can_sign_out());
        assert_eq!(account_row::row_state(&app, &app.accounts[0]), account_row::RowState::SignedIn);
        let _ = app.update(Message::EditAccount(key.clone()));
        let _ = app.update(Message::CloseEditor);
        assert_eq!(app.update(Message::AccountSignOut(key.clone())).units(), 1);
        assert!(app.accounts[0].busy && app.accounts[0].signed_out && !app.accounts[0].can_play());
        // Finished can be published while Windower still has its profile save.
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].signing_out && !app.accounts[0].busy && !app.accounts[0].can_play());
        assert!(!app.can_restart_launcher() && app.sessions_block_update());
        let generation = app.accounts[0].launch_generation;
        let _ = app.update(Message::AccountSignedOut(key.clone(), generation, Ok(())));
        assert!(app.accounts[0].pid.is_none() && app.accounts[0].can_play());
        assert_eq!(app.accounts[0].status, "Signed out of PlayOnline");
        assert!(!app.park_keys().iter().any(|(row, ..)| row == &key));
        assert!(app.take_waiting_startups(Instant::now()).is_empty());
    }

    #[test]
    fn launcher_close_drops_automatic_signins_and_preserves_running_game_state() {
        let (mut app, key) = resign_app("close-window");
        app.startup_waiting.push((key.clone(), Instant::now()));
        let alive = Arc::new(AtomicBool::new(true));
        let game = Arc::new(AtomicBool::new(true));
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(alive.clone());
        app.accounts[0].game_running = Some(game.clone());
        let _ = app.update(Message::CloseRequested);
        assert!(app.closing && app.launch_blocked());
        assert!(app.startup_waiting.is_empty() && !app.startup_signin_pending);
        assert!(app.accounts[0].signed_out && app.accounts[0].resign.is_none());
        assert!(alive.load(Ordering::Acquire) && game.load(Ordering::Acquire));
        assert!(app.accounts[0].is_playing());
        let _ = app.update(Message::Tick);
        let _ = app.update(Message::Launch(key));
        assert!(app.launches.as_ref().unwrap().is_empty(), "shutdown cannot start more work");
        let size = Size::new(1280.0, 720.0);
        let _ = app.update(Message::WindowResized(size));
        assert_eq!(app.viewport, size, "window controls stay responsive during sign-out");
        app.title_held = true;
        let _ = app.update(Message::TitleMoved);
        assert!(!app.title_held);
        let _ = app.update(Message::WindowMaximized(true));
        assert!(app.maximized);
    }

    /// After a normal quit (or FFXI's usual -14) the account signs back in, as a pre-sign-in
    /// named "sign-in after game", only once its row has been free for RESIGN_DELAY (≥ 3 s).
    /// The row says so, and a Play during that sign-in queues behind it (07d §5.4).
    #[test]
    fn a_startup_account_signs_back_in_after_its_game_closes_after_the_delay() {
        assert!(RESIGN_DELAY >= Duration::from_secs(3), "human-plausible: no instant sign-in after a sign-out");
        for end in [launch_log::LoaderEnd::SignedOut, launch_log::LoaderEnd::Exit(-14)] {
            let (mut app, key) = resign_app("resign");
            game_ends(&mut app, 0, launch_log::classify_loader_end(&end, Duration::from_secs(900)));
            let resign = app.accounts[0].resign.expect("pending");
            assert!(!resign.network && resign.free_since.is_some(), "{end:?}");
            assert!(app.accounts[0].status_text().ends_with("Signing back in after the game closed."), "{}", app.accounts[0].status_text());
            assert!(app.launches.as_ref().unwrap().is_empty(), "not at once");
            let free = resign.free_since.unwrap();
            assert!(app.take_resigns(free + RESIGN_DELAY - Duration::from_millis(1)).is_empty());
            let keys = app.take_resigns(free + RESIGN_DELAY);
            assert_eq!(keys, vec![key.clone()]);
            assert!(app.accounts[0].resign.is_none());
            app.start_resigns(keys, launch_timing::LaunchTrigger::SignInAfterGame);
            let launches = app.launches.as_ref().unwrap();
            assert_eq!((&launches[0].0, launches[0].1, launches[0].3), (&vec![key.clone()], true, Some(launch_timing::LaunchTrigger::SignInAfterGame)));
            assert!(app.take_resigns(free + RESIGN_DELAY * 2).is_empty(), "once");
        }
        // The sign-in in flight: its progress says so, and a Play queues on it.
        let (mut app, key) = resign_app("resign-play");
        let row = &mut app.accounts[0];
        (row.busy, row.preparing_session, row.launch_trigger) = (true, true, Some(launch_timing::LaunchTrigger::SignInAfterGame));
        app.worker_event(WorkerEvent::Progress(key.clone(), "Signing in to PlayOnline…".into()));
        assert_eq!(app.accounts[0].status_text(), "Signing in to PlayOnline… · Signing back in after the game closed.");
        app.launches = None;
        let clicked = Instant::now();
        app.start_launch(vec![key.clone()], false, clicked, None);
        assert_eq!(app.accounts[0].queued_play, Some(clicked), "a queued Play, not a second sign-in");
        app.worker_event(WorkerEvent::Progress(key, "Signing in to PlayOnline…".into()));
        assert!(app.accounts[0].status_text().ends_with("FFXI will start when sign-in finishes."));
        assert_eq!(launch_timing::LaunchTrigger::SignInAfterGame.line(), "launch trigger: sign-in after game");
        // Two accounts start together, as the startup sign-ins do (their tool turns stagger them).
        let (mut app, first) = resign_app("resign-two");
        let second = app.accounts[1].key.clone();
        app.settings.account_no_otp.insert(second.clone());
        game_ends(&mut app, 0, signed_out());
        game_ends(&mut app, 1, signed_out());
        let keys = app.take_resigns(Instant::now() + RESIGN_DELAY);
        app.start_resigns(keys, launch_timing::LaunchTrigger::SignInAfterGame);
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![first, second]);
    }

    /// No sign-in after a game for an OTP account (its code is spent), with the setting off or
    /// turned off meanwhile, after an explicit sign-out or an account edit, after a worker that
    /// vanished (its host may live on), or once the user started something during the delay.
    #[test]
    fn no_sign_in_after_game_for_otp_setting_off_sign_out_edit_or_a_users_launch() {
        let (mut app, key) = resign_app("resign-none");
        let pending = |app: &App| app.accounts[0].resign.is_some();
        app.accounts[0].otp_spent = true;
        game_ends(&mut app, 0, signed_out());
        assert!(!pending(&app));
        app.accounts[0].otp_spent = false;
        app.accounts[0].otp = "123456".into();
        game_ends(&mut app, 0, signed_out());
        assert!(!pending(&app), "a typed code is for the user's Play");
        app.accounts[0].otp.clear();
        game_ends(&mut app, 0, launch_log::ExitReport::worker_ended());
        assert!(!pending(&app));
        let _ = app.update(Message::AutoSignIn(false));
        game_ends(&mut app, 0, signed_out());
        assert!(!pending(&app), "setting off");
        let _ = app.update(Message::AutoSignIn(true));
        // Turned off during the delay.
        game_ends(&mut app, 0, signed_out());
        assert!(pending(&app));
        let _ = app.update(Message::AutoSignIn(false));
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty());
        assert!(!pending(&app) && !app.accounts[0].status.contains("Signing back in"), "{}", app.accounts[0].status);
        let _ = app.update(Message::AutoSignIn(true));
        // The account learned to need a code during the delay.
        game_ends(&mut app, 0, signed_out());
        assert!(pending(&app));
        app.settings.note_sign_in(&key, true);
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty() && !pending(&app));
        app.settings.note_sign_in(&key, false);
        // The user's own Play during the delay wins.
        game_ends(&mut app, 0, signed_out());
        app.accounts[0].launch_generation += 1;
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty() && !pending(&app));
        // An explicit sign-out while the session runs.
        app.friends.account = Some(key.clone());
        let alive = Arc::new(AtomicBool::new(true));
        let row = &mut app.accounts[0];
        (row.pid, row.alive, row.game_running) = (Some(77), Some(alive.clone()), Some(Arc::new(AtomicBool::new(true))));
        let _ = app.update(Message::Tick);
        let _ = app.update(Message::FriendsSignOut);
        alive.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(!pending(&app) && !app.accounts[0].resign_declined);
        // An edit while pending.
        game_ends(&mut app, 0, signed_out());
        assert!(pending(&app));
        app.accounts[0].decline_resign();
        assert!(!pending(&app) && !app.accounts[0].resign_declined, "no session to decline");
        // An edit saved while the game runs.
        let alive = Arc::new(AtomicBool::new(true));
        let row = &mut app.accounts[0];
        (row.pid, row.alive, row.game_running) = (Some(77), Some(alive.clone()), Some(Arc::new(AtomicBool::new(true))));
        let _ = app.update(Message::Tick);
        let _ = app.update(Message::EditAccount(key.clone()));
        let saved = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(saved)));
        assert!(app.accounts[0].resign_declined && app.settings.presigns(&key), "an edit of the same login keeps what was learned");
        alive.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(!pending(&app));
        assert!(app.launches.as_ref().unwrap().is_empty());
        // A session that never ran its game (an idle sign-out) is no game end.
        let row = &mut app.accounts[0];
        let alive = Arc::new(AtomicBool::new(true));
        (row.pid, row.alive, row.game_running) = (Some(77), Some(alive.clone()), Some(Arc::new(AtomicBool::new(false))));
        let _ = app.update(Message::Tick);
        alive.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(!pending(&app));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// A crash or error first has the probe check the network (a connection loss waits for it
    /// to return); a sign-in after a game that fails on the network retries once when it is
    /// back, and never again; any other failure is not retried; a probe that gives up drops it.
    #[test]
    fn an_unclean_game_end_waits_for_the_network_and_a_failed_sign_in_after_game_never_loops() {
        let (mut app, key) = resign_app("resign-network");
        let network = "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).";
        let error = launch_log::classify_loader_end(&launch_log::LoaderEnd::Error("PlayOnline session ended (POL result -1025). Sign in again to reconnect.".into()), Duration::from_secs(900));
        game_ends(&mut app, 0, error.clone());
        assert_eq!(app.accounts[0].network_wait, Some(NetworkWait::AfterGame));
        assert!(app.accounts[0].resign.unwrap().network);
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY * 2).is_empty(), "not before the network");
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert!(!app.accounts[0].resign.unwrap().network);
        assert!(app.launches.as_ref().unwrap().is_empty());
        assert!(app.take_resigns(Instant::now()).is_empty(), "the delay still applies");
        let keys = app.take_resigns(Instant::now() + RESIGN_DELAY);
        app.start_resigns(keys, launch_timing::LaunchTrigger::SignInAfterGame);
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
        let fail = |app: &mut App, trigger, error: &str| {
            let row = &mut app.accounts[0];
            (row.busy, row.preparing_session, row.launch_trigger) = (true, true, Some(trigger));
            app.worker_event(WorkerEvent::Complete(app.accounts[0].key.clone(), Err(error.to_owned())));
        };
        fail(&mut app, launch_timing::LaunchTrigger::SignInAfterGame, "Sign-in failed");
        assert!(app.accounts[0].network_wait.is_none(), "not a network failure");
        fail(&mut app, launch_timing::LaunchTrigger::SignInAfterGame, network);
        assert_eq!(app.accounts[0].network_wait, Some(NetworkWait::AfterGameRetry));
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        let launches = app.launches.as_ref().unwrap();
        assert_eq!((launches.len(), launches[1].1, launches[1].3), (2, true, Some(launch_timing::LaunchTrigger::SignInAfterGameRetry)));
        fail(&mut app, launch_timing::LaunchTrigger::SignInAfterGameRetry, network);
        assert!(app.accounts[0].network_wait.is_none() && app.accounts[0].resign.is_none(), "no loop");
        assert_eq!(app.launches.as_ref().unwrap().len(), 2);
        // The network never came back.
        game_ends(&mut app, 0, error);
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), false).for_launch(&key, generation));
        assert!(app.accounts[0].resign.is_none() && app.accounts[0].network_wait.is_none());
        assert!(app.accounts[0].status_text().ends_with("The network did not come back. Sign in again once it is."), "{}", app.accounts[0].status_text());
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty());
        assert_eq!(app.launches.as_ref().unwrap().len(), 2);
    }

    /// Accounts that are not pre-signed (not yet known to sign in without a code, or OTP ones)
    /// keep round 2: nothing signs in after their game, and their key is parked again once the
    /// session has ended (07d §1.5 "Re-park"). A pre-signed account is not parked while its
    /// sign-in after the game is coming.
    #[test]
    fn accounts_without_the_setting_still_re_park_after_their_game() {
        let (mut app, presigned) = resign_app("resign-park");
        let parked = app.accounts[1].key.clone();
        game_ends(&mut app, 1, signed_out());
        assert!(app.accounts[1].resign.is_none() && app.accounts[1].status == "Game closed normally");
        assert!(app.park_keys().iter().any(|(row, ..)| *row == parked));
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_some());
        assert!(!app.park_keys().iter().any(|(row, ..)| *row == presigned), "its sign-in after the game is coming");
        assert!(app.launches.as_ref().unwrap().is_empty());
    }

    /// Review finding (2026-10-13): a known code-less account that Crystal will not sign in by
    /// itself now parks like any other, so its next Play starts warm: after a sign-in after its
    /// game that a maintenance refused, a sign-out (`decline_resign`), or a startup sign-in that
    /// failed. While a pre-sign-in is coming (startup sign-ins still to be taken, one waiting for
    /// a closing session, a sign-in after its game, either's network retry) it does not.
    #[test]
    fn a_pre_signed_account_with_no_sign_in_coming_parks() {
        let (mut app, key) = resign_app("presign-park");
        let parks = |app: &App| app.park_keys().iter().any(|(row, ..)| *row == key);
        assert!(app.presigned(&key) && parks(&app), "idle, signed out, nothing coming");
        app.startup_signin_pending = true;
        assert!(!parks(&app), "before the startup sign-ins are taken");
        app.startup_signin_pending = false;
        app.startup_waiting.push((key.clone(), Instant::now()));
        assert!(!parks(&app), "a startup sign-in waits for a closing session");
        app.startup_waiting.clear();
        for wait in [NetworkWait::StartupRetry, NetworkWait::AfterGame, NetworkWait::AfterGameRetry] {
            app.accounts[0].network_wait = Some(wait);
            assert!(!parks(&app), "{wait:?}");
        }
        app.accounts[0].network_wait = None;
        // A sign-in after its game is pending, then dropped: a maintenance refused it.
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_some() && !parks(&app));
        app.accounts[0].drop_resign();
        assert!(parks(&app), "nothing is coming now: its next Play starts warm");
        // A code on the row (the user's, for their own Play): Crystal does not sign it in either.
        app.accounts[0].otp = "123456".into();
        app.startup_signin_pending = true;
        assert!(parks(&app));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    fn session_lost() -> launch_log::ExitReport {
        launch_log::classify_loader_end(&launch_log::LoaderEnd::Error("PlayOnline session ended (POL result -1025). Sign in again to reconnect.".into()), Duration::from_secs(900))
    }

    /// Review finding: a connection loss longer than STARTUP_WAIT_LIMIT still signs back in when
    /// the network returns (the probe waits up to NETWORK_PROBE_LIMIT); only a probe that never
    /// reports is given up, after that limit.
    #[test]
    fn a_long_connection_loss_still_signs_back_in_when_the_network_returns() {
        let (mut app, key) = resign_app("resign-long-outage");
        game_ends(&mut app, 0, session_lost());
        let ended = app.accounts[0].resign.unwrap().ended;
        assert!(app.take_resigns(ended + STARTUP_WAIT_LIMIT + Duration::from_secs(1)).is_empty());
        assert!(app.accounts[0].resign.is_some() && app.accounts[0].network_wait == Some(NetworkWait::AfterGame), "still waiting for the probe");
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        let now = Instant::now();
        assert!(app.take_resigns(now).is_empty(), "the delay still applies");
        assert_eq!(app.take_resigns(now + RESIGN_DELAY), vec![key.clone()]);
        // A probe that never reports.
        game_ends(&mut app, 0, session_lost());
        let ended = app.accounts[0].resign.unwrap().ended;
        assert!(app.take_resigns(ended + session_worker::NETWORK_PROBE_LIMIT).is_empty());
        assert!(app.accounts[0].resign.is_some());
        assert!(app.take_resigns(ended + session_worker::NETWORK_PROBE_LIMIT + STARTUP_WAIT_LIMIT).is_empty());
        assert!(app.accounts[0].resign.is_none() && app.accounts[0].network_wait.is_none());
    }

    /// Review finding: Crystal opened during the game queues its startup sign-in for the end of
    /// that session; the game's close makes it the delayed "sign-in after game", never a startup
    /// sign-in one Tick after the sign-out.
    #[test]
    fn a_startup_sign_in_waiting_on_the_game_becomes_the_delayed_sign_in_after_it() {
        let (mut app, key) = resign_app("resign-waiting");
        app.startup_waiting.push((key.clone(), Instant::now()));
        game_ends(&mut app, 0, signed_out());
        assert!(app.startup_waiting.is_empty() && app.accounts[0].resign.is_some());
        let _ = app.update(Message::Tick);
        assert!(app.launches.as_ref().unwrap().is_empty(), "no sign-in right after the sign-out");
        let keys = app.take_resigns(Instant::now() + RESIGN_DELAY);
        app.start_resigns(keys, launch_timing::LaunchTrigger::SignInAfterGame);
        let launches = app.launches.as_ref().unwrap();
        assert_eq!((&launches[0].0, launches[0].3), (&vec![key], Some(launch_timing::LaunchTrigger::SignInAfterGame)));
    }

    /// Review finding: during a known maintenance or outage nothing signs back in by itself:
    /// not after the game, not pending, not as the network retry.
    #[test]
    fn no_sign_in_after_game_during_maintenance_or_an_outage() {
        let outage = |app: &mut App, on: bool| {
            let snapshot = app.services.as_mut().unwrap();
            snapshot.availability = if on { services::Availability::Outage } else { services::Availability::Unknown };
            snapshot.checked_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
            snapshot.notices.clear();
        };
        let (mut app, key) = resign_app("resign-outage");
        outage(&mut app, true);
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_none());
        outage(&mut app, false);
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_some());
        outage(&mut app, true);
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty() && app.accounts[0].resign.is_none());
        app.accounts[0].network_wait = Some(NetworkWait::AfterGameRetry);
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert!(app.launches.as_ref().unwrap().is_empty());
    }

    /// 2026-10-13 05:08:17 UTC: the sign-in after the game ran during All Worlds Maintenance
    /// because the status page still showed the worlds up. The window refuses it now, and the
    /// row says why; an early end (all up 15+ minutes into the window) allows it again.
    #[test]
    fn no_sign_in_after_game_during_an_official_maintenance_window() {
        let (mut app, key) = resign_app("resign-window");
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(8 * 60)));
        assert!(!app.resign_allowed(&key));
        game_ends(&mut app, 0, signed_out());
        let row = &app.accounts[0];
        assert!(row.resign.is_none());
        assert!(row.status_text().contains("No sign-in after the game while the worlds are down.") && row.status_text().contains("All Worlds Maintenance (Oct. 12)"), "{}", row.status_text());
        // Pending when the window starts: dropped with the note.
        status_read(&mut app, services::Availability::Operational, None);
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_some());
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(60)));
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty() && app.accounts[0].resign.is_none());
        assert!(app.accounts[0].status_text().contains("No sign-in after the game while the worlds are down."), "{}", app.accounts[0].status_text());
        assert!(!app.accounts[0].status.contains("Signing back in"), "{}", app.accounts[0].status);
        // The network retry is refused the same way.
        app.accounts[0].network_wait = Some(NetworkWait::AfterGameRetry);
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        // Ended early: the page shows every world up 20 minutes in.
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(20 * 60)));
        assert!(app.resign_allowed(&key));
        // The maintenance icon past the window's end still refuses.
        status_read(&mut app, services::Availability::Maintenance, None);
        assert!(!app.resign_allowed(&key));
        assert!(app.launches.as_ref().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Review finding: an account edit also cancels the pending network retry of a failed
    /// sign-in after the game, and an edit whose settings save failed still declines.
    #[test]
    fn an_edit_cancels_a_network_retry_and_a_failed_settings_save_still_declines() {
        let network = "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).";
        let (mut app, key) = resign_app("resign-retry-edit");
        let row = &mut app.accounts[0];
        (row.busy, row.preparing_session, row.launch_trigger) = (true, true, Some(launch_timing::LaunchTrigger::SignInAfterGame));
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(network.into())));
        assert_eq!(app.accounts[0].network_wait, Some(NetworkWait::AfterGameRetry));
        let _ = app.update(Message::EditAccount(key.clone()));
        let saved: Vec<Account> = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(saved.clone())));
        assert!(app.accounts[0].network_wait.is_none());
        assert!(!app.accounts[0].status.contains("Crystal will try again"), "{}", app.accounts[0].status);
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert!(app.launches.as_ref().unwrap().is_empty());
        // The settings save fails (a file where its folder belongs).
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_some());
        std::fs::create_dir_all(&app.config.experiment).unwrap();
        let _ = std::fs::remove_dir_all(app.config.experiment.join("iced-launcher"));
        std::fs::write(app.config.experiment.join("iced-launcher"), b"").unwrap();
        let _ = app.update(Message::EditAccount(key.clone()));
        let _ = app.update(Message::AccountsSaved(Ok(saved)));
        assert!(app.status.starts_with("Accounts saved, but launcher settings were not saved"), "{}", app.status);
        assert!(app.accounts[0].resign.is_none());
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty());
        assert!(app.launches.as_ref().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Review finding: an edit while pre-signed and idle declines nothing about a game started
    /// from that session afterwards (a pre-signed Play): its end still signs back in.
    #[test]
    fn an_edit_before_the_game_still_signs_back_in_after_it() {
        let (mut app, key) = resign_app("resign-edit-idle");
        let (alive, game) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));
        let row = &mut app.accounts[0];
        (row.pid, row.alive, row.game_running) = (Some(77), Some(alive.clone()), Some(game.clone()));
        let _ = app.update(Message::Tick);
        let _ = app.update(Message::EditAccount(key.clone()));
        let saved = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(saved)));
        assert!(app.accounts[0].resign_declined);
        game.store(true, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].game_seen && !app.accounts[0].resign_declined);
        alive.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].resign.is_some());
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Review finding: "fully ended" includes the host process. A session whose worker closed
    /// while its loader still runs (a pipe failure) waits for the loader to exit.
    #[test]
    fn a_sign_in_after_game_waits_for_the_old_host_process_to_exit() {
        let (mut app, key) = resign_app("resign-host");
        RUNNING_HOSTS.with(|hosts| hosts.borrow_mut().push(77));
        game_ends(&mut app, 0, signed_out());
        let resign = app.accounts[0].resign.unwrap();
        assert_eq!(resign.host.map(|host| host.pid), Some(77));
        assert!(resign.free_since.is_none(), "host still running");
        assert!(app.take_resigns(Instant::now() + RESIGN_DELAY * 2).is_empty());
        RUNNING_HOSTS.with(|hosts| hosts.borrow_mut().clear());
        let now = Instant::now();
        assert!(app.take_resigns(now).is_empty());
        assert_eq!(app.take_resigns(now + RESIGN_DELAY), vec![key]);
    }

    #[test]
    fn queued_play_waits_for_vault_and_file_operations_then_uses_signed_in_host() {
        let mut app = app();
        app.preview = false;
        // An explicit missing install keeps this test entirely offline and
        // exercises the normal launch validation before any start request.
        let missing = std::env::temp_dir().join(format!("crystal-no-queued-game-{}", std::process::id()));
        assert!(!missing.exists());
        app.settings.install_selected = true;
        app.settings.install_path = missing.to_string_lossy().into_owned();
        app.settings.account_installs.clear();
        app.vault_saving = true;
        let row = &mut app.accounts[0];
        row.busy = true;
        row.preparing_session = true;
        let clicked = Instant::now() - Duration::from_secs(2);
        row.queued_play = Some(clicked);
        let key = row.key.clone();
        app.worker_event(WorkerEvent::Complete(key, Ok(LaunchResult {
            pid: 123, detail: "fixture".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::new(),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        assert!(!app.accounts[0].can_play());
        app.vault_saving = false;
        app.vault_ready = false;
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        app.vault_ready = true;
        app.maintenance = Some(MaintenanceKind::Repair);
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        app.maintenance = None;
        app.start_queued_plays();
        let row = &app.accounts[0];
        assert_eq!(row.pid, Some(123), "keep the original signed-in host");
        assert!(row.queued_play.is_none());
        assert!(row.problem && row.status.contains("FFXi.dll"), "normal launch validation still runs");
        assert!(app.receiver.try_recv().is_err(), "never start a second sign-in worker");
    }

    /// A Play queued behind a pre-sign-in is re-checked when this window releases it: an outage
    /// or maintenance begun since the click makes it wait for FFXI ("Wait and play") instead of
    /// staying queued on the sign-in.
    #[test]
    fn a_queued_play_waits_at_release_when_downtime_began_since_its_click() {
        for (page, window) in [(services::Availability::Outage, None), (services::Availability::Operational, Some(Duration::from_secs(60)))] {
            let mut app = app();
            app.preview = false;
            let clicked = Instant::now() - Duration::from_secs(2);
            let row = &mut app.accounts[0];
            row.pid = Some(123);
            row.alive = Some(Arc::new(AtomicBool::new(true)));
            row.game_running = Some(Arc::new(AtomicBool::new(false)));
            row.queued_play = Some(clicked);
            status_read(&mut app, page, window);
            assert_eq!(app.service_availability(), if window.is_some() { services::Availability::Maintenance } else { page });
            app.start_queued_plays();
            let row = &app.accounts[0];
            assert!(row.queued_play.is_none(), "{page:?}");
            assert!(!row.busy && !row.batch_pending && row.pid == Some(123));
            assert_eq!(row.status, app.wait_headline());
            assert!(app.is_waiting(&row.key) && app.status == row.status);
            assert!(app.receiver.try_recv().is_err());
            app.start_queued_plays();
            assert!(app.accounts[0].queued_play.is_none(), "not re-queued");
            assert_eq!(app.waiting_plays.len(), 1);
        }
    }

    /// A fresh status read whose page shows `page`; with `window`, an official all-worlds
    /// maintenance window that began that long ago covers now.
    fn status_read(app: &mut App, page: services::Availability, window: Option<Duration>) {
        let now = chrono::Utc::now();
        let snapshot = app.services.as_mut().unwrap();
        snapshot.availability = page;
        snapshot.availability_note = "Official status page reports all 16 production worlds up.".into();
        snapshot.notices.clear();
        snapshot.windows = window.into_iter().map(|since| services::MaintenanceWindow {
            title: "All Worlds Maintenance (Oct. 12)".into(),
            start: now - chrono::Duration::from_std(since).unwrap(),
            end: now + chrono::Duration::minutes(60),
        }).collect();
        snapshot.checked_at = now.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    }

    /// 2026-10-13 05:07 UTC: Play clicked seconds after Crystal opened, during All Worlds
    /// Maintenance, while the status page still showed the worlds up. The Play is held for the
    /// first status read, which finds maintenance: it then waits for FFXI ("Wait and play"); a
    /// sign-in only is never held.
    #[test]
    fn a_play_before_the_first_status_read_is_held_then_waits_during_maintenance() {
        let mut app = app();
        app.preview = false;
        app.status_read_done = false;
        let key = app.accounts[0].key.clone();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert_eq!(app.held_plays.len(), 1);
        assert_eq!(app.accounts[0].status, "Checking whether FINAL FANTASY XI is up…");
        assert_eq!(app.status, "Checking whether FINAL FANTASY XI is up…");
        assert!(!app.accounts[0].busy && app.receiver.try_recv().is_err(), "nothing started");
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert_eq!(app.held_plays.len(), 1, "a second click is the same held Play");
        app.launches = Some(Vec::new());
        let other = app.accounts[1].key.clone();
        app.start_launch(vec![other.clone()], true, Instant::now(), None);
        assert_eq!(app.launches.as_ref().unwrap().len(), 1, "a sign-in only is not held");
        app.launches = None;
        // The read lands: the page still shows every world up, but the window covers now.
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        let snapshot = app.services.clone().unwrap();
        app.worker_event(WorkerEvent::Services(app.news_generation, i18n::current(), services::Update::Status(snapshot)));
        assert!(app.status_read_done && app.held_plays.is_empty());
        assert_eq!(app.service_availability(), services::Availability::Maintenance);
        assert!(app.service_note().contains("All Worlds Maintenance (Oct. 12)") && app.service_note().contains("still shows the worlds up"));
        let row = &app.accounts[0];
        assert!(!row.busy && row.pid.is_none() && row.queued_play.is_none());
        assert_eq!(row.status, "FFXI is down for maintenance. Starting as soon as it's back…");
        assert!(app.is_waiting(&key) && app.status == row.status);
        assert!(app.receiver.try_recv().is_err(), "no sign-in worker");
    }

    /// The held Play starts once the read finds the worlds up, on every path: a cold Play and
    /// a Play queued behind a pre-sign-in in flight.
    #[test]
    fn a_held_play_starts_after_the_first_status_read_finds_the_worlds_up() {
        let mut app = app();
        app.preview = false;
        app.status_read_done = false;
        app.launches = Some(Vec::new());
        let key = app.accounts[0].key.clone();
        let clicked = Instant::now();
        app.start_launch(vec![key.clone()], false, clicked, None);
        assert!(app.launches.as_ref().unwrap().is_empty());
        status_read(&mut app, services::Availability::Operational, None);
        let snapshot = app.services.clone().unwrap();
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Ok(snapshot)));
        assert_eq!(app.launches.as_ref().unwrap(), &vec![(vec![key.clone()], false, clicked, None)]);
        assert_ne!(app.accounts[0].status, "Checking whether FINAL FANTASY XI is up…");
        // Queued during a pre-sign-in: held, then queued on it with the original click.
        let mut app = self::app();
        app.preview = false;
        app.status_read_done = false;
        let row = &mut app.accounts[0];
        (row.busy, row.preparing_session) = (true, true);
        let clicked = Instant::now();
        app.start_launch(vec![key.clone()], false, clicked, None);
        assert!(app.accounts[0].queued_play.is_none() && app.held_plays.len() == 1);
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(20 * 60)));
        let snapshot = app.services.clone().unwrap();
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Ok(snapshot)));
        assert_eq!(app.service_availability(), services::Availability::Operational, "ended early");
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
    }

    /// A read that fails or is slow never blocks: the held Play proceeds after STATUS_HOLD_LIMIT,
    /// and later Plays are not held again.
    #[test]
    fn a_held_play_proceeds_when_the_first_status_read_times_out() {
        let mut app = app();
        app.preview = false;
        app.status_read_done = false;
        app.launches = Some(Vec::new());
        let key = app.accounts[0].key.clone();
        let clicked = Instant::now();
        app.start_launch(vec![key.clone()], false, clicked, None);
        let since = app.held_plays[0].since;
        app.release_held_plays(since + STATUS_HOLD_LIMIT - Duration::from_millis(1));
        assert!(app.launches.as_ref().unwrap().is_empty() && app.held_plays.len() == 1);
        app.held_plays[0].since = Instant::now() - STATUS_HOLD_LIMIT;
        let _ = app.update(Message::Tick);
        assert!(app.held_plays.is_empty() && app.status_hold_spent && !app.status_read_done);
        assert_eq!(app.launches.as_ref().unwrap()[0], (vec![key.clone()], false, clicked, None));
        let second = app.accounts[1].key.clone();
        app.start_launch(vec![second.clone()], false, clicked, None);
        assert_eq!(app.launches.as_ref().unwrap()[1].0, vec![second], "not held again");
        // A failed read releases at once.
        let mut app = self::app();
        app.preview = false;
        app.status_read_done = false;
        app.launches = Some(Vec::new());
        app.start_launch(vec![key.clone()], false, clicked, None);
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Err("timed out".into())));
        assert_eq!(app.launches.as_ref().unwrap().len(), 1);
    }

    /// Review findings: a held Play released into an account save, a deleted account, or a
    /// paused launch was lost or left a blank row; a held Play let a launcher update restart.
    #[test]
    fn a_held_play_survives_saves_and_deletes_and_blocks_a_launcher_restart() {
        let mut app = app();
        app.preview = false;
        app.status_read_done = false;
        let (key, other) = (app.accounts[0].key.clone(), app.accounts[1].key.clone());
        let before = app.accounts[0].status.clone();
        app.status = "Ready.".into();
        assert!(app.can_restart_launcher());
        app.start_launch(vec![key.clone(), other.clone()], false, Instant::now(), None);
        assert!(!app.can_restart_launcher(), "a held Play is pending launch work");
        // The second account is deleted and a save runs while the read lands: still held.
        app.accounts.retain(|row| row.key != other);
        app.vault_saving = true;
        status_read(&mut app, services::Availability::Operational, None);
        app.note_status_read();
        assert_eq!(app.held_plays.len(), 1, "held through the save");
        app.vault_saving = false;
        app.launches = Some(Vec::new());
        let _ = app.update(Message::Tick);
        assert!(app.held_plays.is_empty());
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key.clone()], "the deleted account is left out");
        assert_eq!(app.accounts[0].status, before, "the row says what it said before the hold");
        assert_eq!(app.status, "Ready.");
        // The hold text follows a language switch, and the release still recognises it.
        let mut app = self::app();
        app.preview = false;
        app.status_read_done = false;
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        let english = app.accounts[0].status.clone();
        i18n::set(i18n::Lang::Ja);
        app.retranslate(i18n::Lang::En);
        assert_ne!(app.accounts[0].status, english);
        assert_eq!(app.accounts[0].status, t("Checking whether FINAL FANTASY XI is up…"));
        assert_eq!(app.status, t("Checking whether FINAL FANTASY XI is up…"));
        app.launches = Some(Vec::new());
        app.note_status_read();
        assert_ne!(app.accounts[0].status, t("Checking whether FINAL FANTASY XI is up…"));
        i18n::set(i18n::Lang::En);
    }

    /// A read of the play flag, as `read_play_flag` delivers it, made `ago` before now.
    fn flag_read(app: &mut App, ago: Duration, result: Result<play_flag::Flag, String>) {
        let at = chrono::Utc::now() - chrono::Duration::from_std(ago).unwrap();
        let _ = app.update(Message::PlayFlagLoaded(play_flag::Reading { at, result, patch: None }));
    }

    const SERVER_STATUS: &str = "The game is unavailable for play during maintenance.";

    /// An app during All Worlds Maintenance (a posted window seven minutes in, the status page
    /// still showing the worlds up), whose Plays are recorded instead of started.
    fn maintenance_app() -> App {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        assert_eq!(app.main_action(), MainAction::Maintenance);
        app
    }

    /// Owner decision 2026-10-13, "Wait and play": the main button sets the selected accounts
    /// waiting, a second click adds nothing, and they start together through the normal start
    /// path once the play flag says 1, even inside the posted window.
    #[test]
    fn wait_and_play_waits_during_maintenance_and_starts_when_the_flag_says_ffxi_is_back() {
        let mut app = maintenance_app();
        for row in &mut app.accounts { row.selected = false; }
        app.accounts[0].selected = true;
        app.accounts[1].selected = true;
        let (first, second) = (app.accounts[0].key.clone(), app.accounts[1].key.clone());
        assert_eq!(app.wait_keys(), vec![first.clone(), second.clone()]);
        assert_eq!(app.update(Message::MainAction).units(), 0);
        assert!(app.is_waiting(&first) && app.is_waiting(&second) && app.waiting_plays.len() == 1);
        assert!(!app.show_details && app.wait_keys().is_empty());
        assert_eq!(app.accounts[0].status, "FFXI is down for maintenance. Starting as soon as it's back…");
        assert!(app.launches.as_ref().unwrap().is_empty() && app.receiver.try_recv().is_err(), "nothing signs in while waiting");
        // A second click (main button or the row's Launch) is the same wait; the button then
        // explains the downtime.
        let _ = app.update(Message::MainAction);
        app.start_launch(vec![first.clone()], false, Instant::now(), None);
        assert_eq!(app.waiting_plays.len(), 1);
        assert_eq!(app.waiting_plays[0].keys, vec![first.clone(), second.clone()]);
        assert!(app.show_details);
        // The first Tick reads the flag at once, then one read at a time.
        assert_eq!(app.update(Message::Tick).units(), 1, "an immediate read");
        assert_eq!(app.update(Message::Tick).units(), 0, "one in flight");
        // 0: still waiting, with the server's line under the headline.
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Down(Some(SERVER_STATUS.into()))));
        assert!(app.is_waiting(&first) && app.launches.as_ref().unwrap().is_empty());
        let detail = app.wait_detail();
        assert!(detail.starts_with("Last checked: ") && detail.ends_with(SERVER_STATUS), "{detail}");
        assert_eq!(app.update(Message::Tick).units(), 0, "the next read is a minute later");
        // 1 inside the posted window: FFXI is up, and both start together.
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(app.waiting_plays.is_empty());
        assert_eq!(app.main_action(), MainAction::Play);
        let launches = app.launches.as_ref().unwrap();
        assert_eq!(launches.len(), 1);
        assert_eq!((&launches[0].0, launches[0].1, launches[0].3), (&vec![first.clone(), second.clone()], false, None));
        // Nothing waits: no more reads.
        assert_eq!(app.update(Message::Tick).units(), 0);
    }

    /// The flag wins both ways while fresh; without a fresh read the official status decides, and
    /// the row says why.
    #[test]
    fn a_waiting_play_follows_the_official_status_when_the_flag_is_stale_or_unreadable() {
        let mut app = maintenance_app();
        let key = app.accounts[0].key.clone();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        // 0 while the status page shows every world up 20 minutes in (Round 4: ended early).
        flag_read(&mut app, Duration::from_secs(30), Ok(play_flag::Flag::Down(None)));
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(20 * 60)));
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key), "a fresh 0 keeps it waiting");
        // A later read fails: the 0 read 30 s ago still decides.
        flag_read(&mut app, Duration::ZERO, Err("timed out".into()));
        assert!(app.is_waiting(&key) && app.launches.as_ref().unwrap().is_empty());
        // Once no read is fresh, the official status decides: maintenance still, and the row says
        // the flag could not be read.
        let mut app = maintenance_app();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        flag_read(&mut app, Duration::ZERO, Err("The play flag server answered HTTP 503.".into()));
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key));
        assert!(app.service_note().contains("All Worlds Maintenance (Oct. 12)"));
        assert!(app.wait_detail().contains("The play flag could not be read (The play flag server answered HTTP 503.), so this follows the official status."),
            "{}", app.wait_detail());
        // ... until the official status says up: the next read decides first, and when it fails
        // too, the official status alone starts the Play.
        status_read(&mut app, services::Availability::Operational, None);
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key), "between reads only a fresh 1 starts it");
        flag_read(&mut app, Duration::ZERO, Err("timed out".into()));
        assert!(!app.is_waiting(&key));
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key.clone()]);
        // A stale 1 (over two minutes old) proves nothing.
        let mut app = maintenance_app();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        flag_read(&mut app, Duration::from_secs(150), Ok(play_flag::Flag::Up));
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key) && app.launches.as_ref().unwrap().is_empty());
    }

    /// Review findings: a wait is released only on evidence that FFXI is up (a fresh 1, or the
    /// official status right after a read), never on Unknown; after the PC wakes the read due
    /// then decides, not the stale status before it; and a reading that has just gone stale
    /// waits for the read that follows it instead of the official status deciding between reads.
    #[test]
    fn a_wait_is_released_only_on_evidence_that_ffxi_is_up() {
        let mut app = maintenance_app();
        let key = app.accounts[0].key.clone();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert_eq!(app.update(Message::Tick).units(), 1);
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Down(None)));
        // The PC slept 40 minutes, past the posted end of a maintenance that was extended: the
        // status read and the flag reading are stale, and no window covers now.
        let long_ago = chrono::Utc::now() - chrono::Duration::minutes(40);
        let snapshot = app.services.as_mut().unwrap();
        snapshot.windows.clear();
        snapshot.checked_at = long_ago.format("%Y-%m-%d %H:%M:%S UTC").to_string();
        app.play_flag = play_flag::Known::default();
        app.play_flag.note(play_flag::Reading { at: long_ago, result: Ok(play_flag::Flag::Down(None)), patch: None });
        assert_eq!(app.service_availability(), services::Availability::Unknown);
        app.flag_schedule.tick(long_ago);
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key) && app.launches.as_ref().unwrap().is_empty(), "the wake's read decides");
        // That read fails: still nothing says FFXI is up.
        flag_read(&mut app, Duration::ZERO, Err("timed out".into()));
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key) && app.launches.as_ref().unwrap().is_empty(), "Unknown is not up");
        // 0 (the extended maintenance), then 1.
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Down(None)));
        assert!(app.is_waiting(&key));
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(!app.is_waiting(&key));
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key.clone()]);
        // The page shows every world up 20 minutes into the window (Round 4: ended early) while
        // the flag said 0 just over two minutes ago, and the read after it failed: between reads
        // the official status alone starts nothing; the read that follows decides.
        let mut app = maintenance_app();
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(20 * 60)));
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Down(None)));
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert!(app.is_waiting(&key));
        let now = chrono::Utc::now();
        app.play_flag = play_flag::Known::default();
        app.play_flag.note(play_flag::Reading { at: now - chrono::Duration::seconds(121), result: Ok(play_flag::Flag::Down(None)), patch: None });
        app.play_flag.note(play_flag::Reading { at: now - chrono::Duration::seconds(61), result: Err("timed out".into()), patch: None });
        assert_eq!(app.service_availability(), services::Availability::Operational);
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key), "not between reads");
        // That read fails too: the official status decides, as the fallback does.
        flag_read(&mut app, Duration::ZERO, Err("timed out".into()));
        assert!(!app.is_waiting(&key));
    }

    /// Review finding: the play flag is FFXI's maintenance switch, so a 1 never ends an outage
    /// (every world down outside maintenance): the wait goes on until the status page shows the
    /// worlds back, and no sign-in after a game follows meanwhile.
    #[test]
    fn an_outage_waits_for_the_worlds_even_when_the_play_flag_is_1() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        status_read(&mut app, services::Availability::Outage, None);
        let key = app.accounts[0].key.clone();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert!(app.is_waiting(&key));
        assert_eq!(app.accounts[0].status, "FFXI's worlds are down. Starting as soon as they're back…");
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        let _ = app.update(Message::Tick);
        assert!(app.is_waiting(&key) && app.launches.as_ref().unwrap().is_empty());
        assert_eq!(app.service_availability(), services::Availability::Outage);
        assert_eq!(app.main_action(), MainAction::Offline);
        app.settings.account_no_otp.insert(key.clone());
        app.settings.sign_in_after_game = true;
        assert!(app.resign_eligible(&key) && !app.resign_allowed(&key), "no sign-in after a game while the worlds are down");
        // The worlds are back.
        status_read(&mut app, services::Availability::Operational, None);
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(!app.is_waiting(&key));
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key]);
    }

    /// Review finding: a waiting Play whose startup sign-in is refused (a verdict on the account)
    /// stops waiting, and its row shows the refusal: a release would only sign in again into the
    /// same verdict. A network failure, which retries by itself, keeps the wait.
    #[test]
    fn a_waiting_play_whose_sign_in_is_refused_stops_waiting_and_says_why() {
        for (failure, waits) in [("Sign-in failed", false),
            ("Game loader error during startup: Native PlayOnline login failed (state 39, POL result -19). (Windower via crystal-loader).", true)] {
            let mut app = maintenance_app();
            let key = app.accounts[0].key.clone();
            let row = &mut app.accounts[0];
            (row.busy, row.preparing_session, row.launch_trigger) = (true, true, Some(launch_timing::LaunchTrigger::StartupSignIn));
            app.start_launch(vec![key.clone()], false, Instant::now(), None);
            assert!(app.is_waiting(&key));
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(failure.to_owned())));
            assert_eq!(app.is_waiting(&key), waits, "{failure}");
            if !waits {
                assert!(app.accounts[0].problem && app.accounts[0].status == failure);
                flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
                assert!(app.launches.as_ref().unwrap().is_empty(), "no second login");
            }
        }
    }

    /// Review finding: starting the first released group can pause launches (an interrupted
    /// update it finishes first); a later group then keeps waiting instead of being dropped.
    #[test]
    fn a_later_wait_group_keeps_waiting_when_an_earlier_one_pauses_launches() {
        let mut app = maintenance_app();
        // The real start path, which finishes the interrupted update (a temp folder) first.
        app.launches = None;
        app.viewer = None;
        let (first, second) = (app.accounts[0].key.clone(), app.accounts[1].key.clone());
        app.start_launch(vec![first.clone()], false, Instant::now(), None);
        app.start_launch(vec![second.clone()], false, Instant::now(), None);
        assert_eq!(app.waiting_plays.len(), 2);
        assert!(app.receiver.try_recv().is_err());
        let root = std::env::temp_dir().join(format!("crystal-wait-recover-{}", backend::unique_id().unwrap()));
        std::fs::create_dir_all(root.join(".xi-updater")).unwrap();
        std::fs::write(root.join(".xi-updater").join("transaction.json"), b"{}").unwrap();
        app.settings.install_path = root.to_string_lossy().into_owned();
        app.settings.install_selected = true;
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(app.recovering.is_some(), "the first group finishes the interrupted update first");
        assert!(!app.is_waiting(&first) && app.is_waiting(&second), "the second keeps waiting");
        // The recovery worker answers (it refuses the fixture journal); nothing signed in.
        let event = app.receiver.recv_timeout(Duration::from_secs(30)).ok();
        assert!(matches!(event, Some(WorkerEvent::InterruptedUpdate(_))));
        let _ = std::fs::remove_dir_all(root);
    }

    /// The flag is read only while a Play waits or FFXI is in maintenance (for `$patch_enable`,
    /// patch_watch), once at the start, every minute, and again at once after the PC wakes.
    #[test]
    fn the_flag_is_read_only_while_waiting_or_in_maintenance_and_again_after_a_resume() {
        let mut app = maintenance_app();
        assert_eq!(app.update(Message::Tick).units(), 1, "maintenance: a read with nothing waiting");
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Down(None)));
        assert_eq!(app.update(Message::Tick).units(), 0);
        let key = app.accounts[0].key.clone();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert_eq!(app.update(Message::Tick).units(), 0, "a wait adds no read");
        assert_eq!(app.update(Message::Tick).units(), 0);
        // Review finding: more waits (other rows, Cancel then Play again) send no extra reads.
        let others: Vec<String> = app.accounts[1..].iter().map(|row| row.key.clone()).collect();
        for other in &others { app.start_launch(vec![other.clone()], false, Instant::now(), None); }
        let _ = app.update(Message::CancelWait(vec![others[0].clone()]));
        app.start_launch(vec![others[0].clone()], false, Instant::now(), None);
        assert_eq!(app.waiting_plays.len(), others.len() + 1);
        assert_eq!(app.update(Message::Tick).units(), 0);
        // The last Tick was 40 minutes ago by the wall clock: the PC slept.
        app.flag_schedule.tick(chrono::Utc::now() - chrono::Duration::minutes(40));
        assert_eq!(app.update(Message::Tick).units(), 1, "a read at resume");
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Down(None)));
        // Cancelled, and the maintenance over: no wait, no read, even at a resume.
        let _ = app.update(Message::CancelWait([vec![key.clone()], others].concat()));
        assert!(app.waiting_plays.is_empty());
        status_read(&mut app, services::Availability::Operational, None);
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(!app.patch_watch.active());
        app.flag_schedule.tick(chrono::Utc::now() - chrono::Duration::minutes(40));
        assert_eq!(app.update(Message::Tick).units(), 0);
    }

    /// Cancel drops the wait (and its row says what an idle row says); a waiting Play blocks a
    /// launcher self-update restart, as a held Play does, since waits are not saved.
    #[test]
    fn cancel_ends_a_wait_and_a_wait_blocks_a_launcher_restart() {
        let mut app = maintenance_app();
        let (first, second) = (app.accounts[0].key.clone(), app.accounts[1].key.clone());
        assert!(app.can_restart_launcher());
        app.start_launch(vec![first.clone(), second.clone()], false, Instant::now(), None);
        assert!(!app.can_restart_launcher(), "a waiting Play is pending launch work");
        let _ = app.update(Message::CancelWait(vec![first.clone()]));
        assert!(!app.is_waiting(&first) && app.is_waiting(&second));
        assert_eq!(app.accounts[0].status, "Ready to launch");
        assert!(!app.can_restart_launcher());
        assert_eq!(app.status, "FFXI is down for maintenance. Starting as soon as it's back…", "the other still waits");
        let _ = app.update(Message::CancelWait(vec![second.clone()]));
        assert!(app.waiting_plays.is_empty() && app.can_restart_launcher());
        // Review finding: nothing waits, so the status line no longer promises a start; it says
        // why FFXI is closed.
        assert_eq!(app.status, app.service_note());
        assert!(app.status.contains("All Worlds Maintenance (Oct. 12)"), "{}", app.status);
        // A late Cancel for a row that no longer waits leaves what it says.
        app.accounts[1].status = "FFXI is back — enter your one-time password to start".into();
        let _ = app.update(Message::CancelWait(vec![second.clone()]));
        assert_eq!(app.accounts[1].status, "FFXI is back — enter your one-time password to start");
        // A deleted account no longer waits (nor reads the flag, nor holds a restart back).
        app.start_launch(vec![second.clone()], false, Instant::now(), None);
        app.accounts.retain(|row| row.key != second);
        let _ = app.update(Message::Tick);
        assert!(app.waiting_plays.is_empty() && app.can_restart_launcher());
        // Nothing starts when FFXI is back.
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        let _ = app.update(Message::Tick);
        assert!(app.launches.as_ref().unwrap().is_empty());
        // FFXI is up (the flag read just now): a Play starts at once.
        app.start_launch(vec![first.clone()], false, Instant::now(), None);
        assert!(app.launches.as_ref().unwrap().len() == 1 && app.waiting_plays.is_empty(), "FFXI is up: no wait");
        // The update a Play runs in its session blocks it too.
        app.play_updates.begin(PathBuf::from(r"C:\fixture\FINAL FANTASY XI"), &first, "30261006_0".into(), Instant::now(), Vec::new(), false);
        assert!(!app.can_restart_launcher());
    }

    /// A Play that goes ahead (FFXI up, the wait's own release held back by paused launches)
    /// replaces that account's wait, so the wait cannot start it a second time.
    #[test]
    fn a_play_that_goes_ahead_replaces_its_accounts_wait() {
        let mut app = app();
        app.preview = false;
        let (key, other) = (app.accounts[0].key.clone(), app.accounts[1].key.clone());
        // Private-server rows: their Plays stop at the row note, with no worker to start.
        app.accounts[0].account.mode = AccountMode::Private;
        app.accounts[1].account.mode = AccountMode::Private;
        app.waiting_plays.push(WaitingPlay { keys: vec![key.clone(), other.clone()], otp: Vec::new(), armed: Instant::now(), codes: Vec::new() });
        assert!(!app.service_down());
        app.start_launch_now(vec![key.clone()], false, Instant::now(), None, false);
        assert!(!app.is_waiting(&key) && app.is_waiting(&other));
        assert!(app.receiver.try_recv().is_err());
        // A sign-in only replaces nothing.
        app.start_launch_now(vec![other.clone()], true, Instant::now(), None, false);
        assert!(app.is_waiting(&other));
    }

    /// A wait that began OTP_KEEP or more ago.
    fn age_waits(app: &mut App) {
        for wait in &mut app.waiting_plays { wait.armed = Instant::now() - OTP_KEEP - Duration::from_secs(1); }
    }

    /// Never a sign-in with a stale one-time code: a code typed before the wait goes off the row
    /// and is dropped once stale, and an account that signs in with one asks for a new code when
    /// FFXI is back. A signed-in session needs no code and starts.
    #[test]
    fn an_otp_account_asks_for_a_new_code_when_ffxi_is_back() {
        let mut app = maintenance_app();
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        // A code typed for this Play; a code taken by the last sign-in.
        app.accounts[0].otp = "123456".into();
        app.accounts[1].otp_spent = true;
        // Signed in at startup with a code; its session is idle at the main menu.
        let row = &mut app.accounts[2];
        (row.pid, row.otp_spent) = (Some(4242), true);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(Arc::new(AtomicBool::new(false)));
        app.start_launch(keys.clone(), false, Instant::now(), None);
        assert!(keys.iter().all(|key| app.is_waiting(key)));
        assert!(app.accounts[0].otp.is_empty(), "the typed code goes with the wait");
        assert_eq!(app.waiting_plays[0].otp, vec![keys[0].clone()]);
        assert_eq!(app.waiting_plays[0].codes.len(), 1);
        // Stale: the Tick drops it.
        age_waits(&mut app);
        let _ = app.update(Message::Tick);
        assert!(app.waiting_plays[0].codes.is_empty());
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        let launches = app.launches.as_ref().unwrap();
        assert_eq!(launches.len(), 1);
        assert_eq!((&launches[0].0, launches[0].3), (&vec![keys[2].clone()], None), "the signed-in session starts");
        for row in &app.accounts[..2] {
            assert_eq!(row.status, "FFXI is back — enter your one-time password to start");
            assert!(row.problem && row.otp.is_empty() && !app.is_waiting(&row.key));
        }
        assert!(app.waiting_plays.is_empty());
        // A code typed during a long wait is stale by the release too.
        let mut app = maintenance_app();
        app.start_launch(vec![keys[0].clone()], false, Instant::now(), None);
        app.accounts[0].otp = "654321".into();
        age_waits(&mut app);
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(app.launches.as_ref().unwrap().is_empty() && app.accounts[0].otp.is_empty());
        assert_eq!(app.accounts[0].status, "FFXI is back — enter your one-time password to start");
        // Review finding: an account known (from an earlier window) to sign in with a code, with
        // none typed and no sign-in this run, asks for one instead of signing in without it.
        let mut app = maintenance_app();
        app.settings.account_otp.insert(keys[1].clone());
        app.start_launch(vec![keys[1].clone()], false, Instant::now(), None);
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert!(app.launches.as_ref().unwrap().is_empty());
        assert_eq!(app.accounts[1].status, "FFXI is back — enter your one-time password to start");
    }

    /// Review finding: the code typed in answer to "FFXI is back" while the official status still
    /// lags (the flag reading behind the release has gone stale) waits for the immediate read,
    /// which says 1: that code is seconds old and starts the Play, not a second prompt.
    #[test]
    fn a_code_typed_for_a_play_that_waits_is_used_when_ffxi_is_back_at_once() {
        let mut app = maintenance_app();
        let key = app.accounts[0].key.clone();
        app.accounts[0].otp = "123456".into();
        app.accounts[0].otp_spent = true;
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        assert!(app.is_waiting(&key) && app.accounts[0].otp.is_empty());
        assert_eq!(app.update(Message::Tick).units(), 1, "the immediate read");
        flag_read(&mut app, Duration::ZERO, Ok(play_flag::Flag::Up));
        assert_eq!(app.launches.as_ref().unwrap()[0].0, vec![key.clone()]);
        assert_eq!(app.accounts[0].otp, "123456", "the code goes to the sign-in");
        assert!(!app.accounts[0].problem && app.waiting_plays.is_empty());
        // A Cancel drops the kept code with the wait.
        let mut app = maintenance_app();
        app.accounts[0].otp = "654321".into();
        app.start_launch(vec![key.clone()], false, Instant::now(), None);
        let _ = app.update(Message::CancelWait(vec![key.clone()]));
        assert!(app.waiting_plays.is_empty() && app.accounts[0].otp.is_empty());
    }

    /// Review finding: whether an account signs in with a one-time code is learned from its
    /// sessions and kept in Settings (never the code), so a later window knows it.
    #[test]
    fn an_account_whose_sign_in_took_a_code_is_remembered_as_one() {
        let mut app = app();
        app.preview = false;
        let root = std::env::temp_dir().join(format!("crystal-otp-account-{}", backend::unique_id().unwrap()));
        app.config.experiment = root.clone();
        let key = app.accounts[0].key.clone();
        // `trigger` None: a reconnect to an earlier window's session.
        let completed = |app: &mut App, spent: bool, trigger: Option<launch_timing::LaunchTrigger>| {
            let row = &mut app.accounts[0];
            (row.busy, row.otp_spent, row.launch_trigger) = (true, spent, trigger);
            let key = row.key.clone();
            app.worker_event(WorkerEvent::Complete(key, Ok(LaunchResult {
                pid: 41, detail: "signed in".into(), alive: Arc::new(AtomicBool::new(true)),
                game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::new(),
                outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
            })));
        };
        let signed_in = |app: &mut App, spent: bool| completed(app, spent, Some(launch_timing::LaunchTrigger::Connect));
        signed_in(&mut app, true);
        assert!(app.settings.account_otp.contains(&key) && !app.settings.presigns(&key));
        assert!(load_settings(&app.config).unwrap().account_otp.contains(&key), "saved");
        signed_in(&mut app, false);
        assert!(app.settings.account_otp.is_empty() && load_settings(&app.config).unwrap().account_otp.is_empty());
        // 2026-10-13: and that it signs in without one, which makes it pre-signed from now on.
        assert!(app.settings.account_no_otp.contains(&key) && load_settings(&app.config).unwrap().presigns(&key));
        signed_in(&mut app, true);
        assert!(app.settings.account_no_otp.is_empty() && !load_settings(&app.config).unwrap().presigns(&key));
        // A reconnect never teaches "no code" (an older worker's record defaults to none); a code
        // its record names is still learned.
        app.settings.account_otp.clear();
        completed(&mut app, false, None);
        assert!(!app.settings.account_otp.contains(&key) && !app.settings.account_no_otp.contains(&key));
        completed(&mut app, true, None);
        assert!(app.settings.account_otp.contains(&key));
        // Review finding: nor does a Play that reuses such a session (Launch all over idle
        // sessions, or a Play queued on one still preparing) without signing in again: its
        // record (a 1.0.8 worker's) never said whether that sign-in took a code.
        app.settings.account_otp.clear();
        for trigger in [launch_timing::LaunchTrigger::BatchPlay, launch_timing::LaunchTrigger::QueuedPlay] {
            app.accounts[0].otp_unknown = true;
            completed(&mut app, false, Some(trigger));
            assert!(!app.settings.account_otp.contains(&key) && !app.settings.account_no_otp.contains(&key), "{trigger:?}");
        }
        // A sign-in this window starts is known again (start_launch and start_together clear it).
        app.accounts[0].otp_unknown = false;
        completed(&mut app, false, Some(launch_timing::LaunchTrigger::BatchPlay));
        assert!(app.settings.account_no_otp.contains(&key));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Owner decision 2026-10-13: Crystal signs in by itself exactly the retail accounts it knows
    /// sign in without a one-time code (their latest successful sign-in took none), while the
    /// one switch is on. OTP accounts, accounts never seen signing in, and rows with a code
    /// typed or spent are not; private-server accounts never.
    #[test]
    fn presign_eligibility_table() {
        let mut app = app();
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let (known, otp, unknown) = (&keys[0], &keys[1], &keys[2]);
        app.settings.account_no_otp.insert(known.clone());
        app.settings.account_otp.insert(otp.clone());
        assert!(app.settings.auto_sign_in, "on by default");
        assert_eq!(keys.iter().map(|key| app.presigned(key)).collect::<Vec<_>>(), [true, false, false]);
        // Migrated from a file before 2026-10-13: "Sign in when Crystal opens" was on.
        let mut migrated: Settings = serde_json::from_str(&format!(r#"{{"install_path":"","account_startup_signin":["{unknown}"]}}"#)).unwrap();
        migrated.migrate_startup_signin();
        app.settings.account_no_otp.extend(migrated.account_no_otp);
        assert!(app.presigned(unknown));
        // A code typed on the row (for the user's own Play), or spent by its last sign-in.
        app.accounts[0].otp = "123456".into();
        assert!(!app.presigned(known), "never a pre-sign-in with a typed code");
        app.accounts[0].otp.clear();
        app.accounts[0].otp_spent = true;
        assert!(!app.presigned(known));
        app.accounts[0].otp_spent = false;
        // Private-server accounts never.
        app.accounts[0].account.mode = AccountMode::Private;
        assert!(!app.presigned(known));
        app.accounts[0].account.mode = AccountMode::Retail;
        // Never both: the latest sign-in wins.
        assert!(app.settings.note_sign_in(known, true));
        assert!(!app.presigned(known) && app.settings.account_otp.contains(known) && !app.settings.account_no_otp.contains(known));
        assert!(!app.settings.note_sign_in(known, true), "nothing changed");
        assert!(app.settings.note_sign_in(known, false));
        assert!(app.presigned(known) && !app.settings.account_otp.contains(known));
        // The switch off: none.
        app.settings.auto_sign_in = false;
        assert!(keys.iter().all(|key| !app.presigned(key)));
    }

    /// Owner decision 2026-10-13: an account's first Play teaches Crystal. One that signed in
    /// without a code is eligible for startup sign-in; one that took
    /// a code is not, and neither is one whose sign-in failed.
    #[test]
    fn a_first_play_teaches_whether_the_account_is_pre_signed() {
        let mut app = app();
        app.preview = false;
        app.launches = Some(Vec::new());
        app.config.experiment = std::env::temp_dir().join(format!("crystal-first-play-{}", backend::unique_id().unwrap()));
        let sign_in = |app: &mut App, index: usize, code: bool, result: Result<(), &str>| {
            let row = &mut app.accounts[index];
            (row.busy, row.otp_spent, row.launch_trigger) = (true, code, Some(launch_timing::LaunchTrigger::Play));
            let key = row.key.clone();
            app.worker_event(WorkerEvent::Complete(key, result.map(|()| LaunchResult {
                pid: 41, detail: "Client started".into(), alive: Arc::new(AtomicBool::new(true)),
                game_running: Arc::new(AtomicBool::new(true)), log: PathBuf::new(),
                outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
            }).map_err(str::to_owned)));
        };
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        assert!(keys.iter().all(|key| !app.presigned(key)), "unknown accounts are not pre-signed");
        sign_in(&mut app, 0, false, Ok(()));
        sign_in(&mut app, 1, true, Ok(()));
        sign_in(&mut app, 2, false, Err("Game exited during startup with code 0 (Windower via crystal-loader)."));
        // Rows 0 and 1 are playing; their games end.
        let known = |app: &App| keys.iter().map(|key| app.settings.presigns(key)).collect::<Vec<_>>();
        assert_eq!(known(&app), [true, false, false]);
        assert!(app.settings.account_otp.contains(&keys[1]) && !app.settings.account_otp.contains(&keys[2]), "a failure teaches nothing");
        let _ = app.update(Message::Tick);
        for row in &mut app.accounts[..2] {
            row.outcome = Some(Arc::new(std::sync::Mutex::new(Some(signed_out()))));
            row.alive.as_ref().unwrap().store(false, Ordering::Release);
        }
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].resign.is_none() && app.accounts[0].signed_out, "closing the game keeps POL closed by default");
        assert!(app.accounts[1].resign.is_none(), "an OTP account does not");
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Owner decision 2026-10-13: a pre-sign-in (no code) that the member login refused may be an
    /// account that now uses a one-time code. It is marked as one and stops being pre-signed: no
    /// retry, no sign-in after its next game, none at the next start, and its row says why. Its
    /// next sign-in without a code that works makes it pre-signed again.
    #[test]
    fn a_refused_pre_sign_in_marks_the_account_otp_and_is_not_retried() {
        let refused = "Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -1036). (Windower via crystal-loader).";
        for trigger in [launch_timing::LaunchTrigger::StartupSignIn, launch_timing::LaunchTrigger::SignInAfterGame,
            launch_timing::LaunchTrigger::StartupRetry, launch_timing::LaunchTrigger::SignInAfterGameRetry] {
            assert!(trigger.is_presign());
            let (mut app, key) = resign_app("presign-refused");
            let row = &mut app.accounts[0];
            // What start_launch leaves on a pre-sign-in's row.
            (row.busy, row.preparing_session, row.launch_trigger, row.presign_login) = (true, true, Some(trigger), true);
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(refused.into())));
            let row = &app.accounts[0];
            assert!(row.network_wait.is_none() && row.resign.is_none() && !row.busy, "{trigger:?}");
            assert_eq!(row.status_text(), format!("{refused} · This account may need a one-time password now: Crystal signs it in only when you press Play."));
            assert!(app.settings.account_otp.contains(&key) && !app.settings.presigns(&key), "{trigger:?}");
            assert!(load_settings(&app.config).unwrap().account_otp.contains(&key), "saved");
            // Nothing signs it in by itself any more.
            let _ = app.update(Message::Tick);
            game_ends(&mut app, 0, signed_out());
            assert!(app.accounts[0].resign.is_none() && app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty());
            app.startup_signin_pending = true;
            assert!(app.take_startup_signins().is_empty() && app.startup_waiting.is_empty());
            assert!(app.launches.as_ref().unwrap().is_empty(), "never retried");
            let _ = std::fs::remove_dir_all(&app.config.experiment);
        }
        // A refusal of a sign-in that took a code teaches nothing; a network failure is no refusal.
        let (mut app, key) = resign_app("presign-refused-other");
        let row = &mut app.accounts[0];
        (row.busy, row.otp_spent, row.launch_trigger) = (true, true, Some(launch_timing::LaunchTrigger::Play));
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(refused.into())));
        assert!(!app.settings.account_otp.contains(&key) && app.settings.account_no_otp.contains(&key));
        app.accounts[0].otp_spent = false;
        let row = &mut app.accounts[0];
        (row.busy, row.launch_trigger, row.presign_login) = (true, Some(launch_timing::LaunchTrigger::StartupSignIn), true);
        app.worker_event(WorkerEvent::Complete(key.clone(),
            Err("Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -19). (Windower via crystal-loader).".into())));
        assert!(app.settings.presigns(&key) && app.accounts[0].network_wait == Some(NetworkWait::StartupRetry));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Review findings (2026-10-13): only a refused member login counts, and only of a sign-in
    /// Crystal started by itself. A failure of the network-level connection (before any password
    /// is sent: PlayOnline down for its own maintenance, unreachable), a cancelled login or an
    /// older loader's unnamed stage leaves a known code-less account pre-signed; so does a refusal
    /// of the user's own Play or Connect without a code (Wait and play then does not ask that
    /// account for a code it does not use). A refused pre-sign-in that a Play queued on still
    /// marks it.
    #[test]
    fn only_a_refused_member_login_of_crystals_own_sign_in_marks_the_account_otp() {
        let refused = "Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -1036). (Windower via crystal-loader).";
        for failure in ["Game loader error during startup: Native PlayOnline network connection failed (state -1025, POL result -1036). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline login was cancelled at state -1 (POL result -1036). (Windower via crystal-loader).",
            "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -1036). (Windower via crystal-loader)."] {
            let (mut app, key) = resign_app("presign-not-refused");
            let row = &mut app.accounts[0];
            (row.busy, row.preparing_session, row.launch_trigger, row.presign_login) = (true, true, Some(launch_timing::LaunchTrigger::StartupSignIn), true);
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(failure.into())));
            assert!(app.settings.presigns(&key) && !app.settings.account_otp.contains(&key), "{failure}");
            assert_eq!(app.accounts[0].status_text(), failure, "no 'may need a one-time password' note");
            let _ = std::fs::remove_dir_all(&app.config.experiment);
        }
        // The user's own sign-ins: Play, Connect, a parked one, a batch Play.
        for trigger in [launch_timing::LaunchTrigger::Play, launch_timing::LaunchTrigger::Connect,
            launch_timing::LaunchTrigger::ParkedPlay, launch_timing::LaunchTrigger::BatchPlay, launch_timing::LaunchTrigger::ReissuedPlay] {
            assert!(!trigger.is_presign());
            let (mut app, key) = resign_app("user-refused");
            let row = &mut app.accounts[0];
            (row.busy, row.launch_trigger) = (true, Some(trigger));
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(refused.into())));
            assert!(app.settings.presigns(&key) && !app.settings.account_otp.contains(&key), "{trigger:?}");
            let _ = std::fs::remove_dir_all(&app.config.experiment);
        }
        // A Play queued on a pre-sign-in (start_together keeps the existing session's
        // presign_login): that pre-sign-in's refusal arrives as the Play's, and marks it.
        let (mut app, key) = resign_app("queued-refused");
        let row = &mut app.accounts[0];
        (row.busy, row.batch_requested, row.launch_trigger, row.presign_login) = (true, true, Some(launch_timing::LaunchTrigger::QueuedPlay), true);
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(refused.into())));
        assert!(app.settings.account_otp.contains(&key) && !app.settings.presigns(&key));
        assert!(app.accounts[0].status_text().ends_with("This account may need a one-time password now: Crystal signs it in only when you press Play."));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Review findings (2026-10-13): a batch reports one account's failure to every account in it
    /// (session_batch::coordinate). Another account's refused member login marks no other account
    /// as one that needs a one-time code: not one that got its session (Ready) first, even if the
    /// raw refusal reached it, and not one told in the coordinator's own words. Only the refused
    /// account's own pre-sign-in marks it.
    #[test]
    fn a_batch_marks_only_the_refused_account_otp() {
        let refused = "Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -1036). (Windower via crystal-loader).";
        let (mut app, ready) = resign_app("batch-refused");
        let (refused_key, told) = (app.accounts[1].key.clone(), app.accounts[2].key.clone());
        app.settings.account_no_otp.extend([refused_key.clone(), told.clone()]);
        // Three pre-sign-ins in flight, each with a Play queued on it, started together.
        for row in &mut app.accounts[..3] {
            (row.busy, row.batch_pending, row.batch_requested, row.presign_login) = (true, true, true, true);
            row.launch_trigger = Some(launch_timing::LaunchTrigger::QueuedPlay);
        }
        app.worker_event(WorkerEvent::Complete(ready.clone(), Ok(LaunchResult {
            pid: 70, detail: "Signed in.".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::from("batch.log"),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert!(app.accounts[0].pid.is_some());
        app.worker_event(WorkerEvent::Complete(ready.clone(), Err(refused.into())));
        app.worker_event(WorkerEvent::Complete(refused_key.clone(), Err(refused.into())));
        let others = t("Another selected account's PlayOnline sign-in was refused, so FFXI was not started.");
        app.worker_event(WorkerEvent::Complete(told.clone(), Err(others.into())));
        assert!(app.settings.presigns(&ready) && !app.settings.account_otp.contains(&ready), "it got its session");
        assert!(app.settings.presigns(&told) && !app.settings.account_otp.contains(&told));
        assert!(app.settings.account_otp.contains(&refused_key) && !app.settings.presigns(&refused_key));
        assert!(!app.accounts[0].status_text().contains("may need a one-time password") && !app.accounts[2].status_text().contains("may need a one-time password"));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    const TEST_TOTP_KEY: &str = "JBSWY3DPEHPK3PXP";

    /// An account with an authenticator key needs no typed code: it is signed in when Crystal
    /// opens and again after its game, like an account without codes, even one known to take
    /// codes or whose last typed code was spent, and its row shows no code boxes. The switch and
    /// a code the user typed still win.
    #[test]
    fn an_authenticator_key_makes_an_otp_account_sign_in_by_itself() {
        let (mut app, key) = resign_app("authenticator-presign");
        app.settings.note_sign_in(&key, true);
        assert!(!app.presigned(&key) && account_row::needs_code(&app, &app.accounts[0]), "an OTP account without a key");
        app.accounts[0].account.totp_key = TEST_TOTP_KEY.into();
        assert!(app.accounts[0].authenticator_ready());
        assert!(app.presigned(&key) && app.resign_eligible(&key));
        assert!(!account_row::needs_code(&app, &app.accounts[0]));
        assert_eq!(account_row::row_state(&app, &app.accounts[0]), account_row::RowState::Idle);
        assert!(!account_row::row_view(&app, &app.accounts[0]).otp, "no code boxes on its row");
        // A spent typed code does not matter: the key makes a fresh one.
        app.accounts[0].otp_spent = true;
        assert!(app.presigned(&key) && !wants_typed_code(&app.settings, &app.accounts[0]));
        app.accounts[0].otp_spent = false;
        // The user's choices win: the switch, and a code typed for their own Play.
        app.settings.auto_sign_in = false;
        assert!(!app.presigned(&key));
        app.settings.auto_sign_in = true;
        app.accounts[0].otp = "123456".into();
        assert!(!app.presigned(&key));
        app.accounts[0].otp.clear();
        // When Crystal opens, and after its game.
        app.startup_signin_pending = true;
        assert_eq!(app.take_startup_signins(), vec![key.clone()]);
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_some(), "it signs back in after its game");
        // Its own sign-in without a typed code works: the account stays known to take codes
        // (should the key be removed) and stays eligible.
        let row = &mut app.accounts[0];
        (row.busy, row.resign, row.launch_trigger) = (true, None, Some(launch_timing::LaunchTrigger::StartupSignIn));
        app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
            pid: 41, detail: "Signed in.".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::new(),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert!(app.settings.account_otp.contains(&key) && app.presigned(&key));
        // A failure that never reached a verdict retries once the network is back, as for an
        // account without codes: the key makes the retry's code.
        let row = &mut app.accounts[0];
        (row.pid, row.alive, row.game_running) = (None, None, None);
        (row.busy, row.otp_spent, row.launch_trigger, row.presign_login) = (true, true, Some(launch_timing::LaunchTrigger::StartupSignIn), true);
        app.worker_event(WorkerEvent::Complete(key.clone(),
            Err("Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -19). (Windower via crystal-loader).".into())));
        assert_eq!(app.accounts[0].network_wait, Some(NetworkWait::StartupRetry));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// A code made from the key that PlayOnline refuses: the row says so in one short line and
    /// shows the code boxes, so the next attempt can be a typed code. Nothing signs the account
    /// in again by itself (no retry, none after its game, none at the next start), whatever
    /// started the refused sign-in. A sign-in without a typed code that works, or an edited
    /// key, makes the key usable again.
    #[test]
    fn a_refused_authenticator_code_falls_back_to_a_typed_code_and_is_not_retried() {
        i18n::set(i18n::Lang::En);
        let refused = "Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -1036). (Windower via crystal-loader).";
        for trigger in [launch_timing::LaunchTrigger::StartupSignIn, launch_timing::LaunchTrigger::SignInAfterGame,
            launch_timing::LaunchTrigger::Play, launch_timing::LaunchTrigger::QueuedPlay] {
            let (mut app, key) = resign_app("authenticator-refused");
            app.accounts[0].account.totp_key = TEST_TOTP_KEY.into();
            let row = &mut app.accounts[0];
            (row.busy, row.preparing_session, row.launch_trigger, row.presign_login) = (true, true, Some(trigger), trigger.is_presign());
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(refused.into())));
            let row = &app.accounts[0];
            assert!(row.authenticator_refused && !row.authenticator_ready(), "{trigger:?}");
            assert_eq!(row.status_text(), "Authenticator code refused. Check the key.", "{trigger:?}");
            assert!(row.network_wait.is_none() && row.resign.is_none() && row.queued_play.is_none(), "{trigger:?}");
            assert_eq!(account_row::row_state(&app, row), account_row::RowState::NeedsCode, "{trigger:?}");
            let view = account_row::row_view(&app, row);
            assert!(view.otp && view.detail == "Authenticator code refused. Check the key.", "{trigger:?}");
            assert!(!app.presigned(&key) && wants_typed_code(&app.settings, row), "{trigger:?}");
            // Nothing signs it in by itself any more.
            let _ = app.update(Message::Tick);
            game_ends(&mut app, 0, signed_out());
            assert!(app.accounts[0].resign.is_none() && app.take_resigns(Instant::now() + RESIGN_DELAY).is_empty(), "{trigger:?}");
            app.startup_signin_pending = true;
            assert!(app.take_startup_signins().is_empty() && app.startup_waiting.is_empty(), "{trigger:?}");
            app.maintenance_sign_ins();
            assert!(app.launches.as_ref().unwrap().is_empty(), "never retried: {trigger:?}");
            // A typed code is the next attempt.
            app.accounts[0].otp = "123456".into();
            assert!(matches!(account_row::row_view(&app, &app.accounts[0]).action, Some((_, Message::SignIn(_), true))));
            app.accounts[0].otp.clear();
            let _ = std::fs::remove_dir_all(&app.config.experiment);
        }
        // A refusal of a typed code says nothing about the key.
        let (mut app, key) = resign_app("authenticator-typed-refused");
        app.accounts[0].account.totp_key = TEST_TOTP_KEY.into();
        let row = &mut app.accounts[0];
        (row.busy, row.otp_spent, row.launch_trigger) = (true, true, Some(launch_timing::LaunchTrigger::Play));
        app.worker_event(WorkerEvent::Complete(key.clone(), Err(refused.into())));
        assert!(!app.accounts[0].authenticator_refused && app.accounts[0].status == refused);
        // A sign-in without a typed code that works clears a refusal.
        app.accounts[0].authenticator_refused = true;
        let row = &mut app.accounts[0];
        (row.busy, row.otp_spent, row.launch_trigger) = (true, false, Some(launch_timing::LaunchTrigger::Play));
        app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
            pid: 41, detail: "Client started".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(true)), log: PathBuf::new(),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert!(app.accounts[0].authenticator_ready());
        // An edited key is tried afresh.
        app.accounts[0].authenticator_refused = true;
        let mut accounts: Vec<Account> = app.accounts.iter().map(|row| row.account.clone()).collect();
        accounts[0].totp_key = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".into();
        app.replace_accounts(accounts.clone());
        assert!(app.accounts[0].authenticator_ready());
        // The same key saved again keeps the refusal.
        app.accounts[0].authenticator_refused = true;
        app.replace_accounts(accounts);
        assert!(app.accounts[0].authenticator_refused);
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// The account sheet saves the key as typed from a setup page (spaces, lowercase, padding)
    /// in its stored form, refuses one with other characters with a short reason (the sheet
    /// stays open), and clears it when emptied.
    #[test]
    fn the_account_sheet_saves_the_authenticator_key_in_its_stored_form() {
        i18n::set(i18n::Lang::En);
        let mut app = app();
        let key = app.accounts[0].key.clone();
        let _ = app.update(Message::EditAccount(key.clone()));
        for (typed, error) in [("jbsw y3dp ehpk 3px1", totp::KeyError::Characters), ("jbsw y3dp", totp::KeyError::Length)] {
            let _ = app.update(Message::EditorChanged(Field::TotpKey, typed.into()));
            let _ = app.update(Message::SaveAccount);
            assert_eq!(app.status, error.message(), "{typed}");
            assert_eq!(app.editor.as_ref().unwrap().account.totp_key, typed, "kept for correction");
        }
        let _ = app.update(Message::EditorChanged(Field::TotpKey, "jbsw y3dp ehpk 3pxp ====".into()));
        let _ = app.update(Message::SaveAccount);
        assert_eq!(app.editor.as_ref().unwrap().account.totp_key, TEST_TOTP_KEY);
        let _ = app.update(Message::EditorChanged(Field::TotpKey, "   ".into()));
        let _ = app.update(Message::SaveAccount);
        assert!(app.editor.as_ref().unwrap().account.totp_key.is_empty());
    }

    /// Owner decision 2026-10-13: the Settings switch, off, stops Crystal's own sign-ins at its
    /// start, after a game, a startup sign-in waiting for a closing session and a startup network
    /// retry, and parks every account instead. Sessions already signed in stay. It is saved.
    #[test]
    fn the_switch_off_stops_startup_and_after_game_sign_ins() {
        let (mut app, key) = resign_app("presign-switch");
        let _ = app.update(Message::AutoSignIn(false));
        assert!(!load_settings(&app.config).unwrap().auto_sign_in, "saved");
        app.startup_signin_pending = true;
        assert!(app.take_startup_signins().is_empty());
        app.startup_issued = None; // no real park host from the Ticks below
        assert!(app.park_keys().iter().any(|(row, ..)| *row == key), "it parks like any other account");
        game_ends(&mut app, 0, signed_out());
        assert!(app.accounts[0].resign.is_none());
        // Pending when it was turned off.
        let _ = app.update(Message::AutoSignIn(true));
        app.startup_waiting.push((key.clone(), Instant::now()));
        app.accounts[0].network_wait = Some(NetworkWait::StartupRetry);
        assert!(!app.park_keys().iter().any(|(row, ..)| *row == key), "a pre-sign-in is coming");
        let _ = app.update(Message::AutoSignIn(false));
        assert!(app.take_waiting_startups(Instant::now()).is_empty() && app.startup_waiting.is_empty());
        let generation = app.accounts[0].launch_generation;
        app.worker_event(WorkerEvent::NetworkBack(key.clone(), true).for_launch(&key, generation));
        assert!(app.launches.as_ref().unwrap().is_empty());
        // A signed-in session is not signed out.
        let alive = Arc::new(AtomicBool::new(true));
        let row = &mut app.accounts[0];
        (row.pid, row.alive, row.game_running) = (Some(77), Some(alive.clone()), Some(Arc::new(AtomicBool::new(false))));
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].pid.is_some() && alive.load(Ordering::Acquire));
        // On again: the next start signs it in.
        let _ = app.update(Message::AutoSignIn(true));
        app.accounts[0].pid = None;
        app.accounts[0].alive = None;
        app.accounts[0].game_running = None;
        app.startup_signin_pending = true;
        assert_eq!(app.take_startup_signins(), vec![key]);
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Files before 2026-10-13 load: an account that had "Sign in when Crystal opens" is known to
    /// sign in without a code (unless its latest sign-in took one), the switch is on, and the old
    /// field is never written again. New fields round-trip.
    #[test]
    fn old_startup_sign_in_settings_migrate_and_new_ones_round_trip() {
        let root = std::env::temp_dir().join(format!("crystal-presign-migrate-{}", backend::unique_id().unwrap()));
        let config = crate::backend::Config { experiment: root.clone(), repository: root.clone(), game_install: None, dat_overlays: Vec::new() };
        let path = settings_path(&config);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"install_path":"C:\\Games\\FFXI","account_startup_signin":["a","b"],"account_otp":["b"]}"#).unwrap();
        let loaded = load_settings(&config).unwrap();
        assert!(loaded.auto_sign_in);
        assert_eq!(loaded.account_no_otp, BTreeSet::from(["a".to_owned()]));
        assert_eq!(loaded.account_otp, BTreeSet::from(["b".to_owned()]), "a code taken since is newer evidence");
        assert!(loaded.presigns("a") && !loaded.presigns("b") && !loaded.presigns("c"));
        save_settings(&config, &loaded).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("account_startup_signin"), "{text}");
        let mut reloaded = load_settings(&config).unwrap();
        assert_eq!((reloaded.account_no_otp.clone(), reloaded.account_otp.clone(), reloaded.auto_sign_in), (loaded.account_no_otp.clone(), loaded.account_otp.clone(), true));
        reloaded.auto_sign_in = false;
        save_settings(&config, &reloaded).unwrap();
        assert!(!load_settings(&config).unwrap().auto_sign_in);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A batch (Windower or Ashita accounts started together, or Plays released from a wait)
    /// whose W20 asks for an update reports each account Ready, then Failed
    /// (session_batch::coordinate). Their sessions stay signed in: the update runs in them, and
    /// its success starts every account through the signed-in path with no second check.
    #[test]
    fn a_batch_refused_for_an_update_runs_it_in_the_sessions_the_batch_kept() {
        let (mut app, root) = play_update_app("batch-kept", Some("30260904_1"));
        let keys = vec![app.accounts[0].key.clone(), app.accounts[1].key.clone()];
        // What start_together leaves on the rows, then the coordinator's events.
        let clicked = Instant::now();
        for row in &mut app.accounts[..2] {
            (row.busy, row.batch_pending, row.batch_requested) = (true, true, true);
            row.launch_trigger = Some(launch_timing::LaunchTrigger::BatchPlay);
            row.play_timing = Some(launch_timing::Timeline::begin_at(clicked));
        }
        for (at, key) in keys.iter().enumerate() {
            app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
                pid: 70 + at as u32, detail: "Signed in.".into(), alive: Arc::new(AtomicBool::new(true)),
                game_running: Arc::new(AtomicBool::new(false)), log: root.join("batch.log"),
                outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
            })));
        }
        let message = patch::gate(patch::LaunchCheck::Update(refused_report())).unwrap_err();
        for key in &keys {
            app.accounts.iter_mut().find(|row| row.key == *key).unwrap().batch_requested = true;
            app.worker_event(WorkerEvent::Complete(key.clone(), Err(message.clone())));
        }
        assert_eq!(app.play_updates.spawned.len(), 1);
        assert_eq!(app.play_updates.jobs[0].waiters.iter().map(|w| w.key.clone()).collect::<Vec<_>>(), keys);
        assert!(app.refused_plays.is_empty() && app.maintenance.is_none(), "no Downloads update, nobody signed out");
        assert!(app.accounts[..2].iter().all(|row| row.busy && row.pid.is_some() && !row.problem));
        assert!(!app.can_restart_launcher(), "an update in progress is pending launch work");
        let job = generation(&app);
        app.worker_event(WorkerEvent::PlayUpdate(job, play_update::Event::Finished(Ok(play_update::Done { summary: "Updated.".into(), report: None }))));
        let started: Vec<_> = app.launches.as_ref().unwrap().iter().map(|(keys, _, _, trigger)| (keys.clone(), *trigger)).collect();
        assert_eq!(started, keys.iter().map(|key| (vec![key.clone()], Some(launch_timing::LaunchTrigger::ReissuedPlay))).collect::<Vec<_>>());
        assert_eq!(app.w20_skipped, keys, "each starts without a second W20");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Review finding: a language switch replaced the status with the cached headlines
    /// (Unknown), and the first-read hold was already spent, so Play went through during
    /// maintenance until the new language's read landed.
    #[test]
    fn a_language_switch_keeps_the_play_gate() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-language-gate-{}", backend::unique_id().unwrap()));
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        i18n::set(i18n::Lang::En);
        app.preview = false;
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(7 * 60)));
        assert!(app.service_down());
        let _ = app.update(Message::SetLanguage(i18n::Lang::Ja));
        assert!(app.service_down(), "still maintenance after the switch");
        app.launches = None;
        app.start_launch(vec![app.accounts[0].key.clone()], false, Instant::now(), None);
        assert!(!app.accounts[0].busy && app.receiver.try_recv().is_err(), "refused");
        let _ = app.update(Message::SetLanguage(i18n::Lang::En));
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    /// Review finding: a refused sign-in after the game came back as the startup sign-in that
    /// was waiting for that session.
    #[test]
    fn a_refused_sign_in_after_the_game_cancels_a_waiting_startup_sign_in() {
        let (mut app, key) = resign_app("resign-refused-waiting");
        status_read(&mut app, services::Availability::Operational, Some(Duration::from_secs(8 * 60)));
        app.startup_waiting.push((key.clone(), Instant::now()));
        game_ends(&mut app, 0, signed_out());
        assert!(app.startup_waiting.is_empty());
        let _ = app.update(Message::Tick);
        assert!(app.launches.as_ref().unwrap().is_empty() && app.receiver.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    #[test]
    fn startup_signin_waits_for_unknown_accounts_and_selection_runs_only_once_after_vault_load() {
        let older: Settings = serde_json::from_str(r#"{"install_path":"C:\\Games\\FFXI"}"#).unwrap();
        assert!(older.auto_sign_in && older.account_no_otp.is_empty(), "on, but no account is known yet");
        let mut app = app();
        app.preview = false;
        app.startup_signin_pending = true;
        app.vault_ready = false;
        let first = app.accounts[0].key.clone();
        app.settings.account_no_otp = app.accounts.iter().map(|r| r.key.clone()).collect();
        app.accounts[1].busy = true; // Reconnecting to an existing background worker.
        app.accounts[2].account.mode = AccountMode::Private;
        assert!(app.take_startup_signins().is_empty());
        assert!(app.startup_signin_pending);
        app.vault_ready = true;
        app.installation_loading = true;
        assert!(app.take_startup_signins().is_empty());
        assert!(app.startup_signin_pending);
        app.installation_loading = false;
        assert_eq!(app.take_startup_signins(), vec![first]);
        app.accounts[1].busy = false;
        assert!(app.take_startup_signins().is_empty(), "refresh and client exit must not sign in again");
    }

    /// Finding: a startup key queued while its row reconnects to the previous session is
    /// dropped quietly once that session is back signed in; 60 s later its status is not
    /// overwritten with "skipped". A row still busy past the limit says it was skipped.
    #[test]
    fn a_reconnected_startup_row_keeps_its_status() {
        let mut app = app();
        app.preview = false;
        app.startup_signin_pending = true;
        let key = app.accounts[0].key.clone();
        app.settings.account_no_otp.insert(key.clone());
        app.settings.account_no_otp.insert(app.accounts[1].key.clone());
        app.accounts[0].busy = true; // Reconnecting to its background worker.
        app.accounts[1].busy = true; // Its previous host still closing.
        assert!(app.take_startup_signins().is_empty());
        assert_eq!(app.startup_waiting.len(), 2);
        // The reconnect completes: signed in, FFXI closed.
        let row = &mut app.accounts[0];
        (row.busy, row.pid, row.game_running) = (false, Some(99), Some(Arc::new(AtomicBool::new(false))));
        row.status = "Signed in to PlayOnline · FFXI is closed".into();
        let later = Instant::now() + STARTUP_WAIT_LIMIT + Duration::from_secs(1);
        assert!(app.take_waiting_startups(later).is_empty());
        assert_eq!(app.accounts[0].status, "Signed in to PlayOnline · FFXI is closed");
        assert_eq!(app.accounts[1].status, "Startup sign-in skipped: a session is still running.");
        assert!(app.startup_waiting.is_empty());
    }

    #[test]
    fn startup_signin_skips_existing_sessions_previews_and_file_operations() {
        let mut app = app();
        app.preview = false;
        app.startup_signin_pending = true;
        app.settings.account_no_otp.insert(app.accounts[0].key.clone());
        app.accounts[0].pid = Some(99);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        assert!(app.take_startup_signins().is_empty());
        // A social-only action on that session must not even schedule StartGame.
        app.connect(vec![app.accounts[0].key.clone()]);
        assert!(!app.accounts[0].busy);
        assert!(app.accounts[0].play_timing.is_none());
        assert!(app.receiver.try_recv().is_err());
        app.accounts[0].pid = None;
        app.startup_signin_pending = true;
        app.preview = true;
        assert!(app.take_startup_signins().is_empty());
        app.preview = false;
        app.startup_signin_pending = true;
        app.maintenance = Some(MaintenanceKind::Repair);
        assert!(app.take_startup_signins().is_empty());
    }

    /// What Crystal learned about a login's sign-ins stays with that login: an edit that keeps
    /// it keeps the knowledge, a changed login (a new account key) learns it again, and a deleted
    /// account's goes with it.
    #[test]
    fn sign_in_knowledge_follows_the_login_and_deleted_accounts() {
        let mut app = app();
        let old_key = app.accounts[0].key.clone();
        let other_key = app.accounts[1].key.clone();
        app.settings.account_no_otp.extend([old_key.clone(), other_key.clone()]);
        app.settings.account_otp.insert(app.accounts[2].key.clone());
        let _ = app.update(Message::EditAccount(other_key.clone()));
        let _ = app.update(Message::EditorChanged(Field::Label, "Renamed".into()));
        let saved = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(saved)));
        assert!(app.settings.presigns(&other_key), "the same login keeps what was learned");
        let _ = app.update(Message::EditAccount(old_key.clone()));
        let _ = app.update(Message::EditorChanged(Field::PolId, "NEW12345".into()));
        let edited = app.editor.as_ref().unwrap().account.clone();
        let new_key = account_key(&edited);
        let _ = app.update(Message::AccountsSaved(Ok(vec![edited])));
        assert!(app.settings.account_no_otp.is_empty() && app.settings.account_otp.is_empty(), "deleted accounts are forgotten");
        assert!(!app.settings.presigns(&new_key), "a changed login is unknown until it signs in");
    }

    #[test]
    fn ready_session_timing_waits_for_game_start_and_is_cleared_on_refusal() {
        let mut app = app();
        let game = Arc::new(AtomicBool::new(false));
        let row = &mut app.accounts[0];
        row.pid = Some(99);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(game.clone());
        row.busy = true;
        row.play_timing = Some(launch_timing::Timeline::begin_at(Instant::now() - Duration::from_secs(1)));
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_some());
        game.store(true, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_none());
        assert!(!app.accounts[0].busy);
        assert!(app.accounts[0].timing.starts_with("Game started "));
        assert!(!app.accounts[0].timing.contains("Signed in"), "do not reuse the earlier sign-in timing");
        let row = &mut app.accounts[0];
        row.play_timing = Some(launch_timing::Timeline::begin());
        let key = row.key.clone();
        let report = patch::PatchReport { state: patch::PatchState::Unknown, local_version: String::new(), remote_version: String::new(), detail: String::new(), files_verified: None };
        app.worker_event(WorkerEvent::StartGame(key.clone(), Some(report.clone()), Err("Version check refused".into())));
        assert!(app.accounts[0].play_timing.is_none());
        assert!(app.accounts[0].problem);
        // An accepted start was W20 done: the Play's timeline shows it (07c §5.3).
        app.accounts[0].play_timing = Some(launch_timing::Timeline::begin());
        app.worker_event(WorkerEvent::StartGame(key.clone(), Some(report), Ok("Game version matches.".into())));
        assert!(app.accounts[0].play_timing.as_ref().unwrap().elapsed_at(launch_timing::Milestone::GameChecked).is_some());
        // A session that never reached its main menu ran no W20 and keeps the shown report.
        app.patch = None;
        app.worker_event(WorkerEvent::StartGame(key, None, Err("PlayOnline did not return to its main menu in time, so FFXI was not started.".into())));
        assert!(app.patch.is_none() && app.accounts[0].problem);
    }

    #[test]
    fn vanilla_selection_in_home_and_the_editor_uses_the_same_direct_launch_plan() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        let other = app.accounts[1].account.launch_with;
        let _ = app.set_account_launcher(std::slice::from_ref(&key), crate::backend::LaunchWith::Vanilla);
        let account = &app.accounts[0].account;
        assert_eq!(app.settings.launch_plan(account).unwrap().tool, pol_less::Tool::Vanilla);
        assert_eq!(app.accounts[1].account.launch_with, other);
        app.editor = Some(Editor { key: Some(key), account: account.clone(),
            game_install: String::new(), dat_overlays: Vec::new(), auto_login: pol_less::AutoLogin::default(), flags: EditorFlags::default() });
        let _ = app.update(Message::EditorLaunchWith(Some(crate::backend::LaunchWith::Windower)));
        assert_eq!(app.editor.as_ref().unwrap().account.launch_with, Some(crate::backend::LaunchWith::Windower));
        let _ = app.update(Message::EditorLaunchWith(Some(crate::backend::LaunchWith::Vanilla)));
        assert_eq!(app.settings.launch_plan(&app.editor.as_ref().unwrap().account).unwrap().tool, pol_less::Tool::Vanilla);
        assert_eq!(app.accounts[0].account.launch_with, Some(crate::backend::LaunchWith::Vanilla));
    }

    /// The sheet's "Uses a one-time password" is what Crystal knows about the login: on, it is
    /// never signed in by itself; off, the next sign-in teaches it again.
    #[test]
    fn the_sheet_one_time_password_switch_sets_what_crystal_knows() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.settings.account_no_otp.insert(key.clone());
        assert!(app.settings.presigns(&key) || !app.settings.auto_sign_in);
        let values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
        let _ = app.update(Message::EditAccount(key.clone()));
        assert!(!app.editor.as_ref().unwrap().flags.uses_otp);
        let _ = app.update(Message::EditorFlag(EditorFlag::UsesOtp, true));
        let _ = app.update(Message::AccountsSaved(Ok(values.clone())));
        assert!(app.settings.account_otp.contains(&key) && !app.settings.account_no_otp.contains(&key));
        assert!(!app.settings.presigns(&key));
        let _ = app.update(Message::EditAccount(key.clone()));
        assert!(app.editor.as_ref().unwrap().flags.uses_otp);
        let _ = app.update(Message::EditorFlag(EditorFlag::UsesOtp, false));
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert!(!app.settings.account_otp.contains(&key) && !app.settings.account_no_otp.contains(&key));
        // A new account's sheet signs in in the background by default.
        let _ = app.update(Message::NewAccount);
        assert!(app.editor.as_ref().unwrap().flags.sign_in_now);
    }

    #[test]
    fn save_failure_keeps_account_state_and_editor() {
        let mut app = app();
        app.editor = Some(Editor::blank());
        app.vault_saving = true;
        let _ = app.update(Message::AccountsSaved(Err("Save failed".into())));
        assert_eq!(app.accounts.len(), 3);
        assert!(app.editor.is_some());
        assert!(!app.vault_saving);
    }

    #[test]
    fn dragging_accounts_persists_order_without_rewriting_credentials_or_live_state() {
        let mut app = app();
        let root = std::env::temp_dir().join(format!("crystal-account-order-{}", backend::unique_id().unwrap()));
        app.config.experiment = root.clone();
        app.preview = false;
        app.page = Page::Home;
        let values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
        backend::save_accounts(&app.config, &values).unwrap();
        let vault = root.join("iced-launcher/accounts.dpapi");
        let ciphertext = std::fs::read(&vault).unwrap();
        let original: Vec<_> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let alive = Arc::new(AtomicBool::new(true));
        let running = Arc::new(AtomicBool::new(true));
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(alive.clone());
        app.accounts[0].game_running = Some(running.clone());
        app.accounts[0].launch_generation = 19;
        app.accounts[0].otp = "123456".into();
        let _ = app.update(Message::AccountDragStart(original[0].clone()));
        let _ = app.update(Message::AccountDragOver(original[2].clone()));
        let _ = app.update(Message::AccountDragFinish);
        let expected = vec![original[1].clone(), original[2].clone(), original[0].clone()];
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), expected);
        let moved = &app.accounts[2];
        assert!(Arc::ptr_eq(moved.alive.as_ref().unwrap(), &alive));
        assert!(Arc::ptr_eq(moved.game_running.as_ref().unwrap(), &running));
        assert_eq!((moved.pid, moved.launch_generation, moved.otp.as_str()), (Some(77), 19, "123456"));
        assert_eq!(std::fs::read(&vault).unwrap(), ciphertext);
        assert_eq!(load_settings(&app.config).unwrap().account_order, expected);
        let mut reopened = self::app();
        reopened.config.experiment = root.clone();
        reopened.settings = load_settings(&app.config).unwrap();
        reopened.replace_accounts(backend::load_accounts(&app.config).unwrap());
        assert_eq!(reopened.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), expected);
        assert_eq!(std::fs::read(vault).unwrap(), ciphertext);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_drags_start_from_the_rail_on_every_page() {
        for page in [Page::Home, Page::Friends, Page::Maintenance, Page::Settings] {
            let mut app = app();
            app.page = page;
            let key = app.accounts[0].key.clone();
            let _ = app.update(Message::AccountDragStart(key.clone()));
            assert_eq!(app.account_drag.source.as_deref(), Some(key.as_str()));
        }
    }

    #[test]
    fn controller_shoulders_follow_the_top_bar_through_features() {
        let mut app = app();
        app.page = Page::Maintenance;
        let _ = app.update(Message::ControllerPage(1));
        assert!(app.features_view.open && app.page == Page::Maintenance);
        let _ = app.update(Message::ControllerPage(1));
        assert!(!app.features_view.open && app.page == Page::Settings);
        let _ = app.update(Message::ControllerPage(-1));
        assert!(app.features_view.open);
        let _ = app.update(Message::ControllerPage(-1));
        assert!(!app.features_view.open && app.page == Page::Maintenance);
    }

    #[test]
    fn canceled_or_failed_account_drags_restore_the_previous_order() {
        let mut app = app();
        app.page = Page::Home;
        let original: Vec<_> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let _ = app.update(Message::AccountDragStart(original[0].clone()));
        let _ = app.update(Message::AccountDragOver(original[2].clone()));
        let _ = app.update(Message::ControllerBack);
        assert!(app.account_drag.source.is_none());
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), original);
        let root = std::env::temp_dir().join(format!("crystal-order-failure-{}", backend::unique_id().unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("iced-launcher"), b"blocked settings directory").unwrap();
        app.config.experiment = root.clone();
        app.preview = false;
        let _ = app.update(Message::AccountDragStart(original[0].clone()));
        let _ = app.update(Message::AccountDragOver(original[2].clone()));
        let _ = app.update(Message::AccountDragFinish);
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), original);
        assert!(app.settings.account_order.is_empty());
        assert!(app.status.starts_with("Account order could not be saved:"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pol_handle_setup_survives_an_account_reload_and_can_be_cleared() {
        let mut app=app();
        let key=app.accounts[0].key.clone();
        let _=app.update(Message::EditAccount(key.clone()));
        let _=app.update(Message::EditorChanged(Field::PolHandleNames,"PublicOne, PublicTwo".into()));
        let mut accounts:Vec<_>=app.accounts.iter().map(|r| r.account.clone()).collect();
        accounts[0]=app.editor.as_ref().unwrap().account.clone();
        let _=app.update(Message::AccountsSaved(Ok(accounts.clone())));
        assert_eq!(app.settings.account_pol_handles.get(&key).map(String::as_str),Some("PublicOne, PublicTwo"));
        let decoded:Settings=serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
        app.settings=decoded;
        for account in &mut accounts { account.pol_handle_names.clear(); }
        let _=app.update(Message::AccountsLoaded(Ok(accounts)));
        assert_eq!(app.accounts[0].account.pol_handle_names,"PublicOne, PublicTwo");
        let _=app.update(Message::EditAccount(key.clone()));
        let _=app.update(Message::EditorChanged(Field::PolHandleNames,String::new()));
        let mut accounts:Vec<_>=app.accounts.iter().map(|r| r.account.clone()).collect();
        accounts[0]=app.editor.as_ref().unwrap().account.clone();
        let _=app.update(Message::AccountsSaved(Ok(accounts)));
        assert!(!app.settings.account_pol_handles.contains_key(&key));
        assert!(app.accounts[0].account.pol_handle_names.is_empty());
    }

    #[test]
    fn profile_edits_during_play_keep_the_original_session_and_lock_its_login_identity() {
        let mut app = app();
        let keys: Vec<_> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let alive = Arc::new(AtomicBool::new(true));
        let running = Arc::new(AtomicBool::new(true));
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(alive.clone());
        app.accounts[0].game_running = Some(running.clone());
        app.accounts[0].launch_generation = 22;
        let _ = app.update(Message::EditAccount(keys[0].clone()));
        assert!(app.editor.is_some());
        let _ = app.update(Message::EditorChanged(Field::PolId, "DIFF1234".into()));
        assert_eq!(account_key(&app.editor.as_ref().unwrap().account), keys[0]);
        let _ = app.update(Message::EditorProfile(Some("Next launch".into())));
        let _ = app.update(Message::EditorChanged(Field::Label, "Updated label".into()));
        assert_ne!(app.accounts[0].account.label, "Updated label");
        let edited = app.editor.as_ref().unwrap().account.clone();
        let mut values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
        values[0] = edited;
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), keys);
        let row = &app.accounts[0];
        assert_eq!(row.account.windower_profile.as_deref(), Some("Next launch"));
        assert_eq!(row.account.label, "Updated label");
        assert_eq!((row.pid, row.launch_generation), (Some(77), 22));
        assert!(Arc::ptr_eq(row.alive.as_ref().unwrap(), &alive));
        assert!(Arc::ptr_eq(row.game_running.as_ref().unwrap(), &running));
        assert!(row.is_playing());
    }
    #[test]
    fn saved_account_folders_are_isolated_and_loading_accounts_does_not_replace_them() {
        let mut app = app();
        let first = app.accounts[0].account.clone();
        let second = app.accounts[1].account.clone();
        let first_key = account_key(&first);
        let path = r"C:\Games\Private Server";
        app.settings.install_path = r"C:\Games\Retail".into();
        app.settings.install_selected = true;
        app.editor = Some(Editor { key: Some(first_key.clone()), account: first.clone(), game_install: path.into(), dat_overlays: vec![r"C:\Mods\High Priority".into(), r"C:\Mods\Fallback".into()], auto_login: pol_less::AutoLogin::default(), flags: EditorFlags::default() });
        let values: Vec<_> = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(values.clone())));
        let config = app.settings.config_for(&app.config, &first);
        assert_eq!(config.game_directory(), PathBuf::from(path));
        assert!(config.dat_overlays.is_empty(), "retail injectors do not use the disabled native overlays");
        assert_eq!(app.settings.account_overlays[&first_key], [r"C:\Mods\High Priority", r"C:\Mods\Fallback"], "saved legacy overlay paths are kept");
        let other = app.settings.config_for(&app.config, &second);
        assert_eq!(other.game_directory(), PathBuf::from(r"C:\Games\Retail"));
        assert!(other.dat_overlays.is_empty());
        assert_eq!(other.experiment, config.experiment, "account storage is independent of the chosen game installation");
        let persisted = serde_json::to_vec(&app.settings).unwrap();
        let reloaded: Settings = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(reloaded.config_for(&app.config, &first).game_directory(), PathBuf::from(path));
        let _ = app.update(Message::AccountsLoaded(Ok(values)));
        assert_eq!(app.settings.account_installs[&first_key], path);
        let _ = app.update(Message::EditAccount(first_key.clone()));
        let _ = app.update(Message::AccountInstallChanged(String::new()));
        let values = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.settings.config_for(&app.config, &first).game_directory(), PathBuf::from(r"C:\Games\Retail"));
    }

    #[test]
    fn account_key_edits_migrate_preferences_and_deletion_removes_them() {
        let mut app = app();
        let previous = app.accounts[0].key.clone();
        app.settings.selected_accounts = Some(BTreeSet::from([previous.clone(), "obsolete".into()]));
        app.settings.account_auto_away.insert(previous.clone(), 30);
        app.settings.account_auto_away.insert("obsolete".into(), 60);
        app.settings.account_installs.insert(previous.clone(), r"C:\Games\Account".into());
        app.settings.account_overlays.insert(previous.clone(), vec![r"C:\Mods\Account".into()]);
        app.settings.account_no_otp.insert(previous.clone());
        app.settings.account_order = vec![previous.clone(), app.accounts[1].key.clone(), "obsolete".into()];
        let auto_login = pol_less::AutoLogin { enabled: true, character_slot: 3 };
        app.settings.account_auto_login.insert(previous.clone(), auto_login);
        let _ = app.update(Message::EditAccount(previous.clone()));
        let _ = app.update(Message::EditorChanged(Field::PolId, "XYZ99999".into()));
        let edited = app.editor.as_ref().unwrap().account.clone();
        let key = account_key(&edited);
        let mut values: Vec<_> = app.accounts.iter().filter(|row| row.key != previous).map(|row| row.account.clone()).collect();
        values.push(edited);
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.settings.selected_accounts, Some(BTreeSet::from([key.clone()])));
        assert_eq!(app.settings.account_auto_away, HashMap::from([(key.clone(), 30)]));
        assert_eq!(app.settings.account_auto_login, HashMap::from([(key.clone(), auto_login)]));
        let reloaded: Settings = serde_json::from_slice(&serde_json::to_vec(&app.settings).unwrap()).unwrap();
        assert_eq!(reloaded.account_auto_login, app.settings.account_auto_login);
        assert_eq!(app.settings.account_installs.get(&key).map(String::as_str), Some(r"C:\Games\Account"));
        assert!(app.settings.account_overlays.contains_key(&key));
        assert!(!app.settings.account_no_otp.contains(&key), "a changed login learns its sign-ins again");
        assert_eq!(app.settings.account_order, [key.clone(), app.accounts.iter().find(|row| row.key != key).unwrap().key.clone()]);
        assert!(app.accounts.iter().find(|row| row.key == key).unwrap().selected);
        for exists in [app.settings.account_installs.contains_key(&previous), app.settings.account_overlays.contains_key(&previous),
            app.settings.account_no_otp.contains(&previous)] { assert!(!exists); }

        let values = app.accounts.iter().filter(|row| row.key != key).map(|row| row.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.settings.selected_accounts, Some(BTreeSet::new()));
        assert!(app.settings.account_auto_away.is_empty() && app.settings.account_installs.is_empty());
        assert!(app.settings.account_overlays.is_empty() && app.settings.account_no_otp.is_empty());
        assert!(app.settings.account_auto_login.is_empty());
        assert!(!app.settings.account_order.contains(&key));
    }

    #[test]
    fn account_settings_failure_can_retry_after_the_vault_saved_a_new_key() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-account-retry-{}", backend::unique_id().unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        let blocker = root.join("iced-launcher");
        std::fs::write(&blocker, b"settings parent is temporarily unavailable").unwrap();
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let previous = app.accounts[0].key.clone();
        app.settings.selected_accounts = Some(BTreeSet::from([previous.clone()]));
        app.settings.account_auto_away.insert(previous.clone(), 30);
        let _ = app.update(Message::EditAccount(previous.clone()));
        let _ = app.update(Message::EditorChanged(Field::PolId, "XYZ99999".into()));
        let edited = app.editor.as_ref().unwrap().account.clone();
        let key = account_key(&edited);
        let mut values: Vec<_> = app.accounts.iter().filter(|row| row.key != previous).map(|row| row.account.clone()).collect();
        values.push(edited);
        app.preview = false;
        let _ = app.update(Message::AccountsSaved(Ok(values.clone())));
        assert_eq!(app.editor.as_ref().unwrap().key.as_ref(), Some(&key));
        assert_eq!(app.settings.account_auto_away.get(&key), Some(&30));
        assert!(app.status.contains("launcher settings were not saved"));

        std::fs::remove_file(blocker).unwrap();
        // Creating this Task does not execute the vault write or launch a request.
        assert_eq!(app.update(Message::SaveAccount).units(), 1, "the editor must recognize its saved identity");
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert!(app.editor.is_none() && !app.vault_saving);
        let saved = load_settings(&app.config).unwrap();
        assert_eq!(saved.account_auto_away.get(&key), Some(&30));
        assert_eq!(saved.selected_accounts, Some(BTreeSet::from([key])));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_folder_dialog_does_not_change_a_different_account_editor() {
        let mut app = app();
        let first_key = app.accounts[0].key.clone();
        let second_key = app.accounts[1].key.clone();
        let _ = app.update(Message::EditAccount(second_key));
        let _ = app.update(Message::PickedAccountFolder(first_key.clone(), Some(PathBuf::from(r"C:\Wrong account"))));
        let _ = app.update(Message::PickedAccountOverlay(first_key, Some(PathBuf::from(r"C:\Wrong overlay"))));
        assert!(app.editor.as_ref().unwrap().game_install.is_empty());
        assert!(app.editor.as_ref().unwrap().dat_overlays.is_empty());
    }
    #[test]
    fn closed_child_becomes_launchable_without_reusing_pid() {
        let mut app = app();
        app.accounts[0].pid = Some(99);
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.update(Message::Tick);
        assert_eq!(app.accounts[0].pid, None);
        assert_eq!(app.launchable_count(), 3);
    }
    #[test]
    fn malicious_and_non_http_links_cannot_open() {
        assert!(allowed_link("https://www.ffxiah.com/"));
        assert!(allowed_link("https://wiki.ffo.jp/"));
        assert!(allowed_link("https://www.playonline.com/ff11us/"));
        // The Japanese site's article and "View all news" links open too, and only on its host.
        assert!(allowed_link(services::sources(i18n::Lang::Ja).home));
        assert!(allowed_link("https://www.playonline.com/pcd2/topics/ff11/detail/40431/detail.html"));
        assert!(allowed_link("https://www.playonline.com/ff11/polnews/news27774.shtml"));
        assert!(allowed_link("https://sqex.to/1Twec"));
        for url in [
            "https://www.playonline.com.evil.test/ff11/",
            "https://evil.test/pcd2/topics/ff11/detail/40431/detail.html",
            "http://www.playonline.com/ff11/",
            "file:///C:/Windows/System32/cmd.exe",
            "https://www.playonline.com.evil.test/",
            "https://x@www.ffxiah.com/",
            "https://wiki.ffo.jp.evil.test/",
            "http://wiki.ffo.jp/",
            "https://user@wiki.ffo.jp/",
            "javascript:alert(1)",
        ] {
            assert!(!allowed_link(url));
        }
    }
}

/// Background mode (window closed to the notification area) and the gated idle Tick.
#[cfg(test)]
mod background_tests {
    use super::*;

    fn live_app(name: &str) -> App {
        let mut app = tests::app();
        app.preview = false;
        app.config.experiment = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-background-{name}-{}", backend::unique_id().unwrap()));
        app.window = Some(WindowId::unique());
        app
    }

    #[test]
    fn the_close_button_hides_to_the_tray_only_when_settings_say_so() {
        let mut app = live_app("hide");
        app.settings.close_to_tray = true;
        let _ = app.update(Message::CloseRequested);
        assert!(app.window.is_none(), "the window closed");
        assert!(!app.closing, "Crystal keeps running: sessions stay signed in");
        // From the icon: back again, with a new window.
        let _ = app.update(Message::Tray(tray::Command::Open));
        assert!(app.window.is_some() && !app.closing);

        let mut app = live_app("quit");
        let _ = app.update(Message::CloseRequested);
        assert!(app.closing, "off (the default): closing quits as before");
        assert!(app.window.is_none());
    }

    #[test]
    fn quitting_from_the_icon_while_something_holds_it_opens_the_window_to_say_why() {
        let mut app = live_app("held-quit");
        app.settings.close_to_tray = true;
        let _ = app.update(Message::CloseRequested);
        assert!(app.window.is_none());
        app.vault_saving = true;
        let _ = app.update(Message::Tray(tray::Command::Quit));
        assert!(!app.closing && app.confirm_close);
        assert!(app.window.is_some(), "the confirmation is visible");
        app.vault_saving = false;
        let _ = app.update(Message::Tray(tray::Command::Quit));
        assert!(app.closing);
    }

    #[test]
    fn an_update_handshake_is_never_hidden_away() {
        let mut app = live_app("applying");
        app.settings.close_to_tray = true;
        app.self_update = self_update::State::Applying;
        let _ = app.update(Message::CloseRequested);
        assert!(app.window.is_some() && !app.closing);
        assert!(app.status.contains("update helper"));
    }

    /// A signed-in session at its idle main menu for the first retail account, whose worker is
    /// this test (`take_social` / `publish_friends` play the session worker), and the window
    /// on the Friends page once its opening read asked for the Inbox.
    fn friends_session(name: &str) -> (App, PathBuf, friends::Snapshot) {
        let mut app = live_app(name);
        let row = app.accounts.iter().position(|r| r.account.mode == AccountMode::Retail).unwrap();
        let account = app.accounts[row].account.clone();
        let config = app.settings.config_for(&app.config, &account);
        let directory = config.account_directory(&account);
        session_worker::save_live_record_for_test(&config, &account);
        let mut snapshot = friends::Snapshot { game_running: Some(false), fetched_at: 1, ..Default::default() };
        snapshot.social.ready = true;
        session_worker::publish_friends(&directory, &snapshot).unwrap();
        app.reset_friends(Some(app.accounts[row].key.clone()));
        let _ = app.update(Message::Navigate(Page::Friends));
        let started = Instant::now();
        while app.friends.pending.is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
            let _ = app.update(Message::Tick);
        }
        assert!(app.friends.pending.is_some(), "opening the page lists the Inbox: {}", app.friends.error);
        // The read the request started has finished: what follows decides what is seen.
        tick_until(&mut app, |app| !app.friends.loading);
        (app, directory, snapshot)
    }

    /// The session worker takes the request and PlayOnline answers it at once: busy, then
    /// result 0 (accepted) and result 1 (the page) — live 2026-10-22 11:17:46, one second.
    fn answer_inbox(directory: &Path, snapshot: &mut friends::Snapshot) {
        let (request, _lock) = session_worker::take_social(directory).expect("the request reached the worker");
        snapshot.social.begin(request.id.clone(), &request.request);
        session_worker::publish_friends(directory, snapshot).unwrap();
        snapshot.social.apply(&request.id, crate::social::Action::Inbox, 0, None).unwrap();
        session_worker::publish_friends(directory, snapshot).unwrap();
        snapshot.social.apply(&request.id, crate::social::Action::Inbox, 1, Some(&[0; 12 + 32 * 72])).unwrap();
        session_worker::publish_friends(directory, snapshot).unwrap();
    }

    fn tick_until(app: &mut App, done: impl Fn(&App) -> bool) {
        let started = Instant::now();
        while !done(app) && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
            let _ = app.update(Message::Tick);
        }
    }

    #[test]
    fn opening_friends_lists_the_inbox_and_sees_it_complete() {
        let (mut app, directory, mut snapshot) = friends_session("friends-open");
        answer_inbox(&directory, &mut snapshot);
        tick_until(&mut app, |app| app.friends.pending.is_none());
        assert!(app.friends.pending.is_none() && app.friends.error.is_empty(), "{}", app.friends.error);
        assert!(app.friends.mail.listed[0].is_some());
    }

    /// Live 2026-10-22: Friends opened after FFXI returned, the Inbox listing finished in the
    /// same second (results 0 then 1, the cache idle with its id), yet the page later said
    /// PlayOnline had not reported completion. The limit was checked before anything read
    /// what had arrived: a window that had not looked for 45 s called a finished request lost.
    #[test]
    fn a_finished_request_is_not_reported_missing_when_the_window_looks_late() {
        let (mut app, directory, mut snapshot) = friends_session("friends-late");
        answer_inbox(&directory, &mut snapshot);
        // Nothing read the session since: the window looks again only now, past the limit.
        let id = app.friends.pending.as_ref().unwrap().0.clone();
        app.friends.pending = Some((id, Instant::now() - FRIENDS_REQUEST_LIMIT - Duration::from_secs(1)));
        let _ = app.update(Message::Tick);
        assert!(app.friends.error.is_empty(), "{}", app.friends.error);
        assert!(app.friends.pending.is_none() && !app.friends.mail.stalled);
        assert!(app.friends.mail.listed[0].is_some(), "the listing it finished is learned");
    }

    /// Leaving the page stopped the reads while the limit kept running on every page.
    #[test]
    fn a_request_is_followed_after_leaving_the_friends_page() {
        let (mut app, directory, mut snapshot) = friends_session("friends-left");
        let _ = app.update(Message::Navigate(Page::Home));
        tick_until(&mut app, |app| !app.friends.loading);
        answer_inbox(&directory, &mut snapshot);
        tick_until(&mut app, |app| app.friends.pending.is_none());
        assert!(app.friends.pending.is_none() && app.friends.error.is_empty(), "{}", app.friends.error);
        assert!(app.friends.mail.listed[0].is_some());
        // Off the page nothing new is asked for.
        app.friends.snapshot.as_mut().unwrap().social.inbox_changed = true;
        app.worker_event(WorkerEvent::Friends(app.friends_generation, app.friends.account.clone().unwrap(), true, app.friends.snapshot.clone()));
        assert!(app.friends.pending.is_none());
    }

    /// A request PlayOnline really has not finished still says so at the limit.
    #[test]
    fn an_unanswered_request_still_reports_at_the_limit() {
        let (mut app, directory, mut snapshot) = friends_session("friends-unanswered");
        let (request, lock) = session_worker::take_social(&directory).unwrap();
        snapshot.social.begin(request.id.clone(), &request.request);
        session_worker::publish_friends(&directory, &snapshot).unwrap();
        drop(lock);
        let id = app.friends.pending.as_ref().unwrap().0.clone();
        app.friends.pending = Some((id, Instant::now() - FRIENDS_REQUEST_LIMIT - Duration::from_secs(1)));
        let _ = app.update(Message::Tick);
        assert!(app.friends.pending.is_none() && app.friends.mail.stalled);
        assert!(app.friends.error.contains("has not reported completion"), "{}", app.friends.error);
    }

    #[test]
    fn the_idle_tick_is_quiet_only_with_no_timed_work() {
        let mut app = tests::app();
        app.startup_signin_pending = false;
        app.startup_waiting.clear();
        assert!(app.tick_quiet());
        app.page = Page::Friends;
        assert!(!app.tick_quiet(), "the Friends page polls its list");
        app.page = Page::Home;
        app.startup_signin_pending = true;
        assert!(!app.tick_quiet(), "startup sign-ins wait for the vault");
        app.startup_signin_pending = false;
        app.parks_settled = false;
        assert!(!app.tick_quiet(), "a park to start");
        app.parks_settled = true;
        app.accounts[0].resign = Some(Resign { generation: 0, ended: Instant::now(), free_since: None, network: false, host: None });
        assert!(!app.tick_quiet(), "a sign-in after the game");
        app.accounts[0].resign = None;
        app.accounts[0].queued_play = Some(Instant::now());
        assert!(!app.tick_quiet(), "a queued Play");
        app.accounts[0].queued_play = None;
        assert!(app.tick_quiet());
    }

    #[test]
    fn a_quiet_window_watches_each_session_and_game_flag() {
        let mut app = tests::app();
        let alive = Arc::new(AtomicBool::new(true));
        let game = Arc::new(AtomicBool::new(false));
        app.accounts[0].alive = Some(alive.clone());
        app.accounts[0].game_running = Some(game.clone());
        let flags = app.watched_flags();
        assert_eq!(flags.len(), 2);
        assert!(flags.iter().any(|(flag, seen)| Arc::ptr_eq(flag, &alive) && *seen));
        assert!(flags.iter().any(|(flag, seen)| Arc::ptr_eq(flag, &game) && !*seen));
    }

    #[test]
    fn background_settings_are_off_by_default_and_saved_only_when_on() {
        let settings = Settings::default();
        assert!(!settings.close_to_tray && !settings.start_with_windows);
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("close_to_tray") && !json.contains("start_with_windows"));
        let on = Settings { close_to_tray: true, start_with_windows: true, ..Settings::default() };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
        assert!(back.close_to_tray && back.start_with_windows);
    }

    #[test]
    fn start_with_windows_waits_for_its_task_and_keeps_the_switch_on_failure() {
        let mut app = live_app("startup-task");
        let task = app.update(Message::StartWithWindows(true));
        assert!(task.units() > 0 && app.startup_task_busy());
        assert!(app.update(Message::StartWithWindows(false)).units() == 0, "one change at a time");
        let _ = app.update(Message::StartupTaskSet(true, Err("refused".into())));
        assert!(!app.settings.start_with_windows && !app.startup_task_busy());
        assert_eq!(app.status, "refused");
        let _ = app.update(Message::StartupTaskSet(true, Ok(())));
        assert!(app.settings.start_with_windows);
    }
}
