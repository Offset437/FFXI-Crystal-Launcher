//! One-shot, session-bound game shutdown. The graceful path uses no credentials,
//! process termination, window activation or POL controls to stop the game. The
//! instant path ("Close games instantly") ends only this session's own game host,
//! by the exact process identity its worker recorded, like Windower's //terminate.
use super::*;
use std::os::windows::io::{AsRawHandle, OwnedHandle};

/// What Crystal's shut-down action does (Settings → System → "Close games instantly").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The game's own /shutdown command and logout countdown.
    Graceful,
    /// End the game host at once. Its PlayOnline session ends with it.
    Instant,
}
impl Mode {
    pub(crate) fn from_setting(instant: bool) -> Self {
        if instant { Self::Instant } else { Self::Graceful }
    }
    pub(crate) fn run(self, session: &BackgroundSession) -> Result<(), String> {
        match self {
            Self::Graceful => request(session),
            Self::Instant => terminate(session),
        }
    }
}

/// Written just before the instant path ends `host`: the worker reads it when its loader
/// pipe closes, so the end is a normal close and not a crash (`terminated`).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Terminate {
    worker: Identity,
    host: Identity,
}
const TERMINATE_FILE: &str = "game-terminate.json";
const HOST_EXIT_LIMIT: Duration = Duration::from_secs(5);
const WORKER_EXIT_LIMIT: Duration = Duration::from_secs(15);

/// Crystal's own game host for `worker`: the loader image (`pol.exe`), started after it.
fn crystal_host(process: &OwnedHandle, host: Identity, worker: Identity) -> bool {
    host.created > worker.created
        && crate::win::process::image_path_of(process).ok().is_some_and(|path| {
            path.file_name().is_some_and(|name| name.eq_ignore_ascii_case("pol.exe"))
        })
}

pub(crate) fn terminate(session: &BackgroundSession) -> Result<(), String> {
    terminate_within(session, &crystal_host, HOST_EXIT_LIMIT, WORKER_EXIT_LIMIT)
}

