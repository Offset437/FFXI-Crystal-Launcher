//! Retail launches directly, through Ashita v4, or through Windower 4.
//!
//! The launcher starts the game host directly or through Ashita-cli/Windower and hands
//! credentials to the verified loader over a private named pipe.
//! The installed native PlayOnline core authenticates and owns the retail session;
//! the headless launcher worker supervises it independently of the window.
//!
//! Secrets never appear on a command line, in the environment, in Ashita's boot ini or
//! config.json, or in the per-launch log; they reach the loader only through its pipe.

pub(crate) mod ffxi_settings;
mod native_auth;
pub(crate) mod park;
pub(crate) mod pipe;
pub(crate) mod profile_access;
pub(crate) mod tools;
#[cfg(test)]
mod turns;
mod windower_parallel;

use crate::backend::{plain_windows_path, unique_id, with_log, Account, Config, LaunchResult};
use crate::i18n::{t, tf};
use crate::launch_log::{classify_loader_end, LoaderEnd, Session};
use crate::launch_timing::{noted, Milestone, Timeline};
use pipe::{AcceptError, LoaderEvent, LoaderPipe};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
pub(crate) use tools::Tool;
use zeroize::Zeroizing;

/// Who may spawn a tool now, held from spawn to the loader's hello (`start_host`).
/// Windower 4's injection handshake uses fixed-name objects (`WindowerMMFSettingsHandler`,
/// `windower.signal-injection`), so one Windower start at a time, machine-wide, from spawn to
/// hello. Ashita's POL plugins share files in Ashita's folder while each game host starts
/// (`session_worker::ashita_turn_at`), so one Ashita start at a time too, in a lock of its
/// own; Vanilla starts run in parallel. The third argument says the start was
/// closed meanwhile (a park's abort): the wait then ends with `park::ABORTED`. Tests supply
/// their own gate.
type TurnGate<'a> = dyn Fn(Tool, &mut dyn FnMut(&str), &dyn Fn() -> bool) -> Result<Option<Box<dyn Send>>, String> + Sync + 'a;

/// The production gate: the machine-wide Windower and Ashita launch locks
/// (`session_worker::windower_turn_at`, `session_worker::ashita_turn_at`).
fn launch_gate(tool: Tool, progress: &mut dyn FnMut(&str), aborted: &dyn Fn() -> bool) -> Result<Option<Box<dyn Send>>, String> {
    match tool {
        Tool::Windower => windower_gate_at(&crate::session_worker::windower_lock_path()?, progress, aborted),
        Tool::Ashita => crate::session_worker::ashita_turn_at(&crate::session_worker::ashita_lock_path()?, progress,
            &|| aborted().then(|| park::ABORTED.to_owned()), crate::session_worker::ASHITA_TURN_LIMIT)
            .map(|turn| Some(Box::new(turn) as Box<dyn Send>)),
        Tool::Vanilla => Ok(None),
    }
}

/// The Windower turn at `lock`: waits up to WINDOWER_TURN_LIMIT, or until `aborted`
/// (`park::ABORTED`). The lock is never broken.
fn windower_gate_at(lock: &Path, progress: &mut dyn FnMut(&str), aborted: &dyn Fn() -> bool) -> Result<Option<Box<dyn Send>>, String> {
    crate::session_worker::windower_turn_at(lock, progress, &|| aborted().then(|| park::ABORTED.to_owned()),
        crate::session_worker::WINDOWER_TURN_LIMIT).map(|turn| Some(Box::new(turn) as Box<dyn Send>))
}

/// The Launch tools settings, as the Settings page stores them. Empty means "detect".
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToolSettings {
    pub ashita: String,
    pub windower: String,
    /// Base boot ini file name inside Ashita's `config\boot` (default `default.ini`).
    pub boot_ini: String,
    pub windower_profile: String,
    /// Dev override for the game host (`crystal\pol.exe`).
    pub loader: String,
    /// put dgVoodoo next to the loader (else take Crystal's copy away).
    #[serde(default)]
    pub dgvoodoo: bool,
    #[serde(default)]
    pub reshade: crate::reshade::Preferences,
}

/// One retail launch request: which launch method, with which settings.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Plan {
    pub tool: Tool,
    pub settings: ToolSettings,
    #[serde(default)]
    pub social_only: bool,
    /// Auto-Away after this many idle minutes (0 = off; Viewer setting 4/7).
    #[serde(default = "default_auto_away")]
    pub auto_away_minutes: u8,
    #[serde(default)]
    pub auto_login: AutoLogin,
    /// A pre-sign-in (sign in when Crystal opens, or the explicit sign-in): the loader
    /// restores the Viewer's UI pauses, which cost nothing at Play (pol-spec 07c §4).
    /// Only with `social_only`; a batch's own sign-in is a cold Play and never sets it.
    #[serde(default)]
    pub viewer_gaps: bool,
    /// Retain the authenticated native owner when FFXI returns normally.
    #[serde(default)]
    pub keep_session: bool,
    /// What started this launch, for the launch log's trigger line (pol-spec 07d §4.2).
    /// Older window processes send none: `Play`.
    #[serde(default)]
    pub trigger: crate::launch_timing::LaunchTrigger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct AutoLogin {
    pub enabled: bool,
    /// One-based row in the character selection screen.
    pub character_slot: u8,
}
impl Default for AutoLogin {
    fn default() -> Self { Self { enabled: false, character_slot: 1 } }
}
impl AutoLogin {
    pub(crate) fn normalized(self) -> Self {
        if (1..=16).contains(&self.character_slot) { self } else { Self::default() }
    }
}

/// The Viewer's static default for "Auto-away time" (app 10490ef0 entry 7,
/// index 0 = 10 minutes).
pub(crate) const fn default_auto_away() -> u8 { 10 }
/// The Viewer's choices: 10..60 minutes in steps of 10, or off (-1 there).
pub(crate) const AUTO_AWAY_CHOICES: [u8; 7] = [0, 10, 20, 30, 40, 50, 60];

#[derive(Clone, Copy)]
struct Timeouts {
    /// Injector start to loader connect. Covers Ashita's UAC prompt and Windower's update check.
    connect: Duration,
    /// Ashita-cli's exit to the loader's connect. Ashita-cli exits once its in-process start-up
    /// has run and the game host resumed; the loader connects about 30 ms into the host's
    /// main. Past this the host's Ashita start-up failed and it never ran the loader.
    host_start: Duration,
    /// Connect to hello.
    hello: Duration,
    /// Writing the session line.
    send: Duration,
    /// Session sent to `started`; past this the launch returns as "start unconfirmed".
    start: Duration,
    /// Windower exits once the game window exists; the launch log notes when, if within this
    /// long after the hello. Nothing waits for it.
    injector_exit: Duration,
    /// A park closed by the window while it starts (park::park): set, the start stops before
    /// spawning the tool or, once spawned, stops it through its injector (Windower first).
    abort: Option<&'static AtomicBool>,
}
impl Timeouts {
    fn aborted(&self) -> bool { self.abort.is_some_and(|abort| abort.load(Ordering::Acquire)) }
}

const TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(60),
    host_start: Duration::from_secs(8),
    hello: Duration::from_secs(10),
    send: Duration::from_secs(10),
    start: Duration::from_secs(120),
    injector_exit: Duration::from_secs(30),
    abort: None,
};

/// What the launcher needs from a native sign-in (`NativeAuth`): the contract's `session`
/// line and the session's local state. A trait so tests can stand in a fake.
pub(crate) trait SignedIn: Send + Sync + 'static {
    fn social_only(&self) -> bool { false }
    /// A Play: the session signs in idle (`start_game: false`) and the worker starts FFXI
    /// itself, after the stock version check, once the loader reports the idle session.
    fn start_pending(&self) -> bool { false }
    /// The session is signed in with no game: a sign-in-only launch, or a Play until its start.
    fn idle_first(&self) -> bool { self.social_only() }
    /// The deferred start was sent (`None`) or stopped by the version check (`Some(message)`:
    /// the session stays signed in and idle, with the message as its error).
    fn deferred_start_done(&self, _blocked: Option<&str>) {}
    fn game_running(&self) -> Option<Arc<AtomicBool>> { None }
    fn native_session_state(&self, _game: bool) -> Result<(), String> { Ok(()) }
    fn native_own_mode(&self, _mode: i32) -> Result<(), String> { Ok(()) }
    /// The selected handle's portrait, from `native_own_portrait`.
    fn native_own_portrait(&self, _portrait: u16) -> Result<(), String> { Ok(()) }
    fn native_control_result(&self, _action: &str, _code: i32) -> Result<(), String> { Ok(()) }
    fn take_controller_settings(&self) -> Option<Vec<u8>> { None }
    fn take_game_shutdown(&self) -> Option<Vec<u8>> { None }
    fn game_shutdown_result(&self, _id: u64, _game: u64, _ok: bool, _message: &str) -> Result<(), String> { Ok(()) }
    fn native_game_ended(&self, _game: u64, _returned: bool) -> Result<(), String> { Ok(()) }
    fn controller_result(&self, _id: u64, _ok: bool, _message: &str) -> Result<(), String> { Ok(()) }
    /// A pending control and its "also for my groups" choice.
    fn take_native_control(&self) -> Option<(crate::session_worker::NativeControl, bool)> { None }
    fn take_startup_sign_out(&self) -> bool { false }
    fn take_native_batch(&self) -> Option<Vec<u8>> { None }
    fn native_command_listener(&self) -> Option<crate::session_worker::CommandWake> { None }
    /// The window saw input for this idle session since the last call.
    fn take_native_activity(&self) -> bool { false }
    /// A Play was queued behind this pre-sign-in since the last call; true at most once
    /// per session, and only where the loader has Viewer pauses to skip (07d §4.3).
    fn take_play_pending(&self) -> bool { false }
    fn take_native_social(&self) -> Option<Zeroizing<Vec<u8>>> { None }
    fn native_social_ready(&self) -> Result<(), String> { Ok(()) }
    /// Retry only a failed local status-cache publication; no native POL operation.
    fn retry_native_publication(&self) -> Result<bool, String> { Ok(false) }
    /// A failed status-cache publication waits for `retry_native_publication`.
    fn publication_pending(&self) -> bool { false }
    fn native_mail_changed(&self) -> Result<(), String> { Ok(()) }
    fn native_social_result(&self, _id: &str, _action: &str, _code: i32, _payload: Option<&[u8]>) -> Result<(), String> { Ok(()) }
    fn receive_native_friends(&self, _blob: &[u8], _locations: Option<&[u8]>) -> Result<usize,String> { Err("No native POL session is active.".into()) }
    fn native_status(&self, _code:i32) {}
    /// The one `session` line, `\n`-terminated, in a buffer zeroed on drop.
    fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String>;
    /// A parked host's `credentials` line at Play (pol-spec 07d §1.2): `session_line`'s
    /// per-Play flags and credentials without the host fields the `park` line carried.
    /// One use, like `session_line`.
    fn credentials_line(&self) -> Result<Zeroizing<Vec<u8>>, String>;
    fn sign_out(self) -> String where Self: Sized { "Session closed.".into() }
}

/// Status controls carry `groups`: the Viewer follows a status change with
/// ChgAllGrpStatus 404 when I am in groups (app 10040f39) unless told not to.
fn native_control_line(action: crate::session_worker::NativeControl, groups: bool) -> String {
    use crate::session_worker::NativeControl::{Away, Invisible, Portrait, Visible};
    match action {
        Visible | Away | Invisible => format!("{{\"type\":\"native_{}\",\"groups\":{groups}}}\n", action.name()),
        Portrait(portrait) => format!("{{\"type\":\"native_portrait\",\"portrait\":{portrait}}}\n"),
        _ => format!("{{\"type\":\"native_{}\"}}\n", action.name()),
    }
}

/// Sign in and launch `account` with its selected retail method. Local checks run before
/// authentication, so a missing tool or loader never costs a sign-in.
pub(crate) fn launch(
    config: &Config,
    plan: &Plan,
    account: Account,
    otp: Zeroizing<String>,
    mut timing: Timeline,
    progress: &mut impl FnMut(&str),
) -> Result<LaunchResult, String> {
    let auth = native_auth_for(config, plan, &account, otp)?;
    let prepared = prepare_with_progress(config, plan, &account, &mut timing, progress, &|| false)?;
    drop(account);
    // beside the exact loader this launch runs; never fails the launch.
    if let Some(line) = crate::dgvoodoo::sync_for_launch(&prepared.loader, plan.settings.dgvoodoo) {
        progress(&line);
    }
    timing.stage("preparation: dgVoodoo synchronized");
    timing.mark(Milestone::SignInStart);
    progress(t("Preparing native PlayOnline authentication..."));
    run(prepared, auth, timing, progress, &mut spawn_injector, &launch_gate, TIMEOUTS)
}

/// The native sign-in for `account` with `plan`'s flags, shared by a cold launch and a parked
/// Play: the region check and credential/code validation, then DNS warm-up, so an invalid
/// entry fails with no lookup at all. The W2U at Connect comes seconds later (after the tool
/// and polcore start): resolving its server now spares that check a cold lookup on the
/// sign-in's path, and the member-login host polcore resolves at Open(1) (08 §4 item 3).
fn native_auth_for(config: &Config, plan: &Plan, account: &Account, otp: Zeroizing<String>) -> Result<native_auth::NativeAuth, String> {
    if account.region != 0 {
        return Err(t("Retail login currently supports the verified US install only. EU and JP protocol region values are not verified yet.").into());
    }
    let mut auth = native_auth::NativeAuth::new(account, otp, config.account_directory(account))?;
    crate::patch::warm_dns();
    crate::dns::warm(crate::dns::POL_LOGIN_HOST);
    configure(&mut auth, plan);
    Ok(auth)
}

/// The plan's part of the session line.
fn configure(auth: &mut native_auth::NativeAuth, plan: &Plan) {
    auth.set_social_only(plan.social_only);
    // Every Play signs in idle first, like a sign-in-only launch. The worker runs the stock
    // FFXI version check (W20) once the loader reports the Viewer main menu (the presence
    // publish), never on authentication (pol-spec 07c §5.1). GameStart still requires both
    // the full native idle report and the check's verdict.
    auth.set_deferred_start(!plan.social_only);
    // Viewer pauses only where they cost nothing at Play: a pre-sign-in (07c §4).
    auth.set_viewer_gaps(plan.social_only && plan.viewer_gaps);
    auth.set_keep_session(plan.keep_session);
    auth.set_auto_away(plan.auto_away_minutes);
    auth.set_auto_login(plan.auto_login);
}

/// Everything resolved before sign-in.
struct Prepared {
    tool: Tool,
    tool_dir: PathBuf,
    /// Canonical plain path; the hello's process must run exactly this image.
    loader: PathBuf,
    install_dir: PathBuf,
    ini_name: String,
    base_ini: Option<Vec<u8>>,
    /// Ashita only: the boot ini loads a POL plugin that races when two hosts start together (`tools::SERIAL_POLPLUGINS`), so this start takes the machine-wide turn. Other starts go at once.
    serial_start: bool,
    profile: String,
    /// Profile video/sound values and Crystal's retained menu size, served in-process
    /// (`ffxi_settings`), so the shared registry they are also written to doesn't matter.
    ffxi_settings: Option<ffxi_settings::WindowerSettings>,
    /// Protects the settings snapshot until Windower.exe finishes saving it.
    profile_access: Option<profile_access::Guard>,
    session: Session,
    friends_directory: Option<PathBuf>,
    /// The FFXI version check run before a deferred game start (replaced in tests).
    check: GameCheck,
    /// The Viewer check the loader asks for at Connect (replaced in tests).
    viewer_check: ViewerCheck,
    /// Windower only: the inputs a saved startup snapshot must match, or why
    /// private startup can't apply (an unsupported Hook.dll build, unreadable files).
    bootstrap_key: Result<windower_parallel::Key, &'static str>,
    /// Today's verified snapshot: the host initializes Hook.dll privately and
    /// Windower.exe does not run (windower_parallel.rs).
    private_bootstrap: Option<windower_parallel::Snapshot>,
    /// What started this launch, for the log's trigger line (07d §4.2).
    trigger: crate::launch_timing::LaunchTrigger,
}

/// The stock FFXI check for a game folder: `Ok(note)` to start, `Err(message)` to stop,
/// with the phases of its exchange (07d §2.2: logged on every path).
type GameCheck = fn(&Path) -> (Result<String, String>, crate::patch::CheckTiming);
/// One stock W2U (:54000) for this sign-in, with its "network not ready" retries and the
/// phases of every attempt; returns after the last socket closed (07c §1.2, 07d §2.3).
type ViewerCheck = fn() -> crate::viewer::ConnectCheck;

enum CheckTask {
    Waiting,
    /// The verdict, when it arrived (the join can come later) and its phases.
    Running(std::thread::JoinHandle<(Result<String, String>, Instant, crate::patch::CheckTiming)>),
    Failed(String),
    Consumed,
}

/// The session's stock version checks, moved intact from the startup wait to the
/// supervisor: the Viewer's W2U, answered when the loader asks between Open(0) and
/// Open(1), and one fresh FFXI W20 per Play. W20 starts on its own connection once the
/// loader reports the Viewer main menu, i.e. after the presence publish (07c §2, §5.1;
/// stock r2 :53001 follows the publish), never on authentication: that put :53001
/// beside mail and presence, an order stock cannot produce (07a presign-fast-path (1)).
struct GameVersionCheck {
    install: PathBuf,
    directory: Option<PathBuf>,
    check: GameCheck,
    viewer: ViewerCheck,
    task: CheckTask,
    /// Why this sign-in's W2U got no answer, for the error if the sign-in then fails
    /// (07d §5.6 step 4: the loader goes on after an unanswered W2U).
    viewer_cause: Option<String>,
}
impl GameVersionCheck {
    fn new(prepared: &Prepared) -> Self {
        Self { install: prepared.install_dir.clone(), directory: prepared.friends_directory.clone(), check: prepared.check,
            viewer: prepared.viewer_check, task: CheckTask::Waiting, viewer_cause: None }
    }

    /// Typed authentication: published for status only. W20 waits for the main menu. No
    /// patch-host lookup here: every sign-in reached this, including those no W20 follows,
    /// while stock looks pc001W2U.pol.com up only right before its W20 (~0.2 s before the
    /// SYN, captures 2026-10-13), which is the W20's own lookup.
    fn authenticated(&self, log: &mut SessionLog) {
        if let Some(directory) = &self.directory {
            if let Err(error) = crate::session_worker::publish_authenticated(directory) { log.line(&error); }
        }
    }

    /// `native_main_menu` after sign-in: a Play's W20 may go now (a sign-in-only session
    /// checks nothing until its own Play).
    fn main_menu(&mut self, auth: &impl SignedIn, log: &mut SessionLog) {
        if auth.start_pending() { self.start(log); }
    }

    fn start(&mut self, log: &mut SessionLog) {
        if !matches!(self.task, CheckTask::Waiting) { return; }
        log.line("Viewer main menu reached; checking the FFXI version.");
        let (install, check, directory) = (self.install.clone(), self.check, self.directory.clone());
        self.task = match std::thread::Builder::new().name("ffxi-version-check".into())
            .spawn(move || {
                let (result, timing) = check(&install);
                // The window runs the update this refusal asks for: it spaces its first
                // connection from this W20's FIN (proposal 09 phase 2).
                if let (Err(message), Some(directory)) = (&result, &directory) {
                    if crate::patch::is_update_refusal(message) { crate::patch::save_update_close(directory); }
                }
                (result, Instant::now(), timing)
            }) {
            Ok(task) => CheckTask::Running(task),
            Err(_) => CheckTask::Failed(t("Could not start the game version check.").into()),
        };
    }

    /// The verdict and, when a check ran, when it finished.
    fn finish(&mut self, log: &mut SessionLog) -> (Result<String, String>, Option<Instant>) {
        // An older loader sends no `native_main_menu`; its first idle report already
        // follows the publish, so it still checks exactly once, after the main menu.
        self.start(log);
        match std::mem::replace(&mut self.task, CheckTask::Consumed) {
            CheckTask::Running(task) => task.join().map_or_else(|_| (Err(t("The game version check failed.").into()), None), |(result, at, timing)| {
                for line in crate::launch_timing::check_lines("FFXI check (W20)", &timing) { log.line(&line); }
                (result, Some(at))
            }),
            CheckTask::Failed(error) => (Err(error), None),
            CheckTask::Waiting | CheckTask::Consumed => (Err(t("The game version check was already consumed.").into()), None),
        }
    }
}

#[cfg(test)]
fn prepare(config: &Config, plan: &Plan, account: &Account) -> Result<Prepared, String> {
    prepare_with_progress(config, plan, account, &mut Timeline::begin(), &mut |_| {}, &|| false)
}

