//! A detached coordinator for a multi-account Play or a Play during sign-in. Workers
//! still own authentication, keepalive, and independent game/session exits.
use super::*;
use std::{collections::HashSet, sync::mpsc};

const BATCH_PREFIX: &str = "CRYSTAL-BATCH ";
const BATCH_FRAME: usize = 512 * 1024;
const PREPARE_LIMIT: Duration = Duration::from_secs(180);

#[derive(Serialize, Deserialize)]
pub(crate) struct BatchAccount {
    pub key: String, pub config: Config, pub account: Account, pub otp: String, pub via: Plan,
    #[serde(default)]
    existing_worker: Option<Identity>,
    /// The existing pre-sign-in took the account's one-time code: its failure is never
    /// followed by a cold sign-in here, which would go without a code (07d §5.4).
    #[serde(default)]
    otp_spent: bool,
    /// Set by the coordinator when the batch timed out: a sign-in still running may report
    /// the session it already has, but never starts another one (no reissue).
    #[serde(skip)]
    timed_out: Arc<AtomicBool>,
}
impl BatchAccount {
    pub(crate) fn new(key: String, config: Config, account: Account, otp: String, via: Plan) -> Self {
        Self { key, config, account, otp, via, existing_worker: None, otp_spent: false, timed_out: Arc::default() }
    }

    pub(crate) fn with_otp_spent(mut self, spent: bool) -> Self {
        self.otp_spent = spent;
        self
    }

    pub(crate) fn existing(key: String, config: Config, account: Account, otp: String, via: Plan, session: &BackgroundSession) -> Self {
        let mut job = Self::new(key, config, account, otp, via);
        job.existing_worker = Some(session.worker);
        job
    }

    fn existing_session(&self) -> Result<Option<BackgroundSession>, String> {
        match self.existing_worker {
            Some(worker) => {
                let session = find(&self.config, &self.account).filter(|session| session.worker == worker)
                    .ok_or_else(|| "The batch account's original worker ended or changed.".to_owned())?;
                // A running game is monitored, never started again by this batch.
                // A preference edited during play must not block other accounts.
                if session.game_running() != Some(true) { session.ensure_launch_plan(&self.via)?; }
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }
}
impl Drop for BatchAccount { fn drop(&mut self) { self.otp.zeroize(); } }
#[derive(Serialize, Deserialize)]
struct BatchRequest {
    jobs: Vec<BatchAccount>, elapsed_ms: u64,
    #[serde(default)]
    clicked_clock: Option<crate::launch_timing::MonotonicStamp>,
}
#[derive(Serialize, Deserialize)]
enum BatchEvent {
    Progress(String, String),
    Ready(String, Identity, Started),
    Failed(String, String),
    Done,
}
fn emit_batch(event: &BatchEvent) {
    if let Ok(line) = serde_json::to_string(event) { let _ = writeln!(io::stdout().lock(), "{BATCH_PREFIX}{line}"); }
}

/// Credentials cross an anonymous pipe once. This observer may disappear:
/// the coordinator owns the gate and continues independently of the window.
pub(crate) fn launch_together(
    jobs: Vec<BatchAccount>, clicked: Instant,
    mut progress: impl FnMut(&str, &str),
    mut complete: impl FnMut(&str, Result<LaunchResult, String>),
) -> Result<(), String> {
    let command = self_command("--batch-worker").map_err(|_| "Could not locate Crystal's batch worker.")?;
    launch_batch_command(command, jobs, clicked, &mut progress, &mut complete)
}

fn launch_batch_command(
    mut command: Command, mut jobs: Vec<BatchAccount>, clicked: Instant,
    progress: &mut dyn FnMut(&str, &str), complete: &mut dyn FnMut(&str, Result<LaunchResult, String>),
) -> Result<(), String> {
    // Local readiness only; each new sign-in's worker runs its own W2U at Connect (07c §3).
    prepare_dispatch(&mut jobs, || crate::viewer::gate(&crate::viewer::local_status()))?;
    let paths: std::collections::HashMap<_, _> = jobs.iter().map(|j| (j.key.clone(), record_path(&j.config, &j.account))).collect();
    let request = BatchRequest { jobs, elapsed_ms: clicked.elapsed().as_millis().min(u64::MAX as u128) as u64,
        clicked_clock: crate::launch_timing::MonotonicStamp::capture(clicked) };
    let bytes = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| "Could not encode the batch request.")?);
    drop(request);
    if bytes.len() > BATCH_FRAME { return Err("The batch request is too large.".into()); }
    let mut child = spawn_hidden(&mut command).map_err(|_| "Could not start Crystal's batch worker.")?;
    let sent = child.stdin.take().ok_or_else(|| io::Error::other("Missing batch input"))
        .and_then(|mut input| write_frame(&mut input, &bytes));
    drop(bytes);
    if sent.is_err() { let _ = child.kill(); let _ = child.wait(); return Err("Could not hand the batch to its worker.".into()); }
    let mut output = BufReader::new(child.stdout.take().ok_or("Missing batch output.")?);
    loop {
        let event = match next_line(&mut output, BATCH_PREFIX).map_err(|_| "Could not read batch progress.")? {
            Line::Eof => return Err("The batch worker ended before completing the game starts.".into()),
            Line::TooLarge => return Err("Batch progress is too large.".into()),
            Line::Other => continue,
            Line::Invalid => return Err("Invalid batch progress.".into()),
            Line::Event(event) => event,
        };
        match event {
            BatchEvent::Progress(key, line) => progress(&key, &line),
            BatchEvent::Ready(key, worker, started) => {
                let path = paths.get(&key).ok_or("Unexpected batch account.")?;
                complete(&key, track(path.clone(), worker, started));
            }
            BatchEvent::Failed(key, error) => complete(&key, Err(error)),
            BatchEvent::Done => return Ok(()),
        }
    }
}
fn prepare_dispatch(jobs: &mut [BatchAccount], viewer_gate: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    validate_jobs(jobs)?;
    // Preserve any lifetime required by the caller and capture the other live
    // workers now. Losing either must never turn this into a cold sign-in.
    for job in jobs.iter_mut() {
        if job.existing_worker.is_some() { job.existing_session()?; }
        else { job.existing_worker = find(&job.config, &job.account).map(|session| session.worker); }
    }
    if jobs.iter().any(|job| job.existing_worker.is_none()) { viewer_gate()?; }
    Ok(())
}
fn validate_jobs(jobs: &[BatchAccount]) -> Result<(), String> {
    let mut keys = HashSet::new(); let mut accounts = HashSet::new();
    if !(1..=32).contains(&jobs.len()) || jobs.iter().any(|j| j.key.is_empty() || j.key.len() > 128
        || !keys.insert(&j.key) || !accounts.insert(record_path(&j.config, &j.account))) {
        Err("Choose one to thirty-two different accounts for a batch.".into())
    } else { Ok(()) }
}

pub(crate) fn run_batch() -> i32 {
    private_std_handles();
    run_batch_with(prepare)
}
fn run_batch_with(prepare: fn(BatchAccount, Instant) -> Result<Prepared, String>) -> i32 {
    let result = (|| -> Result<(), String> {
        let mut input = io::stdin().lock();
        // The coordinator's error text goes nowhere: it exits 1.
        let request: BatchRequest = read_frame_limited(&mut input, BATCH_FRAME).map_err(|_| "Invalid batch request.")?;
        drop(input);
        validate_jobs(&request.jobs)?;
        let clicked = crate::launch_timing::MonotonicStamp::restore(request.clicked_clock, request.elapsed_ms);
        coordinate(request.jobs, clicked, prepare, emit_batch);
        Ok(())
    })();
    if result.is_ok() { 0 } else { 1 }
}