fn terminate_within(
    session: &BackgroundSession,
    owned: &dyn Fn(&OwnedHandle, Identity, Identity) -> bool,
    host_limit: Duration,
    worker_limit: Duration,
) -> Result<(), String> {
    use crate::win::process::{created, open, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    use windows_sys::Win32::System::Threading::{TerminateProcess, PROCESS_TERMINATE};
    let refused = || t("Could not close FFXI.").to_owned();
    current(session)?;
    let record = read_record(&session.path)
        .filter(|record| record.worker == session.worker)
        .ok_or(t("The selected game session ended before shutdown could be requested."))?;
    let Event::Started(started) = record.state else {
        return Err(t("Wait for FFXI to finish starting before requesting shutdown.").into());
    };
    let directory = session.path.parent().ok_or(t("This game session has no account directory."))?;
    let _delivery = exclusive(&directory.join("game-shutdown-delivery.lock"))
        .map_err(|_| t("Shutdown is already being requested for this account."))?;
    // One handle is checked and terminated: a reused PID can never be the one ended.
    let process = open(started.pid, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)
        .map_err(|_| refused())?;
    let host = Identity { pid: started.pid, created: created(&process).ok_or_else(refused)? };
    if !owned(&process, host, session.worker) {
        return Err(refused());
    }
    let host_watch = ProcessWatch(process);
    if host_watch.ended()? {
        return Ok(());
    }
    let worker_watch = ProcessWatch::open(session.worker)?;
    let marker = directory.join(TERMINATE_FILE);
    save_json(&marker, &Terminate { worker: session.worker, host })?;
    let forget = || { let _ = fs::remove_file(&marker); };
    if let Err(error) = current(session) {
        forget();
        return Err(error);
    }
    if unsafe { TerminateProcess(host_watch.0.as_raw_handle() as _, 0) } == 0 && !host_watch.ended()? {
        forget();
        return Err(refused());
    }
    // The marker stays for the worker: it is bound to this worker and this host only.
    let until = Instant::now() + host_limit;
    while !host_watch.ended()? {
        if Instant::now() >= until { return Err(refused()); }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The worker records the normal end and closes; the window's monitor shows it either way.
    let until = Instant::now() + worker_limit;
    while !worker_watch.ended()? && session.active() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

/// In the worker: whether this session's game host was ended by the instant path. True at
/// most once, and only for this worker and the host its record names.
pub(crate) fn terminated(directory: &Path) -> bool {
    let path = directory.join(TERMINATE_FILE);
    let Some(marker) = read_bounded(&path, FRAME_LIMIT).and_then(|bytes| serde_json::from_slice::<Terminate>(&bytes).ok())
    else { return false };
    let Some(worker) = identity(std::process::id()) else { return false };
    let host = read_record(&directory.join("session.json"))
        .filter(|record| record.worker == worker)
        .and_then(|record| match record.state { Event::Started(started) => Some(started.pid), _ => None });
    let ours = marker.worker == worker && host == Some(marker.host.pid);
    if ours {
        let _ = fs::remove_file(&path);
    }
    ours
}

const FRAME_LIMIT: usize = 2048;
const CLOSE_LIMIT: Duration = Duration::from_secs(90);
const UNSUPPORTED: &str = "Background shutdown is unavailable for this session. Close FFXI and launch it again with the current Crystal build.";

struct ProcessWatch(std::os::windows::io::OwnedHandle);
impl ProcessWatch {
    fn open(expected: Identity) -> Result<Self, String> {
        // Deliberately no running check: a process that already exited is watched as ended.
        let (handle, identity) = open_identity(expected.pid)
            .ok_or_else(|| t("Could not watch the selected game's shutdown.").to_owned())?;
        if identity != expected {
            return Err(t("The selected game session ended before shutdown could be requested.").into());
        }
        Ok(Self(handle))
    }
    fn ended(&self) -> Result<bool, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::WaitForSingleObject,
        };
        match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(t("Could not confirm that FFXI and PlayOnline closed.").into()),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    worker: Identity,
    request_id: u64,
    game_id: u64,
    deadline_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    worker: Identity,
    request_id: u64,
    game_id: u64,
    ok: bool,
    message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ended {
    worker: Identity,
    game_id: u64,
}

/// Published only from the verified loader after GameStart and its callbacks
/// have returned. The native POL owner may remain signed in for another game.
pub(crate) fn ended(directory: &Path, game_id: u64) -> Result<(), String> {
    let session = owned_session(directory).ok_or("The game belongs to an inactive session.")?;
    if game_id == 0 { return Err("The ended game has no identity.".into()); }
    save_json(&directory.join("game-shutdown-ended.json"), &Ended { worker: session.worker, game_id })
}

fn game_ended(directory: &Path, worker: Identity, game_id: u64) -> bool {
    read_bounded(&directory.join("game-shutdown-ended.json"), FRAME_LIMIT)
        .and_then(|bytes| serde_json::from_slice::<Ended>(&bytes).ok())
        .is_some_and(|ended| ended.worker == worker && ended.game_id >= game_id)
}

fn reply_path(directory: &Path, id: u64) -> PathBuf {
    one_shot_reply_path(directory, "game-shutdown", id)
}
fn reply(directory: &Path, worker: Identity, id: u64) -> Option<Reply> {
    let reply: Reply =
        serde_json::from_slice(&read_bounded(&reply_path(directory, id), FRAME_LIMIT)?).ok()?;
    (reply.worker == worker
        && reply.request_id == id
        && reply.game_id != 0
        && one_shot_message_valid(&reply.message))
    .then_some(reply)
}
fn current(session: &BackgroundSession) -> Result<(), String> {
    if !session.active() {
        return Err(
            t("The selected game session ended before shutdown could be requested.").into(),
        );
    }
    if session.game_running() != Some(true) {
        return Err(t("This account has no running FFXI client.").into());
    }
    Ok(())
}
struct Pending {
    directory: PathBuf,
    worker: Identity,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let Some(_lock) = command_lock(&self.directory.join("game-shutdown.lock")) else {
            return;
        };
        let path = self.directory.join("game-shutdown.json");
        let owned = read_bounded(&path, FRAME_LIMIT)
            .and_then(|bytes| serde_json::from_slice::<Request>(&bytes).ok())
            .is_some_and(|request| request.worker == self.worker && request.request_id == self.id);
        if owned {
            let _ = fs::remove_file(path);
        }
        if reply(&self.directory, self.worker, self.id).is_some() {
            let _ = fs::remove_file(reply_path(&self.directory, self.id));
        }
    }
}

pub(crate) fn request(session: &BackgroundSession) -> Result<(), String> {
    request_within(session, ONE_SHOT_READY_LIMIT, ONE_SHOT_DISPATCH_LIMIT, CLOSE_LIMIT)
}
fn request_within(
    session: &BackgroundSession,
    ready_limit: Duration,
    dispatch_limit: Duration,
    close_limit: Duration,
) -> Result<(), String> {
    current(session)?;
    let record = read_record(&session.path)
        .filter(|record| record.worker == session.worker)
        .ok_or(t(
            "The selected game session ended before shutdown could be requested.",
        ))?;
    let Event::Started(started) = record.state else {
        return Err(t("Wait for FFXI to finish starting before requesting shutdown.").into());
    };
    let host = identity(started.pid).ok_or(t("This account has no running FFXI client."))?;
    let host_watch = ProcessWatch::open(host)?;
    let worker_watch = ProcessWatch::open(session.worker)?;
    let directory = session
        .path
        .parent()
        .ok_or(t("This game session has no account directory."))?;
    // Retain this lease through the logout countdown. Another Crystal window
    // must not repeat /shutdown and accidentally cancel the first countdown.
    let _delivery = exclusive(&directory.join("game-shutdown-delivery.lock"))
        .map_err(|_| t("Shutdown is already being requested for this account."))?;
    let ready_until = Instant::now() + ready_limit;
    let game = loop {
        current(session)?;
        if let Some(ready) = reply(directory, session.worker, 0) {
            if !ready.ok {
                return Err(t(UNSUPPORTED).into());
            }
            break ready.game_id;
        }
        if Instant::now() >= ready_until {
            return Err(t(UNSUPPORTED).into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let id = one_shot_id().ok_or(t("Could not identify the shutdown request."))?;
    let deadline_ms = request_clock_ms()
        .checked_add(dispatch_limit.min(ONE_SHOT_DISPATCH_LIMIT).as_millis() as u64)
        .ok_or(t("Could not time the shutdown request."))?;
    let _pending = Pending {
        directory: directory.into(),
        worker: session.worker,
        id,
    };
    {
        let _lock = exclusive(&directory.join("game-shutdown.lock"))
            .map_err(|_| t("Another shutdown request is being submitted."))?;
        current(session)?;
        let ready = reply(directory, session.worker, 0)
            .filter(|ready| ready.ok && ready.game_id == game)
            .ok_or(t(UNSUPPORTED))?;
        save_json(
            &directory.join("game-shutdown.json"),
            &Request {
                worker: session.worker,
                request_id: id,
                game_id: ready.game_id,
                deadline_ms,
            },
        )?;
    }
    wake::signal(session.worker);
    let close_until = Instant::now() + close_limit;
    let mut accepted = false;
    loop {
        // Both exact process identities must be gone. A new game or a reused
        // PID is never touched, and a dispatch ACK is not a completed exit.
        if host_watch.ended()? && worker_watch.ended()? {
            return Ok(());
        }
        if game_ended(directory, session.worker, game) { return Ok(()); }
        if !accepted {
            if let Some(result) =
                reply(directory, session.worker, id).filter(|result| result.game_id == game)
            {
                if !result.ok {
                    return Err(if result.message.is_empty() {
                        t("FFXI could not accept the shutdown request.").into()
                    } else {
                        result.message
                    });
                }
                accepted = true;
            } else if request_clock_ms() >= deadline_ms {
                return Err(t("FFXI did not confirm the shutdown request in time. No game was forcibly closed.").into());
            }
        }
        if Instant::now() >= close_until {
            return Err(t("FFXI or PlayOnline is still open. The game may have refused or cancelled shutdown; finish it in game, then retry.").into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(crate) fn take(directory: &Path) -> Option<Vec<u8>> {
    let path = directory.join("game-shutdown.json");
    if !path.exists() { return None; } // As take_controller_settings.
    let _lock = exclusive(&directory.join("game-shutdown.lock")).ok()?;
    let request: Request = serde_json::from_slice(&read_bounded(&path, FRAME_LIMIT)?).ok()?;
    let session = owned_session(directory)?;
    if request.worker != session.worker {
        return None;
    }
    let valid = request.request_id != 0
        && request.deadline_ms > request_clock_ms()
        && request.deadline_ms.saturating_sub(request_clock_ms()) <= ONE_SHOT_DISPATCH_LIMIT.as_millis() as u64
        && reply(directory, session.worker, 0)
            .is_some_and(|ready| ready.ok && ready.game_id == request.game_id);
    if !valid {
        let _ = fs::remove_file(path);
        return None;
    }
    current(&session).ok()?;
    let mut frame = serde_json::to_vec(
        &serde_json::json!({"type":"game_shutdown", "request_id":request.request_id,
        "game_id":request.game_id,"deadline_ms":request.deadline_ms}),
    )
    .ok()?;
    fs::remove_file(path).ok()?; // A denied delete must never duplicate dispatch.
    current(&session).ok()?;
    frame.push(b'\n');
    Some(frame)
}
pub(crate) fn publish(
    directory: &Path,
    id: u64,
    game: u64,
    ok: bool,
    message: &str,
) -> Result<(), String> {
    if game == 0 || !one_shot_message_valid(message) {
        return Err("Invalid game shutdown result.".into());
    }
    let session =
        owned_session(directory).ok_or("The shutdown result belongs to an inactive session.")?;
    // A request's reply goes under the command lock, like the requester's cleanup and the
    // controller channel. The id-0 capability announcement sweeps nothing and is written
    // without it, so a busy lock never makes a capable session look unsupported.
    let _lock = if id != 0 {
        let lock = command_lock(&directory.join("game-shutdown.lock"))
            .ok_or("Could not access the game shutdown command.")?;
        sweep_results(directory, "game-shutdown", id);
        Some(lock)
    } else {
        None
    };
    save_json(
        &reply_path(directory, id),
        &Reply {
            worker: session.worker,
            request_id: id,
            game_id: game,
            ok,
            message: message.into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Stdio};
    fn fixture(root: &Path) -> (Config, Account, PathBuf) {
        let config = Config {
            experiment: root.into(),
            repository: root.into(),
            game_install: None,
            dat_overlays: Vec::new(),
        };
        let mut account = crate::blank_account();
        account.pol_id = "TEST0001".into();
        let directory = config.account_directory(&account);
        fs::create_dir_all(&directory).unwrap();
        (config, account, directory)
    }
    struct Helper(Child);
    impl Drop for Helper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "crystal-game-shutdown-{}",
                crate::pol_less::pipe::random_hex().unwrap()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            assert!(self
                .0
                .canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap()));
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn helper(root: &Path, role: &str, host: u32) -> Helper {
        Helper(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session_worker::game_shutdown::tests::game_shutdown_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CRYSTAL_TEST_SHUTDOWN_ROOT", root)
                .env("CRYSTAL_TEST_SHUTDOWN_ROLE", role)
                .env("CRYSTAL_TEST_SHUTDOWN_HOST", host.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }
    fn wait_file(root: &Path, name: &str) {
        let until = Instant::now() + Duration::from_secs(5);
        while !root.join(name).exists() {
            assert!(Instant::now() < until, "fixture milestone {name}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    #[ignore = "Child of the isolated shutdown protocol test; no game or credentials are loaded"]
    fn game_shutdown_helper() {
        let root = PathBuf::from(std::env::var_os("CRYSTAL_TEST_SHUTDOWN_ROOT").unwrap());
        let role = std::env::var("CRYSTAL_TEST_SHUTDOWN_ROLE").unwrap();
        if role == "host" {
            wait_file(&root, "close-host");
            return;
        }
        assert_eq!(role, "worker");
        let (config, account, directory) = fixture(&root);
        let worker = identity(std::process::id()).unwrap();
        let host = std::env::var("CRYSTAL_TEST_SHUTDOWN_HOST")
            .unwrap()
            .parse()
            .unwrap();
        save_record(
            &record_path(&config, &account),
            &Record {
                worker,
                state: Event::Started(Started {
                    pid: host,
                    detail: "fixture".into(),
                    log: root.join("fixture.log"),
                    timing: String::new(),
                }),
                social_only: false,
                authenticated: true,
                native_batch: false,
                otp_used: Some(false),
                launch_choice: None, launch_tool: Some(Tool::Vanilla),
            },
        )
        .unwrap();
        publish_friends(
            &directory,
            &crate::friends::Snapshot {
                game_running: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        publish(&directory, 0, 7, true, "").unwrap();
        fs::write(root.join("ready"), b"").unwrap();
        if root.join("terminate").exists() {
            // As the worker's supervisor, once the loader pipe closes: how did the game end?
            let until = Instant::now() + Duration::from_secs(10);
            while !(directory.join(TERMINATE_FILE).exists() && identity(host).is_none()) {
                assert!(Instant::now() < until, "the host was not ended");
                std::thread::sleep(Duration::from_millis(5));
            }
            let kind = if terminated(&directory) { "normal" } else { "unknown" };
            fs::write(root.join("classified"), kind).unwrap();
            return;
        }
        let until = Instant::now() + Duration::from_secs(5);
        let frame = loop {
            if let Some(frame) = take(&directory) {
                break frame;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        };
        let frame: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(frame["type"], "game_shutdown");
        assert_eq!(frame["game_id"], 7);
        assert!(frame.get("command").is_none());
        assert!(take(&directory).is_none());
        publish(
            &directory,
            frame["request_id"].as_u64().unwrap(),
            7,
            true,
            "Requested",
        )
        .unwrap();
        fs::write(root.join("dispatched"), b"").unwrap();
        if root.join("retain-pol").exists() {
            wait_file(&root, "end-game");
            ended(&directory, 7).unwrap();
        }
        wait_file(&root, "close-worker");
    }
    #[test]
    fn instant_close_ends_only_the_verified_host_and_its_worker_records_a_normal_close() {
        let root = Root::new();
        fs::write(root.0.join("terminate"), b"").unwrap();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, directory) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        // The real check refuses a process that is not a pol.exe started after the worker.
        assert!(terminate(&session).unwrap_err().contains("Could not close FFXI"));
        assert!(host.0.try_wait().unwrap().is_none() && !directory.join(TERMINATE_FILE).exists());
        let (host_pid, owner) = (host.0.id(), session.worker);
        terminate_within(&session, &|_, found, worker| found.pid == host_pid && worker == owner,
            Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        assert!(host.0.try_wait().unwrap().is_some(), "the host was ended");
        assert!(worker.0.wait().unwrap().success());
        assert_eq!(fs::read_to_string(root.0.join("classified")).unwrap(), "normal");
        assert!(!directory.join(TERMINATE_FILE).exists() && !terminated(&directory), "consumed once");
    }
    #[test]
    fn a_terminate_marker_for_another_worker_or_host_is_not_a_normal_close() {
        let root = Root::new();
        let (config, account, directory) = fixture(&root.0);
        let worker = identity(std::process::id()).unwrap();
        let started = |pid| Record {
            worker,
            state: Event::Started(Started { pid, detail: "fixture".into(), log: root.0.join("fixture.log"), timing: String::new() }),
            social_only: false, authenticated: true, native_batch: false, otp_used: Some(false),
            launch_choice: None, launch_tool: None,
        };
        save_record(&record_path(&config, &account), &started(worker.pid)).unwrap();
        let other = Identity { created: worker.created.wrapping_add(1), ..worker };
        for marker in [Terminate { worker: other, host: worker }, Terminate { worker, host: Identity { pid: worker.pid + 4, ..worker } }] {
            save_json(&directory.join(TERMINATE_FILE), &marker).unwrap();
            assert!(!terminated(&directory));
            assert!(directory.join(TERMINATE_FILE).exists(), "a foreign marker is left alone");
        }
        save_json(&directory.join(TERMINATE_FILE), &Terminate { worker, host: worker }).unwrap();
        assert!(terminated(&directory) && !terminated(&directory));
        let report = crate::launch_log::classify_loader_end(&crate::launch_log::LoaderEnd::Terminated, Duration::from_secs(1));
        assert_eq!(report.kind, crate::launch_log::ExitKind::Normal);
        assert_eq!(report.summary, "Game closed normally");
    }
    #[test]
    fn retained_pol_completes_shutdown_only_for_the_ended_game_and_keeps_both_processes_alive() {
        let root = Root::new();
        fs::write(root.0.join("retain-pol"), b"").unwrap();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, directory) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        let captured = session.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || tx.send(request_within(&captured,
            Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4))).unwrap());
        wait_file(&root.0, "dispatched");
        for marker in [
            Ended { worker: Identity { created: session.worker.created.wrapping_add(1), ..session.worker }, game_id: 7 },
            Ended { worker: session.worker, game_id: 6 },
        ] {
            save_json(&directory.join("game-shutdown-ended.json"), &marker).unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(75)).is_err(), "neither an ACK nor a stale marker completes shutdown");
        }
        fs::write(root.0.join("end-game"), b"").unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        thread.join().unwrap();
        assert!(host.0.try_wait().unwrap().is_none() && worker.0.try_wait().unwrap().is_none());
        assert!(!game_ended(&directory, session.worker, 8), "an older game cannot complete a later request");
        fs::write(root.0.join("close-host"), b"").unwrap();
        fs::write(root.0.join("close-worker"), b"").unwrap();
        assert!(host.0.wait().unwrap().success() && worker.0.wait().unwrap().success());
    }
    #[test]
    fn game_shutdown_ack_waits_for_both_processes_and_rejects_an_overlapping_request() {
        let root = Root::new();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, _) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        let captured = session.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let request_thread = std::thread::spawn(move || {
            tx.send(request_within(
                &captured,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ))
            .unwrap()
        });
        wait_file(&root.0, "dispatched");
        assert!(rx.try_recv().is_err(), "an ACK is not a completed exit");
        assert!(request_within(
            &session,
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(20)
        )
        .unwrap_err()
        .contains("already"));
        fs::write(root.0.join("close-host"), b"").unwrap();
        assert!(host.0.wait().unwrap().success());
        assert!(
            rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "POL's owner must finish too"
        );
        fs::write(root.0.join("close-worker"), b"").unwrap();
        assert!(worker.0.wait().unwrap().success());
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        request_thread.join().unwrap();
    }
    #[test]
    fn game_shutdown_timeout_leaves_both_processes_alive() {
        let root = Root::new();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, _) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        let error = request_within(
            &session,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(error.contains("still open"));
        assert!(host.0.try_wait().unwrap().is_none() && worker.0.try_wait().unwrap().is_none());
        fs::write(root.0.join("close-host"), b"").unwrap();
        fs::write(root.0.join("close-worker"), b"").unwrap();
        assert!(host.0.wait().unwrap().success());
        assert!(worker.0.wait().unwrap().success());
    }
    #[test]
    fn game_shutdown_never_consumes_a_foreign_or_expired_game_request() {
        let root = Root::new();
        let (config, account, directory) = fixture(&root.0);
        let worker = identity(std::process::id()).unwrap();
        save_record(
            &record_path(&config, &account),
            &Record {
                worker,
                state: Event::Started(Started {
                    pid: worker.pid,
                    detail: "fixture".into(),
                    log: root.0.join("fixture.log"),
                    timing: String::new(),
                }),
                social_only: false,
                authenticated: true,
                native_batch: false,
                otp_used: Some(false),
                launch_choice: None, launch_tool: None,
            },
        )
        .unwrap();
        publish_friends(
            &directory,
            &crate::friends::Snapshot {
                game_running: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        publish(&directory, 0, 7, true, "").unwrap();
        for (owner, game, deadline) in [
            (worker, 8, request_clock_ms() + 1000),
            (worker, 7, request_clock_ms()),
            (
                Identity {
                    created: worker.created + 1,
                    ..worker
                },
                7,
                request_clock_ms() + 1000,
            ),
        ] {
            let path = directory.join("game-shutdown.json");
            save_json(
                &path,
                &Request {
                    worker: owner,
                    request_id: 1,
                    game_id: game,
                    deadline_ms: deadline,
                },
            )
            .unwrap();
            assert!(take(&directory).is_none());
            assert_eq!(
                path.exists(),
                owner != worker,
                "another worker's file is preserved"
            );
        }
    }
}