/// `aborted`: a park closed meanwhile; the Windower snapshot turn stops waiting (`park::ABORTED`).
fn prepare_with_progress(config: &Config, plan: &Plan, account: &Account, timing: &mut Timeline, progress: &mut dyn FnMut(&str),
    aborted: &dyn Fn() -> bool) -> Result<Prepared, String> {
    timing.stage("preparation: entered");
    let tool = plan.tool;
    let settings = &plan.settings;
    let configured = match tool {
        Tool::Ashita => Some(&settings.ashita),
        Tool::Windower => Some(&settings.windower),
        Tool::Vanilla => None,
    };
    let tool_dir = if let Some(configured) = configured {
        Some(tools::detect(tool, configured).ok_or_else(|| {
            if configured.trim().is_empty() {
                tf(
                    "{tool} was not found in the usual folders. Set its folder in Settings → System → Launch tools, or choose another Launch with.",
                    &[("tool", &tool.name())],
                )
            } else {
                tf(
                    "{tool} was not found: {folder} has no {exe}. Fix the folder in Settings → System → Launch tools.",
                    &[("tool", &tool.name()), ("folder", &configured.trim()), ("exe", &tool.exe())],
                )
            }
        })?)
    } else { None };
    timing.stage("preparation: tool directory resolved");
    let loader = tools::loader_path(&settings.loader).ok_or_else(|| {
        if settings.loader.trim().is_empty() {
            tf(
                "{loader} is missing from the launcher's folder. Reinstall the launcher, or set the loader path in Settings → System → Launch tools.",
                &[("loader", &tools::LOADER_EXE)],
            )
        } else {
            tf("The crystal-loader path {path} does not exist.", &[("path", &settings.loader.trim())])
        }
    })?;
    let loader = crate::reshade::loader_for_launch(&loader, &settings.reshade, settings.dgvoodoo)?;
    let loader = plain_windows_path(&fs::canonicalize(&loader).unwrap_or(loader));
    timing.stage("preparation: loader resolved");
    let install_dir = install_directory(config)?;
    timing.stage("preparation: game directory resolved");
    let tool_dir = tool_dir.unwrap_or_else(|| install_dir.clone());
    let mut base_ini = None;
    let mut serial_start = true;
    let mut profile = String::new();
    let mut profile_access = None;
    let mut bootstrap_key = Err("not a Windower launch");
    let ffxi_settings;
    // Windower gives Hook.dll its profile's UI dimensions during injection.
    // Overriding only FFXiMain's menu dimensions would leave game and plugins
    // using different coordinate systems. Crystal's menu preference belongs to
    // the other launch methods, including when that preference is unreadable.
    let menu = if tool == Tool::Windower { None } else { crate::game_config::retail_menu(config)? };
    match tool {
        Tool::Vanilla => ffxi_settings = Some(ffxi_settings::menu_settings(menu)),
        Tool::Ashita => {
            let explicit = !settings.boot_ini.trim().is_empty();
            let name = if explicit { settings.boot_ini.trim() } else { tools::DEFAULT_BOOT_INI };
            if !tools::valid_boot_ini_name(name) {
                return Err(t("The base boot config must be an .ini file name in Ashita's config\\boot folder.").into());
            }
            let path = tool_dir.join("config").join("boot").join(name);
            serial_start = tools::boot_ini_needs_turn(&path);
            match fs::metadata(&path) {
                Ok(meta) if meta.len() > 1024 * 1024 => return Err(tf("{path} is too large to be a boot config.", &[("path", &path.display())])),
                Ok(_) => base_ini = Some(fs::read(&path).map_err(|_| tf("Could not read {path}.", &[("path", &path.display())]))?),
                Err(_) if explicit => {
                    return Err(tf(
                        "The base boot config {name} was not found in {folder}. Choose another in the account's launch settings or in Settings → System → Launch tools.",
                        &[("name", &name), ("folder", &path.parent().unwrap_or(&path).display())],
                    ))
                }
                Err(_) => {}
            }
            // Fails early on a loader path Ashita can't read (non-ASCII).
            tools::ashita_boot_ini(base_ini.as_deref(), &loader, "check", 0)?;
            ffxi_settings = Some(ffxi_settings::menu_settings(menu));
        }
        Tool::Windower => {
            profile = settings.windower_profile.trim().to_owned();
            if !tools::valid_profile(&profile) {
                return Err(t("The Windower profile name can't contain quotes or control characters.").into());
            }
            profile_access = Some(profile_access::launching(&tool_dir, progress)?);
            timing.stage("preparation: profile access acquired");
            // A previous Windower launcher may now be saving this same file.
            // Its native host holds the shared launch turn through that exit.
            let _snapshot_turn = launch_gate(Tool::Windower, progress, aborted)?;
            timing.stage("preparation: profile snapshot turn acquired");
            ffxi_settings = Some(ffxi_settings::for_windower(&tool_dir, &profile)?);
            bootstrap_key = windower_parallel::Key::new(&tool_dir, &profile, &install_dir, &windower_startup_store(config));
            timing.stage("preparation: Windower startup inputs checked");
        }
    }
    // The launcher features' values (a remembered window's size and mode, background mute).
    let slot = plan.auto_login.enabled.then_some(plan.auto_login.character_slot);
    let overrides = crate::features::runtime::launch_values(config, &crate::backend::account_key(account), slot);
    let ffxi_settings = ffxi_settings.map(|settings| ffxi_settings::with_overrides(settings, &overrides));
    timing.stage("preparation: launch settings read");
    let logs = config.logs_directory(account);
    fs::create_dir_all(&logs).map_err(|_| t("Could not create the account's log folder."))?;
    timing.stage("preparation: log directory ready");
    Ok(Prepared {
        tool,
        tool_dir,
        loader,
        install_dir,
        ini_name: tools::boot_ini_name(&crate::backend::account_key(account)),
        base_ini,
        serial_start,
        profile,
        ffxi_settings,
        profile_access,
        session: Session {
            history: config.history_path(),
            label: account.label.clone(),
            log: logs.join(format!("{}.crystal.log", unique_id()?)),
        },
        friends_directory: Some(config.account_directory(account)),
        check: crate::patch::gate_before_start_timed,
        viewer_check: crate::viewer::check_at_connect_timed,
        bootstrap_key,
        private_bootstrap: None,
        trigger: plan.trigger,
    })
}

/// Saved Windower startup snapshots, in Crystal's per-user data folder next to `crystal\pol.exe`.
fn windower_startup_store(config: &Config) -> PathBuf {
    config.experiment.join("iced-launcher").join("crystal").join("windower-startup")
}

/// The retail game folder the loader runs: the one chosen on Downloads, else the installed
/// client's registry entry.
pub(crate) fn install_directory(config: &Config) -> Result<PathBuf, String> {
    let dir = match &config.game_install {
        Some(path) => plain_windows_path(path),
        None => registry_install_dir().ok_or(
            t("The FINAL FANTASY XI folder is not known. Select it in Settings."),
        )?,
    };
    for name in ["FFXi.dll", "FFXiMain.dll"] {
        if !dir.join(name).is_file() {
            return Err(tf(
                "{folder} has no {name}. Select the FINAL FANTASY XI folder in Settings.",
                &[("folder", &dir.display()), ("name", &name)],
            ));
        }
    }
    Ok(dir)
}

fn registry_install_dir() -> Option<PathBuf> {
    crate::discovery::find(crate::discovery::Product::Ffxi, None)
}

/// The per-launch log (`<id>.crystal.log` in the account's logs folder): what the launcher
/// did and what the loader reported. Never session bytes or credentials.
#[derive(Default)]
pub(crate) struct SessionLog {
    file: Option<File>,
    // Diagnostic context from verified pipe events; log writes may fail, so
    // the exit classification must not depend on rereading their text.
    native_idle: bool,
    native_sign_out: bool,
    native_game_started: bool,
    native_shutdown_returned: bool,
    /// The core refused a sign-out or game start, or its sign-out teardown failed.
    native_failure: bool,
    /// The POL connection is down (`native_pol_status` < 0); a later 0 clears it.
    native_connection_lost: bool,
}
impl SessionLog {
    fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| t("Could not create the launch log."))?;
        Ok(Self { file: Some(file), ..Self::default() })
    }
    fn line(&mut self, text: &str) {
        if let Some(file) = &mut self.file {
            let time = chrono::Local::now().format("%H:%M:%S");
            let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            // One append per line: the Windower watcher writes to the same log from its thread.
            let _ = file.write_all(format!("[{time}] [launcher] {flat}\n").as_bytes());
        }
    }
    /// Write any timing marks the log has not seen yet.
    fn timing(&mut self, timeline: &mut Timeline) {
        for line in timeline.unwritten_lines() {
            self.line(&line);
        }
    }
    /// Record a milestone and write it.
    fn reached(&mut self, timeline: &mut Timeline, milestone: Milestone) {
        timeline.mark(milestone);
        self.timing(timeline);
    }
    fn reached_at(&mut self, timeline: &mut Timeline, milestone: Milestone, at: Instant) {
        timeline.mark_at(milestone, at);
        self.timing(timeline);
    }
    /// The log as a child's stdout/stderr, so the injector's own messages land in it too.
    fn stdio(&self) -> Stdio {
        self.file
            .as_ref()
            .and_then(|f| f.try_clone().ok())
            .map_or_else(Stdio::null, Stdio::from)
    }
}

/// The Ashita-cli or Windower process, watched until the loader connects.
pub(crate) enum Injector {
    Child(std::process::Child),
    /// The host itself, already loaded with a private Windower bootstrap.
    NativeWindower(std::process::Child),
    /// Started through the UAC prompt; only waitable.
    Elevated(std::os::windows::io::OwnedHandle),
    /// No process to watch (tests).
    #[cfg(test)]
    Detached,
    /// A stand-in process that "exits" when its flag is set (tests).
    #[cfg(test)]
    Fake(Arc<AtomicBool>),
}
impl Injector {
    /// Injection and the host handshake have completed. Close only the
    /// launcher we started, before it waits for a game window and rewrites
    /// its old settings snapshot. Hook and the game live in the separate host.
    fn finish_windower_handoff(&mut self) -> Result<(), String> {
        match self {
            Injector::Child(child) => {
                let running = child.try_wait().map_err(|e| format!("Could not finish the Windower handoff: {e}"))?.is_none();
                if running {
                    if let Err(error) = child.kill() {
                        // It can finish naturally between the exit check and
                        // termination. A still-running process remains an error.
                        if !matches!(child.try_wait(), Ok(Some(_))) {
                            return Err(format!("Could not close the Windower launcher after injection: {error}"));
                        }
                    }
                    child.wait().map_err(|e| format!("Could not wait for Windower's launcher to close: {e}"))?;
                }
                Ok(())
            }
            #[cfg(test)]
            Injector::Fake(exited) => { exited.store(true, Ordering::SeqCst); Ok(()) }
            #[cfg(test)]
            Injector::Detached => Ok(()),
            _ => Err("Windower's launcher process could not be verified for the completed handoff.".into()),
        }
    }

    /// `Some(code)` once the process has exited.
    fn exited(&mut self) -> Option<Option<i32>> {
        match self {
            Injector::Child(child) | Injector::NativeWindower(child) => child.try_wait().ok().flatten().map(|s| s.code()),
            Injector::Elevated(handle) => {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::{
                    Foundation::WAIT_OBJECT_0,
                    System::Threading::{GetExitCodeProcess, WaitForSingleObject},
                };
                unsafe {
                    if WaitForSingleObject(handle.as_raw_handle() as _, 0) != WAIT_OBJECT_0 {
                        return None;
                    }
                    let mut code = 0u32;
                    Some((GetExitCodeProcess(handle.as_raw_handle() as _, &mut code) != 0).then_some(code as i32))
                }
            }
            #[cfg(test)]
            Injector::Detached => None,
            #[cfg(test)]
            Injector::Fake(exited) => exited.load(Ordering::SeqCst).then_some(Some(0)),
        }
    }
    /// Stop our own child if it is still running (a stuck Ashita-cli or Windower).
    fn stop(&mut self) {
        if let Injector::Child(child) | Injector::NativeWindower(child) = self {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // A stand-in "exits" when stopped, so tests see the stop and its order.
        #[cfg(test)]
        if let Injector::Fake(exited) = self { exited.store(true, Ordering::SeqCst); }
    }

    fn wait(&mut self) -> std::io::Result<()> {
        match self {
            Injector::Child(child) | Injector::NativeWindower(child) => child.wait().map(|_| ()),
            Injector::Elevated(handle) => {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::{WaitForSingleObject, INFINITE}};
                if unsafe { WaitForSingleObject(handle.as_raw_handle() as _, INFINITE) } == WAIT_OBJECT_0 {
                    Ok(())
                } else { Err(std::io::Error::last_os_error()) }
            }
            #[cfg(test)]
            Injector::Detached => Ok(()),
            #[cfg(test)]
            Injector::Fake(exited) => {
                while !exited.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(5)); }
                Ok(())
            }
        }
    }
}

/// On a failed Windower start, stop its launcher before releasing permission to
/// edit profiles. A successful start moves this owner into the exit observer.
struct LaunchedInjector {
    injector: Injector,
    profile_access: Option<profile_access::Guard>,
    stop_on_drop: bool,
}
impl Drop for LaunchedInjector {
    fn drop(&mut self) {
        if self.stop_on_drop { self.injector.stop(); }
    }
}

/// "Could not start {exe}: {error}", for every spawn below.
fn start_error(exe: &Path, error: std::io::Error) -> String {
    tf("Could not start {exe}: {error}", &[("exe", &exe.display()), ("error", &error)])
}

/// Start the injector for this launch.
fn spawn_injector(p: &Prepared, pipe: &str, log: &mut SessionLog) -> Result<Injector, String> {
    let pid = std::process::id();
    let exe = p.tool_dir.join(p.tool.exe());
    match p.tool {
        Tool::Vanilla => {
            let mut command = tools::vanilla_command(&p.loader, &p.install_dir, pipe, pid);
            command.stdout(log.stdio()).stderr(log.stdio());
            command.spawn().map(Injector::Child).map_err(|e| start_error(&p.loader, e))
        }
        Tool::Ashita => {
            let ini = tools::ashita_boot_ini(p.base_ini.as_deref(), &p.loader, pipe, pid)?;
            let boot = p.tool_dir.join("config").join("boot");
            let path = boot.join(&p.ini_name);
            fs::create_dir_all(&boot)
                .and_then(|_| crate::fsutil::write_atomic(&path, &ini).map_err(std::io::Error::from))
                .map_err(|_| tf("Could not write Ashita's boot config {path}.", &[("path", &path.display())]))?;
            log.line(&format!("Wrote {}", path.display()));
            let mut command = tools::ashita_command(&p.tool_dir, &p.ini_name);
            command.stdout(log.stdio()).stderr(log.stdio());
            match command.spawn() {
                Ok(child) => Ok(Injector::Child(child)),
                // ERROR_ELEVATION_REQUIRED: Ashita-cli's manifest requires administrator.
                Err(e) if e.raw_os_error() == Some(740) => {
                    log.line("Ashita-cli needs administrator rights; asking Windows.");
                    run_elevated(&exe, &p.ini_name, &p.tool_dir).map(Injector::Elevated)
                }
                Err(e) => Err(start_error(&exe, e)),
            }
        }
        Tool::Windower => {
            if let Some(snapshot) = &p.private_bootstrap {
                let mut command = tools::vanilla_command(&p.loader, &snapshot.viewer_directory()?, pipe, pid);
                command.env("__COMPAT_LAYER", "HighDpiAware").stdout(Stdio::null()).stderr(Stdio::null());
                return command.spawn().map(Injector::NativeWindower).map_err(|e| start_error(&p.loader, e));
            }
            let mut command = tools::windower_command(&p.tool_dir, &p.profile, &p.loader, pipe, pid)?;
            command.spawn().map(Injector::Child).map_err(|e| start_error(&exe, e))
        }
    }
}

/// ShellExecuteEx "runas": the UAC prompt, for a launcher that isn't elevated.
fn run_elevated(exe: &Path, argument: &str, directory: &Path) -> Result<std::os::windows::io::OwnedHandle, String> {
    use crate::win::{shell_execute, ShellError};
    use windows_sys::Win32::{Foundation::ERROR_CANCELLED, UI::WindowsAndMessaging::SW_HIDE};
    // Windows' own error UI stays on here (no SEE_MASK_FLAG_NO_UI), as before.
    shell_execute("runas", exe, Some(argument), Some(directory), SW_HIDE, false).map_err(|error| match error {
        ShellError::Failed(ERROR_CANCELLED) => {
            t("The Windows administrator prompt for Ashita was declined. Ashita needs administrator rights: accept the prompt, or run the launcher as administrator.").into()
        }
        ShellError::Failed(_) => tf("Windows could not start {exe} as administrator.", &[("exe", &exe.display())]),
        ShellError::NoProcess => tf("Windows started {exe} but returned no process to watch.", &[("exe", &exe.display())]),
    })
}

