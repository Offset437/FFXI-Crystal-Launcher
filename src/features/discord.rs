//! Optional Discord rich presence over Discord's local IPC named pipe. Off by default.
//!
//! Only the local pipe `\\.\pipe\discord-ipc-N` is used: nothing is sent over the network by
//! Crystal, and nothing at all unless the setting is on, Discord is running and a game runs.
//! The one background thread starts on the first update and blocks on a channel, so it costs
//! nothing while idle. A failed connection is reported as a status and tried again only when
//! the next update arrives: there is no reconnect loop and no timer.
//!
//! A synchronous pipe handle serializes reads and writes, so a read is never left pending: each
//! command is written, then its one reply is read.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Crystal's Discord application ID.
///
/// The application is named "FINAL FANTASY XI", so Discord shows "Playing FINAL FANTASY XI".
/// Its art assets are `crystal` (large) and `ffxi` (small); sources are in `assets\discord`.
pub(crate) const CLIENT_ID: Option<&str> = Some("1569489361571942442");

const PIPE_BASE: &str = r"\\.\pipe\discord-ipc-";
const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
/// Larger replies are not Discord's; refuse them rather than allocate.
const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Disabled in settings.
    Off,
    /// Crystal has no Discord application ID (see [`CLIENT_ID`]).
    Unavailable,
    Connected,
    /// Discord is not running (no IPC pipe opened).
    NotRunning,
    Error(String),
}

/// What to show. `character` is shown only when privacy is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Activity {
    pub clients: usize,
    pub character: Option<String>,
    pub started_unix: u64,
}

/// The `activity` object. With `privacy` no character or account label is sent: only a count.
/// A name is shown only for a single client; several show as a count.
pub(crate) fn activity_json(activity: &Activity, privacy: bool) -> Value {
    let name = activity.character.as_deref().map(str::trim).filter(|c| !privacy && !c.is_empty());
    let state = match (activity.clients, name) {
        (0 | 1, Some(name)) => name.to_owned(),
        (0 | 1, None) => "1 character".to_owned(),
        (n, _) => format!("{n} characters"),
    };
    json!({
        "details": "via Crystal Launcher",
        "state": state,
        "timestamps": { "start": activity.started_unix },
        "assets": {
            "large_image": "crystal",
            "large_text": "Crystal Launcher",
            "small_image": "ffxi",
            "small_text": "FINAL FANTASY XI",
        },
    })
}

/// One IPC frame: opcode and length (little-endian u32 each), then UTF-8 JSON.
pub(crate) fn encode(opcode: u32, json: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + json.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(json.as_bytes());
    frame
}

pub(crate) fn decode(reader: &mut dyn Read) -> std::io::Result<(u32, String)> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let mut head = [0u8; 8];
    reader.read_exact(&mut head)?;
    let opcode = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if len > MAX_FRAME { return Err(bad("frame too large")); }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    String::from_utf8(body).map(|s| (opcode, s)).map_err(|_| bad("not UTF-8"))
}

enum Message {
    Update(Activity, bool),
    #[cfg(test)]
    Clear,
    Stop,
}

struct Worker { tx: Sender<Message>, join: JoinHandle<()> }

/// The presence handle. Dropping it stops the thread.
pub(crate) struct Presence {
    enabled: bool,
    pipe_base: String,
    client_id: Option<String>,
    status: Arc<Mutex<Status>>,
    worker: Option<Worker>,
}

impl Presence {
    pub(crate) fn new(enabled: bool) -> Self {
        Self::with(enabled, PIPE_BASE.to_owned(), CLIENT_ID.map(str::to_owned))
    }

    fn with(enabled: bool, pipe_base: String, client_id: Option<String>) -> Self {
        let status = if !enabled { Status::Off } else if client_id.is_none() { Status::Unavailable } else { Status::NotRunning };
        Self { enabled, pipe_base, client_id, status: Arc::new(Mutex::new(status)), worker: None }
    }