/// The account's W20; its phases go to the account's launch log (the `&Path`).
type VersionCheck = fn(&Config, &Path) -> Result<String, String>;
/// `version`: the account's W20, still to run (`verify`) once every account is prepared.
struct Prepared { job: BatchAccount, session: BackgroundSession, launch: LaunchResult, check: Result<(), String>, start_game: bool, version: Option<VersionCheck> }
pub(crate) fn batch_pending(config: &Config, account: &Account) -> bool {
    read_bounded(&config.account_directory(account).join("batch-owner.json"), 1024)
        .and_then(|b| serde_json::from_slice::<Identity>(&b).ok())
        .is_some_and(|owner| identity(owner.pid) == Some(owner))
}
/// Each account's batch.lock and batch-owner.json, released per account.
struct Ownership { held: Vec<Owned> }
struct Owned { key: String, _lock: File, owner: PathBuf }
impl Drop for Owned { fn drop(&mut self) { let _ = fs::remove_file(&self.owner); } }
impl Ownership {
    fn acquire(jobs: &[BatchAccount]) -> Result<Self, String> {
        let owner = identity(std::process::id()).ok_or("Could not identify the batch worker.")?;
        let mut ownership = Self { held: Vec::new() };
        for job in jobs {
            if job.existing_session()?.is_some_and(|session| session.game_running() == Some(true)) { continue; }
            let directory = job.config.account_directory(&job.account);
            let lock = exclusive(&directory.join("batch.lock")).map_err(|_| "An account already belongs to a pending batch or game start.")?;
            // A terminated worker can leave a command behind. Its identity
            // makes that command inert; it must not prevent a fresh batch.
            if find_in(&directory).is_some()
                && (directory.join("native-control.json").exists() || directory.join("native-social.json").exists()) {
                return Err("Wait for the account's pending PlayOnline action to finish.".into());
            }
            let path = directory.join("batch-owner.json");
            save_json(&path, &owner)?;
            ownership.held.push(Owned { key: job.key.clone(), _lock: lock, owner: path });
        }
        Ok(ownership)
    }

    /// Releases every account except `keep`.
    fn keep_only(&mut self, keep: &[String]) { self.held.retain(|owned| keep.contains(&owned.key)); }

    fn release(&mut self, key: &str) { self.held.retain(|owned| owned.key != key); }
}
fn prepare(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
    prepare_with(job, clicked, check_version)
}
fn check_version(config: &Config, log: &Path) -> Result<String, String> {
    let mut timing = None;
    let result = check_version_with(config, |install| {
        let (check, phases) = crate::patch::check_before_launch_timed(install);
        timing = Some(phases);
        check
    });
    // W20 phases on every path (08 §4 item 6).
    if let Some(timing) = timing {
        crate::launch_log::append_launcher_lines(log, &crate::launch_timing::check_lines("FFXI check (W20)", &timing));
    }
    result
}
fn check_version_with(config: &Config, check: impl FnOnce(&Path) -> crate::patch::LaunchCheck) -> Result<String, String> {
    let install = crate::pol_less::install_directory(config)?;
    // A batch uses the same fresh-check policy as one Play: known updates block,
    // and an incomparable version remains explicitly unverified in the returned note.
    crate::patch::gate(check(&install))
}
fn report_version(key: &str, log: &Path, result: &Result<String, String>, mut emit: impl FnMut(&BatchEvent)) {
    let note = match result { Ok(note) | Err(note) => note };
    crate::launch_log::append_launcher_lines(log, std::slice::from_ref(note));
    let progress = if result.is_ok() {
        format!("{note} Ready; waiting for the other selected accounts…")
    } else { note.clone() };
    emit(&BatchEvent::Progress(key.into(), progress));
}
fn prepare_with(mut job: BatchAccount, clicked: Instant, check_version: VersionCheck) -> Result<Prepared, String> {
    let deadline = Instant::now() + PREPARE_LIMIT;
    // A batch's own sign-in is a cold Play: no restored Viewer pauses (07c §4).
    let mut plan = job.via.clone(); plan.social_only = true; plan.viewer_gaps = false;
    let otp = Zeroizing::new(std::mem::take(&mut job.otp));
    let existing = job.existing_session()?;
    if let Some(session) = existing.as_ref().filter(|session| session.game_running() == Some(true)) {
        let launch = session.clone().reconnect(|line| emit_batch(&BatchEvent::Progress(job.key.clone(), line.into())))?;
        return Ok(Prepared { session: session.clone(), job, launch, check: Ok(()), start_game: false, version: None });
    }
    let progress = |line: &str| emit_batch(&BatchEvent::Progress(job.key.clone(), line.into()));
    let mut reissued = false;
    let launch = match existing {
        Some(session) => {
            // A Play during this pre-sign-in: the loader skips the pauses still to come.
            let _ = note_play_pending(&job.config, &job.account);
            match session.reconnect(progress) {
                Ok(launch) => {
                    let lines = crate::pol_less::launch_clock_lines(&crate::launch_timing::Timeline::begin_at(clicked), job.via.trigger);
                    crate::launch_log::append_launcher_lines(&launch.log, &lines);
                    launch
                }
                // 07d §5.4: the pre-sign-in failed before giving a verdict on the account (no
                // network, or its host went away before authentication): this Play signs in
                // once, cold. A refused password, OTP or update never costs a second login,
                // and neither does a spent one-time code.
                Err(error) if reissue_allowed(&error) && !job.otp_spent
                    && !(crate::launch_log::transient_failure(launch_log_message(&error)) && reached_authentication(&job.config, &job.account)) => {
                    if crate::launch_log::network_failure(launch_log_message(&error)) && !wait_for_network(deadline) { return Err(error); }
                    // A batch that already reported its failure starts no new sign-in.
                    if job.timed_out.load(Ordering::Acquire) || Instant::now() >= deadline { return Err(error); }
                    reissued = true;
                    plan.trigger = crate::launch_timing::LaunchTrigger::ReissuedPlay;
                    launch_preflighted(&job.config, job.account.clone(), otp, Some(plan), clicked, progress)?
                }
                Err(error) => return Err(error),
            }
        }
        None => launch_preflighted(&job.config, job.account.clone(), otp, Some(plan), clicked, progress)?,
    };
    let session = find(&job.config, &job.account).ok_or("The prepared session ended.")?;
    if !reissued && job.existing_worker.is_some_and(|worker| worker != session.worker) {
        return Err("The batch account's original worker changed.".into());
    }
    // pol-spec 07c §5.2: W20 only once this account's Viewer main menu is up and idle,
    // never on authentication (an early :53001 beside mail and presence is an order stock
    // cannot produce, 07a presign-fast-path (1)). Also for a Play during a pre-sign-in.
    // The W20 itself waits for the other accounts (`verify`).
    let idle = main_menu(&session, &launch, deadline);
    if let Err(error) = &idle { report_version(&job.key, &launch.log, &Err(error.clone()), emit_batch); }
    Ok(Prepared { job, session, launch, version: idle.is_ok().then_some(check_version), check: idle, start_game: true })
}

fn launch_log_message(error: &str) -> &str { crate::launch_log::split_log(error).0 }

/// A failed pre-sign-in may be followed by one cold sign-in for the queued Play only when it
/// never reached a verdict on the account (07d §5.4): no network, or a host that went away.
pub(crate) fn reissue_allowed(error: &str) -> bool {
    let message = launch_log_message(error);
    crate::patch::parse_update_required(message).is_none()
        && (crate::launch_log::network_failure(message) || crate::launch_log::transient_failure(message))
}