/// After a Windower start's hello: wait (on its own thread) until Windower.exe has exited or
/// `until` passes, and note which in the launch log. If still running, retain the
/// profile guard in a blocking child wait until its final settings.xml save.
fn watch_windower_exit(mut child: LaunchedInjector, hello: Instant, until: Instant, log: PathBuf) -> impl FnOnce() + Send + 'static {
    move || {
        let exited = loop {
            if child.injector.exited().is_some() {
                break true;
            }
            if Instant::now() >= until {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let line = if exited {
            format!("Windower.exe exited {:.1} s after the loader's hello.", hello.elapsed().as_secs_f32())
        } else {
            format!("Windower.exe was still running {:.0} s after the loader's hello.", hello.elapsed().as_secs_f32())
        };
        if let Ok(mut log) = SessionLog::open(&log) {
            log.line(&line);
        }
        if !exited && child.profile_access.is_some() {
            let waited = child.injector.wait();
            if let Ok(mut log) = SessionLog::open(&log) {
                log.line(&match waited {
                    Ok(()) => format!("Windower.exe exited {:.1} s after the loader's hello; profile editing is available.", hello.elapsed().as_secs_f32()),
                    Err(error) => format!("Could not wait for Windower.exe to finish saving profiles: {error}"),
                });
            }
        }
    }
}

fn observe_windower_exit(child: LaunchedInjector, hello: Instant, until: Instant, log: PathBuf) -> Result<(), String> {
    let protected = child.profile_access.is_some();
    let watcher = watch_windower_exit(child, hello, until, log);
    let observer = std::thread::Builder::new().name("windower-exit".into()).spawn(watcher)
        .map_err(|_| t("Could not monitor Windower's profile save.").to_owned())?;
    if protected {
        profile_access::retain_observer(observer);
    }
    Ok(())
}

enum Step {
    Started,
    Continue,
    End(LoaderEnd),
}

/// The answer to the loader's `native_viewer_check` (pol-spec 07c §1). Sent only once the
/// W2U connection is closed; any `ok:false` stops the sign-in before Open(1).
fn viewer_check_result_line(verdict: &crate::viewer::ConnectVerdict) -> String {
    match verdict.reason() {
        None => "{\"type\":\"native_viewer_check_result\",\"ok\":true}\n".into(),
        Some(reason) => format!(
            "{{\"type\":\"native_viewer_check_result\",\"ok\":false,\"reason\":\"{reason}\",\"error\":{}}}\n",
            serde_json::to_string(&verdict.message()).unwrap_or_else(|_| "\"\"".into())
        ),
    }
}

/// `text (code)`, the OS wording with its number, as the row and the log show it.
fn net_error_text(error: &crate::dns::NetError) -> String {
    match error.code { Some(code) => format!("{} ({code})", error.text), None => error.text.clone() }
}

/// One stock W2U for the loader's `native_viewer_check`, never on the window thread.
/// No result is shared between accounts or carried into another sign-in.
/// A first attempt that failed instantly and locally was retried with fresh DNS (07d §2.3).
fn answer_viewer_check(pipe: &mut LoaderPipe, log: &mut SessionLog, game: &mut GameVersionCheck) -> Result<(), LoaderEnd> {
    log.line("The loader reached Connect; checking the PlayOnline Viewer version (W2U).");
    let crate::viewer::ConnectCheck { verdict, timing } = (game.viewer)();
    if let Some(first) = timing.attempts.first().and_then(|attempt| attempt.error.as_ref()).filter(|error| error.network_not_ready()) {
        log.line(&format!("Viewer check (W2U): network not ready ({}); retrying with fresh DNS for up to {} s.",
            net_error_text(first), crate::patch::NETWORK_WAIT.as_secs()));
    }
    for line in crate::launch_timing::check_lines("Viewer check (W2U)", &timing) { log.line(&line); }
    match verdict.reason() {
        None => log.line("PlayOnline Viewer version is current; the sign-in continues."),
        // The loader signs in anyway when the server did not answer (a7550a6). Keep the
        // cause: if the sign-in then fails, the row says why the check failed too.
        Some("unreachable") => {
            log.line(&format!("The PlayOnline Viewer check got no answer; the sign-in continues: {}", verdict.message()));
            game.viewer_cause = Some(verdict.message());
        }
        // Nothing reached Square Enix; the loader stops before Open(1) (07d §2.5, §5.6).
        Some("network") => log.line(&format!("The network is not ready; the loader stops before the member login: {}", verdict.message())),
        Some(reason) => log.line(&format!("The PlayOnline Viewer check stops the sign-in ({reason}): {}", verdict.message())),
    }
    if pipe.send(viewer_check_result_line(&verdict).as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() {
        log.line("Could not answer the Viewer version check; the session pipe failed.");
        return Err(LoaderEnd::Closed);
    }
    Ok(())
}

fn handle_event(event: LoaderEvent, auth: &impl SignedIn, log: &mut SessionLog) -> Step {
    match event {
        // Answered by `wait_started` / `watch` (`answer_viewer_check`), which hold the check.
        LoaderEvent::NativeViewerCheck => Step::Continue,
        // W20 is started by `wait_started` / `watch` (`GameVersionCheck::main_menu`).
        LoaderEvent::NativeMainMenu { code, returned } => {
            log.line(&format!("Native POL main menu reached{} (publish result {code}).", if returned { " after FFXI returned" } else { "" }));
            Step::Continue
        }
        LoaderEvent::Started { pid } => {
            log.native_idle = false;
            log.native_sign_out = false;
            log.native_game_started = true;
            log.native_shutdown_returned = false;
            log.native_failure = false;
            log.native_connection_lost = false;
            log.line(&format!("Game started (GameStart called){}", pid.map_or(String::new(), |p| format!(", PID {p}"))));
            Step::Started
        }
        LoaderEvent::NativeSocialReady => {
            if let Err(error) = auth.native_social_ready() { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::NativeSocialResult { id, action, code, payload } => {
            log.line(&format!("Native POL {action} result {code}."));
            if let Err(error) = auth.native_social_result(&id, &action, code, payload.as_deref()) { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::NativeMailChanged => {
            if let Err(error) = auth.native_mail_changed() { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::NativeFriends { blob, locations } => {
            match auth.receive_native_friends(&blob, locations.as_deref()) {
                Ok(count) => log.line(&format!("Native POL published {count} friend records.")),
                Err(error) => log.line(&error),
            }
            Step::Continue
        }
        LoaderEvent::NativeStatus(code) => {
            // A recovered connection (07d §5.5) no longer blocks a normal close.
            log.native_connection_lost = code < 0;
            if code < 0 {
                log.native_idle = false;
                log.native_sign_out = false;
            }
            auth.native_status(code);
            log.line(&format!("Native POL connection result {code}."));
            Step::Continue
        }
        LoaderEvent::NativeSessionState(game) => {
            log.native_idle = !game;
            log.native_sign_out = false;
            if let Err(error) = auth.native_session_state(game) { log.line(&error); }
            log.line(if game { "Native POL session handed to FFXI." } else { "Native POL session is signed in to the Viewer; FFXI is closed." });
            if !game && auth.idle_first() { Step::Started } else { Step::Continue }
        }
        LoaderEvent::NativeOwnMode(mode) => {
            if let Err(error) = auth.native_own_mode(mode) { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::NativeOwnPortrait(portrait) => {
            if let Err(error) = auth.native_own_portrait(portrait) { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::GameShutdownResult { id, game, ok, message } => {
            if let Err(error) = auth.game_shutdown_result(id, game, ok, &message) { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::ControllerResult { id, ok, message } => {
            if let Err(error) = auth.controller_result(id, ok, &message) { log.line(&error); }
            Step::Continue
        }
        LoaderEvent::NativeControlResult { action, code } => {
            // A presence or group change that failed says nothing about how the game ended.
            if code < 0 && code != -261 && matches!(action.as_str(), "sign_out" | "start_game") { log.native_failure = true; }
            if action == "sign_out" {
                // Zero only accepts the command. Teardown reports its own
                // result once the core has finished signing out and closing.
                if code < 0 { log.native_sign_out = false; }
            } else if action == "start_game" && code == 0 {
                log.native_idle = false;
                log.native_sign_out = false;
            }
            if let Err(error) = auth.native_control_result(&action, code) { log.line(&error); }
            log.line(&format!("Native POL {action} request result {code}."));
            Step::Continue
        }
        LoaderEvent::NativeGameEnded { code, game_returned, game_id } => {
            // Native FFXI uses -14 for /shutdown after GameStart returns S_OK.
            // Require this run's explicit successful return and sign-out acknowledgment.
            log.native_shutdown_returned = log.native_game_started && game_returned && code == -14;
            if let Err(error) = auth.native_game_ended(game_id, game_returned) { log.line(&error); }
            log.line(&format!("FFXI returned {code}."));
            Step::Continue
        }
        LoaderEvent::NativeSignOutComplete(code) => {
            log.native_sign_out = code == 0;
            log.native_idle = true;
            if code < 0 { log.native_failure = true; }
            if let Err(error) = auth.native_session_state(false) { log.line(&error); }
            if let Err(error) = auth.native_control_result("sign_out", code) { log.line(&error); }
            log.line(&format!("Native POL sign-out completed with result {code}."));
            Step::Continue
        }
        LoaderEvent::Exit(code) => {
            log.line(&format!("Loader reported exit code {code}."));
            Step::End(if log.native_sign_out && (code == 0 || code == -14 && log.native_shutdown_returned && !log.native_failure && !log.native_connection_lost) {
                LoaderEnd::SignedOut
            } else { LoaderEnd::Exit(code) })
        }
        LoaderEvent::Error(message) => {
            log.line(&format!("Loader reported an error: {message}"));
            Step::End(LoaderEnd::Error(message))
        }
        LoaderEvent::Closed => {
            log.line("The session pipe closed.");
            Step::End(LoaderEnd::Closed)
        }
        // `parked` comes before credentials; the park worker reads it (park.rs, 07d §1.3).
        LoaderEvent::Hello { .. } | LoaderEvent::Parked { .. } | LoaderEvent::WindowerReady | LoaderEvent::Unknown | LoaderEvent::NativeAuthStage(_) | LoaderEvent::Authenticated => Step::Continue,
    }
}

/// How often a pre-sign-in's start wait looks for a queued Play between loader events
/// when no command wake is installed (a wake interrupts the read otherwise).
const PLAY_PENDING_POLL: Duration = Duration::from_millis(100);
/// The idle supervisor's longest wait with a command wake installed: commands wake it,
/// so this only bounds how late anything unsignalled is noticed.
const IDLE_SWEEP: Duration = Duration::from_secs(2);

/// Until `started` (`Ok(true)`), the deadline (`Ok(false)`, start unconfirmed), or the end.
/// `until_game`: the wait after a deferred `native_start_game`, which ends when the loader
/// reports the session handed to FFXI (an idle report is not a start).
fn wait_started(pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog, timeout: Duration, timing:&mut Timeline, progress:&mut dyn FnMut(&str), until_game: bool, game: &mut GameVersionCheck) -> Result<bool, LoaderEnd> {
    let deadline = Instant::now() + timeout;
    loop {
        // A Play queued behind this pre-sign-in: the loader skips the Viewer pauses still
        // to come (07d §4.3). Checked between events and on each command wake.
        if auth.take_play_pending() {
            if pipe.send(park::PLAY_PENDING_LINE.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() {
                log.line("The session pipe failed.");
                return Err(LoaderEnd::Closed);
            }
            log.line("A Play is waiting; the loader skips the remaining Viewer pauses.");
        }
        // Older hosts accept sign-out only at the main menu. Leave their
        // command queued for watch rather than consuming an unsupported cancel.
        if !until_game && auth.social_only() && pipe.supports_startup_sign_out() && auth.take_startup_sign_out() {
            if pipe.send(native_control_line(crate::session_worker::NativeControl::SignOut, true).as_bytes(),
                Instant::now() + Duration::from_secs(2)).is_err() {
                return Err(LoaderEnd::Closed);
            }
            log.line("Cancelling the unfinished PlayOnline sign-in before closing.");
        }
        let poll = auth.social_only() && !pipe.has_command_wake();
        let slice = if poll { (Instant::now() + PLAY_PENDING_POLL).min(deadline) } else { deadline };
        match pipe.next_event(Some(slice)) {
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut && slice < deadline => continue,
            // A command writer's wake: look at the commands again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                log.line(&format!("No start report within {} s; the game may still be loading.", timeout.as_secs()));
                return Ok(false);
            }
            Err(_) => {
                log.line("The session pipe failed.");
                return Err(LoaderEnd::Closed);
            }
            Ok(Err(message)) => log.line(&message),
            Ok(Ok(event)) => {
                match &event {
                    LoaderEvent::NativeAuthStage(state)=>{
                        log.line(&format!("Native POL authentication state {state}."));
                        progress(match state { 26..=28=>t("PlayOnline: authenticating Square Enix account..."),40..=41=>t("PlayOnline: selecting service..."),30=>t("PlayOnline: loading profile and friends..."),_=>t("PlayOnline: connecting...") });
                    }
                    LoaderEvent::NativeViewerCheck=>{
                        progress(t("PlayOnline: checking the Viewer version..."));
                        answer_viewer_check(pipe, log, game)?;
                        log.reached(timing,Milestone::ViewerChecked);
                    }
                    LoaderEvent::Authenticated=>{
                        log.reached(timing,Milestone::SignInDone);
                        game.authenticated(log);
                    }
                    LoaderEvent::NativeMainMenu { returned: false, .. }=>{
                        log.reached(timing,Milestone::MainMenu);
                        game.main_menu(auth, log);
                    }
                    _=>{},
                }
                let (handed_over, idle) = (matches!(&event, LoaderEvent::NativeSessionState(true)), matches!(&event, LoaderEvent::NativeSessionState(false)));
                match handle_event(event, auth, log) {
                Step::Started if until_game && idle => {}
                Step::Started => return Ok(true),
                Step::Continue if until_game && handed_over => return Ok(true),
                Step::Continue => {}
                Step::End(end) => return Err(end),
                }
            },
        }
    }
}

/// A Play whose session is now signed in and idle: consume its FFXI version check, then
/// `native_start_game`. `Ok(Ok(()))`: start sent. `Ok(Err(message))`: the check says update;
/// nothing is sent and the session stays signed in. `Err(())`: the pipe failed.
/// `timing`: the launch's timeline while the start wait still owns it.
fn start_deferred(pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog, game: &mut GameVersionCheck, progress: &mut dyn FnMut(&str), timing: Option<&mut Timeline>) -> Result<Result<(), String>, ()> {
    progress(t("Checking the game version..."));
    log.line("Native startup services finished; awaiting the FFXI version check before starting the game.");
    let (verdict, checked) = game.finish(log);
    if let (Some(timing), Some(at)) = (timing, checked) { log.reached_at(timing, Milestone::GameChecked, at); }
    match verdict {
        Err(message) => {
            log.line(&message);
            auth.deferred_start_done(Some(&message));
            Ok(Err(message))
        }
        Ok(note) => {
            log.line(&note);
            progress(t("Starting FFXI..."));
            let line = native_control_line(crate::session_worker::NativeControl::StartGame, false);
            if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() {
                log.line("Could not send the game start; the session pipe failed.");
                return Err(());
            }
            log.line("Game start sent to the signed-in session.");
            auth.deferred_start_done(None);
            Ok(Ok(()))
        }
    }
}

/// The rest of the session, on the supervisor thread.
fn watch(pipe: &mut LoaderPipe, auth: &impl SignedIn, log: &mut SessionLog, game: &mut GameVersionCheck) -> LoaderEnd {
    let mut next_publication = Instant::now() + Duration::from_secs(1);
    // An idle main menu trims and lowers this worker and its host; a Play restores them first.
    let mut footprint = crate::footprint::IdleSession::default();
    loop {
        footprint_note(log, footprint.tend(log.native_idle, Instant::now()));
        if let Some(line) = auth.take_game_shutdown() {
            if pipe.send(&line, Instant::now() + Duration::from_secs(2)).is_err() { return LoaderEnd::Closed; }
        }
        if let Some(line) = auth.take_controller_settings() {
            if pipe.send(&line, Instant::now() + Duration::from_secs(2)).is_err() { return LoaderEnd::Closed; }
        }
        if Instant::now() >= next_publication {
            if matches!(auth.retry_native_publication(), Ok(true)) {
                log.line("Recovered the native PlayOnline session status cache.");
            }
            next_publication = Instant::now() + Duration::from_secs(1);
        }
        if let Some(line) = auth.take_native_batch() {
            footprint_note(log, footprint.wake());
            if pipe.send(&line, Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
        }
        if let Some((action, groups)) = auth.take_native_control() {
            if matches!(action, crate::session_worker::NativeControl::StartGame) { footprint_note(log, footprint.wake()); }
            let line = native_control_line(action, groups);
            if pipe.send(line.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
        }
        if auth.take_native_activity()
            && pipe.send(b"{\"type\":\"native_activity\"}\n", Instant::now() + Duration::from_secs(10)).is_err() {
            return LoaderEnd::Closed;
        }
        if auth.take_play_pending() {
            footprint_note(log, footprint.wake());
            if pipe.send(park::PLAY_PENDING_LINE.as_bytes(), Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
            log.line("A Play is waiting; the loader skips the remaining Viewer pauses.");
        }
        if let Some(line) = auth.take_native_social() {
            if pipe.send(&line, Instant::now() + Duration::from_secs(10)).is_err() { return LoaderEnd::Closed; }
        }
        // Commands wake the read; without that wake, look for them every 250 ms. With it, wait
        // for the next timed duty: the idle trim, a status-cache retry, or the sweep.
        let now = Instant::now();
        let until = if pipe.has_command_wake() {
            let mut until = now + IDLE_SWEEP;
            if let Some(at) = footprint.next_change() { until = until.min(at.max(now)); }
            if auth.publication_pending() { until = until.min(next_publication.max(now)); }
            until
        } else {
            now + Duration::from_millis(250)
        };
        match pipe.next_event(Some(until)) {
            Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => continue,
            Err(_) => {
                log.line("The session pipe failed.");
                return LoaderEnd::Closed;
            }
            Ok(Err(message)) => log.line(&message),
            Ok(Ok(event)) => {
                // A sign-in that outlasted the start wait reaches its checks here.
                match &event {
                    LoaderEvent::NativeViewerCheck => if let Err(end) = answer_viewer_check(pipe, log, game) { return end; },
                    LoaderEvent::Authenticated => game.authenticated(log),
                    LoaderEvent::NativeMainMenu { returned: false, .. } => game.main_menu(auth, log),
                    _ => {}
                }
                // A Play whose sign-in outlasted the start wait: the idle session arrives here.
                if auth.start_pending() && matches!(event, LoaderEvent::NativeSessionState(false)) {
                    let _ = handle_event(event, auth, log);
                    footprint_note(log, footprint.wake());
                    if start_deferred(pipe, auth, log, game, &mut |_| {}, None).is_err() {
                        return LoaderEnd::Closed;
                    }
                    continue;
                }
                if let Step::End(end) = handle_event(event, auth, log) {
                    return end;
                }
            }
        }
    }
}

fn footprint_note(log: &mut SessionLog, change: crate::footprint::Change) {
    if let Some(line) = crate::footprint::IdleSession::apply(change) { log.line(&line); }
}

fn accept_failure(p: &Prepared, error: AcceptError, timeout: Duration) -> String {
    let hint = match p.tool {
        Tool::Vanilla => tf(
            "Check that Crystal's game host {loader} can run. Windows may have blocked or removed it.",
            &[("loader", &p.loader.display())],
        ),
        Tool::Ashita => tf(
            "Check that {exe} in {folder} started (accept its administrator prompt) and that it can run {loader}.",
            &[("exe", &Tool::Ashita.exe()), ("folder", &p.tool_dir.display()), ("loader", &p.loader.display())],
        ),
        Tool::Windower => tf(
            "Check that Windower in {folder} started (it may be updating itself) and that it can run {loader}.",
            &[("folder", &p.tool_dir.display()), ("loader", &p.loader.display())],
        ),
    };
    match error {
        AcceptError::TimedOut => tf("crystal-loader did not connect within {seconds} s. {hint}", &[("seconds", &timeout.as_secs()), ("hint", &hint)]),
        AcceptError::Check(message) => tf("{message} {hint}", &[("message", &message), ("hint", &hint)]),
        AcceptError::Io(e) => tf("The session pipe failed while waiting for crystal-loader ({error}).", &[("error", &e)]),
    }
}

/// An Ashita start whose host never ran the loader after Ashita-cli finished (`connect_host`),
/// with what Ashita's own log says went wrong in that host's start-up when it says anything.
fn host_never_ran(p: &Prepared, since: std::time::SystemTime, waited: Duration) -> String {
    let logs = p.tool_dir.join("logs");
    let errors = ashita_start_errors(&logs, &p.ini_name, since);
    let detail = if errors.is_empty() {
        tf("Check Ashita's log in {folder}.", &[("folder", &logs.display())])
    } else {
        tf("Ashita's log: {errors}", &[("errors", &errors.join(" "))])
    };
    tf("{exe} finished, but the game host it started did not run crystal-loader within {seconds} s. {detail}",
        &[("exe", &p.tool.exe()), ("seconds", &waited.as_secs()), ("detail", &detail)])
}

/// The errors Ashita logged while starting the host that boots `ini_name`, in logs created
/// since `since`. Ashita names a log after the second its host started, so hosts that start
/// in the same second share one; each host's part begins with its `Installing Ashita` line
/// and names its boot ini a few lines later.
fn ashita_start_errors(logs: &Path, ini_name: &str, since: std::time::SystemTime) -> Vec<String> {
    const MAX_LOG: u64 = 8 * 1024 * 1024;
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(logs).into_iter().flatten().flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok().filter(|m| m.is_file() && m.len() <= MAX_LOG)?;
            let created = meta.created().ok().filter(|created| *created >= since)?;
            Some((created, entry.path()))
        })
        .collect();
    files.sort();
    let boot = format!("\\boot\\{ini_name}");
    let mut errors: Vec<String> = Vec::new();
    for (_, path) in files {
        let Ok(bytes) = fs::read(&path) else { continue };
        let mut ours = false;
        for line in String::from_utf8_lossy(&bytes).lines() {
            let parts: Vec<&str> = line.split(" | ").collect();
            if parts.len() < 4 { continue; }
            let message = parts[3..].join(" | ");
            let message = message.trim();
            if parts[2].trim() == "InstallAshita" && message.starts_with("Installing Ashita") {
                ours = false;
            } else if message.contains("Config Script:") {
                ours = message.trim_end().ends_with(&boot);
            } else if ours && matches!(parts[1].trim(), "ERROR" | "CRITICAL" | "FATAL") {
                let message: String = message.chars().take(240).collect();
                if !errors.contains(&message) { errors.push(message); }
            }
        }
    }
    errors.truncate(3);
    errors
}

type Spawner<'a> = dyn FnMut(&Prepared, &str, &mut SessionLog) -> Result<Injector, String> + 'a;

struct HandoffTiming {
    publication: Duration,
    transfer: Duration,
    to_session: Duration,
    total: Duration,
}

/// Publish capabilities before transferring the session to the verified loader.
fn deliver_handoff(
    log: &mut SessionLog,
    mut publish: impl FnMut(&mut SessionLog),
    send: impl FnOnce(&mut SessionLog) -> Result<Instant, String>,
) -> Result<HandoffTiming, String> {
    let began = Instant::now();
    publish(log);
    let publication = began.elapsed();
    let transfer_started = Instant::now();
    let sent_at = send(log)?;
    Ok(HandoffTiming {
        publication,
        transfer: sent_at.saturating_duration_since(transfer_started),
        to_session: sent_at.saturating_duration_since(began),
        total: began.elapsed(),
    })
}

/// The launch log's opening lines: `first` (the park's `park start` line), else the launch
/// clock and the trigger (07d §4.2), then the launcher image and route.
fn open_launch_log(p: &Prepared, timing: &mut Timeline, first: Option<&str>) -> Result<SessionLog, String> {
    let mut log = SessionLog::open(&p.session.log)?;
    match first {
        Some(line) => log.line(line),
        None => clock_lines(&mut log, timing, p.trigger),
    }
    log.line(&format!("Launcher image: {}; embedded loader SHA-256: {}",
        std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default(),
        tools::embedded::embedded().map_or("none", |(_, hash)| hash)));
    log.line(&format!("POL-less launch through {} in {}; loader {}", p.tool.name(), p.tool_dir.display(), p.loader.display()));
    // The sign-in marks were taken before this log existed; write them now.
    log.timing(timing);
    Ok(log)
}

/// `launch clock …` and, right after it, `launch trigger: <name>` (07d §4.2).
fn clock_lines(log: &mut SessionLog, timing: &Timeline, trigger: crate::launch_timing::LaunchTrigger) {
    for line in launch_clock_lines(timing, trigger) { log.line(&line); }
}

/// The two opening lines of every launch, also appended to an existing session log by the
/// pre-signed paths (main.rs xi-start-game, session_worker::launch_command).
pub(crate) fn launch_clock_lines(timing: &Timeline, trigger: crate::launch_timing::LaunchTrigger) -> [String; 2] {
    let play_clock = timing.start_host_micros();
    [format!("launch clock: play_start_host_us={} host_clock_valid={}", play_clock.unwrap_or_default(), play_clock.is_some()),
        trigger.line()]
}

type SupervisorHandoff<A> = (LoaderPipe, Arc<A>, SessionLog, Instant, String, GameVersionCheck);

/// The session's supervisor half, started before the game so a thread failure can't leave
/// a running game without the keepalive's owner.
struct Supervision<A: SignedIn> {
    auth: Arc<A>,
    game_running: Arc<AtomicBool>,
    log_path: PathBuf,
    sender: std::sync::mpsc::SyncSender<SupervisorHandoff<A>>,
    alive: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<crate::launch_log::ExitReport>>>,
    game_check: GameVersionCheck,
    tool: Tool,
}

fn supervise<A: SignedIn>(p: &Prepared, auth: A) -> Result<Supervision<A>, String> {
    let auth = Arc::new(auth);
    let game_running = auth.game_running().unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
    let (sender, receiver) = std::sync::mpsc::sync_channel::<SupervisorHandoff<A>>(1);
    let alive = Arc::new(AtomicBool::new(true));
    let outcome = Arc::new(Mutex::new(None));
    let (tracked, tracked_outcome, session) = (alive.clone(), outcome.clone(), p.session.clone());
    std::thread::Builder::new()
        .name("retail-loader-session".into())
        .spawn(move || {
            if let Ok((mut pipe, auth, mut log, started, launch_timing, mut game_check)) = receiver.recv() {
                let end = watch(&mut pipe, auth.as_ref(), &mut log, &mut game_check);
                drop(pipe);
                let report = classify_loader_end(&end, started.elapsed());
                log.line(&format!("Session over: {}. Closing the sign-in session.", report.summary));
                session.record(&noted(&report.summary, &launch_timing));
                if let Ok(mut slot) = tracked_outcome.lock() {
                    *slot = Some(report);
                }
                if let Ok(auth) = Arc::try_unwrap(auth) { log.line(&auth.sign_out()); }
            }
            tracked.store(false, Ordering::Release);
        })
        .map_err(|_| t("Could not start the retail session supervisor."))?;
    Ok(Supervision { auth, game_running, log_path: p.session.log.clone(), sender, alive, outcome, game_check: GameVersionCheck::new(p), tool: p.tool })
}

/// A started tool whose loader connected, said hello and runs the expected image.
struct Host {
    /// Declared first, so it drops first: Windower.exe is stopped before the loader's pipe
    /// closes, and its `Core.Stop(save: true)` never rewrites settings.xml (07a).
    injector: LaunchedInjector,
    pipe: LoaderPipe,
    pid: u32,
}

/// Start the tool and wait for its loader's verified hello, holding the tool's turn from
/// spawn to hello (`TurnGate`). Windower's update chain runs in here; any change it made to its
/// own files is noted (07d §4.6).
fn start_host(p: &mut Prepared, log: &mut SessionLog, timing: &mut Timeline, progress: &mut dyn FnMut(&str),
    spawn: &mut Spawner<'_>, turns: &TurnGate<'_>, limits: Timeouts) -> Result<Host, String> {
    let mut never_ran = false;
    let mut result = start_host_once(p, log, timing, progress, spawn, turns, limits, true, &mut never_ran);
    if let Err(error) = &result {
        if never_ran && !limits.aborted() {
            // Ashita's start-up in the host failed before the loader ran, so no credentials
            // were sent: once more, in a turn of its own (a plugin that lost a race to
            // another host's start-up loads alone this time).
            log.line(&format!("{error} Starting {} once more.", p.tool.exe()));
            timing.stage("ashita: game host never ran crystal-loader; starting once more");
            never_ran = false;
            result = start_host_once(p, log, timing, progress, spawn, turns, limits, true, &mut never_ran);
        }
    }
    if let Err(error) = &result {
        if p.private_bootstrap.take().is_some() && !limits.aborted() {
            // No credentials were sent. Drop the snapshot that failed, so this
            // Windower.exe run (and any later launch) supplies a fresh one.
            if let Ok(key) = &p.bootstrap_key { key.discard(); }
            log.line(&format!("Private Windower startup failed ({error}); starting Windower.exe instead, before any credentials are sent."));
            timing.stage("windower: private startup failed; falling back to Windower.exe");
            return start_host_once(p, log, timing, progress, spawn, turns, limits, false, &mut false);
        }
    }
    result
}

/// Today's saved snapshot for this launch's unchanged Windower inputs, if any.
/// `waited`: a second look after the Windower.exe turn, which another launch's
/// Windower.exe run may just have filled.
fn load_windower_snapshot(p: &mut Prepared, log: &mut SessionLog, timing: &mut Timeline, waited: bool) {
    if p.tool != Tool::Windower { return; }
    let key = match &p.bootstrap_key {
        Ok(key) => key,
        Err(reason) => {
            if !waited { log.line(&format!("Windower.exe starts this launch: {reason}.")); }
            return;
        }
    };
    match key.load() {
        Ok(snapshot) => {
            p.private_bootstrap = Some(snapshot);
            timing.stage(if waited { "windower: startup snapshot supplied by another launch" } else { "windower: saved startup snapshot verified" });
        }
        Err(miss) if !waited => {
            log.line(&format!("Windower.exe starts this launch: {}.", miss.reason()));
            timing.stage("windower: no usable startup snapshot");
        }
        Err(_) => {}
    }
}

/// `never_ran`: set when the failure is a game host that never ran the loader after its tool
/// finished (`connect_host`), the one failure `start_host` starts again.
#[allow(clippy::too_many_arguments)]
fn start_host_once(p: &mut Prepared, log: &mut SessionLog, timing: &mut Timeline, progress: &mut dyn FnMut(&str),
    spawn: &mut Spawner<'_>, turns: &TurnGate<'_>, limits: Timeouts, allow_private: bool, never_ran: &mut bool) -> Result<Host, String> {
    if allow_private && p.private_bootstrap.is_none() { load_windower_snapshot(p, log, timing, false); }
    // An Ashita start whose boot ini loads none of `tools::SERIAL_POLPLUGINS` needs no turn either.
    // A private host needs no turn: it never runs Windower.exe, so no
    // launcher, update chain or profile save can collide with another launch.
    let mut turn = if p.private_bootstrap.is_none() && (p.tool != Tool::Ashita || p.serial_start) { turns(p.tool, progress, &|| limits.aborted())? } else { None };
    // Another launch's Windower.exe may have supplied this profile while this
    // one waited. Release the turn before starting a private host.
    if allow_private && p.private_bootstrap.is_none() && p.bootstrap_key.is_ok() {
        load_windower_snapshot(p, log, timing, true);
        if p.private_bootstrap.is_some() { drop(turn.take()); }
    }
    // Windower.exe fills its well-known startup mapping; holding it open here
    // lets this launch keep a copy for the next ones.
    let capture = if p.bootstrap_key.is_ok() && p.private_bootstrap.is_none() { windower_parallel::Capture::new() } else { None };
    // A park closed while it waited for its turn: nothing is started.
    if limits.aborted() { return Err(park::ABORTED.into()); }
    let mut pipe = LoaderPipe::create().map_err(|e| tf("Could not create the session pipe ({error}).", &[("error", &e)]))?;
    progress(&tf("Starting {tool}...", &[("tool", &p.tool.name())]));
    let files = (p.tool == Tool::Windower && p.private_bootstrap.is_none()).then(|| tools::windower_files(&p.tool_dir));
    let spawned = Instant::now();
    let injector = spawn(p, pipe.name(), log)?;
    let mut injector = LaunchedInjector { injector, profile_access: p.profile_access.take(), stop_on_drop: p.tool == Tool::Windower };
    if p.private_bootstrap.is_some(){log.line("Started the game host for private Windower initialization; authentication is waiting.");}
    else{log.line(&format!("Started {}; waiting for crystal-loader on its session pipe.", p.tool.exe()));}
    log.reached(timing, Milestone::ToolStarted);
    progress(t("Waiting for crystal-loader to connect..."));
    let connected = connect_host(p, log, timing, &mut pipe, &mut injector, capture, limits, never_ran);
    let pid = match connected {
        Ok(pid) => pid,
        Err(error) => {
            // Keep the profile guard with the launch: a Windower.exe fallback start must
            // never run without it (`start_host`). The failed host is still stopped on drop.
            p.profile_access = injector.profile_access.take();
            return Err(error);
        }
    };
    if let Some(before) = files {
        let changed = tools::windower_changes(&before, &tools::windower_files(&p.tool_dir));
        if !changed.is_empty() {
            log.line(&format!("Windower updated during its start: {} (hello +{:.2}s)", changed.join(", "), spawned.elapsed().as_secs_f64()));
        }
    }
    // The game's main thread is running, so the tool's handshake is over: the next start
    // may go now, whatever its profile (see `TurnGate`).
    drop(turn);
    Ok(Host { pipe, pid, injector })
}

/// A spawned tool's connect, verified hello and handoff (private Windower readiness, or
/// closing Windower's launcher). Returns the loader's PID.
#[allow(clippy::too_many_arguments)]
fn connect_host(p: &mut Prepared, log: &mut SessionLog, timing: &mut Timeline, pipe: &mut LoaderPipe,
    injector: &mut LaunchedInjector, capture: Option<windower_parallel::Capture>, limits: Timeouts, never_ran: &mut bool) -> Result<u32, String> {
    let tool = p.tool;
    let since = std::time::SystemTime::now();
    // When Ashita-cli was seen to have exited after a successful injection.
    let mut injected: Option<Instant> = None;
    let accepted = pipe.accept(limits.connect, || match injector.injector.exited() {
        _ if limits.aborted() => Err(park::ABORTED.into()),
        Some(Some(code)) if code != 0 => Err(tf("{exe} exited with code {code} before crystal-loader connected.", &[("exe", &tool.exe()), ("code", &code)])),
        Some(None) => Err(tf("{exe} ended before crystal-loader connected.", &[("exe", &tool.exe())])),
        // Live 2026-11-10: Ashita-cli reported "injected and installed" and exited, but a POL
        // plugin threw inside the host's start-up and the host ended without running the
        // loader. Waiting out `connect` (60 s) helped nothing.
        Some(Some(_)) if tool == Tool::Ashita => {
            let at = *injected.get_or_insert_with(Instant::now);
            if at.elapsed() < limits.host_start { return Ok(()); }
            *never_ran = true;
            Err(String::new())
        }
        _ => Ok(()),
    });
    if let Err(error) = accepted {
        // Windower.exe first; the loader it started finds no pipe and exits (07a).
        injector.injector.stop();
        if limits.aborted() { return Err(park::ABORTED.into()); }
        if *never_ran {
            return Err(host_never_ran(p, since, limits.host_start));
        }
        return Err(accept_failure(p, error, limits.connect));
    }
    let pid = pipe.handshake(&p.loader, limits.hello)?;
    crate::footprint::set_host(pid);
    if let Some(snapshot) = &p.private_bootstrap {
        // Hook.dll initializes from the snapshot in the host; the session line
        // (and so any credential) is only sent after `windower_ready`.
        let started = Instant::now();
        let initialized = (|| {
            if !pipe.supports_private_windower() {
                return Err("The selected loader cannot initialize private Windower startup objects.".to_owned());
            }
            let nonce = pipe::random_hex().map_err(|e| e.to_string())?;
            let line = snapshot.line(&p.tool_dir, &nonce[..24]);
            pipe.send(line.as_bytes(), Instant::now() + limits.send).map_err(|_| "Could not send Windower startup configuration.")?;
            let deadline = Instant::now() + Duration::from_secs(35);
            loop {
                match pipe.next_event(Some(deadline)) {
                    Ok(Ok(LoaderEvent::WindowerReady)) => break Ok(()),
                    Ok(Ok(LoaderEvent::Error(error))) => break Err(error),
                    Ok(Ok(LoaderEvent::Closed | LoaderEvent::Exit(_))) => break Err("The private Windower host ended before initialization.".into()),
                    Err(error) => break Err(format!("Private Windower initialization failed: {error}")),
                    Ok(Err(error)) => break Err(error),
                    Ok(Ok(_)) => continue,
                }
            }
        })();
        if let Err(error) = initialized {
            injector.injector.stop();
            return Err(error);
        }
        injector.stop_on_drop = false;
        log.line(&format!("Private Windower startup is ready after {:.2} s; profile data stayed isolated and authentication has not started.",
            started.elapsed().as_secs_f64()));
        timing.stage("windower: private Hook initialization ready");
    } else if p.tool == Tool::Windower {
        // The verified hello runs on the resumed host's main thread, after
        // Windower copied its MMF settings and completed injection. No session
        // has been sent and no game window exists yet. Finish our own launcher
        // while still holding the injection turn, so no delayed Profile.Save
        // can collide with another launch or overwrite a newer profile edit.
        let owned_source = match &mut injector.injector {
            Injector::Child(child) => matches!(child.try_wait(), Ok(None)),
            #[cfg(test)]
            Injector::Fake(exited) => !exited.load(Ordering::SeqCst),
            _ => false,
        };
        injector.injector.finish_windower_handoff()?;
        log.line("Windower injection completed; its launcher closed before the profile-save wait.");
        // An updater may have relaunched Windower. Only the owned launcher we
        // just stopped may supply data; that chain keeps the ordinary path.
        if let (true, Ok(key), Some(bytes)) = (owned_source, &p.bootstrap_key, capture.as_ref().and_then(windower_parallel::Capture::read)) {
            match key.save(&bytes) {
                Ok(()) => log.line("Windower supplied today's protected startup snapshot; later launches with the same unchanged profile start privately."),
                Err(error) => log.line(&error),
            }
        }
        // Our launcher exited before the hello: the relaunched Windower.exe that injected the
        // host still holds its settings snapshot and saves it later. Keep the profile guard
        // until that process exits, or for the whole session when it can't be verified.
        if !owned_source {
            if let Some(guard) = injector.profile_access.take() {
                match relaunched_windower(pid, &p.tool_dir) {
                    Some(relaunched) => {
                        log.line("Windower relaunched itself before the hello; profile editing waits until that Windower.exe exits.");
                        injector.injector = relaunched;
                        injector.profile_access = Some(guard);
                    }
                    None => {
                        log.line("Windower relaunched itself and that process could not be verified; profile editing waits until this session ends.");
                        profile_access::hold_for_session(guard);
                    }
                }
            }
        }
    }
    Ok(pid)
}

/// The Windower.exe that started host `host`, opened for waiting: its parent process, whose
/// image is `folder`\Windower.exe and which started no later than the host.
fn relaunched_windower(host: u32, folder: &Path) -> Option<Injector> {
    use crate::win::process::{self, open_with_created, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    let parent = process::processes()?.into_iter().find(|entry| entry.pid == host)?.parent;
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let (windower, windower_created) = open_with_created(parent, access)?;
    let image = process::image_path_of(&windower).ok()?;
    let started_first = windower_created <= open_with_created(host, access)?.1;
    (started_first && pipe::same_file(&image, &folder.join(Tool::Windower.exe()))).then_some(Injector::Elevated(windower))
}

/// After sign-in: start the injector, hand the loader its session, wait for the game to
/// start, then leave the rest of the session (and the keepalive) to a supervisor thread.
fn run<A: SignedIn>(
    mut p: Prepared,
    auth: A,
    mut timing: Timeline,
    progress: &mut dyn FnMut(&str),
    spawn: &mut Spawner<'_>,
    turns: &TurnGate<'_>,
    limits: Timeouts,
) -> Result<LaunchResult, String> {
    let mut log = open_launch_log(&p, &mut timing, None)?;
    let supervision = supervise(&p, auth)?;
    let auth = supervision.auth.clone();
    let log_path = supervision.log_path.clone();
    let handed = (|| -> Result<(LoaderPipe, u32), String> {
        let Host { mut pipe, pid, injector } = start_host(&mut p, &mut log, &mut timing, progress, spawn, turns, limits)?;
        if !pipe.supports_native_auth() {
            return Err(t("This crystal-loader cannot perform native POL authentication. Use the loader embedded in this launcher, or update the configured loader path.").into());
        }
        if !pipe.supports_native_social() {
            return Err(t("This loader cannot retain a native PlayOnline session. Use the loader embedded in this launcher or update the configured loader path.").into());
        }
        // An older loader would
        // open :51240 with no :54000 in front of it, a shape stock never sends
        // (07a w2u-at-connect). Refuse before sending any credentials.
        if !pipe.supports_viewer_check() {
            return Err(t("This loader does not run the PlayOnline Viewer version check. Use the loader embedded in this launcher or update the configured loader path.").into());
        }
        log.line(&format!("crystal-loader connected: PID {pid}, image verified."));
        log.reached(&mut timing, Milestone::Hello);
        let native_batch = pipe.supports_native_batch();
        let measured = deliver_handoff(&mut log, |log| publish_batch_capability(&p, native_batch, log), |log| {
            if p.tool == Tool::Windower && p.private_bootstrap.is_none() {
                let hello = Instant::now();
                observe_windower_exit(injector, hello, hello + limits.injector_exit, log_path.clone())?;
            }
            progress(t("Handing the session to the game..."));
            let line = auth.session_line(&p.install_dir)?;
            let line = match &p.ffxi_settings {
                Some(settings) => {
                    note_ffxi_settings(&p, settings, log);
                    ffxi_settings::splice(line, &settings.json)?
                }
                None => line,
            };
            let sent = pipe.send(&line, Instant::now() + limits.send);
            drop(line);
            sent.map_err(|_| t("crystal-loader did not accept the session.").to_owned())?;
            let sent_at = Instant::now();
            log.line("Session sent; launcher copy cleared.");
            log.reached(&mut timing, Milestone::SessionSent);
            Ok(sent_at)
        })?;
        if cfg!(feature = "launcher-startup-diagnostics") {
            log.line(&format!("startup handoff: worker={} loader={pid} publication_us={} transfer_us={} to_session_us={} total_us={} end_host_us={}",
                std::process::id(), measured.publication.as_micros(),
                measured.transfer.as_micros(), measured.to_session.as_micros(), measured.total.as_micros(),
                crate::launch_timing::diagnostic_host_micros().unwrap_or(0)));
        }
        Ok((pipe, pid))
    })();
    drop(auth);
    let (pipe, pid) = handed.map_err(|message| fail(&mut log, &log_path, message))?;
    finish(supervision, log, pipe, pid, timing, progress, limits)
}

/// The loader's batch-start capability, published before its session goes out. A
/// status-cache failure stays nonfatal to the native session.
fn publish_batch_capability(p: &Prepared, native_batch: bool, log: &mut SessionLog) {
    if let Some(directory) = &p.friends_directory {
        if let Err(error) = crate::session_worker::publish_native_batch(directory, native_batch) {
            log.line(&error);
        }
    }
}

fn note_ffxi_settings(p: &Prepared, settings: &ffxi_settings::WindowerSettings, log: &mut SessionLog) {
    let source = if p.tool == Tool::Windower { "Windower profile" } else { "Crystal" };
    log.line(&format!("The game gets {} {source} settings in-process.", settings.count));
    if let Some(note) = &settings.note {
        log.line(note);
    }
}

/// Log a launch failure and point it at the log.
fn fail(log: &mut SessionLog, log_path: &Path, message: String) -> String {
    log.line(&message);
    with_log(message, log_path)
}

/// The session line (or a parked host's credentials) went out: wait for the sign-in and
/// the game start, then hand the session to its supervisor.
fn finish<A: SignedIn>(supervision: Supervision<A>, mut log: SessionLog, mut pipe: LoaderPipe, pid: u32, mut timing: Timeline,
    progress: &mut dyn FnMut(&str), limits: Timeouts) -> Result<LaunchResult, String> {
    let Supervision { auth, game_running, log_path, sender, alive, outcome, mut game_check, tool } = supervision;
    // From here on a command writer's wake interrupts the reads (start wait and watch).
    pipe.listen_for_commands(auth.native_command_listener());
    let started = Instant::now();
    let deferred = auth.start_pending();
    progress(if deferred { t("Signing in to PlayOnline...") } else { t("Waiting for the game to start...") });
    // The loader went on after an unanswered W2U and then failed: say why the W2U failed
    // too (07d §5.6 step 4).
    let ended = |log: &mut SessionLog, end: LoaderEnd, cause: Option<&str>| {
        let report = classify_loader_end(&end, started.elapsed());
        let mut message = tf("{summary} ({tool} via crystal-loader).", &[("summary", &report.message()), ("tool", &tool.name())]);
        if let Some(cause) = cause {
            message.push(' ');
            message.push_str(&tf("The PlayOnline version check also failed: {cause}", &[("cause", &cause)]));
        }
        fail(log, &log_path, message)
    };
    let mut confirmed = match wait_started(&mut pipe, auth.as_ref(), &mut log, limits.start, &mut timing, progress, false, &mut game_check) {
        Ok(confirmed) => confirmed,
        Err(end) => return Err(ended(&mut log, end, game_check.viewer_cause.as_deref())),
    };
    // Full native idle is required even if the version check, started at the main menu,
    // finished earlier.
    let mut blocked = None;
    if deferred && confirmed {
        match start_deferred(&mut pipe, auth.as_ref(), &mut log, &mut game_check, progress, Some(&mut timing)) {
            Ok(Ok(())) => {
                confirmed = match wait_started(&mut pipe, auth.as_ref(), &mut log, limits.start, &mut timing, progress, true, &mut game_check) {
                    Ok(confirmed) => confirmed,
                    Err(end) => return Err(ended(&mut log, end, None)),
                };
            }
            Ok(Err(message)) => blocked = Some(message),
            Err(()) => return Err(fail(&mut log, &log_path, t("crystal-loader did not accept the game start.").into())),
        }
    }
    if confirmed && blocked.is_none() && (deferred || !auth.social_only()) {
        log.reached(&mut timing, Milestone::GameStarted);
    }
    let detail = if let Some(message) = &blocked {
        message.clone()
    } else if auth.social_only() && confirmed {
        with_log(tf("Signed in to PlayOnline. Friends are available; start FFXI from this session without another login. Host PID {pid}.", &[("pid", &pid)]), &log_path)
    } else if confirmed {
        with_log(
            tf("Retail authentication completed. Game PID {pid} started through {tool}.", &[("pid", &pid), ("tool", &tool.name())]),
            &log_path,
        )
    } else {
        with_log(
            tf("Game PID {pid} is loading through {tool}; start is not yet confirmed.", &[("pid", &pid), ("tool", &tool.name())]),
            &log_path,
        )
    };
    let (summary, row_timing) = (timing.summary(), timing.row_summary());
    if sender.send((pipe, auth, log, started, summary, game_check)).is_err() {
        return Err(t("The retail session supervisor could not accept the game session.").into());
    }
    Ok(LaunchResult { pid, detail, alive, game_running, log: log_path, outcome, timing: row_timing })
}

/// The Settings page's view of the tools: what was found where.
#[derive(Clone, Default, Debug)]
pub(crate) struct Detected {
    pub ashita: Option<PathBuf>,
    pub windower: Option<PathBuf>,
    pub loader: Option<PathBuf>,
    pub boot_inis: Vec<String>,
    pub profiles: Vec<String>,
}

pub(crate) fn detect_all(settings: &ToolSettings) -> Detected {
    let ashita = tools::detect(Tool::Ashita, &settings.ashita);
    let windower = tools::detect(Tool::Windower, &settings.windower);
    Detected {
        boot_inis: ashita.as_deref().map(tools::boot_inis).unwrap_or_default(),
        profiles: windower.as_deref().map(tools::windower_profiles).unwrap_or_default(),
        loader: tools::loader_path(&settings.loader),
        ashita,
        windower,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pol-spec 07c §1: the launcher's answer, as crystal-loader proto.rs parses it.
    #[test]
    fn viewer_check_answers_follow_the_round1_contract() {
        use crate::viewer::ConnectVerdict;
        let parse = |line: String| { assert!(line.ends_with('\n') && !line.trim_end().contains('\n')); serde_json::from_str::<serde_json::Value>(&line).unwrap() };
        assert_eq!(parse(viewer_check_result_line(&ConnectVerdict::Current { version: "20110829_E".into() })),
            serde_json::json!({"type":"native_viewer_check_result","ok":true}));
        assert_eq!(parse(viewer_check_result_line(&ConnectVerdict::Unreachable("timed out\nagain \"x\"".into()))),
            serde_json::json!({"type":"native_viewer_check_result","ok":false,"reason":"unreachable","error":"timed out\nagain \"x\""}));
        assert_eq!(parse(viewer_check_result_line(&ConnectVerdict::Failed("no Viewer".into())))["reason"], "failed");
        let outdated = parse(viewer_check_result_line(&ConnectVerdict::Outdated { local: "20110829_E".into(), remote: "20261001_0".into() }));
        assert_eq!(outdated["reason"], "outdated");
        assert!(outdated["error"].as_str().unwrap().contains("20261001_0"));
    }

    #[test]
    fn status_controls_carry_the_groups_choice() {
        use crate::session_worker::NativeControl;
        assert_eq!(native_control_line(NativeControl::Visible, true), "{\"type\":\"native_visible\",\"groups\":true}\n");
        assert_eq!(native_control_line(NativeControl::Away, false), "{\"type\":\"native_away\",\"groups\":false}\n");
        assert_eq!(native_control_line(NativeControl::Invisible, true), "{\"type\":\"native_invisible\",\"groups\":true}\n");
        assert_eq!(native_control_line(NativeControl::SignOut, true), "{\"type\":\"native_sign_out\"}\n");
        assert_eq!(native_control_line(NativeControl::StartGame, false), "{\"type\":\"native_start_game\"}\n");
        assert_eq!(native_control_line(NativeControl::Portrait(301), true), "{\"type\":\"native_portrait\",\"portrait\":301}\n");
    }

    #[test]
    fn native_close_requires_completed_sign_out_and_preserves_failures() {
        struct Auth;
        impl SignedIn for Auth {
            fn session_line(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
            fn credentials_line(&self) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
        }
        let end = |events: Vec<LoaderEvent>| {
            // Diagnostics still know the acknowledged intent when log I/O is unavailable.
            let mut log = SessionLog::default();
            for event in events {
                if let Step::End(end) = handle_event(event, &Auth, &mut log) {
                    return end;
                }
            }
            panic!("fixture did not report an end");
        };
        let idle = || LoaderEvent::NativeSessionState(false);
        let sign_out = |code| LoaderEvent::NativeControlResult { action: "sign_out".into(), code };
        let completed = |code| pipe::parse_event(format!("{{\"type\":\"native_sign_out_complete\",\"code\":{code}}}").as_bytes()).unwrap();
        assert_eq!(end(vec![idle(), sign_out(0), completed(0), LoaderEvent::Exit(0)]), LoaderEnd::SignedOut);
        assert_eq!(end(vec![completed(0), LoaderEvent::Exit(0)]), LoaderEnd::SignedOut);

        for events in [
            vec![LoaderEvent::Exit(0)],
            vec![idle(), LoaderEvent::Exit(0)],
            vec![sign_out(0), LoaderEvent::Exit(0)],
            vec![sign_out(0), idle(), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(-256), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), sign_out(-256), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::NativeSessionState(true), LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::Started { pid: None }, LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::NativeControlResult { action: "start_game".into(), code: 0 }, LoaderEvent::Exit(0)],
            vec![idle(), sign_out(0), LoaderEvent::NativeStatus(-1), LoaderEvent::Exit(0)],
        ] {
            assert_eq!(end(events), LoaderEnd::Exit(0));
        }
        assert_eq!(end(vec![idle(), sign_out(0), LoaderEvent::Exit(-14)]), LoaderEnd::Exit(-14));
        let started = || LoaderEvent::Started { pid: Some(std::process::id()) };
        let returned = |successful| LoaderEvent::NativeGameEnded { code: -14, game_returned: successful, game_id: 1 };
        // A direct launch never returns to idle or submits a sign-out command.
        assert_eq!(end(vec![LoaderEvent::NativeSessionState(true), started(), returned(true), completed(0), LoaderEvent::Exit(-14)]), LoaderEnd::SignedOut);
        assert_eq!(end(vec![started(), returned(true), completed(-1038), LoaderEvent::Exit(-14)]), LoaderEnd::Exit(-14));
        assert_eq!(end(vec![started(), returned(true), completed(-2046), LoaderEvent::Exit(-14)]), LoaderEnd::Exit(-14));
        assert_eq!(end(vec![started(), returned(true), completed(0), LoaderEvent::Exit(-1025)]), LoaderEnd::Exit(-1025));
        for events in [
            vec![returned(true), completed(0), LoaderEvent::Exit(-14)],
            vec![started(), returned(false), completed(0), LoaderEvent::Exit(-14)],
            vec![started(), returned(true), idle(), sign_out(-256), LoaderEvent::Exit(-14)],
            vec![started(), LoaderEvent::NativeStatus(-1025), returned(true), completed(0), LoaderEvent::Exit(-14)],
            vec![started(), returned(true), idle(), LoaderEvent::Exit(-14)],
        ] { assert_eq!(end(events), LoaderEnd::Exit(-14)); }
        // A connection that recovered, or a failed presence change, still closes normally.
        let visible = |code| LoaderEvent::NativeControlResult { action: "visible".into(), code };
        for events in [
            vec![started(), LoaderEvent::NativeStatus(-1025), LoaderEvent::NativeStatus(0), returned(true), completed(0), LoaderEvent::Exit(-14)],
            vec![started(), visible(-2046), returned(true), completed(0), LoaderEvent::Exit(-14)],
        ] { assert_eq!(end(events), LoaderEnd::SignedOut); }
        assert_eq!(end(vec![started(), LoaderEvent::NativeControlResult { action: "start_game".into(), code: -2046 },
            returned(true), completed(0), LoaderEvent::Exit(-14)]), LoaderEnd::Exit(-14));
        assert_eq!(end(vec![idle(), sign_out(0), LoaderEvent::Closed]), LoaderEnd::Closed);
        assert_eq!(end(vec![idle(), sign_out(0), LoaderEvent::Error("sign-out failed".into())]), LoaderEnd::Error("sign-out failed".into()));
    }

    /// Finding: when Windower's updater relaunched it, the profile guard must follow the
    /// relaunched Windower.exe (the host's parent), never a process outside the tool folder.
    #[test]
    fn a_relaunched_windower_is_found_only_as_the_hosts_verified_parent() {
        use std::os::windows::{io::{FromRawHandle, OwnedHandle}, process::CommandExt};
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, CREATE_NO_WINDOW, PROCESS_TERMINATE};
        let dir = std::env::temp_dir().join(format!("crystal-relaunched-{}", unique_id().unwrap()));
        let other = dir.join("other");
        fs::create_dir_all(&other).unwrap();
        // A stand-in Windower.exe (a copy of cmd) that starts the "host" (ping) as its child.
        fs::copy(r"C:\Windows\System32\cmd.exe", dir.join("Windower.exe")).unwrap();
        let mut windower = std::process::Command::new(dir.join("Windower.exe")).args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .creation_flags(CREATE_NO_WINDOW).spawn().unwrap();
        let child_of = |parent: u32| {
            crate::win::process::processes()?.into_iter()
                .find(|entry| entry.parent == parent && entry.name.eq_ignore_ascii_case("PING.EXE"))
                .map(|entry| entry.pid)
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let host = loop {
            if let Some(pid) = child_of(windower.id()) { break pid; }
            assert!(Instant::now() < deadline, "the stand-in host never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        let found = relaunched_windower(host, &dir);
        let wrong_folder = relaunched_windower(host, &other);
        let not_windower = relaunched_windower(std::process::id(), &dir);
        unsafe {
            let process = OpenProcess(PROCESS_TERMINATE, 0, host);
            if !process.is_null() { TerminateProcess(process, 1); drop(OwnedHandle::from_raw_handle(process as _)); }
        }
        let _ = windower.kill();
        let _ = windower.wait();
        let Some(mut found) = found else { panic!("the host's verified parent is the relaunched Windower.exe") };
        assert!(wrong_folder.is_none() && not_windower.is_none());
        assert!(found.wait().is_ok() && found.exited().is_some(), "waitable until that Windower.exe exits");
        let _ = fs::remove_dir_all(dir);
    }

    use std::io::{BufRead, BufReader, Read};
    use std::sync::atomic::AtomicUsize;

    /// A native sign-in whose session line carries a recognizable stand-in secret.
    struct FakeAuth {
        dropped: Arc<AtomicBool>,
    }
    impl FakeAuth {
        fn new() -> Self { Self { dropped: Arc::new(AtomicBool::new(false)) } }
    }
    impl SignedIn for FakeAuth {
        fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
            let mut line = Zeroizing::new(Vec::with_capacity(256));
            line.extend_from_slice(format!("{{\"type\":\"session\",\"v\":1,\"install_dir\":{},\"v_key\":\"",
                serde_json::to_string(install_dir.to_str().unwrap()).unwrap()).as_bytes());
            pipe::base64_into(&mut line, &[7; 16]);
            line.extend_from_slice(b"\"}\n");
            Ok(line)
        }
        fn credentials_line(&self) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
    }
    impl Drop for FakeAuth {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// The hello of a loader with every capability a cold launch requires.
    fn native_hello() -> String {
        format!(r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true}}"#, std::process::id())
    }

    fn fixture(tool: Tool) -> (Prepared, PathBuf) {
        let dir = std::env::temp_dir().join(format!("xi-pol-less-{}", unique_id().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let prepared = Prepared {
            tool,
            tool_dir: dir.clone(),
            loader: std::env::current_exe().unwrap(),
            install_dir: PathBuf::from(r"C:\FFXI"),
            ini_name: "crystal-test.ini".into(),
            base_ini: None,
            serial_start: true,
            profile: String::new(),
            ffxi_settings: None,
            profile_access: None,
            session: Session { history: dir.join("history.log"), label: "Fake".into(), log: dir.join("a.crystal.log") },
            friends_directory: None,
            check: |_| (Ok("Game version matches.".into()), Default::default()),
            viewer_check: || panic!("this fixture loader never asks for the Viewer check"),
            bootstrap_key: Err("fixture"),
            private_bootstrap: None,
            trigger: crate::launch_timing::LaunchTrigger::Play,
        };
        (prepared, dir)
    }

    const FAST: Timeouts = Timeouts {
        connect: Duration::from_millis(400),
        host_start: Duration::from_millis(150),
        hello: Duration::from_secs(5),
        send: Duration::from_secs(5),
        start: Duration::from_secs(5),
        injector_exit: Duration::from_millis(10),
        abort: None,
    };

    #[test]
    fn startup_sign_out_is_forwarded_before_idle_only_to_a_capable_sign_in_only_host() {
        struct StartingAuth { social: bool, pending: AtomicBool }
        impl SignedIn for StartingAuth {
            fn social_only(&self) -> bool { self.social }
            fn idle_first(&self) -> bool { true }
            fn take_startup_sign_out(&self) -> bool { self.pending.swap(false, Ordering::AcqRel) }
            fn session_line(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
            fn credentials_line(&self) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
        }
        for (capable, social, until_game) in [(true, true, false), (false, true, false), (true, false, false), (true, true, true)] {
            let (prepared, dir) = fixture(Tool::Vanilla);
            let mut log = SessionLog::open(&prepared.session.log).unwrap();
            let mut pipe = LoaderPipe::create().unwrap();
            let name = pipe.name().to_owned();
            let forward = capable && social && !until_game;
            let loader = std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                let mut writer = file.try_clone().unwrap();
                writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"startup_sign_out":{capable}}}"#,
                    std::process::id()).unwrap();
                if forward {
                    let mut command = String::new();
                    BufReader::new(file).read_line(&mut command).unwrap();
                    assert_eq!(command, "{\"type\":\"native_sign_out\"}\n");
                    writeln!(writer, r#"{{"type":"error","message":"synthetic sign-in cancelled"}}"#).unwrap();
                } else {
                    writeln!(writer, r#"{{"type":"started"}}"#).unwrap();
                }
            });
            pipe.accept(FAST.hello, || Ok(())).unwrap();
            pipe.handshake(&prepared.loader, FAST.hello).unwrap();
            let auth = StartingAuth { social, pending: AtomicBool::new(true) };
            let mut game = GameVersionCheck::new(&prepared);
            let result = wait_started(&mut pipe, &auth, &mut log, FAST.start, &mut Timeline::begin(), &mut |_| {}, until_game, &mut game);
            if forward {
                assert!(matches!(result, Err(LoaderEnd::Error(_))));
            } else {
                assert_eq!(result, Ok(true));
            }
            assert_eq!(auth.pending.load(Ordering::Acquire), !forward);
            loader.join().unwrap();
            drop(pipe);
            drop(log);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn handoff_publishes_before_transferring_to_the_verified_loader() {
        use std::sync::mpsc;
        let (prepared, dir) = fixture(Tool::Ashita);
        let mut log = SessionLog::open(&prepared.session.log).unwrap();
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let (received_tx, received_rx) = mpsc::channel();
        let loader = std::thread::spawn(move || {
            let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
            let mut writer = file.try_clone().unwrap();
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true}}"#,
                std::process::id()).unwrap();
            let mut session = String::new();
            BufReader::new(file).read_line(&mut session).unwrap();
            received_tx.send(session).unwrap();
            // Early progress must remain readable after the durable publication.
            writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
        });
        pipe.accept(FAST.hello, || Ok(())).unwrap();
        assert_eq!(pipe.handshake(&prepared.loader, FAST.hello).unwrap(), std::process::id());
        assert!(pipe.supports_native_auth() && pipe.supports_native_social() && pipe.supports_viewer_check());
        let (publishing_tx, publishing_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let measured = std::thread::scope(|scope| {
            let (pipe_ref, log_ref) = (&mut pipe, &mut log);
            let handoff = scope.spawn(move || deliver_handoff(log_ref, |_| {
                publishing_tx.send(()).unwrap();
                release_rx.recv_timeout(FAST.hello).unwrap();
            }, |_| {
                let line = Zeroizing::new(b"private synthetic session\n".to_vec());
                let sent = pipe_ref.send(&line, Instant::now() + FAST.send);
                drop(line);
                sent.map_err(|error| error.to_string())?;
                Ok(Instant::now())
            }));
            publishing_rx.recv_timeout(FAST.hello).unwrap();
            let early = received_rx.recv_timeout(Duration::from_millis(30));
            // Always release the private publisher before assertions or joining.
            release_tx.send(()).unwrap();
            assert!(early.is_err(), "the session must wait for capability publication");
            let received = early.unwrap_or_else(|_| received_rx.recv_timeout(FAST.hello).unwrap());
            assert_eq!(received, "private synthetic session\n");
            handoff.join().unwrap().unwrap()
        });
        loader.join().unwrap();
        assert!(matches!(pipe.next_event(Some(Instant::now() + FAST.hello)).unwrap().unwrap(), LoaderEvent::Authenticated));
        assert!(measured.total >= measured.publication);
        assert!(!fs::read_to_string(&prepared.session.log).unwrap().contains("private synthetic session"));
        drop(log);
        drop(pipe);
        fs::remove_dir_all(dir).unwrap();
    }


    #[test]
    fn handoff_reports_failed_sends_and_keeps_publication_failure_nonfatal() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let mut log = SessionLog::open(&prepared.session.log).unwrap();
        let mut published = false;
        let error = deliver_handoff(&mut log, |_| published = true,
            |_| Err("private send failed".into())).err().unwrap();
        assert_eq!(error, "private send failed");
        assert!(published, "the capability is published before the transfer attempt");

        let directory = dir.join("unavailable-status");
        // There is no worker record: the actual publication API must fail, but a
        // successful handoff still owns its session and retains the error in the log.
        let result = deliver_handoff(&mut log, |log| {
            let error = crate::session_worker::publish_native_batch(&directory, true).unwrap_err();
            log.line(&error);
        }, |_| Ok(Instant::now()));
        assert!(result.is_ok());
        assert!(fs::read_to_string(&prepared.session.log).unwrap().contains("session status"));
        drop(log);
        fs::remove_dir_all(dir).unwrap();
    }

    fn wait_for(flag: &AtomicBool, value: bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while flag.load(Ordering::SeqCst) != value {
            assert!(Instant::now() < end, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The file's text once it contains `what` (another thread writes it).
    fn path_wait(path: &Path, what: &str) -> String {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let text = fs::read_to_string(path).unwrap_or_default();
            if text.contains(what) {
                return text;
            }
            assert!(Instant::now() < end, "{what} never appeared: {text}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A gate of its own, so parallel tests don't wait on each other's turns.
    fn test_turns() -> &'static TurnGate<'static> {
        gate_of(Box::leak(Box::new(turns::Turns::new())))
    }

    /// `turns` as a start gate.
    fn gate_of(turns: &'static turns::Turns) -> &'static TurnGate<'static> {
        Box::leak(Box::new(move |tool, progress: &mut dyn FnMut(&str), _: &dyn Fn() -> bool| Ok(Some(Box::new(turns.enter(tool, progress)) as Box<dyn Send>))))
    }

    /// Counts how many of something are in progress at once, and the most ever seen.
    #[derive(Default)]
    struct Overlap {
        now: AtomicUsize,
        most: AtomicUsize,
    }
    impl Overlap {
        fn begin(&self) {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
        }
        fn end(&self) {
            self.now.fetch_sub(1, Ordering::SeqCst);
        }
        fn most(&self) -> usize {
            self.most.load(Ordering::SeqCst)
        }
    }

    /// Launch `count` accounts at once through `tool`, the way the app does (one worker per
    /// account), with a fake tool whose loader says hello after `handshake`. Returns the most
    /// tool handshakes at once.
    fn launch_batch(tool: Tool, count: usize, handshake: Duration) -> usize {
        let turns = test_turns();
        let shaking = Arc::new(Overlap::default());
        let done = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..count)
            .map(|_| {
                let (shaking, done) = (shaking.clone(), done.clone());
                std::thread::spawn(move || {
                    let (prepared, dir) = fixture(tool);
                    let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                        shaking.begin();
                        let (name, shaking, done) = (pipe.to_owned(), shaking.clone(), done.clone());
                        std::thread::spawn(move || {
                            std::thread::sleep(handshake);
                            let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                            let mut writer = file.try_clone().unwrap();
                            // The handshake ends with the hello: count it over before sending it.
                            shaking.end();
                            writeln!(writer, "{}", native_hello()).unwrap();
                            let mut session = String::new();
                            BufReader::new(file).read_line(&mut session).unwrap();
                            writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                            while !done.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                        });
                        Ok(Injector::Detached)
                    };
                    let result = run(prepared, FakeAuth::new(), Timeline::begin(), &mut |_| {}, &mut spawn, turns, FAST);
                    (result, dir)
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        done.store(true, Ordering::SeqCst);
        for (result, dir) in results {
            let result = result.unwrap();
            wait_for(&result.alive, false);
            let _ = fs::remove_dir_all(dir);
        }
        shaking.most()
    }

    /// `launch_batch` gives every account the same (default) profile; profiles no longer change
    /// the order (see `another_windower_profile_starts_at_the_previous_hello`).
    #[test]
    fn launch_all_with_windower_hands_over_one_at_a_time() {
        let started = Instant::now();
        let handshakes = launch_batch(Tool::Windower, 4, Duration::from_millis(150));
        assert_eq!(handshakes, 1, "Windower handshakes never overlap");
        // Each turn ends at the hello: about 4 x 0.15 s, where holding the turn past the
        // hello would take far longer.
        assert!(started.elapsed() < Duration::from_millis(1600), "{:?}", started.elapsed());
    }

    #[test]
    fn launch_all_direct_starts_every_host_at_once() {
        // The fake loader connects within FAST's 400 ms connect bound.
        let handshakes = launch_batch(Tool::Vanilla, 3, Duration::from_millis(300));
        assert_eq!(handshakes, 3, "direct starts don't wait for each other");
    }

    /// Live 2026-11-10: Ashita's POL plugins load inside each host's start-up and share files
    /// (pivot.ini); five at once made pivot throw in two hosts. One start at a time, each turn
    /// ending at its hello.
    #[test]
    fn launch_all_with_ashita_hands_over_one_at_a_time() {
        let started = Instant::now();
        let handshakes = launch_batch(Tool::Ashita, 4, Duration::from_millis(150));
        assert_eq!(handshakes, 1, "Ashita host start-ups never overlap");
        assert!(started.elapsed() < Duration::from_millis(1600), "{:?}", started.elapsed());
    }

    /// Ashita-cli reported a good injection and exited, and the host never connected: the start
    /// fails at `host_start`, not at `connect`, with what Ashita's log says about this host, and
    /// is started once more. A second start that connects signs in.
    #[test]
    fn ashita_host_that_never_runs_the_loader_fails_fast_and_starts_once_more() {
        for second_connects in [true, false] {
            let (prepared, dir) = fixture(Tool::Ashita);
            let logs = dir.join("logs");
            fs::create_dir_all(&logs).unwrap();
            let spawns = Arc::new(AtomicUsize::new(0));
            let done = Arc::new(AtomicBool::new(false));
            let (counted, log_dir, finished) = (spawns.clone(), logs.clone(), done.clone());
            let mut spawn = move |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                let attempt = counted.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 || !second_connects {
                    // Two hosts in one log (same second): another account's start-up, then ours.
                    let text = [
                        "11/10/2026 08:27:19.450 | DEBUG    |                                      InstallAshita | Installing Ashita into the current process.",
                        "11/10/2026 08:27:19.450 | DEBUG    |                             AshitaCore::Initialize |  - Ashita Config Script: \\boot\\crystal-other.ini",
                        "11/10/2026 08:27:19.456 | ERROR    |                             PolPluginManager::Load | Failed to load POL plugin 'other'.",
                        "11/10/2026 08:27:19.450 | DEBUG    |                                      InstallAshita | Installing Ashita into the current process.",
                        "11/10/2026 08:27:19.450 | DEBUG    |                             AshitaCore::Initialize |  - Ashita Config Script: \\boot\\crystal-test.ini",
                        "11/10/2026 08:27:19.456 | ERROR    |                             PolPluginManager::Load | Failed to load POL plugin 'pivot'; POL plugin threw an unknown exception.",
                    ].join("\r\n");
                    fs::write(log_dir.join(format!("attempt {attempt}.txt")), text).unwrap();
                    return Ok(Injector::Fake(Arc::new(AtomicBool::new(true))));
                }
                let (name, finished) = (pipe.to_owned(), finished.clone());
                std::thread::spawn(move || {
                    let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                    let mut writer = file.try_clone().unwrap();
                    writeln!(writer, "{}", native_hello()).unwrap();
                    let mut session = String::new();
                    BufReader::new(file).read_line(&mut session).unwrap();
                    writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                    while !finished.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(20)); }
                    writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                });
                Ok(Injector::Fake(Arc::new(AtomicBool::new(true))))
            };
            let started = Instant::now();
            let slow = Timeouts { connect: Duration::from_secs(30), ..FAST };
            let result = run(prepared, FakeAuth::new(), Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), slow);
            assert!(started.elapsed() < Duration::from_secs(5), "failed at host_start, not at connect: {:?}", started.elapsed());
            assert_eq!(spawns.load(Ordering::SeqCst), 2, "started once more, and only once");
            let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
            assert!(log.contains("did not run crystal-loader") && log.contains("Starting Ashita-cli.exe once more."), "{log}");
            match result {
                Ok(result) => {
                    assert!(second_connects);
                    done.store(true, Ordering::SeqCst);
                    wait_for(&result.alive, false);
                }
                Err(error) => {
                    assert!(!second_connects, "{error}");
                    assert!(error.contains("Ashita-cli.exe finished, but the game host it started did not run crystal-loader within 0 s."), "{error}");
                    assert!(error.contains("Ashita's log: Failed to load POL plugin 'pivot'; POL plugin threw an unknown exception."), "{error}");
                    assert!(!error.contains("'other'"), "another host's start-up is not this one's: {error}");
                    assert!(!error.contains("administrator prompt"), "{error}");
                }
            }
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn ashita_start_errors_without_a_log_point_at_the_folder() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let message = host_never_ran(&prepared, std::time::SystemTime::now(), Duration::from_secs(8));
        assert!(message.contains("within 8 s. Check Ashita's log in "), "{message}");
        assert!(ashita_start_errors(&dir.join("logs"), "crystal-test.ini", std::time::SystemTime::now()).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    /// Each game gets its own profile's settings in-process. Verified bootstrap
    /// handoffs close before releasing the next start, while both games continue.
    #[test]
    fn another_windower_profile_starts_at_the_previous_hello() {
        let turns = test_turns();
        let windower_exited = Arc::new(AtomicBool::new(false));
        let first_done = Arc::new(AtomicBool::new(false));
        let slow_exit = Timeouts { injector_exit: Duration::from_secs(20), ..FAST };
        let (session_tx, session_rx) = std::sync::mpsc::channel::<(String, String)>();
        let start = |profile: &'static str, width: u32, exited: Arc<AtomicBool>, done: Arc<AtomicBool>| {
            let session_tx = session_tx.clone();
            std::thread::spawn(move || {
                let (mut prepared, dir) = fixture(Tool::Windower);
                prepared.profile = profile.into();
                let values = [("0001", ffxi_settings::Value::Dword(width))];
                prepared.ffxi_settings = Some(ffxi_settings::WindowerSettings { json: ffxi_settings::to_json(&values), count: 1, note: None });
                let auth = FakeAuth::new();
                let spawned = Arc::new(Mutex::new(None::<Instant>));
                let when = spawned.clone();
                let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                    *when.lock().unwrap() = Some(Instant::now());
                    let (name, done, session_tx) = (pipe.to_owned(), done.clone(), session_tx.clone());
                    std::thread::spawn(move || {
                        let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                        let mut writer = file.try_clone().unwrap();
                        writeln!(writer, "{}", native_hello()).unwrap();
                        let mut session = String::new();
                        BufReader::new(file).read_line(&mut session).unwrap();
                        session_tx.send((profile.to_owned(), session)).unwrap();
                        writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                        while !done.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                    });
                    Ok(Injector::Fake(exited.clone()))
                };
                let result = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, turns, slow_exit).unwrap();
                let spawned = spawned.lock().unwrap().unwrap();
                (result, spawned, dir)
            })
        };
        let began = Instant::now();
        let first = start("Main", 1920, windower_exited.clone(), first_done.clone());
        let (first_result, _, first_dir) = first.join().unwrap();
        let first_hello = Instant::now();
        let (second_done, second_exited) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let second = start("Mule", 1280, second_exited.clone(), second_done.clone());
        let (second_result, second_spawned, second_dir) = second.join().unwrap();
        // The injected hosts continue after both bootstrap processes close.
        // A second profile never waits for the first game or its shutdown.
        assert!(windower_exited.load(Ordering::SeqCst));
        assert!(second_exited.load(Ordering::SeqCst));
        assert!(second_spawned.duration_since(first_hello) < Duration::from_secs(3), "{:?}", second_spawned.duration_since(first_hello));
        assert!(began.elapsed() < Duration::from_secs(10));
        assert!(first_result.alive.load(Ordering::SeqCst) && second_result.alive.load(Ordering::SeqCst));
        // Each game got its own profile's values.
        let mut sessions: Vec<(String, serde_json::Value)> =
            session_rx.try_iter().map(|(p, s)| (p, serde_json::from_str(&s).unwrap())).collect();
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sessions.len(), 2);
        assert_eq!((sessions[0].0.as_str(), &sessions[0].1["ffxi_settings"]), ("Main", &serde_json::json!({"0001": 1920})));
        assert_eq!((sessions[1].0.as_str(), &sessions[1].1["ffxi_settings"]), ("Mule", &serde_json::json!({"0001": 1280})));
        assert_eq!(sessions[0].1["v_key"], sessions[1].1["v_key"], "the rest of the line is unchanged");
        // The exit is still noted in the launch log, for timing evidence.
        let first_log = path_wait(&first_dir.join("a.crystal.log"), "Windower.exe exited ");
        assert!(first_log.contains(" s after the loader's hello."), "{first_log}");
        assert!(first_log.contains("1 Windower profile settings in-process"), "{first_log}");
        for (result, done, dir) in [(first_result, first_done, first_dir), (second_result, second_done, second_dir)] {
            done.store(true, Ordering::SeqCst);
            wait_for(&result.alive, false);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn completed_windower_handoff_closes_only_the_bootstrap_before_sending_the_session() {
        use std::os::windows::{io::{AsRawHandle, FromRawHandle, OwnedHandle}, process::CommandExt};
        use std::process::{Command, Stdio};
        use windows_sys::Win32::{Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::{GetCurrentProcess, WaitForSingleObject, CREATE_NO_WINDOW}};
        let (prepared, dir) = fixture(Tool::Windower);
        let child = Command::new("sort.exe").stdin(Stdio::piped()).stdout(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW).spawn().unwrap();
        let mut handle = std::ptr::null_mut();
        assert_ne!(unsafe { DuplicateHandle(GetCurrentProcess(), child.as_raw_handle(), GetCurrentProcess(),
            &mut handle, 0, 0, DUPLICATE_SAME_ACCESS) }, 0);
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        let peer_process = handle as usize;
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut child = Some(child);
        let mut peer = None;
        let mut spawn = |_: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let (name, done, got) = (name.to_owned(), done.clone(), got.clone());
            peer = Some(std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                assert_eq!(unsafe { WaitForSingleObject(peer_process as _, 0) }, WAIT_TIMEOUT,
                    "the launcher must remain alive before its host is verified");
                writeln!(writer, "{}", native_hello()).unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(line);
                assert_eq!(unsafe { WaitForSingleObject(peer_process as _, 0) }, WAIT_OBJECT_0,
                    "the old profile snapshot must never be saved after session delivery");
                writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                while !done.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(10)); }
                writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            }));
            Ok(Injector::Child(child.take().unwrap()))
        };
        let auth = FakeAuth::new();
        let result = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST).unwrap();
        assert!(result.alive.load(Ordering::SeqCst), "the injected game host continues after bootstrap exit");
        assert_eq!(unsafe { WaitForSingleObject(process.as_raw_handle(), 0) }, WAIT_OBJECT_0);
        assert_eq!(lines(&got).len(), 1);
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        peer.unwrap().join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    /// A fake loader that connects, says hello, reads the session, reports `started`, then
    /// exits when told.
    #[test]
    fn full_session_runs_to_exit_and_records_the_outcome() {
        for tool in [Tool::Windower, Tool::Vanilla] { check_full_session(tool); }
    }

    fn check_full_session(tool: Tool) {
        let (prepared, dir) = fixture(tool);
        let dropped = Arc::new(AtomicBool::new(false));
        let auth = FakeAuth { dropped: dropped.clone() };
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        let (session_tx, session_rx) = std::sync::mpsc::channel::<String>();
        let mut exit_rx = Some(exit_rx);
        let mut spawned_args = String::new();
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            spawned_args = tools::loader_args(pipe, std::process::id());
            let name = pipe.to_owned();
            let exit_rx = exit_rx.take().unwrap();
            let session_tx = session_tx.clone();
            std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                writeln!(writer, "{}", native_hello()).unwrap();
                let mut session = String::new();
                reader.read_line(&mut session).unwrap();
                session_tx.send(session).unwrap();
                writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
                exit_rx.recv().unwrap();
                writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            });
            Ok(Injector::Detached)
        };
        let mut stages = Vec::new();
        let result = run(prepared, auth, Timeline::begin(), &mut |s: &str| stages.push(s.to_owned()), &mut spawn, test_turns(), FAST).unwrap();
        assert_eq!(result.pid, std::process::id());
        assert!(result.detail.contains(&format!("started through {}", tool.name())), "{}", result.detail);
        assert!(result.alive.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst), "the sign-in lives while the game runs");
        assert!(stages.iter().any(|s| s.contains("Waiting for crystal-loader")));
        assert!(!spawned_args.contains("BwcH") && spawned_args.starts_with("--crystal-pipe "));
        let session: serde_json::Value = serde_json::from_str(&session_rx.recv().unwrap()).unwrap();
        assert_eq!(session["install_dir"], r"C:\FFXI");
        exit_tx.send(()).unwrap();
        wait_for(&result.alive, false);
        assert!(dropped.load(Ordering::SeqCst), "the sign-in is dropped when the session ends");
        let report = result.outcome.lock().unwrap().clone().unwrap();
        assert_eq!(report.summary, "Game exited during startup with code 0");
        let history = fs::read_to_string(dir.join("history.log")).unwrap();
        assert!(history.contains("| Fake | Game exited during startup with code 0 (launch: tool "), "{history}");
        assert!(history.contains(", hello ") && history.contains(", game started "), "{history}");
        assert!(result.timing.starts_with("Game started "), "{}", result.timing);
        let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
        for what in ["tool process started", "loader hello", "session delivered", "game started"] {
            assert!(log.lines().any(|l| l.contains("[launcher] timing +") && l.ends_with(what)), "{what}: {log}");
        }
        assert!(log.contains("image verified"), "{log}");
        // The log never carries session material.
        assert!(!log.contains("BwcHBwcH"), "{log}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loader_that_never_connects_times_out_with_a_hint() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let dropped = Arc::new(AtomicBool::new(false));
        let auth = FakeAuth { dropped: dropped.clone() };
        let mut spawn = |_: &Prepared, _: &str, _: &mut SessionLog| Ok(Injector::Detached);
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap_err();
        assert!(error.contains("crystal-loader did not connect within"), "{error}");
        assert!(error.contains("administrator prompt"), "{error}");
        assert!(error.contains("Log: "), "{error}");
        assert!(dropped.load(Ordering::SeqCst), "a failed launch releases the sign-in");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn injector_failure_stops_the_wait_early() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let auth = FakeAuth::new();
        let mut spawn = |_: &Prepared, _: &str, _: &mut SessionLog| {
            std::process::Command::new("cmd")
                .args(["/c", "exit 3"])
                .spawn()
                .map(Injector::Child)
                .map_err(|e| e.to_string())
        };
        let started = Instant::now();
        let slow = Timeouts { connect: Duration::from_secs(30), ..FAST };
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), slow).unwrap_err();
        assert!(error.contains("Ashita-cli.exe exited with code 3"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(20));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loader_error_before_start_fails_the_launch() {
        let (prepared, dir) = fixture(Tool::Ashita);
        let auth = FakeAuth::new();
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let name = pipe.to_owned();
            std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                let mut writer = file.try_clone().unwrap();
                writeln!(writer, "{}", native_hello()).unwrap();
                let mut session = String::new();
                BufReader::new(file).read_line(&mut session).unwrap();
                writeln!(writer, r#"{{"type":"error","message":"FFXiMain.dll could not be loaded"}}"#).unwrap();
            });
            Ok(Injector::Detached)
        };
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap_err();
        assert!(error.contains("Game loader error during startup: FFXiMain.dll could not be loaded"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn impostor_pid_gets_no_session() {
        let (prepared, dir) = fixture(Tool::Windower);
        let auth = FakeAuth::new();
        let (got_tx, got_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let name = pipe.to_owned();
            let got_tx = got_tx.clone();
            std::thread::spawn(move || {
                let mut file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
                writeln!(file, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id() + 4).unwrap();
                let mut rest = Vec::new();
                let _ = file.read_to_end(&mut rest);
                got_tx.send(rest).unwrap();
            });
            Ok(Injector::Detached)
        };
        let error = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap_err();
        assert!(error.contains("not the one that said hello"), "{error}");
        assert!(got_rx.recv_timeout(Duration::from_secs(10)).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_tools_fail_before_sign_in() {
        let dir = std::env::temp_dir().join(format!("xi-pol-less-prep-{}", unique_id().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let config = Config { experiment: dir.clone(), repository: dir.clone(), game_install: Some(dir.clone()), dat_overlays: Vec::new() };
        let account = Account {
            label: "Fake".into(),
            mode: crate::backend::AccountMode::Retail,
            server: String::new(),
            pol_id: "ABCD1234".into(),
            pol_password: "synthetic".into(),
            se_id: "synthetic".into(),
            se_password: "synthetic".into(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
            pol_handle_names: String::new(),
        };
        let plan = Plan {
            social_only: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger: Default::default(),
            tool: Tool::Ashita,
            settings: ToolSettings { ashita: dir.join("none").to_string_lossy().into_owned(), ..Default::default() },
        };
        let error = launch(&config, &plan, account.clone(), Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("Ashita v4 was not found") || error.contains("pol.exe is missing"), "{error}");
        // A found tool but no loader.
        fs::write(dir.join("Ashita-cli.exe"), b"").unwrap();
        let plan = Plan {
            social_only: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger: Default::default(),
            tool: Tool::Ashita,
            settings: ToolSettings {
                ashita: dir.to_string_lossy().into_owned(),
                loader: dir.join("missing-loader.exe").to_string_lossy().into_owned(),
                ..Default::default()
            },
        };
        let error = launch(&config, &plan, account.clone(), Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("crystal-loader path"), "{error}");
        // Missing either game DLL is caught before authenticating.
        fs::write(dir.join("crystal-loader.exe"), b"").unwrap();
        let plan = Plan {
            social_only: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger: Default::default(),
            tool: Tool::Ashita,
            settings: ToolSettings {
                ashita: dir.to_string_lossy().into_owned(),
                loader: dir.join("crystal-loader.exe").to_string_lossy().into_owned(),
                ..Default::default()
            },
        };
        let error = launch(&config, &plan, account.clone(), Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("has no FFXi.dll"), "{error}");
        fs::write(dir.join("FFXi.dll"), b"").unwrap();
        let error = launch(&config, &plan, account, Zeroizing::new(String::new()), Timeline::begin(), &mut |_| {}).err().unwrap();
        assert!(error.contains("has no FFXiMain.dll"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn vanilla_preparation_ignores_tool_folders_and_profiles_and_keeps_menu_settings() {
        let root = std::env::temp_dir().join(format!("crystal-vanilla-{}", unique_id().unwrap()));
        let game = root.join("FINAL FANTASY XI");
        fs::create_dir_all(&game).unwrap();
        let loader = root.join("pol.exe");
        fs::write(&loader, []).unwrap();
        let config = Config { experiment: root.clone(), repository: root.clone(),
            game_install: Some(game.clone()), dat_overlays: Vec::new() };
        let account = Account { label: "Fixture".into(), mode: crate::backend::AccountMode::Retail,
            server: String::new(), pol_id: "ABCD1234".into(), pol_password: "synthetic".into(),
            se_id: "synthetic".into(), se_password: "synthetic".into(), region: 0,
            launch_with: None, ashita_boot_ini: None, windower_profile: None, pol_handle_names: String::new() };
        let plan = Plan { tool: Tool::Vanilla, social_only: false, auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger: Default::default(),
            settings: ToolSettings { loader: loader.to_string_lossy().into_owned(),
                ashita: root.join("missing-ashita").to_string_lossy().into_owned(),
                windower: root.join("missing-windower").to_string_lossy().into_owned(),
                boot_ini: "../invalid.ini".into(), windower_profile: "not installed".into(), ..Default::default() } };
        let error = prepare(&config, &plan, &account).err().unwrap();
        assert!(error.contains("has no FFXi.dll"), "{error}");
        fs::write(game.join("FFXi.dll"), []).unwrap();
        let error = prepare(&config, &plan, &account).err().unwrap();
        assert!(error.contains("has no FFXiMain.dll"), "{error}");
        fs::write(game.join("FFXiMain.dll"), []).unwrap();
        let prepared = prepare(&config, &plan, &account).unwrap();
        assert_eq!(prepared.tool, Tool::Vanilla);
        assert_eq!(prepared.install_dir, game);
        assert!(prepared.base_ini.is_none() && prepared.profile.is_empty());
        assert_eq!(prepared.ffxi_settings.unwrap().json, "{}", "default settings still prepare the scoped live-controller store");
        let ashita = root.join("Ashita");
        fs::create_dir_all(ashita.join("config/boot")).unwrap();
        fs::write(ashita.join("Ashita-cli.exe"), []).unwrap();
        let boot = b"[boot]\ncommand=pol.exe\n[ffxi.registry]\npadmode000=1,1,0,0,0,0\n";
        fs::write(ashita.join("config/boot/fixture.ini"), boot).unwrap();
        let mut ashita_plan = plan.clone();
        ashita_plan.tool = Tool::Ashita;
        ashita_plan.settings.ashita = ashita.to_string_lossy().into_owned();
        ashita_plan.settings.boot_ini = "fixture.ini".into();
        let prepared = prepare(&config, &ashita_plan, &account).unwrap();
        assert_eq!(prepared.ffxi_settings.unwrap().json, "{}");
        assert_eq!(prepared.base_ini.as_deref(), Some(boot.as_slice()));
        assert_eq!(fs::read(ashita.join("config/boot/fixture.ini")).unwrap(), boot);
        let menu = root.join("iced-launcher/menu-playonlineus.json");
        fs::create_dir_all(menu.parent().unwrap()).unwrap();
        fs::write(&menu, "[1600,900]").unwrap();
        let prepared = prepare(&config, &plan, &account).unwrap();
        let values: serde_json::Value = serde_json::from_str(&prepared.ffxi_settings.unwrap().json).unwrap();
        assert_eq!(values, serde_json::json!({"0037":1600,"0038":900}));
        assert!(!root.join("missing-ashita").exists() && !root.join("missing-windower").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windower_preparation_preserves_profile_menu_and_settings_files() {
        let root = std::env::temp_dir().join(format!("crystal-windower-profile-{}", unique_id().unwrap()));
        let game = root.join("FINAL FANTASY XI");
        let windower = root.join("Windower");
        fs::create_dir_all(&game).unwrap();
        fs::create_dir_all(windower.join("plugins/settings")).unwrap();
        for dll in ["FFXi.dll", "FFXiMain.dll"] { fs::write(game.join(dll), []).unwrap(); }
        let loader = root.join("pol.exe");
        fs::write(&loader, []).unwrap();
        fs::write(windower.join("Windower.exe"), []).unwrap();
        let profile_file = windower.join("settings.xml");
        let profiles = br#"<settings><profile name=""><width>1920</width><height>1080</height><uiscale>1.5</uiscale><sound>false</sound></profile><profile name="Mule"><width>1280</width><height>720</height><uiscale>1</uiscale></profile></settings>"#;
        fs::write(&profile_file, profiles).unwrap();
        let plugin_file = windower.join("plugins/settings/ffxidb.xml");
        let plugin = br#"<settings><global><Map><X>700</X><Y>300</Y><Scale>1.2</Scale></Map></global></settings>"#;
        fs::write(&plugin_file, plugin).unwrap();
        let menu_file = root.join("iced-launcher/menu-playonlineus.json");
        fs::create_dir_all(menu_file.parent().unwrap()).unwrap();
        let config = Config { experiment: root.clone(), repository: root.clone(),
            game_install: Some(game), dat_overlays: Vec::new() };
        let account = Account { label: "Fixture".into(), mode: crate::backend::AccountMode::Retail,
            server: String::new(), pol_id: "ABCD1234".into(), pol_password: "synthetic".into(),
            se_id: "synthetic".into(), se_password: "synthetic".into(), region: 0,
            launch_with: None, ashita_boot_ini: None, windower_profile: None, pol_handle_names: String::new() };
        let mut plan = Plan { tool: Tool::Windower, social_only: false,
            auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger: Default::default(),
            settings: ToolSettings { loader: loader.to_string_lossy().into_owned(),
                windower: windower.to_string_lossy().into_owned(), ..Default::default() } };
        for retained_menu in ["[640,480]", "not valid JSON"] {
            fs::write(&menu_file, retained_menu).unwrap();
            for (profile, window, menu, sound) in [
                ("", (1920, 1080), (1280, 720), 0),
                ("Mule", (1280, 720), (0, 0), 1),
            ] {
                plan.settings.windower_profile = profile.into();
                let prepared = prepare(&config, &plan, &account).unwrap();
                assert_eq!(prepared.profile, profile);
                let values: serde_json::Value = serde_json::from_str(&prepared.ffxi_settings.unwrap().json).unwrap();
                assert_eq!((values["0001"].as_u64(), values["0002"].as_u64()), (Some(window.0), Some(window.1)));
                assert_eq!((values["0037"].as_u64(), values["0038"].as_u64()), (Some(menu.0), Some(menu.1)));
                assert_eq!(values["0007"], sound);
                assert_eq!(fs::read(&profile_file).unwrap(), profiles);
                assert_eq!(fs::read(&plugin_file).unwrap(), plugin);
                assert_eq!(fs::read_to_string(&menu_file).unwrap(), retained_menu);
            }
        }
        // A resolved directory in a retained plan must not cache its profile.
        plan.settings.windower_profile.clear();
        for width in [1600, 1800] {
            fs::write(&profile_file, format!("<settings><profile name=\"\"><width>{width}</width><height>900</height></profile></settings>")).unwrap();
            let mut timing = Timeline::begin();
            let prepared = prepare_with_progress(&config, &plan, &account, &mut timing, &mut |_| {}, &|| false).unwrap();
            assert!(prepared.profile.is_empty());
            let values: serde_json::Value = serde_json::from_str(&prepared.ffxi_settings.unwrap().json).unwrap();
            assert_eq!(values["0001"], width);
            let marks = timing.unwritten_lines();
            // Includes "Windower startup inputs checked" (the snapshot key).
            assert_eq!(marks.len(), 9);
            assert!(marks.iter().all(|line| line.contains("sign-in stage: preparation:")));
            assert!(!marks.iter().any(|line| line.contains("ABCD1234") || line.contains("synthetic") || line.contains(&root.to_string_lossy().to_string())));
        }
        fs::remove_dir_all(root).unwrap();
    }

    // ---- Play: sign in idle, then the stock FFXI check, then the game start ----

    #[test]
    #[ignore = "local installed-tool probe; creates no POL core and sends no credentials"]
    fn local_original_windower_supplies_three_parallel_private_hosts_without_auth(){
        use std::os::windows::io::{OwnedHandle,FromRawHandle,AsRawHandle};
        use windows_sys::Win32::System::Threading::{OpenProcess,TerminateProcess,PROCESS_TERMINATE,PROCESS_SYNCHRONIZE};
        let store=std::env::temp_dir().join("crystal-windower-startup-probe");
        let folder=PathBuf::from(std::env::var_os("CRYSTAL_WINDOWER_DIR").expect("CRYSTAL_WINDOWER_DIR"));
        let loader=PathBuf::from(std::env::var_os("CRYSTAL_BOOTSTRAP_LOADER").expect("CRYSTAL_BOOTSTRAP_LOADER"));
        let install=PathBuf::from(std::env::var_os("CRYSTAL_FFXI_DIR").expect("CRYSTAL_FFXI_DIR"));
        let settings=fs::read(folder.join("settings.xml")).unwrap();
        let prepare_probe=||{
            let(mut p,dir)=fixture(Tool::Windower);p.tool_dir=folder.clone();p.loader=loader.clone();p.install_dir=install.clone();
            p.bootstrap_key=Ok(windower_parallel::Key::new(&folder,"",&install,&store).expect("supported installed Hook"));
            (p,dir)
        };
        struct Probe(#[allow(dead_code)] Host,OwnedHandle,PathBuf); // the Host keeps its pipe open
        impl Drop for Probe{fn drop(&mut self){unsafe{TerminateProcess(self.1.as_raw_handle(),0);};let _=fs::remove_dir_all(&self.2);}}
        let start=|mut p:Prepared,dir:PathBuf|{
            let mut log=SessionLog::open(&p.session.log).unwrap();let mut timeline=Timeline::begin();
            let host=start_host(&mut p,&mut log,&mut timeline,&mut |_|{},&mut spawn_injector,&launch_gate,TIMEOUTS).unwrap();
            let handle=unsafe{OpenProcess(PROCESS_TERMINATE|PROCESS_SYNCHRONIZE,0,host.pid)};
            assert!(!handle.is_null());let handle=unsafe{OwnedHandle::from_raw_handle(handle)};
            let private=p.private_bootstrap.is_some();drop(log);
            (Probe(host,handle,dir),private)
        };
        let(p,dir)=prepare_probe();let(first,_) =start(p,dir);
        for line in fs::read_to_string(first.2.join("a.crystal.log")).unwrap().lines().filter(|line|line.contains("snapshot")||line.contains("changed")||line.contains("startup")){eprintln!("{line}");}
        assert!(windower_parallel::Key::new(&folder,"",&install,&store).unwrap().load().is_ok(),"the original injector must supply a usable, protected snapshot");
        let began=Instant::now();
        let clients=std::thread::scope(|scope|{
            let tasks:Vec<_>=(0..3).map(|_|{let(p,dir)=prepare_probe();scope.spawn(||start(p,dir))}).collect();
            tasks.into_iter().map(|t|t.join().unwrap()).collect::<Vec<_>>()
        });
        assert!(clients.iter().all(|(_,private)|*private));
        eprintln!("Three installed-profile private handshakes: {:.3}s",began.elapsed().as_secs_f64());
        assert_eq!(fs::read(folder.join("settings.xml")).unwrap(),settings,"original Windower profiles must not change");
        for (probe,_) in &clients {let log=fs::read_to_string(probe.2.join("a.crystal.log")).unwrap();assert!(!log.contains("Session sent") && log.contains("authentication has not started"));}
        drop(clients);drop(first);
    }

    /// A signed-in native session that a Play will start itself (the parts of `NativeAuth`
    /// the flow depends on), recording what the worker told it.
    struct DeferAuth {
        pending: AtomicBool,
        idle_seen: AtomicBool,
        running: Arc<AtomicBool>,
        outcome: Arc<Mutex<Option<Option<String>>>>,
    }
    impl DeferAuth {
        fn new() -> (Self, Arc<Mutex<Option<Option<String>>>>) {
            let outcome = Arc::new(Mutex::new(None));
            (Self { pending: AtomicBool::new(true), idle_seen: AtomicBool::new(false), running: Arc::new(AtomicBool::new(true)), outcome: outcome.clone() }, outcome)
        }
    }
    impl SignedIn for DeferAuth {
        fn start_pending(&self) -> bool { self.pending.load(Ordering::SeqCst) }
        fn idle_first(&self) -> bool { self.start_pending() }
        fn deferred_start_done(&self, blocked: Option<&str>) {
            assert!(self.idle_seen.load(Ordering::SeqCst), "a check result cannot release Play before full native idle");
            self.pending.store(false, Ordering::SeqCst);
            if blocked.is_some() {
                self.running.store(false, Ordering::SeqCst);
            }
            *self.outcome.lock().unwrap() = Some(blocked.map(str::to_owned));
        }
        fn game_running(&self) -> Option<Arc<AtomicBool>> { Some(self.running.clone()) }
        fn native_session_state(&self, game: bool) -> Result<(), String> {
            if !game { self.idle_seen.store(true, Ordering::SeqCst); }
            if !game && self.start_pending() { return Ok(()); }
            self.running.store(game, Ordering::SeqCst);
            Ok(())
        }
        fn session_line(&self, _: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
            Ok(Zeroizing::new(format!("{{\"type\":\"session\",\"start_game\":{}}}\n", !self.idle_first()).into_bytes()))
        }
        fn credentials_line(&self) -> Result<Zeroizing<Vec<u8>>, String> { unreachable!() }
    }

    /// A loader for a native Play: hello, read the session, report the idle Viewer session
    /// after `idle_after`, then (`expect_start`) wait for `native_start_game` and report the
    /// game started; exit when `done`, then read whatever else the launcher sent until it closes.
    /// Everything the launcher wrote is pushed to `got`, in order.
    fn deferred_loader(name: String, idle_after: Duration, expect_start: bool, done: Arc<AtomicBool>, got: Arc<Mutex<Vec<String>>>, on_hello: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
        deferred_loader_events(name, expect_start, done, got, on_hello, move |_| std::thread::sleep(idle_after))
    }

    #[test]
    fn private_windower_readiness_precedes_session_delivery_and_failure_retries_without_credentials(){
        use std::os::windows::io::AsRawHandle;
        for fail in [false,true]{
            let(mut p,dir)=fixture(Tool::Windower);p.private_bootstrap=Some(windower_parallel::tests::synthetic("Isolated"));
            let(auth,outcome)=DeferAuth::new();let done=Arc::new(AtomicBool::new(false));let got=Arc::new(Mutex::new(Vec::new()));
            let mut workers=Vec::new();let mut calls=0;
            let mut spawn=|p:&Prepared,name:&str,_:&mut SessionLog|->Result<Injector,String>{
                calls+=1;
                if calls==1{
                    assert!(p.private_bootstrap.is_some());
                    let(name,got,done)=(name.to_owned(),got.clone(),done.clone());
                    workers.push(std::thread::spawn(move||{
                        let file=OpenOptions::new().read(true).write(true).open(name).unwrap();let mut writer=file.try_clone().unwrap();let mut reader=BufReader::new(file);
                        writeln!(writer,r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true,"windower_private":true}}"#,std::process::id()).unwrap();
                        let mut line=String::new();reader.read_line(&mut line).unwrap();
                        let startup:serde_json::Value=serde_json::from_str(&line).unwrap();assert_eq!(startup["type"],"windower_startup");assert!(startup.get("native_auth").is_none());
                        got.lock().unwrap().push(std::mem::take(&mut line));
                        let mut available=0;
                        assert_ne!(unsafe{windows_sys::Win32::System::Pipes::PeekNamedPipe(reader.get_ref().as_raw_handle(),std::ptr::null_mut(),0,std::ptr::null_mut(),&mut available,std::ptr::null_mut())},0);
                        assert_eq!(available,0,"no credentials before Hook readiness");
                        if fail{
                            writeln!(writer,r#"{{"type":"error","message":"fixture bootstrap rejected"}}"#).unwrap();
                            while reader.read_line(&mut line).map(|n|n>0).unwrap_or(false){got.lock().unwrap().push(std::mem::take(&mut line));}
                            return;
                        }
                        writeln!(writer,r#"{{"type":"windower_ready"}}"#).unwrap();
                        reader.read_line(&mut line).unwrap();assert!(line.contains("\"session\""));got.lock().unwrap().push(std::mem::take(&mut line));
                        for event in [r#"{"type":"authenticated"}"#,r#"{"type":"native_main_menu","code":1,"returned":false}"#,r#"{"type":"native_session_state","game":false}"#]{writeln!(writer,"{event}").unwrap();}
                        reader.read_line(&mut line).unwrap();assert!(line.contains("native_start_game"));got.lock().unwrap().push(std::mem::take(&mut line));
                        writeln!(writer,r#"{{"type":"native_session_state","game":true}}"#).unwrap();
                        while !done.load(Ordering::SeqCst){std::thread::sleep(Duration::from_millis(5));}
                        writeln!(writer,r#"{{"type":"exit","code":0}}"#).unwrap();
                    }));
                }else{
                    assert!(fail && p.private_bootstrap.is_none());
                    workers.push(deferred_loader(name.into(),Duration::ZERO,true,done.clone(),got.clone(),||{}));
                }
                Ok(Injector::Fake(Arc::new(AtomicBool::new(false))))
            };
            let result=run(p,auth,Timeline::begin(),&mut |_|{},&mut spawn,test_turns(),FAST).unwrap();
            assert_eq!(*outcome.lock().unwrap(),Some(None));assert_eq!(calls,if fail{2}else{1});
            done.store(true,Ordering::SeqCst);wait_for(&result.alive,false);for worker in workers{worker.join().unwrap();}
            let frames=lines(&got);assert_eq!(frames.iter().filter(|line|line.contains("\"type\":\"session\"")).count(),1);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Windower.exe's well-known startup mapping is machine-wide; one test at a time.
    static WINDOWER_MAPPING: Mutex<()> = Mutex::new(());

    /// Stands in for Windower.exe filling its startup mapping for Hook.dll.
    fn windower_exe_supplies(label: &str) {
        use windows_sys::Win32::System::Memory::{MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_WRITE};
        use std::os::windows::io::{FromRawHandle, OwnedHandle, AsRawHandle};
        let record = windower_parallel::tests::record(label);
        let name = crate::win::wide(windower_parallel::layout::MAP_NAME);
        let handle = unsafe { OpenFileMappingW(FILE_MAP_WRITE, 0, name.as_ptr()) };
        assert!(!handle.is_null(), "the launcher holds the startup mapping open while Windower.exe runs");
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        unsafe {
            let view = MapViewOfFile(handle.as_raw_handle(), FILE_MAP_WRITE, 0, 0, record.len());
            assert!(!view.Value.is_null());
            std::ptr::copy_nonoverlapping(record.as_ptr(), view.Value as *mut u8, record.len());
            UnmapViewOfFile(view);
        }
    }

    /// A host: hello, then (private) the `windower_startup` line answered with
    /// `windower_ready` or an error. Records every later frame it receives.
    fn startup_loader(name: String, private: Option<bool>, got: Arc<Mutex<Vec<String>>>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
            let mut writer = file.try_clone().unwrap();
            let mut reader = BufReader::new(file);
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true,"windower_private":true}}"#, std::process::id()).unwrap();
            let mut line = String::new();
            if let Some(ready) = private {
                reader.read_line(&mut line).unwrap();
                let startup: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(startup["type"], "windower_startup");
                for field in ["native_auth", "password", "user", "session", "v_key"] { assert!(startup.get(field).is_none(), "{field}"); }
                got.lock().unwrap().push(std::mem::take(&mut line));
                if ready { writeln!(writer, r#"{{"type":"windower_ready"}}"#).unwrap(); }
                else { writeln!(writer, r#"{{"type":"error","message":"fixture Hook rejected the snapshot"}}"#).unwrap(); }
            }
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
        })
    }

    fn start_windower_host(mut p: Prepared, turns: &TurnGate<'_>, spawn: &mut Spawner<'_>) -> (Result<Host, String>, String, bool) {
        let mut log = SessionLog::open(&p.session.log).unwrap();
        let host = start_host(&mut p, &mut log, &mut Timeline::begin(), &mut |_| {}, spawn, turns, FAST);
        drop(log);
        let text = fs::read_to_string(&p.session.log).unwrap_or_default();
        (host, text, p.private_bootstrap.is_some())
    }

    #[test]
    fn every_account_starts_private_hosts_at_once_from_todays_snapshot() {
        let startup = windower_parallel::tests::Fixture::new();
        startup.key("").save(&windower_parallel::tests::record("Main")).unwrap();
        let held: &'static turns::Turns = Box::leak(Box::new(turns::Turns::new()));
        let turns = gate_of(held);
        // Another launch's Windower.exe holds the Windower turn the whole time;
        // private hosts never wait for it.
        let _held = held.enter(Tool::Windower, &mut |_| {});
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let accounts: Vec<_> = (0..4).map(|_| {
            let (key, done_tx) = (startup.key(""), done_tx.clone());
            std::thread::spawn(move || {
                let (mut p, dir) = fixture(Tool::Windower);
                p.bootstrap_key = Ok(key);
                let got = Arc::new(Mutex::new(Vec::new()));
                let mut workers = Vec::new();
                let mut spawn = |p: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
                    assert!(p.private_bootstrap.is_some(), "Windower.exe must not start with today's snapshot");
                    workers.push(startup_loader(name.into(), Some(true), got.clone()));
                    Ok(Injector::Fake(Arc::new(AtomicBool::new(false))))
                };
                let (host, log, private) = start_windower_host(p, turns, &mut spawn);
                let host = host.unwrap();
                assert!(private && log.contains("windower: saved startup snapshot verified") && log.contains("Private Windower startup is ready"), "{log}");
                let _ = done_tx.send(());
                drop(host);
                for worker in workers { worker.join().unwrap(); }
                assert!(!lines(&got).iter().any(|line| line.contains("\"session\"")), "no session without a Play");
                fs::remove_dir_all(dir).unwrap();
            })
        }).collect();
        for _ in 0..4 { done_rx.recv_timeout(Duration::from_secs(20)).expect("a private host waited for the Windower turn"); }
        for account in accounts { account.join().unwrap(); }
    }

    #[test]
    fn each_snapshot_miss_runs_windower_exe_and_a_good_run_refreshes_todays_snapshot() {
        let _one = WINDOWER_MAPPING.lock().unwrap_or_else(|e| e.into_inner());
        let startup = windower_parallel::tests::Fixture::new();
        let key = startup.key("");
        let cases: [(&str, &dyn Fn(), bool); 4] = [
            ("no saved startup snapshot", &|| key.discard(), true),
            ("first launch today", &|| key.save_on(&windower_parallel::tests::record("Yesterday"), "2000-01-01").unwrap(), true),
            ("unreadable and was removed", &|| { fs::create_dir_all(startup.store()).unwrap(); fs::write(startup.store().join(format!("{}.dpapi", key.file_stem())), b"damaged").unwrap(); }, true),
            ("not supported for private startup", &|| key.discard(), false),
        ];
        for (reason, arrange, supported) in cases {
            arrange();
            let (mut p, dir) = fixture(Tool::Windower);
            p.bootstrap_key = if supported { Ok(startup.key("")) } else { Err("this Windower Hook.dll build is not supported for private startup") };
            let got = Arc::new(Mutex::new(Vec::new()));
            let mut workers = Vec::new();
            let mut spawn = |p: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
                assert!(p.private_bootstrap.is_none(), "{reason}: Windower.exe runs");
                if supported { windower_exe_supplies("Fresh"); }
                workers.push(startup_loader(name.into(), None, got.clone()));
                Ok(Injector::Fake(Arc::new(AtomicBool::new(false))))
            };
            let (host, log, private) = start_windower_host(p, test_turns(), &mut spawn);
            drop(host.unwrap());
            for worker in workers { worker.join().unwrap(); }
            assert!(!private && log.contains("Windower.exe starts this launch") && log.contains(reason), "{reason}: {log}");
            let refreshed = startup.key("").load();
            if supported {
                assert_eq!(refreshed.ok().and_then(|s| s.label()).as_deref(), Some("Fresh"), "{reason}: {log}");
                assert!(log.contains("today's protected startup snapshot"), "{log}");
            } else {
                assert!(refreshed.is_err(), "an unsupported Hook never saves a snapshot");
            }
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_failed_private_startup_discards_its_snapshot_and_windower_exe_refreshes_it_before_credentials() {
        let _one = WINDOWER_MAPPING.lock().unwrap_or_else(|e| e.into_inner());
        let startup = windower_parallel::tests::Fixture::new();
        startup.key("").save(&windower_parallel::tests::record("Rejected")).unwrap();
        let (mut p, dir) = fixture(Tool::Windower);
        p.bootstrap_key = Ok(startup.key(""));
        p.profile_access = Some(profile_access::launching_at(&dir.join("data"), &dir).unwrap());
        let got = Arc::new(Mutex::new(Vec::new()));
        let (mut workers, mut calls) = (Vec::new(), 0);
        let mut spawn = |p: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
            calls += 1;
            if calls == 1 {
                assert!(p.private_bootstrap.is_some());
                workers.push(startup_loader(name.into(), Some(false), got.clone()));
            } else {
                assert!(p.private_bootstrap.is_none(), "the fallback is Windower.exe");
                assert!(p.profile_access.is_some(), "the fallback Windower.exe keeps the profile guard");
                assert!(startup.key("").load().is_err(), "the failed snapshot was removed before the fallback");
                windower_exe_supplies("Fresh");
                workers.push(startup_loader(name.into(), None, got.clone()));
            }
            Ok(Injector::Fake(Arc::new(AtomicBool::new(false))))
        };
        let (host, log, private) = start_windower_host(p, test_turns(), &mut spawn);
        drop(host.unwrap());
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(calls, 2);
        assert!(!private && log.contains("fixture Hook rejected the snapshot") && log.contains("before any credentials are sent"), "{log}");
        assert_eq!(startup.key("").load().ok().and_then(|s| s.label()).as_deref(), Some("Fresh"));
        assert!(!lines(&got).iter().any(|line| line.contains("\"session\"") || line.contains("native_auth")), "no credentials on either host");
        fs::remove_dir_all(dir).unwrap();
    }

    fn deferred_loader_events(name: String, expect_start: bool, done: Arc<AtomicBool>, got: Arc<Mutex<Vec<String>>>, on_hello: impl FnOnce() + Send + 'static, before_idle: impl FnOnce(&mut File) + Send + 'static) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let file = OpenOptions::new().read(true).write(true).open(&name).unwrap();
            let mut writer = file.try_clone().unwrap();
            let mut reader = BufReader::new(file);
            on_hello();
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true}}"#, std::process::id()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            got.lock().unwrap().push(std::mem::take(&mut line));
            before_idle(&mut writer);
            writeln!(writer, r#"{{"type":"native_session_state","game":false}}"#).unwrap();
            if expect_start {
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                writeln!(writer, r#"{{"type":"native_session_state","game":true}}"#).unwrap();
            }
            while !done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                got.lock().unwrap().push(std::mem::take(&mut line));
            }
        })
    }

    fn lines(got: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        got.lock().unwrap().clone()
    }

    #[test]
    fn main_menu_check_follows_the_publish_and_survives_both_supervisor_handoffs() {
        use std::sync::mpsc::{self, Receiver, Sender};
        type Signals = (Sender<()>, Receiver<Result<String, String>>, Sender<()>);
        static SIGNALS: Mutex<Option<Signals>> = Mutex::new(None);
        static CHECKS: AtomicUsize = AtomicUsize::new(0);
        // pol-spec 07c §5.1: `authenticated` arrives early and starts nothing; W20 starts on
        // `native_main_menu`, on its own thread, and the idle report waits for it here. Neither
        // a fast success nor a refusal may bypass the idle report.
        for (late_menu, late_idle, refused) in [(false, false, false), (false, false, true), (true, false, false), (false, true, false)] {
            CHECKS.store(0, Ordering::SeqCst);
            let (entered_tx, entered_rx) = mpsc::channel();
            let (verdict_tx, verdict_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            *SIGNALS.lock().unwrap() = Some((entered_tx, verdict_rx, finished_tx));
            let (mut prepared, dir) = fixture(Tool::Windower);
            prepared.check = |_| {
                CHECKS.fetch_add(1, Ordering::SeqCst);
                let (entered, verdict, finished) = SIGNALS.lock().unwrap().take().expect("one check only");
                entered.send(()).unwrap();
                let verdict = verdict.recv_timeout(Duration::from_secs(10)).unwrap();
                finished.send(()).unwrap();
                (verdict, Default::default())
            };
            let (auth, outcome) = DeferAuth::new();
            let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
            let (resume_tx, resume_rx) = mpsc::channel();
            let (checking_tx, checking_rx) = mpsc::channel();
            let observed = outcome.clone();
            let mut before_idle = Some(move |writer: &mut File| {
                writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
                writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
                assert!(matches!(entered_rx.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)),
                    "no W20 on authentication: stock :53001 follows the main-menu publish");
                if late_menu { resume_rx.recv_timeout(Duration::from_secs(10)).unwrap(); }
                writeln!(writer, r#"{{"type":"native_main_menu","code":1,"returned":false}}"#).unwrap();
                // A duplicate signal must not issue a second version request.
                writeln!(writer, r#"{{"type":"native_main_menu","code":0,"returned":false}}"#).unwrap();
                entered_rx.recv_timeout(Duration::from_secs(10)).expect("the version check must start at the main menu, before native idle");
                checking_tx.send(()).unwrap();
                if late_idle { resume_rx.recv_timeout(Duration::from_secs(10)).unwrap(); }
                verdict_tx.send(if refused { Err("Update required: fixture version differs.".into()) } else { Ok("matches".into()) }).unwrap();
                finished_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(observed.lock().unwrap().is_none(), "native services are still pending");
            });
            let mut loader = None;
            let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                loader = Some(deferred_loader_events(pipe.to_owned(), !refused, done.clone(), got.clone(), || {}, before_idle.take().unwrap()));
                Ok(Injector::Detached)
            };
            let limits = if late_menu || late_idle { Timeouts { start: Duration::from_millis(400), ..FAST } } else { FAST };
            let result = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), limits).unwrap();
            if late_menu || late_idle {
                assert!(result.detail.contains("not yet confirmed"), "{}", result.detail);
                if late_menu { assert_eq!(CHECKS.load(Ordering::SeqCst), 0, "no request before the main menu"); }
                if late_idle { checking_rx.recv_timeout(Duration::from_secs(10)).unwrap(); }
                assert!(outcome.lock().unwrap().is_none());
                resume_tx.send(()).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while outcome.lock().unwrap().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(CHECKS.load(Ordering::SeqCst), 1);
            assert_eq!(outcome.lock().unwrap().as_ref().unwrap().is_some(), refused);
            done.store(true, Ordering::SeqCst);
            wait_for(&result.alive, false);
            loader.unwrap().join().unwrap();
            let sent = lines(&got);
            assert_eq!(sent.iter().filter(|line| line.contains("native_start_game")).count(), usize::from(!refused));
            let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
            let at = |what: &str| log.find(what).unwrap_or_else(|| panic!("{what}: {log}"));
            // A timing line ends with its label: "timing +1.20s (+0.10s) W20 done".
            let mark = |label: &str| at(&format!(") {label}\n"));
            assert!(mark("sign-in done") < at("Viewer main menu reached; checking the FFXI version"), "{log}");
            assert!(at("Viewer main menu reached; checking the FFXI version") < at("Native startup services finished"), "{log}");
            assert_eq!(log.matches("checking the FFXI version").count(), 1, "{log}");
            if !late_menu {
                // Timed while the start wait owns the timeline: the next live log shows them.
                assert!(mark("main menu reached") < at("checking the FFXI version"), "{log}");
                if !late_idle {
                    assert!(mark("W20 done") > at("checking the FFXI version"), "{log}");
                    if !refused { assert!(mark("W20 done") < at("Game start sent"), "{log}"); }
                }
            }
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// pol-spec 07c §1: each `native_viewer_check` runs the injected W2U once, on the session
    /// thread, and answers with the exact contract line; a refusal leaves the loader to fail
    /// the sign-in before Open(1). The late case is answered by the supervisor (`watch`).
    #[test]
    fn viewer_check_requests_get_one_fresh_check_and_the_exact_answer() {
        use crate::viewer::ConnectVerdict;
        static VERDICT: Mutex<Option<ConnectVerdict>> = Mutex::new(None);
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let verdicts = [
            (ConnectVerdict::Current { version: "20110829_E".into() }, false),
            (ConnectVerdict::Current { version: "20110829_E".into() }, true),
            (ConnectVerdict::Outdated { local: "20110829_E".into(), remote: "20261001_0".into() }, false),
            (ConnectVerdict::Failed("fixture: no PlayOnline Viewer".into()), false),
            (ConnectVerdict::Unreachable("fixture: connection timed out".into()), true),
            // An unanswered W2U lets the loader go on; if it then fails, the cause is kept.
            (ConnectVerdict::Unreachable("fixture: connection timed out".into()), false),
            // 07d §2.5: no network after the retries: reason "network", stop before Open(1).
            (ConnectVerdict::NetworkNotReady("fixture: No such host is known. (11001)".into()), false),
        ];
        for (verdict, late) in verdicts {
            CALLS.store(0, Ordering::SeqCst);
            *VERDICT.lock().unwrap() = Some(verdict.clone());
            let (mut prepared, dir) = fixture(Tool::Windower);
            prepared.viewer_check = || {
                CALLS.fetch_add(1, Ordering::SeqCst);
                // A lost SYN on the connect (07d §4.5): its loss note reaches the launch log.
                let attempt = crate::patch::Attempt { resolve: Some(Duration::from_millis(3)), connect: Some(Duration::from_millis(1180)),
                    reply: Some(Duration::from_millis(164)), close: Some(Duration::from_millis(104)), ..Default::default() };
                crate::viewer::ConnectCheck { verdict: VERDICT.lock().unwrap().clone().unwrap(),
                    timing: crate::patch::CheckTiming { attempts: vec![attempt], ..Default::default() } }
            };
            let (auth, outcome) = DeferAuth::new();
            let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
            let mut loader = None;
            let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                assert_eq!(CALLS.load(Ordering::SeqCst), 0, "no W2U before the loader's Connect");
                let (name, done, got) = (pipe.to_owned(), done.clone(), got.clone());
                loader = Some(std::thread::spawn(move || {
                    let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                    let mut writer = file.try_clone().unwrap();
                    let mut reader = BufReader::new(file);
                    writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true}}"#, std::process::id()).unwrap();
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    got.lock().unwrap().push(std::mem::take(&mut line));
                    // Past the 200 ms start wait: the supervisor must answer instead.
                    if late { std::thread::sleep(Duration::from_millis(600)); }
                    writeln!(writer, r#"{{"type":"native_viewer_check"}}"#).unwrap();
                    reader.read_line(&mut line).unwrap();
                    let answer = std::mem::take(&mut line);
                    got.lock().unwrap().push(answer.clone());
                    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
                    if answer["ok"] == true {
                        for event in [r#"{"type":"authenticated"}"#, r#"{"type":"native_main_menu","code":1,"returned":false}"#, r#"{"type":"native_session_state","game":false}"#] {
                            writeln!(writer, "{event}").unwrap();
                        }
                        reader.read_line(&mut line).unwrap();
                        got.lock().unwrap().push(std::mem::take(&mut line));
                        writeln!(writer, r#"{{"type":"native_session_state","game":true}}"#).unwrap();
                        while !done.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(20)); }
                        writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                    } else {
                        // What the loader does with any `ok:false`: the sign-in fails before Open(1).
                        writeln!(writer, r#"{{"type":"error","message":{}}}"#, answer["error"]).unwrap();
                    }
                    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
                }));
                Ok(Injector::Detached)
            };
            let limits = if late { Timeouts { start: Duration::from_millis(200), ..FAST } } else { FAST };
            let result = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), limits);
            let expected = viewer_check_result_line(&verdict);
            match (&verdict, late) {
                (ConnectVerdict::Current { .. }, _) => {
                    let result = result.unwrap();
                    if late { assert!(result.detail.contains("not yet confirmed"), "{}", result.detail); }
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while outcome.lock().unwrap().is_none() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5)); }
                    assert_eq!(*outcome.lock().unwrap(), Some(None), "the start was released");
                    done.store(true, Ordering::SeqCst);
                    wait_for(&result.alive, false);
                }
                (_, false) => {
                    let error = result.err().unwrap();
                    assert!(error.contains(&verdict.message()), "{error}");
                    assert_eq!(error.contains(&format!("The PlayOnline version check also failed: {}", verdict.message())),
                        matches!(verdict, ConnectVerdict::Unreachable(_)), "{error}");
                }
                (_, true) => {
                    // Answered by the supervisor; the loader's error ends the session there.
                    let result = result.unwrap();
                    wait_for(&result.alive, false);
                    assert!(outcome.lock().unwrap().is_none(), "no game start after a refused Viewer check");
                }
            }
            loader.unwrap().join().unwrap();
            let sent = lines(&got);
            assert_eq!(sent[1], expected);
            if matches!(verdict, ConnectVerdict::Current { .. }) {
                assert_eq!(sent[1], "{\"type\":\"native_viewer_check_result\",\"ok\":true}\n");
                assert_eq!(sent[2], "{\"type\":\"native_start_game\"}\n");
            } else {
                assert!(sent[1].contains(&format!("\"reason\":\"{}\"", verdict.reason().unwrap())), "{}", sent[1]);
                assert!(!sent.iter().any(|line| line.contains("native_start_game")), "{sent:?}");
            }
            assert_eq!(CALLS.load(Ordering::SeqCst), 1, "one W2U per request");
            let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
            // 07d §4.2: the trigger right after the launch clock; §4.5: the loss note.
            let opening: Vec<_> = log.lines().take(2).collect();
            assert!(opening[0].contains("[launcher] launch clock: ") && opening[1].ends_with("[launcher] launch trigger: Play"), "{log}");
            assert!(log.contains("possible packet loss: W2U connect took 1.18 s, +1.00 s over its clean 0.18 s (1.0 s SYN/DNS retry)"), "{log}");
            if !late && matches!(verdict, ConnectVerdict::Current { .. }) {
                // W2U is timed before the member login completes (Open(1) follows the answer).
                assert!(log.find(") W2U done\n").unwrap() < log.find(") sign-in done\n").unwrap(), "{log}");
            }
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// pol-spec 07c §3: no launcher path but the loader's Connect request reaches the Viewer
    /// server. The window, its gates and the batch use the local readiness check only.
    #[test]
    fn no_viewer_network_check_outside_connect() {
        let connect = ["check_at", "_connect"].concat();
        let network_status = ["viewer::", "status("].concat();
        for (file, source) in [("main.rs", include_str!("main.rs")), ("session_worker.rs", include_str!("session_worker.rs")),
            ("session_batch.rs", include_str!("session_batch.rs")), ("slint_host/view.rs", include_str!("slint_host/view.rs"))] {
            assert!(!source.contains(&connect), "{file} must not run the Connect-time W2U");
            assert!(!source.contains(&network_status), "{file} must use viewer::local_status");
        }
        let own = include_str!("pol_less.rs");
        assert_eq!(own.matches(&format!("viewer::{connect}")).count(), 1, "only Prepared's Connect answer");
    }

    #[test]
    fn native_auth_or_startup_failure_never_releases_a_checked_game() {
        static CHECKED: AtomicBool = AtomicBool::new(false);
        // `main_menu`: the failure comes after the publish, with W20 already done.
        for main_menu in [false, true] {
            CHECKED.store(false, Ordering::SeqCst);
            let (mut prepared, dir) = fixture(Tool::Windower);
            prepared.check = |_| { CHECKED.store(true, Ordering::SeqCst); (Ok("matches".into()), Default::default()) };
            let (auth, outcome) = DeferAuth::new();
            let got = Arc::new(Mutex::new(Vec::new()));
            let mut loader = None;
            let mut spawn = |_: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
                let (name, got) = (name.to_owned(), got.clone());
                loader = Some(std::thread::spawn(move || {
                    let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                    let mut writer = file.try_clone().unwrap();
                    let mut reader = BufReader::new(file);
                    writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"viewer_check":true}}"#, std::process::id()).unwrap();
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    got.lock().unwrap().push(std::mem::take(&mut line));
                    writeln!(writer, r#"{{"type":"authenticated"}}"#).unwrap();
                    if main_menu {
                        writeln!(writer, r#"{{"type":"native_main_menu","code":1,"returned":false}}"#).unwrap();
                        wait_for(&CHECKED, true);
                    }
                    writeln!(writer, r#"{{"type":"error","message":"fixture native startup failed"}}"#).unwrap();
                    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                        got.lock().unwrap().push(std::mem::take(&mut line));
                    }
                }));
                Ok(Injector::Detached)
            };
            let error = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST).err().unwrap();
            assert!(error.contains("fixture native startup failed"), "{error}");
            loader.unwrap().join().unwrap();
            assert_eq!(CHECKED.load(Ordering::SeqCst), main_menu, "W20 only after the main menu");
            assert!(outcome.lock().unwrap().is_none());
            assert_eq!(lines(&got).len(), 1, "no native_start_game before all native services succeed");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// A loader from before the Connect W2U (16e26f3) passes every other hello check but never
    /// asks for the W2U, and the launcher sends none of its own: its :51240 would go out with no
    /// :54000 in front of it. Refused before the session line, so the loader never signs in.
    #[test]
    fn a_loader_without_the_viewer_check_is_refused_before_the_session_line() {
        let (prepared, dir) = fixture(Tool::Windower);
        let (auth, outcome) = DeferAuth::new();
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut loader = None;
        let mut spawn = |_: &Prepared, name: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let (name, got) = (name.to_owned(), got.clone());
            loader = Some(std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                writeln!(writer, r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"native_batch":true}}"#, std::process::id()).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
            }));
            Ok(Injector::Detached)
        };
        let error = run(prepared, auth, Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST).err().unwrap();
        assert!(error.contains("does not run the PlayOnline Viewer version check"), "{error}");
        loader.unwrap().join().unwrap();
        assert!(lines(&got).is_empty(), "no session line: {:?}", lines(&got));
        assert!(outcome.lock().unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_play_signs_in_idle_then_checks_the_version_and_only_then_starts_the_game() {
        for tool in [Tool::Windower, Tool::Vanilla] { check_native_play(tool); }
    }

    fn check_native_play(tool: Tool) {
        static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        ORDER.lock().unwrap().clear();
        let (mut prepared, dir) = fixture(tool);
        prepared.check = |_| {
            ORDER.lock().unwrap().push("check");
            (Ok("Game version 30260904_1 matches the official server.".into()), Default::default())
        };
        let (auth, outcome) = DeferAuth::new();
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(300), true, done.clone(), got.clone(), || {}));
            Ok(Injector::Detached)
        };
        let mut stages = Vec::new();
        let result = run(prepared, auth, Timeline::begin(), &mut |s: &str| stages.push(s.to_owned()), &mut spawn, test_turns(), FAST).unwrap();
        // No check went out before the sign-in finished (the loader's idle report came 300 ms
        // after the session line, and the check ran after it, before the start line).
        let received = lines(&got);
        assert_eq!(received.len(), 2, "{received:?}");
        let session: serde_json::Value = serde_json::from_str(&received[0]).unwrap();
        assert_eq!(session["start_game"], false, "a Play signs in idle");
        assert_eq!(received[1], "{\"type\":\"native_start_game\"}\n");
        assert_eq!(*ORDER.lock().unwrap(), vec!["check"]);
        assert_eq!(*outcome.lock().unwrap(), Some(None), "the start was released");
        assert!(result.game_running.load(Ordering::SeqCst) && result.alive.load(Ordering::SeqCst));
        assert!(result.detail.contains(&format!("started through {}", tool.name())), "{}", result.detail);
        let position = |what: &str| stages.iter().position(|s| s.contains(what)).unwrap_or_else(|| panic!("{what}: {stages:?}"));
        assert!(position("Signing in to PlayOnline") < position("Checking the game version") && position("Checking the game version") < position("Starting FFXI"));
        let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
        let at = |what: &str| log.find(what).unwrap_or_else(|| panic!("{what}: {log}"));
        assert!(at("session delivered") < at("checking the FFXI version") && at("checking the FFXI version") < at("Game start sent") && at("Game start sent") < at("game started"), "{log}");
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_update_required_check_leaves_the_session_signed_in_without_a_start() {
        let (mut prepared, dir) = fixture(Tool::Windower);
        prepared.check = |_| (Err(format!("{} installed 30260904_1, official server 30260905_0. Open Downloads.", crate::patch::UPDATE_REQUIRED)), Default::default());
        let (auth, outcome) = DeferAuth::new();
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(50), false, done.clone(), got.clone(), || {}));
            Ok(Injector::Detached)
        };
        let result = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), FAST).unwrap();
        // Reported like a signed-in session with FFXI closed, carrying the update-required text
        // that the account row shows.
        assert!(result.detail.starts_with(crate::patch::UPDATE_REQUIRED), "{}", result.detail);
        let shown = crate::patch::parse_update_required(&result.detail).unwrap();
        assert_eq!((shown.local_version.as_str(), shown.remote_version.as_str()), ("30260904_1", "30260905_0"));
        assert!(!result.game_running.load(Ordering::SeqCst));
        assert!(result.alive.load(Ordering::SeqCst), "the session stays signed in");
        assert!(outcome.lock().unwrap().as_ref().unwrap().as_deref().unwrap().starts_with(crate::patch::UPDATE_REQUIRED));
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        let received = lines(&got);
        assert_eq!(received.len(), 1, "only the session line: {received:?}");
        assert!(!received.iter().any(|l| l.contains("native_start_game")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_play_whose_sign_in_outlasts_the_start_wait_still_checks_then_starts() {
        static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        let (mut prepared, dir) = fixture(Tool::Windower);
        prepared.check = |_| {
            ORDER.lock().unwrap().push("check");
            (Ok("matches".into()), Default::default())
        };
        let (auth, outcome) = DeferAuth::new();
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(900), true, done.clone(), got.clone(), || {}));
            Ok(Injector::Detached)
        };
        let slow = Timeouts { start: Duration::from_millis(300), ..FAST };
        let result = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, test_turns(), slow).unwrap();
        assert!(result.detail.contains("not yet confirmed"), "{}", result.detail);
        assert!(ORDER.lock().unwrap().is_empty(), "no check before the idle session is reported");
        let end = Instant::now() + Duration::from_secs(10);
        while lines(&got).len() < 2 {
            assert!(Instant::now() < end, "the supervisor never started the game: {:?}", lines(&got));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(lines(&got)[1], "{\"type\":\"native_start_game\"}\n");
        assert_eq!(*ORDER.lock().unwrap(), vec!["check"]);
        assert_eq!(*outcome.lock().unwrap(), Some(None));
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    /// Launch all: every account signs in idle and starts after its own check, while Windower
    /// still holds its turn only until the loader's hello.
    #[test]
    fn launch_all_plays_each_start_after_their_own_check_and_windower_keeps_one_turn_at_a_time() {
        let turns = test_turns();
        let shaking = Arc::new(Overlap::default());
        let done = Arc::new(AtomicBool::new(false));
        let began = Instant::now();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (shaking, done) = (shaking.clone(), done.clone());
                std::thread::spawn(move || {
                    let (prepared, dir) = fixture(Tool::Windower);
                    let (auth, _) = DeferAuth::new();
                    let got = Arc::new(Mutex::new(Vec::new()));
                    let mut loader = None;
                    let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                        shaking.begin();
                        let hello = shaking.clone();
                        loader = Some(deferred_loader(pipe.to_owned(), Duration::from_millis(250), true, done.clone(), got.clone(), move || {
                            std::thread::sleep(Duration::from_millis(120));
                            hello.end();
                        }));
                        Ok(Injector::Detached)
                    };
                    let result = run(prepared, auth, Timeline::begin(), &mut |_: &str| {}, &mut spawn, turns, FAST).unwrap();
                    (result, loader.unwrap(), got, dir)
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(shaking.most(), 1, "the Windower turn ends at the hello, never later");
        // Four 120 ms hellos one at a time, with the 250 ms idle waits overlapping.
        assert!(began.elapsed() < Duration::from_millis(2500), "{:?}", began.elapsed());
        done.store(true, Ordering::SeqCst);
        for (result, loader, got, dir) in results {
            assert!(result.game_running.load(Ordering::SeqCst), "every account's game started");
            assert!(lines(&got)[1].contains("native_start_game"));
            wait_for(&result.alive, false);
            loader.join().unwrap();
            let _ = fs::remove_dir_all(dir);
        }
    }

    // ---- the parked warm host (pol-spec 07d §1.3-1.4, §5.2) ----

    fn park_hello(park: bool) -> String {
        format!(r#"{{"type":"hello","v":1,"pid":{},"native_auth":true,"native_social":true,"native_batch":true,"viewer_check":true,"park":{park}}}"#, std::process::id())
    }

    fn retail_account() -> Account {
        Account { label: "Parked".into(), mode: crate::backend::AccountMode::Retail, server: String::new(), pol_id: "ABCD1234".into(),
            pol_password: "synthetic-pw".into(), se_id: "synthetic".into(), se_password: "synthetic-se".into(), region: 0,
            launch_with: None, ashita_boot_ini: None, windower_profile: None, pol_handle_names: String::new() }
    }

    fn parked_plan(trigger: crate::launch_timing::LaunchTrigger) -> Plan {
        Plan { tool: Tool::Windower, settings: Default::default(), social_only: false, auto_away_minutes: 10,
            auto_login: AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger }
    }

    /// 07d §1.4, §5.2: the parked host gets only the `park` line until Play; at Play the
    /// `credentials` line, then exactly the cold flow (W2U answered, W20 after the main menu,
    /// the deferred start), with the trigger and `parked for` lines in the launch log.
    #[test]
    fn a_parked_host_gets_credentials_only_at_play_then_signs_in_like_a_cold_one() {
        let (mut prepared, dir) = fixture(Tool::Windower);
        prepared.viewer_check = || crate::viewer::ConnectCheck { verdict: crate::viewer::ConnectVerdict::Current { version: "20110829_E".into() }, timing: Default::default() };
        prepared.check = |_| (Ok("Game version matches.".into()), Default::default());
        let windower = Arc::new(AtomicBool::new(false));
        let (done, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let (name, done, got) = (pipe.to_owned(), done.clone(), got.clone());
            let parked_tx = parked_tx.clone();
            loader = Some(std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                writeln!(writer, "{}", park_hello(true)).unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                writeln!(writer, r#"{{"type":"parked","pid":{},"polcore_ms":420}}"#, std::process::id()).unwrap();
                parked_tx.send(()).unwrap();
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                writeln!(writer, r#"{{"type":"native_viewer_check"}}"#).unwrap();
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                for event in [r#"{"type":"authenticated"}"#, r#"{"type":"native_main_menu","code":1,"returned":false}"#, r#"{"type":"native_session_state","game":false}"#] {
                    writeln!(writer, "{event}").unwrap();
                }
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                writeln!(writer, r#"{{"type":"native_session_state","game":true}}"#).unwrap();
                while !done.load(Ordering::SeqCst) { std::thread::sleep(Duration::from_millis(20)); }
                writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
                while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
            }));
            Ok(Injector::Fake(windower.clone()))
        };
        let parked = park::park_prepared(prepared, "Main", AutoLogin { enabled: true, character_slot: 2 }, Timeline::begin(),
            &mut |_| {}, &mut spawn, test_turns(), FAST, Duration::from_secs(5)).unwrap();
        parked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(parked.info.polcore_ms, Some(420));
        assert_eq!(parked.info.loader_pid, std::process::id());
        std::thread::sleep(Duration::from_millis(100));
        let before = lines(&got);
        assert_eq!(before.len(), 1, "nothing but the park line before Play: {before:?}");
        let park: serde_json::Value = serde_json::from_str(&before[0]).unwrap();
        assert_eq!((park["type"].as_str(), park["auto_login"]["character_slot"].as_u64()), (Some("park"), Some(2)));
        for secret in ["native_auth", "start_game", "pol_password", "otp"] { assert!(!before[0].contains(secret), "{secret}"); }
        assert!(windower.load(Ordering::SeqCst), "the verified injector closes before any profile-save wait");
        let config = Config { experiment: dir.clone(), repository: dir.clone(), game_install: None, dat_overlays: Vec::new() };
        let result = parked.play(&config, &parked_plan(crate::launch_timing::LaunchTrigger::ParkedPlay), retail_account(),
            Zeroizing::new(String::new()), Instant::now(), &mut |_| {}, |_, _, _| panic!("the parked host was alive")).unwrap();
        assert!(result.detail.contains("started through Windower"), "{}", result.detail);
        done.store(true, Ordering::SeqCst);
        wait_for(&result.alive, false);
        loader.unwrap().join().unwrap();
        let sent = lines(&got);
        let credentials: serde_json::Value = serde_json::from_str(&sent[1]).unwrap();
        assert_eq!(credentials["type"], "credentials");
        assert_eq!(credentials["start_game"], false, "a Play signs in idle first");
        assert!(credentials["native_auth"]["pol_id"].is_string() && credentials.get("install_dir").is_none());
        assert_eq!(sent[2], "{\"type\":\"native_viewer_check_result\",\"ok\":true}\n");
        assert_eq!(sent[3], "{\"type\":\"native_start_game\"}\n");
        let log = fs::read_to_string(dir.join("a.crystal.log")).unwrap();
        let at = |what: &str| log.find(what).unwrap_or_else(|| panic!("{what}: {log}"));
        assert!(at("[launcher] park start: Windower 4 Main") < at("loader hello") && at("loader hello") < at("Host parked after"), "{log}");
        assert!(at("Host parked after") < at("launch clock: ") && at("launch clock: ") < at("launch trigger: parked Play")
            && at("launch trigger: parked Play") < at("parked for ") && at("parked for ") < at("credentials delivered")
            && at("credentials delivered") < at("checking the FFXI version") && at("checking the FFXI version") < at("Game start sent"), "{log}");
        assert!(!log.contains("synthetic-pw") && !log.contains("c3ludGhldGlj"), "{log}");
        fs::remove_dir_all(dir).unwrap();
    }

    /// 07d §1.4: no Play is coming. Windower is stopped first, then the loader's pipe closes;
    /// nothing else reached the loader.
    #[test]
    fn a_park_that_ends_before_play_stops_windower_first_and_sends_nothing() {
        let (prepared, dir) = fixture(Tool::Windower);
        let windower = Arc::new(AtomicBool::new(false));
        let (got, order) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let (name, got, order, stopped) = (pipe.to_owned(), got.clone(), order.clone(), windower.clone());
            loader = Some(std::thread::spawn(move || {
                let windower = stopped;
                let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                writeln!(writer, "{}", park_hello(true)).unwrap();
                writeln!(writer, r#"{{"type":"parked","pid":{}}}"#, std::process::id()).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
                // The pipe closed: was Windower already stopped?
                *order.lock().unwrap() = Some(windower.load(Ordering::SeqCst));
            }));
            Ok(Injector::Fake(windower.clone()))
        };
        let mut parked = park::park_prepared(prepared, "", AutoLogin::default(), Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST, Duration::from_secs(5)).unwrap();
        assert!(parked.watch(Duration::from_millis(50)).is_none(), "still parked");
        parked.kill();
        loader.unwrap().join().unwrap();
        assert_eq!(*order.lock().unwrap(), Some(true), "Windower.exe is stopped before the host");
        assert_eq!(lines(&got).len(), 1, "only the park line: {:?}", lines(&got));
        assert!(fs::read_to_string(dir.join("a.crystal.log")).unwrap().contains("Nothing was sent."));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Finding: a park the window closes while it is still starting stops itself, never
    /// leaving Windower.exe running: before the tool starts nothing is spawned; while waiting
    /// for the loader, or for `parked`, Windower is stopped first and nothing but the park
    /// line ever reached the loader.
    #[test]
    fn a_park_closed_while_starting_stops_windower_itself() {
        for phase in ["before spawn", "connect", "parked"] {
            let (prepared, dir) = fixture(Tool::Windower);
            let abort: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(phase == "before spawn")));
            let windower = Arc::new(AtomicBool::new(false));
            let (spawned, got) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
            let mut loader = None;
            let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                spawned.store(true, Ordering::SeqCst);
                let (name, got) = (pipe.to_owned(), got.clone());
                if phase == "parked" {
                    loader = Some(std::thread::spawn(move || {
                        let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                        let mut writer = file.try_clone().unwrap();
                        let mut reader = BufReader::new(file);
                        writeln!(writer, "{}", park_hello(true)).unwrap();
                        let mut line = String::new();
                        while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
                    }));
                }
                Ok(Injector::Fake(windower.clone()))
            };
            // The window closes the park 150 ms in, whatever it is waiting for.
            let closer = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(150)); abort.store(true, Ordering::Release); });
            let started = Instant::now();
            let limits = Timeouts { connect: Duration::from_secs(20), abort: Some(abort), ..FAST };
            let result = park::park_prepared(prepared, "", AutoLogin::default(), Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), limits, Duration::from_secs(20));
            closer.join().unwrap();
            let error = result.err().unwrap();
            assert!(error.starts_with(park::ABORTED), "{phase}: {error}");
            assert!(started.elapsed() < Duration::from_secs(5), "{phase}: stopped promptly, not at a limit");
            assert_eq!(spawned.load(Ordering::SeqCst), phase != "before spawn", "{phase}");
            assert_eq!(windower.load(Ordering::SeqCst), phase != "before spawn", "{phase}: Windower is stopped by the park itself");
            if let Some(loader) = loader { loader.join().unwrap(); }
            assert!(lines(&got).iter().all(|line| line.contains("\"type\":\"park\"")), "{phase}: {:?}", lines(&got));
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// A park closed while another Windower start holds the machine-wide lock stops waiting at
    /// once with `park::ABORTED`: nothing is spawned and the other start keeps its lock.
    #[test]
    fn a_park_closed_while_waiting_for_the_windower_lock_aborts_without_breaking_it() {
        let (prepared, dir) = fixture(Tool::Windower);
        let lock = dir.join("locks").join("windower.lock");
        let held = crate::session_worker::windower_turn_at(&lock, &mut |_| {}, &|| None, Duration::from_secs(1)).unwrap();
        let abort: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let gate_lock = lock.clone();
        let gate = move |tool: Tool, progress: &mut dyn FnMut(&str), aborted: &dyn Fn() -> bool| {
            assert_eq!(tool, Tool::Windower);
            windower_gate_at(&gate_lock, progress, aborted)
        };
        let spawned = AtomicBool::new(false);
        let mut spawn = |_: &Prepared, _: &str, _: &mut SessionLog| -> Result<Injector, String> {
            spawned.store(true, Ordering::SeqCst);
            Err("never spawned".into())
        };
        let waited = Arc::new(AtomicBool::new(false));
        let saw = waited.clone();
        let closer = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(150)); abort.store(true, Ordering::Release); });
        let started = Instant::now();
        let limits = Timeouts { abort: Some(abort), ..FAST };
        let result = park::park_prepared(prepared, "", AutoLogin::default(), Timeline::begin(),
            &mut |line| if line.contains("Windower") { saw.store(true, Ordering::SeqCst) }, &mut spawn, &gate, limits, Duration::from_secs(20));
        closer.join().unwrap();
        let error = result.err().unwrap();
        assert!(error.starts_with(park::ABORTED), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2), "stopped at the close, not at the lock's limit");
        assert!(waited.load(Ordering::SeqCst), "it was waiting for the turn");
        assert!(!spawned.load(Ordering::SeqCst), "nothing started");
        assert!(crate::session_worker::windower_turn_at(&lock, &mut |_| {}, &|| None, Duration::from_millis(50)).is_err(), "the lock is still held");
        drop(held);
        assert!(crate::session_worker::windower_turn_at(&lock, &mut |_| {}, &|| None, Duration::from_millis(500)).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    /// 07d §1.6: a hello without `park` is closed as unsupported before any line; a loader
    /// that never parks, or ends while parking, fails the park; one that ends while parked is
    /// seen by `watch`.
    #[test]
    fn park_failures_close_the_host_and_say_why() {
        for case in ["unsupported", "error", "ended", "parked then ended"] {
            let (prepared, dir) = fixture(Tool::Vanilla);
            let got = Arc::new(Mutex::new(Vec::new()));
            let mut loader = None;
            let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
                let (name, got) = (pipe.to_owned(), got.clone());
                loader = Some(std::thread::spawn(move || {
                    let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                    let mut writer = file.try_clone().unwrap();
                    let mut reader = BufReader::new(file);
                    writeln!(writer, "{}", park_hello(case != "unsupported")).unwrap();
                    let mut line = String::new();
                    if case != "unsupported" {
                        reader.read_line(&mut line).unwrap();
                        got.lock().unwrap().push(std::mem::take(&mut line));
                    }
                    match case {
                        "error" => writeln!(writer, r#"{{"type":"error","message":"polcore could not be created"}}"#).unwrap(),
                        "parked then ended" => {
                            writeln!(writer, r#"{{"type":"parked","pid":{}}}"#, std::process::id()).unwrap();
                            return;
                        }
                        _ => {}
                    }
                    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) { got.lock().unwrap().push(std::mem::take(&mut line)); }
                }));
                Ok(Injector::Detached)
            };
            let result = park::park_prepared(prepared, "", AutoLogin::default(), Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST, Duration::from_millis(300));
            match case {
                "unsupported" => assert_eq!(result.err().unwrap(), park::UNSUPPORTED),
                "error" => assert_eq!(result.err().unwrap(), "polcore could not be created"),
                "ended" => assert!(result.err().unwrap().contains("did not park within"), "never parked"),
                _ => {
                    let mut parked = result.unwrap();
                    assert!(parked.watch(Duration::from_secs(5)).is_some(), "a host that ends while parked is reported");
                    parked.kill();
                }
            }
            loader.unwrap().join().unwrap();
            let sent = lines(&got);
            assert!(sent.iter().all(|line| line.contains("\"type\":\"park\"")), "{case}: {sent:?}");
            assert_eq!(sent.len(), usize::from(case != "unsupported"), "{case}");
            fs::remove_dir_all(dir).unwrap();
        }
    }

    /// 07d §1.4: a host lost after frame 2 and before the credentials went out never got
    /// them; the same worker launches cold instead.
    #[test]
    fn a_parked_host_lost_before_the_credentials_falls_back_cold() {
        let (prepared, dir) = fixture(Tool::Vanilla);
        let got = Arc::new(Mutex::new(Vec::new()));
        let (gone_tx, gone_rx) = std::sync::mpsc::channel::<()>();
        let mut loader = None;
        let mut spawn = |_: &Prepared, pipe: &str, _: &mut SessionLog| -> Result<Injector, String> {
            let (name, got, gone_tx) = (pipe.to_owned(), got.clone(), gone_tx.clone());
            loader = Some(std::thread::spawn(move || {
                let file = OpenOptions::new().read(true).write(true).open(name).unwrap();
                let mut writer = file.try_clone().unwrap();
                let mut reader = BufReader::new(file);
                writeln!(writer, "{}", park_hello(true)).unwrap();
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                got.lock().unwrap().push(std::mem::take(&mut line));
                writeln!(writer, r#"{{"type":"parked","pid":{}}}"#, std::process::id()).unwrap();
                // The host dies while parked.
                drop(writer);
                drop(reader);
                gone_tx.send(()).unwrap();
            }));
            Ok(Injector::Detached)
        };
        let parked = park::park_prepared(prepared, "", AutoLogin::default(), Timeline::begin(), &mut |_| {}, &mut spawn, test_turns(), FAST, Duration::from_secs(5)).unwrap();
        gone_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        loader.unwrap().join().unwrap();
        let config = Config { experiment: dir.clone(), repository: dir.clone(), game_install: None, dat_overlays: Vec::new() };
        let cold = std::cell::Cell::new(false);
        let error = parked.play(&config, &parked_plan(crate::launch_timing::LaunchTrigger::ParkedPlay), retail_account(),
            Zeroizing::new("123456".into()), Instant::now(), &mut |_| {}, |account, otp, _| {
                assert_eq!((account.pol_password.as_str(), otp.as_str()), ("synthetic-pw", "123456"), "the same credentials go cold");
                cold.set(true);
                Err("fixture cold launch".into())
            }).err().unwrap();
        assert_eq!(error, "fixture cold launch");
        assert!(cold.get());
        assert_eq!(lines(&got).len(), 1, "the credentials never reached the dead host");
        assert!(fs::read_to_string(dir.join("a.crystal.log")).unwrap().contains("parked host lost before Play"));
        fs::remove_dir_all(dir).unwrap();
    }
}