    pub(crate) fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Turns presence on or off; off closes the connection and ends the thread.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled { return; }
        self.enabled = enabled;
        self.stop();
        set(&self.status, if !enabled { Status::Off } else if self.client_id.is_none() { Status::Unavailable } else { Status::NotRunning });
    }

    /// Shows `activity` (starting the thread on first use). Without an application ID, or while
    /// off, nothing is started or connected.
    pub(crate) fn update(&mut self, activity: &Activity, privacy: bool) {
        self.send(Message::Update(activity.clone(), privacy));
    }

    /// Removes the activity (a game ended). Does nothing if never connected.
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        if self.worker.is_some() { self.send(Message::Clear); }
    }

    /// Closes the connection and joins the thread.
    pub(crate) fn stop(&mut self) {
        if let Some(Worker { tx, join }) = self.worker.take() {
            let _ = tx.send(Message::Stop);
            let _ = join.join();
        }
    }

    fn send(&mut self, message: Message) {
        if !self.enabled || self.client_id.is_none() { return; }
        if self.worker.is_none() {
            let (tx, rx) = mpsc::channel();
            let (base, id, status) = (self.pipe_base.clone(), self.client_id.clone().unwrap_or_default(), self.status.clone());
            match std::thread::Builder::new().name("discord-presence".into()).spawn(move || run(rx, &base, &id, &status)) {
                Ok(join) => self.worker = Some(Worker { tx, join }),
                Err(_) => return set(&self.status, Status::Error(crate::i18n::t("Discord presence could not start.").into())),
            }
        }
        if let Some(worker) = &self.worker { let _ = worker.tx.send(message); }
    }
}

impl Drop for Presence {
    fn drop(&mut self) { self.stop(); }
}

fn set(status: &Mutex<Status>, value: Status) {
    *status.lock().unwrap_or_else(|e| e.into_inner()) = value;
}

enum Failure { NotRunning, Error(String) }

type Pipe = std::fs::File;

fn io_error(_: std::io::Error) -> Failure { Failure::Error(crate::i18n::t("Discord is not responding.").into()) }