fn main_menu(session: &BackgroundSession, launch: &LaunchResult, deadline: Instant) -> Result<(), String> {
    let directory = session.directory();
    loop {
        if !launch.alive.load(Ordering::Acquire) || identity(session.worker.pid) != Some(session.worker) { return Err("The batch account's worker ended.".into()); }
        if let Some(snapshot) = read_friend_cache(directory, session.worker) {
            if snapshot.game_running == Some(true) { return Err("This account already has a game running.".into()); }
            if let Some(error) = snapshot.session_error { return Err(error); }
        }
        if main_menu_idle(directory, session.worker) { return Ok(()); }
        if Instant::now() >= deadline { return Err("Timed out waiting for the account to finish signing in.".into()); }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Barrier 1, before any account's W20: its idle main menu again (immediate while it stays
/// there). A sign-in that fails meanwhile leaves no account with a passed check and no
/// :54001 after it (stock always follows a passing W20 into the launch), and no account's
/// W20 waits on another account's busy session.
fn reconfirm(item: &mut Prepared, until: Instant) {
    if item.version.is_none() { return; }
    if let Err(error) = main_menu(&item.session, &item.launch, until) {
        report_version(&item.job.key, &item.launch.log, &Err(error.clone()), emit_batch);
        item.version = None;
        item.check = Err(error);
    }
}

/// Barrier 2: the account's W20 (:53001), run together with the other accounts' once every
/// one reconfirmed its idle main menu. Its verdict is this account's own: an account whose
/// check passed still starts when another account's check asks for an update.
fn verify(item: &mut Prepared) {
    let Some(check_version) = item.version.take() else { return };
    emit_batch(&BatchEvent::Progress(item.job.key.clone(), t("Checking the game version...").into()));
    let mut check = check_version(&item.job.config, &item.launch.log);
    // The window's update spaces its first connection from this W20's FIN (phase 2).
    if check.as_ref().is_err_and(|message| crate::patch::is_update_refusal(message)) {
        crate::patch::save_update_close(item.session.directory());
    }
    if find_in(item.session.directory()).is_none_or(|now| now.worker != item.session.worker) {
        check = Err("The batch account's worker changed during its version check.".into());
    }
    report_version(&item.job.key, &item.launch.log, &check, emit_batch);
    item.check = check.map(|_| ());
}

fn coordinate(
    jobs: Vec<BatchAccount>, clicked: Instant,
    prepare: fn(BatchAccount, Instant) -> Result<Prepared, String>,
    mut emit: impl FnMut(&BatchEvent),
) {
    let keys: Vec<_> = jobs.iter().map(|j| j.key.clone()).collect();
    let ownership = match Ownership::acquire(&jobs) {
        Ok(ownership) => ownership,
        Err(error) => { for key in keys { emit(&BatchEvent::Failed(key, error.clone())); } emit(&BatchEvent::Done); return; }
    };
    let (sender, receiver) = mpsc::channel();
    let timed_out = Arc::new(AtomicBool::new(false));
    for mut job in jobs {
        job.timed_out = timed_out.clone();
        let sender = sender.clone(); let key = job.key.clone();
        let _ = std::thread::Builder::new().name("batch-sign-in".into()).spawn(move || {
            let result = prepare(job, clicked);
            let _ = sender.send((key, result));
        });
    }
    drop(sender);
    coordinate_results(keys, ownership, receiver, &timed_out, PREPARE_LIMIT, STRAGGLER_LIMIT, emit);
}

/// After a batch timed out, how long its sign-ins still running are waited for, so that a
/// session that does come up is reported (and tracked by the window) instead of staying
/// signed in behind a row that says the batch failed. Covers a full Windower turn.
const STRAGGLER_LIMIT: Duration = Duration::from_secs(120);

fn coordinate_results(
    keys: Vec<String>, mut ownership: Ownership, receiver: mpsc::Receiver<(String, Result<Prepared, String>)>,
    timed_out: &AtomicBool, prepare_limit: Duration, straggler_limit: Duration, mut emit: impl FnMut(&BatchEvent),
) {
    let deadline = Instant::now() + prepare_limit;
    // The batch's first failure, and the account it is about when it is one account's.
    let mut prepared = Vec::new(); let mut error: Option<(Option<String>, String)> = None;
    let mut received = HashSet::new();
    for _ in &keys {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((key, Ok(item))) => {
                if let Err(reason) = &item.check { error.get_or_insert((Some(key.clone()), reason.clone())); }
                received.insert(key); prepared.push(item);
            }
            Ok((key, Err(reason))) => { received.insert(key.clone()); error.get_or_insert((Some(key), reason)); }
            Err(_) => {
                timed_out.store(true, Ordering::Release);
                error.get_or_insert((None, "A batch account did not become ready in time.".into()));
                break;
            }
        }
    }
    if error.is_none() {
        // Every account is at its idle main menu. Confirm that again for all of them
        // before the first W20, then run the W20s together.
        let until = Instant::now() + MAIN_MENU_LIMIT;
        std::thread::scope(|scope| { for item in prepared.iter_mut() { scope.spawn(move || reconfirm(item, until)); } });
        if let Some(item) = prepared.iter().find(|item| item.check.is_err()) {
            error = item.check.clone().err().map(|reason| (Some(item.job.key.clone()), reason));
        }
    }
    if error.is_none() {
        std::thread::scope(|scope| { for item in prepared.iter_mut() { scope.spawn(move || verify(item)); } });
        // Each W20 verdict is that account's own: the accounts whose check passed start.
        let starts: Vec<_> = prepared.iter().filter(|item| item.start_game && item.check.is_ok()).collect();
        if !starts.is_empty() {
            if let Err(reason) = release_prepared(&starts) { error = Some((None, reason)); }
        }
    }
    // Sign-ins the timeout left running keep their account's batch ownership until they report.
    let unreported: Vec<String> = keys.iter().filter(|key| !received.contains(*key)).cloned().collect();
    ownership.keep_only(&unreported);
    // Keep monitors for every live prepared session, also on a failed check.
    let started = |item: &Prepared, error: &Option<(Option<String>, String)>| error.is_none() && item.check.is_ok();
    for item in &prepared {
        emit(&BatchEvent::Ready(item.job.key.clone(), item.session.worker, Started {
            pid: item.launch.pid,
            detail: if !item.start_game { "FFXI is already running for this account.".into() }
                else if started(item, &error) { "FFXI started with the selected accounts.".into() } else { item.launch.detail.clone() },
            log: item.launch.log.clone(), timing: item.launch.timing.clone(),
        }));
    }
    let own_failure = |key: &str| prepared.iter().find(|item| item.job.key == key).and_then(|item| item.check.clone().err());
    let mut stragglers = Vec::new();
    match &error {
        Some((source, error)) => {
            let others = others_failure(error);
            for key in &keys {
                if prepared.iter().any(|item| &item.job.key == key && !item.start_game) { continue; }
                // A sign-in still running when the batch timed out reports when it ends.
                if !received.contains(key) { stragglers.push(key.clone()); continue; }
                let message = own_failure(key).unwrap_or_else(|| if source.as_ref() == Some(key) { error.clone() } else { others.clone() });
                emit(&BatchEvent::Failed(key.clone(), message));
            }
        }
        None => for key in &keys {
            if let Some(reason) = own_failure(key) { emit(&BatchEvent::Failed(key.clone(), reason)); }
        },
    }
    // Sign-ins the timeout left running: report each one as it ends, so a session that
    // did come up has a monitor in the window, then the batch's failure.
    let until = Instant::now() + straggler_limit;
    let failure = error.map(|(source, error)| if source.is_some() { others_failure(&error) } else { error }).unwrap_or_default();
    while !stragglers.is_empty() {
        let Ok((key, result)) = receiver.recv_timeout(until.saturating_duration_since(Instant::now())) else { break };
        let Some(at) = stragglers.iter().position(|k| *k == key) else { continue };
        stragglers.remove(at);
        ownership.release(&key);
        match &result {
            Ok(item) => {
                emit(&BatchEvent::Ready(key.clone(), item.session.worker, Started {
                    pid: item.launch.pid, detail: item.launch.detail.clone(),
                    log: item.launch.log.clone(), timing: item.launch.timing.clone(),
                }));
                emit(&BatchEvent::Failed(key, failure.clone()));
            }
            // Its own reason (a refused password, a network failure) is the row's message.
            Err(reason) => emit(&BatchEvent::Failed(key, reason.clone())),
        }
    }
    drop(ownership);
    for key in stragglers { emit(&BatchEvent::Failed(key, failure.clone())); }
    emit(&BatchEvent::Done);
}

/// What the other accounts of a batch are told when one account's failure stops it. The same
/// words, so that an update its W20 asked for, for example, starts every Play again; except a
/// refused member login, which is a verdict on that one account only: the window learns from it
/// that the account may need a one-time code, and must not learn that of the others.
fn others_failure(error: &str) -> String {
    if crate::launch_log::sign_in_refused(launch_log_message(error)) {
        t("Another selected account's PlayOnline sign-in was refused, so FFXI was not started.").to_owned()
    } else {
        error.to_owned()
    }
}

fn release_prepared(prepared: &[&Prepared]) -> Result<(), String> {
    let id = crate::pol_less::pipe::random_hex().map_err(|_| "Could not create the batch gate.")?;
    let (gate, tokens) = start_gate::Coordinator::new(&id, prepared.len())?;
    let mut members: Vec<_> = prepared.iter().map(|item| Member::new(item)).collect();
    // File controls are bound to exact worker lifetimes. The native gate itself
    // belongs to this detached coordinator, not the UI or any one game.
    for (member, token) in members.iter_mut().zip(tokens) {
        if !member.healthy() { return Err("A batch session ended before preparation.".into()); }
        save_json(&member.directory.join("native-batch.json"), &BatchControl { worker: member.item.session.worker, token })?;
        super::wake::signal(member.item.session.worker);
    }
    let until = Instant::now() + start_gate::LIMIT;
    while !gate.ready() {
        if gate.canceled() { return Err("A prepared game canceled the batch.".into()); }
        if !members.iter_mut().all(Member::healthy) { return Err("A batch session ended while preparing its game.".into()); }
        if Instant::now() >= until { return Err("Timed out preparing all games; the batch was canceled.".into()); }
        std::thread::sleep(RELEASE_POLL);
    }
    if !members.iter_mut().all(Member::healthy) { return Err("A batch session ended before release.".into()); }
    gate.release()?;
    // Released: the participants signal `started` within their own 10 ms poll. This wait has
    // its own bound, and the gate stays open through it: dropping it cancels a participant
    // that has not seen the release yet.
    let started_until = Instant::now() + RELEASED_LIMIT;
    while !gate.started() {
        // A timeout keeps the existing release-failure message (no new user-facing text).
        if !members.iter_mut().all(Member::healthy) || Instant::now() >= started_until {
            return Err("A prepared game ended during batch release.".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(gate);
    Ok(())
}

/// How often the release wait looks at the gate and at each member's processes.
const RELEASE_POLL: Duration = Duration::from_millis(25);
/// How long released games have to report `started`.
const RELEASED_LIMIT: Duration = Duration::from_secs(5);

/// One released account: its directory resolved once, its friends cache read again only
/// when the file changes.
struct Member<'a> { item: &'a Prepared, directory: PathBuf, friends: Option<std::time::SystemTime>, session_error: bool }
impl<'a> Member<'a> {
    fn new(item: &'a Prepared) -> Self {
        Self { item, directory: item.job.config.account_directory(&item.job.account), friends: None, session_error: false }
    }
    fn healthy(&mut self) -> bool {
        let modified = fs::metadata(self.directory.join("friends.json")).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified != self.friends {
            self.session_error = read_friend_cache(&self.directory, self.item.session.worker).is_some_and(|s| s.session_error.is_some());
            self.friends = modified;
        }
        self.item.launch.alive.load(Ordering::Acquire) && identity(self.item.session.worker.pid) == Some(self.item.session.worker)
            && identity(self.item.launch.pid).is_some() && !self.session_error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static FIXTURE_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("crystal-batch-{}", crate::pol_less::pipe::random_hex().unwrap()));
        fs::create_dir_all(&path).unwrap(); path
    }
    fn job(root: &Path, key: &str) -> BatchAccount {
        BatchAccount::new(key.into(),
            Config { experiment: root.into(), repository: root.into(), game_install: None, dat_overlays: Vec::new() },
            Account { label: key.into(), mode: backend::AccountMode::Retail, server: String::new(), pol_id: key.into(), pol_password: "fixture-secret".into(), se_id: String::new(), se_password: String::new(), region: 0, launch_with: None, ashita_boot_ini: None, windower_profile: None, pol_handle_names: String::new() },
            "123456".into(), Plan { tool: crate::pol_less::Tool::Windower, settings: Default::default(), social_only: false, auto_away_minutes: 10, auto_login: crate::pol_less::AutoLogin::default(), keep_session: false, viewer_gaps: false, trigger: Default::default() })
    }
    fn watch_fixture_start(job: &BatchAccount) {
        let directory = job.config.account_directory(&job.account);
        fs::create_dir_all(&directory).unwrap();
        let stop = job.config.experiment.join("stop-fixture");
        let started = job.config.experiment.join(format!("{}-started", job.key));
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until && !stop.exists() {
                if let Some(bytes) = read_bounded(&directory.join("native-batch.json"), 1024) {
                    let request: BatchControl = serde_json::from_slice(&bytes).unwrap();
                    // The actual native owners signal this only after all setup.
                    if start_gate::Participant::open(&request.token).and_then(|gate| gate.wait()).is_ok() {
                        fs::write(started, []).unwrap();
                        FIXTURE_STARTED.fetch_add(1, Ordering::SeqCst);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
    }
    fn fixture_prepare(job: BatchAccount, _: Instant) -> Result<Prepared, String> {
        if job.key == "cold" { std::thread::sleep(Duration::from_millis(40)); }
        let worker = identity(std::process::id()).unwrap();
        watch_fixture_start(&job);
        let alive = Arc::new(AtomicBool::new(job.key != "dead"));
        let check = if job.key == "fail" { Err("Missing game version.".into()) } else { Ok(()) };
        Ok(Prepared { session: BackgroundSession { path: record_path(&job.config, &job.account), worker },
            launch: LaunchResult { pid: worker.pid, detail: "Fixture idle session".into(), alive, game_running: Arc::new(AtomicBool::new(false)), outcome: Arc::new(Mutex::new(None)), log: root_log(&job), timing: String::new() }, job, check, start_game: true, version: None })
    }
    fn root_log(job: &BatchAccount) -> PathBuf { job.config.experiment.join("fixture.log") }
    fn until(mut test: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !test() { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5)); }
    }
    #[test]
    fn mixed_warm_cold_batch_releases_only_after_both_prepare() {
        let root = root(); let mut events = Vec::new();
        coordinate(vec![job(&root, "warm"), job(&root, "cold")], Instant::now(), fixture_prepare, |e| events.push(serde_json::to_string(e).unwrap()));
        until(|| root.join("warm-started").exists() && root.join("cold-started").exists());
        assert_eq!(events.iter().filter(|e| e.contains("Ready")).count(), 2);
        assert!(!events.iter().any(|e| e.contains("Failed")));
        assert!(!batch_pending(&job(&root,"warm").config, &job(&root,"warm").account));
    }

    #[test]
    fn one_running_account_does_not_enter_or_block_the_other_five_start_gates() {
        static CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn prepare_six(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
            let prepared = prepare_with(job, clicked, |_, _| {
                CHECKS.fetch_add(1, Ordering::SeqCst);
                Ok("Fixture version is current.".into())
            })?;
            if prepared.start_game { watch_fixture_start(&prepared.job); }
            Ok(prepared)
        }
        CHECKS.store(0, Ordering::SeqCst);
        let root = root();
        let worker = identity(std::process::id()).unwrap();
        let keys = ["running", "second", "third", "fourth", "fifth", "sixth"];
        let mut jobs = Vec::new();
        let mut paths = Vec::new();
        for key in keys {
            let request = job(&root, key);
            let path = record_path(&request.config, &request.account);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let log = root.join(format!("{key}.log")); fs::write(&log, []).unwrap();
            save_record(&path, &Record { worker, state: Event::Started(Started {
                pid: worker.pid, detail: "Existing session".into(), log, timing: String::new(),
            }), social_only: true, authenticated: true, native_batch: true, otp_used: Some(false),
                launch_choice: None, launch_tool: Some(crate::pol_less::Tool::Windower) }).unwrap();
            let mut snapshot = crate::friends::Snapshot { game_running: Some(key == "running"), ..Default::default() };
            snapshot.social.ready = true;
            publish_friends(path.parent().unwrap(), &snapshot).unwrap();
            paths.push(path); jobs.push(request);
        }
        // A changed next-launch preference and a queued social action on the
        // running account must not become conditions for starting the other five.
        jobs[0].via.tool = crate::pol_less::Tool::Vanilla;
        let running_directory = paths[0].parent().unwrap();
        let queued = b"existing running account action";
        fs::write(running_directory.join("native-control.json"), queued).unwrap();
        prepare_dispatch(&mut jobs, || panic!("all six workers already exist")).unwrap();
        let mut events = Vec::new();
        coordinate(jobs, Instant::now(), prepare_six, |event| events.push(serde_json::to_string(event).unwrap()));
        until(|| keys[1..].iter().all(|key| root.join(format!("{key}-started")).exists()));
        assert_eq!(CHECKS.load(Ordering::SeqCst), 5);
        assert_eq!(events.iter().filter(|event| event.contains("Ready")).count(), 6);
        assert!(!events.iter().any(|event| event.contains("Failed")), "{events:?}");
        assert!(!root.join("running-started").exists());
        assert!(!running_directory.join("native-batch.json").exists());
        assert_eq!(fs::read(running_directory.join("native-control.json")).unwrap(), queued);
        fs::write(root.join("stop-fixture"), []).unwrap();
        for path in paths { update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap(); }
    }
    fn signing_in_record(job: &BatchAccount) -> Identity {
        let worker = identity(std::process::id()).unwrap();
        let path = record_path(&job.config, &job.account);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        save_record(&path, &Record { worker, state: Event::Progress("Fixture signing in".into()),
            social_only: true, authenticated: false, native_batch: true, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
        worker
    }

    /// Finding: a pre-sign-in that spent the account's one-time code, or lost its host after
    /// authentication, is never followed by the batch's own cold sign-in: that login would go
    /// without a code, or be a second one.
    #[test]
    fn a_failed_presign_that_spent_its_code_or_authenticated_is_not_signed_in_again() {
        for case in ["one-time code", "authenticated"] {
            let root = root();
            let mut request = job(&root, "ABCD1234");
            request.otp = String::new();
            let worker = signing_in_record(&request);
            request.existing_worker = Some(worker);
            let request = request.with_otp_spent(case == "one-time code");
            let path = record_path(&request.config, &request.account);
            let failing = path.clone();
            // The pre-sign-in's host goes away while the batch waits on it.
            let fail = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                update_record(&failing, worker, |record| {
                    record.authenticated = case == "authenticated";
                    record.state = Event::Failed("The background session worker ended before starting the game.".into());
                }).unwrap();
            });
            let error = prepare_with(request, Instant::now(), |_, _| panic!("no version check")).err().unwrap();
            fail.join().unwrap();
            assert!(error.starts_with("The background session worker ended"), "{case}: {error}");
            assert!(!path.with_extension("lock").exists(), "{case}: no cold sign-in was started");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn batch_reuse_refuses_an_injected_host_for_vanilla_before_any_version_check() {
        let root = root(); let mut request = job(&root, "ABCD1234");
        let worker = signing_in_record(&request);
        request.existing_worker = Some(worker);
        let path = record_path(&request.config, &request.account);
        update_record(&path, worker, |record| record.launch_tool = Some(crate::pol_less::Tool::Windower)).unwrap();
        request.via.tool = crate::pol_less::Tool::Vanilla;
        let error = prepare_with(request, Instant::now(), |_, _| panic!("must stop before checking the version")).err().unwrap();
        assert!(error.contains("before switching launch methods"), "{error}");
        assert!(!path.parent().unwrap().join("native-batch.json").exists());
        update_record(&path, worker, |record| record.launch_tool = Some(crate::pol_less::Tool::Vanilla)).unwrap();
        let mut request = job(&root, "ABCD1234");
        request.existing_worker = Some(worker);
        request.via.tool = crate::pol_less::Tool::Vanilla;
        assert!(request.existing_session().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }
    struct ReplacementWorker { child: std::process::Child, worker: Identity }
    impl ReplacementWorker {
        fn new() -> Self {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "session_worker::batch::tests::identity_fixture", "--ignored"])
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let worker = identity(child.id()).unwrap();
            Self { child, worker }
        }
    }
    impl Drop for ReplacementWorker { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }
    #[test]
    #[ignore = "offline identity subprocess only"]
    fn identity_fixture() { let _ = io::stdin().read(&mut [0]); }
    #[test]
    fn dispatch_skips_viewer_for_warm_jobs_but_gates_a_mixed_batch_once() {
        let root = root();
        let mut jobs = vec![job(&root, "first"), job(&root, "second")];
        let worker = signing_in_record(&jobs[0]);
        signing_in_record(&jobs[1]);
        prepare_dispatch(&mut jobs, || panic!("existing sessions must not query the Viewer")).unwrap();
        let serialized = Zeroizing::new(serde_json::to_vec(&jobs).unwrap());
        let captured: Vec<BatchAccount> = serde_json::from_slice(&serialized).unwrap();
        assert!(captured.iter().all(|job| job.existing_worker == Some(worker)));
        fs::remove_file(record_path(&jobs[1].config, &jobs[1].account)).unwrap();
        let mut jobs = vec![job(&root, "first"), job(&root, "second")];
        let mut checks = 0;
        let result = prepare_dispatch(&mut jobs, || { checks += 1; Err("Fixture Viewer update required".into()) });
        assert_eq!(checks, 1);
        assert_eq!(result, Err("Fixture Viewer update required".into()));
        assert_eq!(jobs[0].existing_worker, Some(worker));
        assert_eq!(jobs[1].existing_worker, None);
    }
    #[test]
    fn a_worker_required_by_the_caller_cannot_be_reclassified_before_dispatch() {
        for replaced in [false, true] {
            let root = root();
            let mut selected = job(&root, "existing");
            let worker = signing_in_record(&selected);
            let session = find(&selected.config, &selected.account).unwrap();
            let mut jobs = vec![BatchAccount::existing(selected.key.clone(), selected.config.clone(),
                selected.account.clone(), std::mem::take(&mut selected.otp), selected.via.clone(), &session)];
            let path = record_path(&jobs[0].config, &jobs[0].account);
            let replacement = replaced.then(ReplacementWorker::new);
            if replaced {
                save_record(&path, &Record { worker: replacement.as_ref().unwrap().worker,
                    state: Event::Progress("Replacement worker".into()), social_only: true,
                    authenticated: false, native_batch: true, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
                assert_ne!(find(&jobs[0].config, &jobs[0].account).unwrap().worker, worker);
            } else { fs::remove_file(&path).unwrap(); }
            let error = prepare_dispatch(&mut jobs, || panic!("a selected worker must not become cold"))
                .expect_err("the caller's original worker is required");
            assert!(error.contains("original worker ended or changed"), "{error}");
            assert_eq!(jobs[0].existing_worker, Some(worker));
            assert!(!path.with_extension("lock").exists());
        }
    }
    #[test]
    fn an_existing_worker_that_disappears_or_changes_never_becomes_a_cold_launch() {
        for replaced in [false, true] {
            let root = root();
            let mut jobs = vec![job(&root, "existing")];
            let worker = signing_in_record(&jobs[0]);
            prepare_dispatch(&mut jobs, || panic!("the existing worker needs no Viewer query")).unwrap();
            let path = record_path(&jobs[0].config, &jobs[0].account);
            let replacement = replaced.then(ReplacementWorker::new);
            if replaced {
                save_record(&path, &Record { worker: replacement.as_ref().unwrap().worker,
                    state: Event::Progress("Replacement worker".into()), social_only: true,
                    authenticated: false, native_batch: true, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
                assert_ne!(find(&jobs[0].config, &jobs[0].account).unwrap().worker, worker);
            } else { fs::remove_file(&path).unwrap(); }
            let error = prepare_with(jobs.remove(0), Instant::now(), |_, _| panic!("must stop before the version check"))
                .err().expect("the dispatched worker must still exist");
            assert!(error.contains("original worker ended or changed"), "{error}");
            assert!(!path.with_extension("lock").exists(), "no replacement session worker was started");
            assert!(!path.parent().unwrap().join("native-batch.json").exists());
        }
    }
    #[test]
    fn batch_allows_explicitly_unverified_versions_and_a_known_update_blocks_only_its_account() {
        fn unverified_check(config: &Config, _: &Path) -> Result<String, String> {
            check_version_with(config, |install| {
                let mut report = crate::patch::local_report(install);
                assert_eq!(report.local_version, "30260904_1");
                report.detail = "Fixture official server is unreachable.".into();
                crate::patch::LaunchCheck::Unverified(report)
            })
        }
        fn update_check(config: &Config, _: &Path) -> Result<String, String> {
            check_version_with(config, |_| crate::patch::LaunchCheck::Update(crate::patch::PatchReport {
                state: crate::patch::PatchState::Required,
                local_version: "30260904_1".into(), remote_version: "30260905_0".into(),
                detail: "Official fixture reply requires an update.".into(), files_verified: None,
            }))
        }
        fn prepare_policy(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
            let update = job.key == "update";
            // The actual native gate protocol is still exercised by fixture participants;
            // prepare_with owns reconnect, full idle, the version check and its reporting.
            let fixture = fixture_prepare(job, clicked)?;
            prepare_with(fixture.job, clicked, if update { update_check } else { unverified_check })
        }
        for update in [false, true] {
            let root = root();
            fs::write(root.join("FFXi.dll"), []).unwrap();
            fs::write(root.join("FFXiMain.dll"), []).unwrap();
            // Unreachable servers allow a readable installed version, while
            // missing/corrupt records now require initialization/repair.
            let record = crate::patch::encode_applied_record("30260904_1", crate::patch::applied_seed(&root).unwrap()).unwrap();
            fs::write(root.join("patch.ver"), record).unwrap();
            let mut config = job(&root, "warm").config.clone();
            config.game_install = Some(root.clone());
            let single_note = unverified_check(&config, Path::new("unused.log")).unwrap();
            assert!(single_note.contains("could not be compared") && !single_note.contains("matches"));
            let keys = ["warm", if update { "update" } else { "unknown" }];
            let worker = identity(std::process::id()).unwrap();
            let mut jobs = Vec::new(); let mut records = Vec::new();
            for key in keys {
                let mut job = job(&root, key); job.config.game_install = Some(root.clone());
                let path = record_path(&job.config, &job.account);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let log = root.join(format!("{key}.crystal.log")); fs::write(&log, []).unwrap();
                save_record(&path, &Record { worker, state: Event::Started(Started {
                    pid: worker.pid, detail: "Existing native idle fixture".into(), log, timing: String::new(),
                }), social_only: true, authenticated: true, native_batch: true, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
                let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
                snapshot.social.ready = true;
                publish_friends(path.parent().unwrap(), &snapshot).unwrap();
                records.push(path); jobs.push(job);
            }
            let mut errors = Vec::new();
            coordinate(jobs, Instant::now(), prepare_policy, |event| {
                if let BatchEvent::Failed(_, error) = event { errors.push(error.clone()); }
            });
            if update {
                // Owner decision 2026-11-09: each W20 verdict is its own account's. The account
                // whose check passed starts; only the refused one is told to update.
                assert_eq!(errors.len(), 1);
                assert!(errors.iter().all(|error| error.starts_with(crate::patch::UPDATE_REQUIRED)));
                until(|| root.join("warm-started").exists());
                assert!(!root.join("update-started").exists());
                assert!(fs::read_to_string(root.join("update.crystal.log")).unwrap().contains(crate::patch::UPDATE_REQUIRED));
            } else {
                assert!(errors.is_empty(), "{errors:?}");
                until(|| root.join("warm-started").exists() && root.join("unknown-started").exists());
                assert!(fs::read_to_string(root.join("unknown.crystal.log")).unwrap().contains(&single_note));
            }
            assert!(fs::read_to_string(root.join("warm.crystal.log")).unwrap().contains(&single_note));
            fs::write(root.join("stop-fixture"), []).unwrap();
            for path in records { update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap(); }

            // The same reporting path carries the exact unverified note to GUI progress
            // and retains it in the account log instead of labeling it current.
            let log = root.join("progress.crystal.log"); fs::write(&log, []).unwrap();
            let mut progress = Vec::new();
            report_version("unknown", &log, &Ok(single_note.clone()), |event| {
                if let BatchEvent::Progress(key, note) = event { progress.push((key.clone(), note.clone())); }
            });
            assert_eq!(progress.len(), 1);
            assert_eq!(progress[0].0, "unknown");
            assert!(progress[0].1.starts_with(&single_note));
            assert!(fs::read_to_string(log).unwrap().contains(&single_note));
        }
    }
    #[test]
    fn refused_version_or_dead_worker_cancels_every_start_and_retains_monitors() {
        for failed in ["fail", "dead"] {
            let root = root(); let mut ready = 0; let mut errors = 0;
            coordinate(vec![job(&root, "warm"), job(&root, failed)], Instant::now(), fixture_prepare, |e| match e {
                BatchEvent::Ready(..) => ready += 1, BatchEvent::Failed(..) => errors += 1, _ => {},
            });
            fs::write(root.join("stop-fixture"), []).unwrap();
            assert_eq!((ready, errors), (2, 2));
            assert!(!root.join("warm-started").exists()); assert!(!root.join(format!("{failed}-started")).exists());
        }
    }
    #[test]
    fn an_account_that_fails_to_sign_in_leaves_no_other_account_with_a_passed_version_check() {
        static CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn prepare_one_fails(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
            if job.key == "wrong-otp" {
                // Fails long after the other account reached its idle main menu.
                std::thread::sleep(Duration::from_millis(80));
                return Err("Fixture one-time password refused.".into());
            }
            let mut prepared = fixture_prepare(job, clicked)?;
            prepared.version = Some(|_, _| { CHECKS.fetch_add(1, Ordering::SeqCst); Ok("Fixture version matches.".into()) });
            Ok(prepared)
        }
        CHECKS.store(0, Ordering::SeqCst);
        let root = root(); let (mut ready, mut errors) = (0, 0);
        coordinate(vec![job(&root, "warm"), job(&root, "wrong-otp")], Instant::now(), prepare_one_fails, |e| match e {
            BatchEvent::Ready(..) => ready += 1, BatchEvent::Failed(..) => errors += 1, _ => {},
        });
        fs::write(root.join("stop-fixture"), []).unwrap();
        assert_eq!(CHECKS.load(Ordering::SeqCst), 0, "no :53001 for an account that will not be started");
        assert_eq!((ready, errors), (1, 2));
        assert!(!root.join("warm-started").exists());
    }

    /// Review finding (2026-10-13): one account's refused member login stops the batch, and only
    /// that account is told its sign-in was refused. The others (one that got its session, one
    /// still signing in) are told in words that never read as their own refusal: the window marks
    /// a refused account as one that may need a one-time code. Other failures still reach every
    /// account as they are (an update the W20 asked for starts every Play again).
    #[test]
    fn a_refused_sign_in_is_reported_as_refused_to_its_own_account_only() {
        const REFUSED: &str = "Game loader error during startup: Native PlayOnline member login failed (state 39, POL result -1036). (Windower via crystal-loader).";
        fn prepare_refused(job: BatchAccount, clicked: Instant) -> Result<Prepared, String> {
            match job.key.as_str() {
                "refused" => { std::thread::sleep(Duration::from_millis(40)); Err(REFUSED.into()) }
                "slow" => { std::thread::sleep(Duration::from_millis(120)); Err("Timed out waiting for the account to finish signing in.".into()) }
                _ => fixture_prepare(job, clicked),
            }
        }
        crate::i18n::set(crate::i18n::Lang::En);
        let root = root(); let mut ready = Vec::new(); let mut failed = std::collections::BTreeMap::new();
        coordinate(vec![job(&root, "warm"), job(&root, "refused"), job(&root, "slow")], Instant::now(), prepare_refused, |e| match e {
            BatchEvent::Ready(key, ..) => ready.push(key.clone()),
            BatchEvent::Failed(key, error) => { failed.insert(key.clone(), error.clone()); }
            _ => {}
        });
        fs::write(root.join("stop-fixture"), []).unwrap();
        assert_eq!(ready, ["warm"]);
        assert_eq!(failed.len(), 3);
        assert_eq!(failed["refused"], REFUSED);
        for key in ["warm", "slow"] {
            assert_eq!(failed[key], "Another selected account's PlayOnline sign-in was refused, so FFXI was not started.");
            assert!(!crate::launch_log::sign_in_refused(&failed[key]));
        }
        assert!(!root.join("warm-started").exists());
        // Not a refusal: every account hears it as it is.
        let update = format!("{} installed 30260904_1, official server 30260905_0. Open Downloads.", crate::patch::UPDATE_REQUIRED);
        assert_eq!(others_failure(&update), update);
        assert_eq!(others_failure("Missing game version."), "Missing game version.");
        let older = "Game loader error during startup: Native PlayOnline login failed (state 39, POL result -1036).";
        assert_eq!(others_failure(older), older);
    }

    /// Audit 2026-11-09: on a batch timeout, a sign-in still running went on in an orphaned
    /// worker while its row said the batch failed. It is now reported when it ends: Ready
    /// (so the window tracks the session it got), then the batch's failure. Nothing starts.
    #[test]
    fn a_sign_in_still_running_at_the_timeout_is_reported_when_it_ends() {
        let root = root();
        let jobs = vec![job(&root, "warm"), job(&root, "slow")];
        let keys: Vec<_> = jobs.iter().map(|j| j.key.clone()).collect();
        let ownership = Ownership::acquire(&jobs).unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut jobs = jobs.into_iter();
        let (warm, slow) = (jobs.next().unwrap(), jobs.next().unwrap());
        let (warm_job, slow_job) = (job(&root, "warm"), job(&root, "slow"));
        sender.send(("warm".to_owned(), fixture_prepare(warm, Instant::now()))).unwrap();
        let timed_out = Arc::new(AtomicBool::new(false));
        let flag = timed_out.clone();
        let late = std::thread::spawn(move || {
            // A straggler sees the timeout before it reports (and so never reissues).
            until(|| flag.load(Ordering::Acquire));
            std::thread::sleep(Duration::from_millis(150));
            let _ = sender.send(("slow".to_owned(), fixture_prepare(slow, Instant::now())));
        });
        let mut events = Vec::new();
        coordinate_results(keys, ownership, receiver, &timed_out, Duration::from_millis(50), Duration::from_secs(10),
            |event| {
                let text = serde_json::to_string(event).unwrap();
                // The reported account's ownership is released; the straggler's is held until it reports.
                if text.contains("\"warm\"") && text.contains("Failed") {
                    assert!(!batch_pending(&warm_job.config, &warm_job.account));
                    assert!(batch_pending(&slow_job.config, &slow_job.account));
                }
                events.push(text);
            });
        late.join().unwrap();
        assert!(!batch_pending(&slow_job.config, &slow_job.account));
        fs::write(root.join("stop-fixture"), []).unwrap();
        let slow: Vec<_> = events.iter().filter(|event| event.contains("\"slow\"")).collect();
        assert_eq!(slow.len(), 2, "{events:?}");
        assert!(slow[0].contains("Ready") && slow[1].contains("did not become ready in time"), "{events:?}");
        assert!(events.last().unwrap().contains("Done"));
        assert!(!root.join("warm-started").exists() && !root.join("slow-started").exists());
    }

    /// A straggler whose sign-in failed reports its own reason, not the batch's timeout.
    #[test]
    fn a_straggler_that_failed_reports_its_own_reason() {
        let root = root();
        let jobs = vec![job(&root, "warm"), job(&root, "slow")];
        let keys: Vec<_> = jobs.iter().map(|j| j.key.clone()).collect();
        let ownership = Ownership::acquire(&jobs).unwrap();
        let (sender, receiver) = mpsc::channel();
        let warm = jobs.into_iter().next().unwrap();
        sender.send(("warm".to_owned(), fixture_prepare(warm, Instant::now()))).unwrap();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let _ = sender.send(("slow".to_owned(), Err("Wrong password.".to_owned())));
        });
        let mut failed = std::collections::HashMap::new();
        coordinate_results(keys, ownership, receiver, &AtomicBool::new(false), Duration::from_millis(50), Duration::from_secs(10),
            |event| if let BatchEvent::Failed(key, error) = event { failed.insert(key.clone(), error.clone()); });
        late.join().unwrap();
        fs::write(root.join("stop-fixture"), []).unwrap();
        assert_eq!(failed["slow"], "Wrong password.");
        assert!(failed["warm"].contains("did not become ready in time"));
    }

    #[test]
    fn ownership_blocks_overlap_and_controls_but_stale_owner_does_not() {
        let root = root(); let jobs = vec![job(&root, "warm"), job(&root, "cold")];
        let ownership = Ownership::acquire(&jobs).unwrap();
        assert!(batch_pending(&jobs[0].config, &jobs[0].account)); assert!(Ownership::acquire(&jobs).is_err());
        assert!(request_native_control(&jobs[0].config, &jobs[0].account, NativeControl::StartGame).is_err());
        drop(ownership); assert!(!batch_pending(&jobs[0].config, &jobs[0].account));
        save_json(&jobs[0].config.account_directory(&jobs[0].account).join("batch-owner.json"), &Identity { pid: std::process::id(), created: 0 }).unwrap();
        assert!(!batch_pending(&jobs[0].config, &jobs[0].account));
    }

    #[test]
    fn orphaned_commands_do_not_block_a_new_batch_but_live_commands_do() {
        let root = root();
        let jobs = vec![job(&root, "orphaned")];
        let directory = jobs[0].config.account_directory(&jobs[0].account);
        fs::create_dir_all(&directory).unwrap();
        let worker = identity(std::process::id()).unwrap();
        for name in ["native-control.json", "native-social.json"] {
            fs::write(directory.join(name), b"orphaned command").unwrap();
        }
        let path = record_path(&jobs[0].config, &jobs[0].account);
        save_record(&path, &Record { worker: Identity { created: 0, ..worker }, state: Event::Progress("old worker".into()),
            social_only: true, authenticated: false, native_batch: false, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
        drop(Ownership::acquire(&jobs).expect("a dead worker's files cannot execute in the replacement session"));
        save_record(&path, &Record { worker, state: Event::Progress("live worker".into()),
            social_only: true, authenticated: false, native_batch: false, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
        assert!(Ownership::acquire(&jobs).is_err(), "the live worker still owns its queued action");
        assert!(!batch_pending(&jobs[0].config, &jobs[0].account));
    }
    #[test]
    fn preparation_reuses_warm_and_still_signing_in_workers() {
        for signing_in in [false, true] {
            let root = root(); let job = job(&root, "warm");
            let worker = identity(std::process::id()).unwrap();
            let path = record_path(&job.config, &job.account);
            let started = Event::Started(Started { pid: worker.pid, detail: "Existing worker".into(), log: root.join("fixture.log"), timing: String::new() });
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            save_record(&path, &Record { worker, state: if signing_in { Event::Progress("Still signing in".into()) } else { started.clone() }, social_only: true, authenticated: false, native_batch: false, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
            let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
            snapshot.social.ready = true;
            publish_friends(path.parent().unwrap(), &snapshot).unwrap();
            if signing_in {
                let path = path.clone();
                std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(40)); update_record(&path, worker, |record| record.state = started).unwrap(); });
            }
            // A second process would reject the test executable's --session-worker;
            // reaching this injected version check proves exact-worker reconnect.
            let mut prepared = prepare_with(job, Instant::now(), |_, _| Ok("Fixture version matches.".into())).unwrap();
            assert!(prepared.version.is_some(), "the W20 waits for the batch");
            verify(&mut prepared);
            assert_eq!(prepared.launch.pid, worker.pid); assert!(prepared.check.is_ok() && prepared.version.is_none());
            assert!(!prepared.job.config.account_directory(&prepared.job.account).join("native-control.json").exists());
            update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap();
        }
    }
    #[test]
    fn one_account_play_checks_the_version_only_at_the_idle_main_menu() {
        type Signals = (mpsc::Sender<()>, mpsc::Receiver<Result<String, String>>, mpsc::Sender<()>);
        static SIGNALS: Mutex<Option<Signals>> = Mutex::new(None);
        static CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        for refused in [false, true] {
            CHECKS.store(0, Ordering::SeqCst);
            let root = root(); let job = job(&root, "pending-services");
            let worker = identity(std::process::id()).unwrap();
            let path = record_path(&job.config, &job.account);
            let directory = path.parent().unwrap().to_path_buf();
            fs::create_dir_all(&directory).unwrap();
            save_record(&path, &Record { worker, state: Event::Progress("fixture locale-independent sign-in".into()), social_only: true, authenticated: false, native_batch: true, otp_used: Some(false), launch_choice: None, launch_tool: None }).unwrap();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (verdict_tx, verdict_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            *SIGNALS.lock().unwrap() = Some((entered_tx, verdict_rx, finished_tx));
            let mut jobs = vec![job];
            prepare_dispatch(&mut jobs, || panic!("reuse the existing sign-in")).unwrap();
            let preparing = std::thread::spawn(move || {
                let mut events = Vec::new();
                coordinate(jobs, Instant::now(), |job, clicked| {
                    let prepared = prepare_with(job, clicked, |_, _| {
                        CHECKS.fetch_add(1, Ordering::SeqCst);
                        let (entered, verdict, finished) = SIGNALS.lock().unwrap().take().expect("one version request");
                        entered.send(()).unwrap();
                        let result = verdict.recv_timeout(Duration::from_secs(10)).unwrap();
                        finished.send(()).unwrap();
                        result
                    })?;
                    if prepared.check.is_ok() { watch_fixture_start(&prepared.job); }
                    Ok(prepared)
                }, |event| events.push(serde_json::to_string(event).unwrap()));
                events
            });
            // pol-spec 07c §5.2: no W20 on authentication, at the bare idle report, or while a
            // social request runs; only at the idle main menu (social.ready follows the publish).
            let no_check = |why: &str| assert!(matches!(entered_rx.recv_timeout(Duration::from_millis(60)), Err(mpsc::RecvTimeoutError::Timeout)), "{why}");
            no_check("the version check must wait for the main menu, not sign-in");
            publish_authenticated(&directory).unwrap();
            no_check("typed authentication starts no version check");
            let mut snapshot = crate::friends::Snapshot { game_running: Some(false), ..Default::default() };
            publish_friends(&directory, &snapshot).unwrap();
            update_record(&path, worker, |record| record.state = Event::Started(Started {
                pid: worker.pid, detail: "Native startup services complete".into(), log: root.join("fixture.log"), timing: String::new(),
            })).unwrap();
            no_check("idle without social readiness is not the main menu");
            snapshot.social.ready = true; snapshot.social.busy = true;
            publish_friends(&directory, &snapshot).unwrap();
            no_check("a busy native owner is not idle");
            snapshot.social.busy = false;
            publish_friends(&directory, &snapshot).unwrap();
            entered_rx.recv_timeout(Duration::from_secs(10)).expect("the check starts at the idle main menu");
            assert!(!directory.join("native-batch.json").exists(), "no start before the check's verdict");
            verdict_tx.send(if refused { Err("fixture version refusal".into()) } else { Ok("Fixture version matches.".into()) }).unwrap();
            finished_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let events = preparing.join().unwrap();
            assert_eq!(events.iter().any(|event| event.contains("Failed")), refused);
            assert_eq!(events.iter().filter(|event| event.contains("Ready")).count(), 1);
            assert_eq!(CHECKS.load(Ordering::SeqCst), 1);
            if !refused { until(|| root.join("pending-services-started").exists()); }
            assert_eq!(root.join("pending-services-started").exists(), !refused);
            assert!(!directory.join("native-control.json").exists());
            assert!(!path.with_extension("lock").exists(), "the original worker was reused");
            update_record(&path, worker, |record| record.state = Event::Finished(None)).unwrap();
        }
    }
    #[test]
    #[ignore = "offline batch subprocess only"]
    fn batch_fixture() {
        let code = run_batch_with(fixture_prepare);
        if code == 0 { until(|| FIXTURE_STARTED.load(Ordering::SeqCst) == 2); }
        // libtest prints its result to stdout; the real headless worker exits
        // directly and deliberately tolerates a closed observer pipe.
        std::process::exit(code);
    }
    #[test]
    fn batch_continues_after_its_window_observer_closes() {
        let root = root();
        let request = BatchRequest { jobs: vec![job(&root,"warm"), job(&root,"cold")], elapsed_ms: 0, clicked_clock: None };
        let bytes = Zeroizing::new(serde_json::to_vec(&request).unwrap());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session_worker::batch::tests::batch_fixture", "--ignored", "--nocapture"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap(); input.write_all(&bytes).unwrap(); drop(input);
        // Let libtest finish its own initial stdout banner before detaching;
        // the actual worker uses fallible writes throughout.
        until(|| batch_pending(&request.jobs[0].config, &request.jobs[0].account));
        drop(child.stdout.take()); // Same broken observer pipe as closing the window.
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(root.join("warm-started").exists()); assert!(root.join("cold-started").exists());
    }
}