fn command(pipe: &mut Pipe, opcode: u32, json: &str) -> Result<(u32, Value), Failure> {
    pipe.write_all(&encode(opcode, json)).and_then(|()| pipe.flush()).map_err(io_error)?;
    let (op, body) = decode(pipe).map_err(io_error)?;
    Ok((op, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

/// Opens the first `<base>0`..`<base>9` that accepts us and completes the handshake.
fn connect(base: &str, client_id: &str) -> Result<Pipe, Failure> {
    let mut pipe = (0..10).find_map(|n| OpenOptions::new().read(true).write(true).open(format!("{base}{n}")).ok())
        .ok_or(Failure::NotRunning)?;
    let hello = json!({ "v": 1, "client_id": client_id }).to_string();
    match command(&mut pipe, OP_HANDSHAKE, &hello)? {
        (OP_FRAME, reply) if reply["evt"] == "READY" => Ok(pipe),
        _ => Err(Failure::Error(crate::i18n::t("Discord refused the connection.").into())),
    }
}

fn set_activity(pipe: &mut Pipe, activity: Value) -> Result<(), Failure> {
    let nonce = crate::backend::unique_id().unwrap_or_default();
    let body = json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": std::process::id(), "activity": activity },
        "nonce": nonce,
    });
    match command(pipe, OP_FRAME, &body.to_string())? {
        (OP_FRAME, reply) if reply["evt"] != "ERROR" => Ok(()),
        _ => Err(Failure::Error(crate::i18n::t("Discord rejected the presence update.").into())),
    }
}

/// The thread body: blocks on `rx`, applies only the latest queued change, connects on demand.
fn run(rx: mpsc::Receiver<Message>, base: &str, client_id: &str, status: &Mutex<Status>) {
    let mut pipe: Option<Pipe> = None;
    loop {
        let Ok(mut message) = rx.recv() else { break };
        let mut stop = matches!(message, Message::Stop);
        // Several updates queued while we were busy: only the newest matters.
        while !stop {
            match rx.try_recv() {
                Ok(Message::Stop) => stop = true,
                Ok(next) => message = next,
                Err(_) => break,
            }
        }
        let activity = match message {
            Message::Update(activity, privacy) => Some(activity_json(&activity, privacy)),
            #[cfg(test)]
            Message::Clear => Some(Value::Null),
            Message::Stop => None,
        };
        if let Some(activity) = activity {
            let cleared = activity.is_null();
            let result = match pipe.as_mut().map(|p| set_activity(p, activity.clone())) {
                Some(Ok(())) => Ok(()),
                // No connection yet, or the old one broke (Discord restarted): connect once.
                Some(Err(_)) | None if cleared && pipe.is_none() => Ok(()),
                _ => connect(base, client_id).and_then(|mut fresh| {
                    set_activity(&mut fresh, activity)?;
                    pipe = Some(fresh);
                    Ok(())
                }),
            };
            match result {
                Ok(()) if pipe.is_some() => set(status, Status::Connected),
                Ok(()) => {}
                Err(failure) => {
                    pipe = None;
                    set(status, match failure { Failure::NotRunning => Status::NotRunning, Failure::Error(e) => Status::Error(e) });
                }
            }
        }
        if stop { break; }
    }
    if let Some(mut pipe) = pipe {
        let _ = pipe.write_all(&encode(OP_CLOSE, "{}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT};

    fn base() -> String { format!(r"\\.\pipe\xi-discord-test-{}-", crate::backend::unique_id().unwrap()) }

    /// A fake Discord on `<base>0`: answers READY and every SET_ACTIVITY, returns what it got.
    fn fake_server(base: &str, handshake_reply: &'static str) -> JoinHandle<Vec<(u32, String)>> {
        let name = crate::win::wide(&format!("{base}0"));
        // SAFETY: a NUL-terminated name; the handle is owned by the File made below.
        let handle = unsafe {
            CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 4096, 4096, 0, std::ptr::null())
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let handle = handle as usize;
        std::thread::spawn(move || {
            // SAFETY: created above and used by this thread only.
            let mut pipe = unsafe { std::fs::File::from_raw_handle(handle as _) };
            unsafe { ConnectNamedPipe(handle as _, std::ptr::null_mut()) };
            let mut got = Vec::new();
            while let Ok((op, body)) = decode(&mut pipe) {
                got.push((op, body.clone()));
                let reply = match op {
                    OP_HANDSHAKE => handshake_reply.to_owned(),
                    OP_FRAME => {
                        let nonce = serde_json::from_str::<Value>(&body).unwrap()["nonce"].clone();
                        json!({ "cmd": "SET_ACTIVITY", "evt": null, "data": {}, "nonce": nonce }).to_string()
                    }
                    _ => break,
                };
                pipe.write_all(&encode(OP_FRAME, &reply)).unwrap();
            }
            got
        })
    }

    fn wait_for(p: &Presence, want: Status) {
        for _ in 0..300 {
            if p.status() == want { return; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("status stayed {:?}, wanted {want:?}", p.status());
    }

    fn act(character: Option<&str>) -> Activity {
        Activity { clients: 2, character: character.map(str::to_owned), started_unix: 1_700_000_000 }
    }

    const READY: &str = r#"{"cmd":"DISPATCH","evt":"READY","data":{}}"#;

    #[test]
    fn frames_encode_and_decode() {
        let frame = encode(1, r#"{"a":1}"#);
        assert_eq!(&frame[..8], &[1, 0, 0, 0, 7, 0, 0, 0]);
        assert_eq!(decode(&mut &frame[..]).unwrap(), (1, r#"{"a":1}"#.to_owned()));
        // Truncated body, oversize length and bad UTF-8 are errors.
        assert!(decode(&mut &frame[..10]).is_err());
        let mut huge = vec![1, 0, 0, 0];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&mut &huge[..]).is_err());
        let mut bad = encode(1, "xx");
        bad[8] = 0xFF;
        assert!(decode(&mut &bad[..]).is_err());
    }

    #[test]
    fn privacy_sends_no_names() {
        let single = Activity { clients: 1, ..act(Some("Vanadiel")) };
        let named = activity_json(&single, false);
        assert_eq!(named["state"], "Vanadiel");
        assert_eq!(named["details"], "via Crystal Launcher");
        assert_eq!(named["assets"]["large_image"], "crystal");
        assert_eq!(named["assets"]["small_image"], "ffxi");
        assert_eq!(named["timestamps"]["start"], 1_700_000_000u64);
        let private = activity_json(&single, true).to_string();
        assert!(!private.contains("Vanadiel"), "{private}");
        assert!(private.contains("1 character"));
        let several = activity_json(&act(Some("Vanadiel")), false).to_string();
        assert!(several.contains("2 characters") && !several.contains("Vanadiel"), "{several}");
    }

    #[test]
    fn no_application_id_means_unavailable_and_nothing_is_started() {
        let base = base();
        let server = fake_server(&base, READY);
        let mut p = Presence::with(true, base.clone(), None);
        p.update(&act(None), false);
        p.clear();
        assert_eq!(p.status(), Status::Unavailable);
        assert!(p.worker.is_none());
        assert_eq!(Presence::new(true).status(), Status::NotRunning);
        assert_eq!(Presence::new(false).status(), Status::Off);
        // No connection was attempted: the server is still waiting. Connect to release it.
        drop(OpenOptions::new().read(true).write(true).open(format!("{base}0")).unwrap());
        assert!(server.join().unwrap().is_empty());
    }

    #[test]
    fn handshake_set_activity_clear_close_sequence() {
        let base = base();
        let server = fake_server(&base, READY);
        let mut p = Presence::with(true, base, Some("12345".into()));
        p.update(&act(Some("Vanadiel")), true);
        wait_for(&p, Status::Connected);
        p.clear();
        p.stop();
        let got = server.join().unwrap();
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[0].0, OP_HANDSHAKE);
        assert_eq!(serde_json::from_str::<Value>(&got[0].1).unwrap(), json!({ "v": 1, "client_id": "12345" }));
        let set: Value = serde_json::from_str(&got[1].1).unwrap();
        assert_eq!((got[1].0, set["cmd"].as_str()), (OP_FRAME, Some("SET_ACTIVITY")));
        assert_eq!(set["args"]["pid"], std::process::id());
        assert_eq!(set["args"]["activity"]["state"], "2 characters");
        assert!(!got[1].1.contains("Vanadiel"));
        assert!(!set["nonce"].as_str().unwrap().is_empty());
        let clear: Value = serde_json::from_str(&got[2].1).unwrap();
        assert!(clear["args"]["activity"].is_null());
        assert_eq!(got[3].0, OP_CLOSE);
    }

    #[test]
    fn discord_not_running_is_a_status_not_a_loop() {
        let mut p = Presence::with(true, base(), Some("1".into()));
        p.update(&act(None), false);
        wait_for(&p, Status::NotRunning);
        p.set_enabled(false);
        assert_eq!(p.status(), Status::Off);
        assert!(p.worker.is_none());
        p.update(&act(None), false);
        assert!(p.worker.is_none(), "off starts nothing");
    }

    #[test]
    fn a_refused_handshake_is_an_error() {
        let base = base();
        let server = fake_server(&base, r#"{"evt":"ERROR","data":{"code":4000}}"#);
        let mut p = Presence::with(true, base, Some("1".into()));
        p.update(&act(None), false);
        for _ in 0..300 {
            if matches!(p.status(), Status::Error(_)) { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(matches!(p.status(), Status::Error(_)), "{:?}", p.status());
        p.stop();
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
