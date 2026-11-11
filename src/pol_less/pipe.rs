//! The launcher (server) side of the crystal-loader session pipe: a randomly
//! named, user-only, local-only, single-instance
//! byte pipe carrying UTF-8 JSON lines. The loader says hello, the launcher checks the
//! client's PID and image, sends the one session line, then keeps reading status events
//! until the game ends.
//!
//! Synchronous overlapped I/O on a handle we own outright (no IOCP, no async runtime), so
//! the session bytes pass through no buffer this module doesn't zeroize.

use crate::i18n::{t, tf};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};
#[cfg(test)]
use zeroize::Zeroizing;

pub(crate) use crate::wire::{MAX_LINE, PROTOCOL};

// ---- protocol -------------------------------------------------------------------------------

/// A line from the loader. Unknown types and fields are tolerated.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoaderEvent {
    Hello { v: u32, pid: u32, caps: Caps },
    /// `parked`: polcore is created and the host waits for `credentials` (07d §1.3).
    Parked { polcore_ms: Option<u64> },
    WindowerReady,
    Started { pid: Option<u32> },
    NativeAuthStage(i32),
    Authenticated,
    NativeFriends { blob: Vec<u8>, locations: Option<Vec<u8>> },
    NativeStatus(i32),
    NativeSessionState(bool),
    NativeOwnMode(i32),
    /// `native_own_portrait`: the selected handle's portrait ID (13 bits).
    NativeOwnPortrait(u16),
    NativeGameEnded { code: i32, game_returned: bool, game_id: u64 },
    NativeSignOutComplete(i32),
    NativeControlResult { action: String, code: i32 },
    ControllerResult { id: u64, ok: bool, message: String },
    GameShutdownResult { id: u64, game: u64, ok: bool, message: String },
    NativeSocialReady,
    /// `native_viewer_check`: the loader reached Open(0) and waits for this sign-in's
    /// W2U (:54000) verdict before Open(1) (pol-spec 07c §1).
    NativeViewerCheck,
    /// `native_main_menu`: the Viewer presence publish completed (07c §2); W20 may follow.
    NativeMainMenu { code: i32, returned: bool },
    NativeMailChanged,
    NativeSocialResult { id: String, action: String, code: i32, payload: Option<Vec<u8>> },
    Exit(i32),
    Error(String),
    /// `notice`: one line for the launch history (a protection the host could not provide,
    /// and why). The game goes on.
    Notice(String),
    /// A type this launcher doesn't know; ignored.
    Unknown,
    /// The pipe closed.
    Closed,
}

/// What a loader's hello says it supports.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    pub native_auth: bool,
    pub native_social: bool,
    pub native_batch: bool,
    /// The loader asks for the sign-in's W2U at Connect (07c §1).
    pub viewer_check: bool,
    pub startup_sign_out: bool,
    /// It accepts a credential-less `park` line first (pol-spec 07d §1).
    pub park: bool,
    pub windower_private: bool,
}

#[derive(serde::Deserialize)]
struct Incoming {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    v: Option<u64>,
    #[serde(default)]
    pid: Option<u64>,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    locations: Option<String>,
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    game_returned: bool,
    #[serde(default)]
    returned: bool,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    native_auth: bool,
    #[serde(default)]
    native_social: bool,
    #[serde(default)]
    native_batch: bool,
    #[serde(default)]
    viewer_check: bool,
    #[serde(default)]
    startup_sign_out: bool,
    #[serde(default)]
    park: bool,
    #[serde(default)]
    windower_private: bool,
    #[serde(default)]
    polcore_ms: Option<u64>,
    #[serde(default)]
    game: Option<bool>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    mode: Option<i64>,
    #[serde(default)]
    portrait: Option<i64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    request_id: Option<u64>,
    #[serde(default)]
    game_id: Option<u64>,
    #[serde(default)]
    ok: Option<bool>,
}

/// Parse one loader line (without its `\n`).
pub(crate) fn parse_event(line: &[u8]) -> Result<LoaderEvent, String> {
    if line.len() > MAX_LINE {
        return Err("crystal-loader sent a message that exceeds the session protocol limit.".into());
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let value: Incoming = serde_json::from_slice(line)
        .map_err(|_| "crystal-loader sent a line that is not a valid message.".to_owned())?;
    let pid = |p: Option<u64>| p.and_then(|p| u32::try_from(p).ok());
    Ok(match value.kind.as_str() {
        "hello" => LoaderEvent::Hello {
            v: value.v.and_then(|v| u32::try_from(v).ok()).ok_or("crystal-loader's hello has no protocol version.")?,
            pid: pid(value.pid).ok_or("crystal-loader's hello has no process ID.")?,
            caps: Caps {
                native_auth: value.native_auth,
                native_social: value.native_social,
                native_batch: value.native_batch,
                viewer_check: value.viewer_check,
                startup_sign_out: value.startup_sign_out,
                park: value.park,
                windower_private: value.windower_private,
            },
        },
        "parked" => LoaderEvent::Parked { polcore_ms: value.polcore_ms },
        "windower_ready" => LoaderEvent::WindowerReady,
        "started" => LoaderEvent::Started { pid: pid(value.pid) },
        "native_auth_stage" => LoaderEvent::NativeAuthStage(value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL authentication state.")?),
        "authenticated" => LoaderEvent::Authenticated,
        "native_friends" => {
            let blob=base64_decode(value.payload.as_deref().unwrap_or(""))
                .filter(|b| b.len()==crate::wire::FRIENDS_LEN && &b[..8]==crate::wire::FRIENDS_MAGIC)
                .ok_or("Invalid native POL friend snapshot.")?;
            let locations = value.locations.as_deref().map(|encoded| {
                base64_decode(encoded).filter(|b| crate::friends::valid_locations(b))
                    .ok_or("Invalid game friend locations.")
            }).transpose()?;
            LoaderEvent::NativeFriends { blob, locations }
        },
        "native_pol_status" => {
            let code=value.code.and_then(|c| i32::try_from(c).ok()).filter(|c| matches!(c,0|-1025|-1038))
                .ok_or("Invalid native POL connection status.")?;
            LoaderEvent::NativeStatus(code)
        },
        "native_session_state" => LoaderEvent::NativeSessionState(value.game.ok_or("Invalid native POL session state.")?),
        "native_own_presence" => LoaderEvent::NativeOwnMode(value.mode.and_then(|m| i32::try_from(m).ok()).filter(|m| (-1..=4).contains(m)).ok_or("Invalid native POL presence mode.")?),
        "native_own_portrait" => LoaderEvent::NativeOwnPortrait(value.portrait.filter(|p| (0..=i64::from(crate::wire::PORTRAIT_MASK)).contains(p)).ok_or("Invalid native POL portrait.")? as u16),
        "native_game_ended" => LoaderEvent::NativeGameEnded {
            code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid game exit result.")?,
            game_returned: value.game_returned,
            game_id: value.game_id.unwrap_or(0),
        },
        "native_sign_out_complete" => LoaderEvent::NativeSignOutComplete(
            value.code.and_then(|c| i32::try_from(c).ok()).filter(|c| *c <= 0)
                .ok_or("Invalid native POL sign-out result.")?
        ),
        "native_control_result" => LoaderEvent::NativeControlResult {
            action: value.action.filter(|a| matches!(a.as_str(), "start_game" | "sign_out" | "visible" | "away" | "invisible" | "groups" | "portrait")).ok_or("Invalid native POL control action.")?,
            code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL control result.")?,
        },
        "game_shutdown_result" => LoaderEvent::GameShutdownResult {
            id: value.request_id.ok_or("Invalid shutdown request identity.")?,
            game: value.game_id.filter(|game| *game != 0).ok_or("Invalid shutdown game identity.")?,
            ok: value.ok.ok_or("Invalid shutdown result.")?,
            message: value.message.filter(|text| text.len() <= 512 && !text.chars().any(char::is_control))
                .ok_or("Invalid shutdown result message.")?,
        },
        "controller_result" => LoaderEvent::ControllerResult {
            id: value.request_id.ok_or("Invalid controller request identity.")?,
            ok: value.ok.ok_or("Invalid controller result.")?,
            message: value.message.filter(|text| text.len() <= 1024 && !text.contains('\0'))
                .ok_or("Invalid controller result message.")?,
        },
        "native_social_ready" => LoaderEvent::NativeSocialReady,
        "native_viewer_check" => LoaderEvent::NativeViewerCheck,
        "native_main_menu" => LoaderEvent::NativeMainMenu {
            code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native POL main-menu result.")?,
            returned: value.returned,
        },
        "native_mail_changed" => LoaderEvent::NativeMailChanged,
        "native_social_result" => {
            let id = value.id.filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).ok_or("Invalid native social request identity.")?;
            let action = value.action.filter(|s| matches!(s.as_str(), "inbox" | "check" | "read" | "send" | "remove")).ok_or("Invalid native social operation.")?;
            let payload = value.payload.as_deref().map(|s| base64_decode(s).filter(|b| b.len() <= 8192).ok_or("Invalid native social result.")).transpose()?;
            LoaderEvent::NativeSocialResult { id, action, code: value.code.and_then(|c| i32::try_from(c).ok()).ok_or("Invalid native social result code.")?, payload }
        },
        "exit" => LoaderEvent::Exit(value.code.map_or(0, |c| c.clamp(i32::MIN as i64, i32::MAX as i64) as i32)),
        "error" => LoaderEvent::Error(clean_message(value.message.as_deref().unwrap_or("unspecified error"))),
        "notice" => match value.message.as_deref().map(|text| clean_line(text, 400)).filter(|text| !text.is_empty()) {
            Some(text) => LoaderEvent::Notice(text),
            None => LoaderEvent::Unknown,
        },
        _ => LoaderEvent::Unknown,
    })
}

/// One line of loader text, safe for the status bar and history file.
fn clean_message(message: &str) -> String {
    clean_line(message, 240)
}

/// `message` on one line: control characters become spaces, at most `limit` characters.
fn clean_line(message: &str, limit: usize) -> String {
    let flat: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(limit)
        .collect();
    flat.trim().to_owned()
}

/// Standard base64 with padding, appended in place (the caller reserves the space).
pub(crate) use crate::b64::encode_into as base64_into;

/// Canonical, padded standard base64 (the wire contract's only form).
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    crate::b64::decode_padded(text.as_bytes())
}

// ---- pipe -------------------------------------------------------------------------------

/// Why waiting for the loader to connect stopped.
#[derive(Debug)]
pub(crate) enum AcceptError {
    TimedOut,
    /// The caller's check (the injector process) reported a failure.
    Check(String),
    Io(io::Error),
}

/// The server end of one session pipe. Dropping it closes the pipe.
pub(crate) struct LoaderPipe {
    name: String,
    handle: std::os::windows::io::OwnedHandle,
    event: std::os::windows::io::OwnedHandle,
    buffer: Vec<u8>,
    /// What the verified hello said the loader supports.
    caps: Caps,
    command_wake: Option<crate::session_worker::CommandWake>,
    command_wake_pending: bool,
}

impl LoaderPipe {
    pub(crate) fn listen_for_commands(&mut self, wake: Option<crate::session_worker::CommandWake>) {
        self.command_wake = wake;
    }
    /// A command writer's wake interrupts reads: the supervisor need not poll for commands.
    pub(crate) fn has_command_wake(&self) -> bool {
        self.command_wake.is_some()
    }
    pub(crate) fn supports_native_auth(&self)->bool { self.caps.native_auth }
    pub(crate) fn supports_native_social(&self)->bool { self.caps.native_social }
    pub(crate) fn supports_native_batch(&self)->bool { self.caps.native_batch }
    pub(crate) fn supports_startup_sign_out(&self)->bool { self.caps.startup_sign_out }
    /// The launcher sends no W2U of its own; a loader without this never
    /// sends one before :51240 either.
    pub(crate) fn supports_viewer_check(&self)->bool { self.caps.viewer_check }
    /// The loader accepts a credential-less `park` line (pol-spec 07d §1); a loader without
    /// it is never parked and only gets today's `session` line.
    pub(crate) fn supports_park(&self)->bool { self.caps.park }
    pub(crate) fn supports_private_windower(&self)->bool { self.caps.windower_private }
    /// The full `\\.\pipe\...` path the loader opens.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Next line, or `None` once the pipe closes. `deadline` bounds the whole wait.
    pub(crate) fn read_line(&mut self, deadline: Option<Instant>) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                if end > MAX_LINE {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if self.buffer.len() > MAX_LINE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            // A wake may race a successful partial read. Keep the bytes, but let the
            // supervisor consume commands before issuing another blocking read.
            if std::mem::take(&mut self.command_wake_pending) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let mut chunk = [0u8; 4096];
            match self.read_some(&mut chunk, deadline)? {
                0 => return Ok(None),
                n => self.buffer.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// The next loader event; `Closed` when the pipe ends. A malformed line is an error.
    pub(crate) fn next_event(&mut self, deadline: Option<Instant>) -> io::Result<Result<LoaderEvent, String>> {
        Ok(match self.read_line(deadline)? {
            None => Ok(LoaderEvent::Closed),
            Some(line) => parse_event(&line),
        })
    }

    /// Read the hello, then check that the connected process is the one it claims to be
    /// and runs `expected_image`. Returns the loader's PID.
    pub(crate) fn handshake(&mut self, expected_image: &Path, timeout: Duration) -> Result<u32, String> {
        let client = self
            .client_pid()
            .map_err(|_| t("Could not identify the process on the session pipe; the session was not sent.").to_owned())?;
        let line = match self.read_line(Some(Instant::now() + timeout)) {
            Ok(Some(line)) => line,
            Ok(None) => return Err(t("crystal-loader closed the session pipe before saying hello.").into()),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(tf("crystal-loader connected but did not say hello within {seconds} s.", &[("seconds", &timeout.as_secs())]))
            }
            Err(_) => return Err(t("The session pipe failed before crystal-loader said hello.").into()),
        };
        match parse_event(&line)? {
            LoaderEvent::Hello { v, .. } if v != PROTOCOL => Err(tf(
                "crystal-loader speaks session protocol v{loader}; this launcher speaks v{launcher}. Update the launcher and the loader together.",
                &[("loader", &v), ("launcher", &PROTOCOL)],
            )),
            LoaderEvent::Hello { pid, .. } if pid != client => Err(tf(
                "The process on the session pipe (PID {client}) is not the one that said hello (PID {pid}); the session was not sent.",
                &[("client", &client), ("pid", &pid)],
            )),
            LoaderEvent::Hello { pid, caps, .. } => {
                verify_image(pid, expected_image)?;
                self.caps = caps;
                Ok(pid)
            }
            _ => Err(t("crystal-loader did not start with a hello; the session was not sent.").into()),
        }
    }
}

/// The process's image must be the loader we pointed the injector at.
fn verify_image(pid: u32, expected: &Path) -> Result<(), String> {
    let actual = crate::win::process::image_path(pid)
        .map_err(|_| tf("Could not read the image path of PID {pid}; the session was not sent.", &[("pid", &pid)]))?;
    if same_file(&actual, expected) {
        Ok(())
    } else {
        Err(tf(
            "The process on the session pipe runs {actual}, not {expected}; the session was not sent.",
            &[("actual", &actual.display()), ("expected", &expected.display())],
        ))
    }
}

pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        crate::backend::plain_windows_path(&p).to_string_lossy().to_lowercase()
    };
    norm(a) == norm(b)
}

/// 32 random hex digits from the system RNG.
pub(crate) fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    {
        use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
        let status = unsafe {
            BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status < 0 {
            return Err(io::Error::other("system RNG failed"));
        }
    }
    Ok(super::tools::embedded::hex(&bytes))
}

mod sys {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{
            GetLastError, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
            WAIT_OBJECT_0, WAIT_FAILED,
        },
        Security::{
            AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
            SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, SECURITY_ATTRIBUTES,
            SECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Threading::{
                CreateEventW, GetCurrentProcess, OpenProcessToken,
                WaitForMultipleObjects, WaitForSingleObject, INFINITE,
            },
            IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        },
    };

    const GENERIC_ALL: u32 = 0x1000_0000;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    /// How often a pending operation wakes to check its deadline and the caller's check.
    const SLICE_MS: u32 = 100;

    fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
        }
    }

    /// The current user's SID, as a TOKEN_USER buffer (u64-aligned).
    fn token_user() -> io::Result<Vec<u64>> {
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = owned(token)?;
            let mut needed = 0u32;
            GetTokenInformation(token.as_raw_handle() as _, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            if GetTokenInformation(token.as_raw_handle() as _, TokenUser, buffer.as_mut_ptr() as _, (buffer.len() * 8) as u32, &mut needed) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(buffer)
        }
    }

    impl LoaderPipe {
        /// `\\.\pipe\crystal-loader-<32 hex>`: duplex byte pipe, first and only instance,
        /// remote clients rejected, DACL granting only the current user.
        pub(crate) fn create() -> io::Result<Self> {
            let name = format!(r"\\.\pipe\crystal-loader-{}", random_hex()?);
            let user = token_user()?;
            unsafe {
                let sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
                let acl_len = std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                    - std::mem::size_of::<u32>()
                    + GetLengthSid(sid) as usize;
                let mut acl = vec![0u64; acl_len.div_ceil(8)];
                let acl_ptr = acl.as_mut_ptr() as *mut ACL;
                if InitializeAcl(acl_ptr, (acl.len() * 8) as u32, ACL_REVISION) == 0
                    || AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, sid) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let mut descriptor: SECURITY_DESCRIPTOR = std::mem::zeroed();
                let descriptor_ptr = &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut std::ffi::c_void;
                if InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) == 0
                    || SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor_ptr,
                    bInheritHandle: 0,
                };
                let wide = crate::win::wide(&name);
                let handle = owned(CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    64 * 1024,
                    64 * 1024,
                    0,
                    &attributes,
                ))?;
                let event = owned(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()))?;
                Ok(Self { name, handle, event, buffer: Vec::new(), caps: Caps::default(),
                    command_wake:None, command_wake_pending:false })
            }
        }

        fn raw(&self) -> HANDLE {
            self.handle.as_raw_handle() as HANDLE
        }

        /// Wait for an overlapped operation; `Ok(bytes)` on completion. Always leaves the
        /// operation finished (cancelled and drained if it did not complete), so the
        /// OVERLAPPED and buffer may be freed afterwards.
        fn finish(
            &mut self,
            overlapped: &mut OVERLAPPED,
            deadline: Option<Instant>,
            check: &mut dyn FnMut() -> Result<(), String>,
            idle_forever: bool,
            sliced: bool,
        ) -> Result<u32, AcceptError> {
            // Only `check` needs the 100 ms slices; reads and writes wait out their deadline.
            let limit = if sliced { SLICE_MS } else { INFINITE - 1 };
            loop {
                let slice = match deadline {
                    None if idle_forever => INFINITE,
                    None => SLICE_MS,
                    Some(end) => end
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(limit as u128) as u32,
                };
                let wake = idle_forever.then_some(self.command_wake.as_ref()).flatten();
                let waited = if let Some(wake) = wake {
                    let handles = [self.event.as_raw_handle() as HANDLE, wake.raw()];
                    unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, slice) }
                } else {
                    unsafe { WaitForSingleObject(self.event.as_raw_handle() as HANDLE, slice) }
                };
                if waited == WAIT_OBJECT_0 {
                    let mut bytes = 0u32;
                    return if unsafe { GetOverlappedResult(self.raw(), overlapped, &mut bytes, 0) } == 0 {
                        Err(AcceptError::Io(io::Error::last_os_error()))
                    } else {
                        Ok(bytes)
                    };
                }
                let notified = wake.is_some() && waited == WAIT_OBJECT_0 + 1;
                if notified { self.command_wake_pending = true; }
                let stop = match check() {
                    _ if notified => Some(AcceptError::Check("Native command ready.".into())),
                    _ if waited == WAIT_FAILED => Some(AcceptError::Io(io::Error::last_os_error())),
                    Err(message) => Some(AcceptError::Check(message)),
                    Ok(()) if deadline.is_some_and(|end| Instant::now() >= end) => Some(AcceptError::TimedOut),
                    Ok(()) => None,
                };
                if let Some(stop) = stop {
                    let mut bytes = 0u32;
                    unsafe {
                        CancelIoEx(self.raw(), overlapped);
                        // Completed after all (raced the cancel): take the result.
                        if GetOverlappedResult(self.raw(), overlapped, &mut bytes, 1) != 0 {
                            return Ok(bytes);
                        }
                    }
                    return Err(stop);
                }
            }
        }

        /// Wait for the loader to connect. `check` runs about every 100 ms and can abort.
        pub(crate) fn accept(
            &mut self,
            timeout: Duration,
            mut check: impl FnMut() -> Result<(), String>,
        ) -> Result<(), AcceptError> {
            let deadline = Instant::now() + timeout;
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
            if unsafe { ConnectNamedPipe(self.raw(), &mut overlapped) } != 0 {
                return Ok(());
            }
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => Ok(()),
                ERROR_IO_PENDING => self.finish(&mut overlapped, Some(deadline), &mut check, false, true).map(|_| ()),
                error => Err(AcceptError::Io(io::Error::from_raw_os_error(error as i32))),
            }
        }

        pub(crate) fn client_pid(&self) -> io::Result<u32> {
            let mut pid = 0u32;
            if unsafe { GetNamedPipeClientProcessId(self.raw(), &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pid)
        }

        /// Read what is available (at least one byte), 0 at end of pipe.
        pub(super) fn read_some(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
            let mut read = 0u32;
            let issued = unsafe {
                ReadFile(self.raw(), buffer.as_mut_ptr(), buffer.len() as u32, &mut read, &mut overlapped)
            };
            let result = if issued != 0 {
                Ok(read)
            } else {
                match unsafe { GetLastError() } {
                    ERROR_IO_PENDING => self.finish(&mut overlapped, deadline, &mut || Ok(()), true, false),
                    error => Err(AcceptError::Io(io::Error::from_raw_os_error(error as i32))),
                }
            };
            match result {
                Ok(n) => Ok(n as usize),
                Err(AcceptError::Io(e)) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
                Err(AcceptError::Io(e)) => Err(e),
                Err(AcceptError::TimedOut) => Err(io::ErrorKind::TimedOut.into()),
                Err(AcceptError::Check(_)) => {
                    self.command_wake_pending = false;
                    Err(io::ErrorKind::Interrupted.into())
                }
            }
        }

        /// Write all of `bytes` before `deadline`.
        pub(crate) fn send(&mut self, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
            while !bytes.is_empty() {
                let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
                overlapped.hEvent = self.event.as_raw_handle() as HANDLE;
                let mut written = 0u32;
                let issued = unsafe {
                    WriteFile(self.raw(), bytes.as_ptr(), bytes.len().min(u32::MAX as usize) as u32, &mut written, &mut overlapped)
                };
                let result = if issued != 0 {
                    Ok(written)
                } else {
                    match unsafe { GetLastError() } {
                        ERROR_IO_PENDING => self.finish(&mut overlapped, Some(deadline), &mut || Ok(()), false, false),
                        error => Err(AcceptError::Io(io::Error::from_raw_os_error(error as i32))),
                    }
                };
                match result {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => bytes = &bytes[n as usize..],
                    Err(AcceptError::Io(e)) => return Err(e),
                    Err(_) => return Err(io::ErrorKind::TimedOut.into()),
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn game_shutdown_result_requires_both_scoped_identities() {
        let line=r#"{"type":"game_shutdown_result","request_id":18446744073709551615,"game_id":7,"ok":true,"message":"Requested"}"#;
        assert!(matches!(parse_event(line.as_bytes()),Ok(LoaderEvent::GameShutdownResult { id:u64::MAX,game:7,ok:true,.. })));
        for value in [line.replace("\"game_id\":7", "\"game_id\":0"),
            line.replace("\"game_id\":7,", ""),line.replace("\"ok\":true,", ""),
            line.replace("Requested", "bad\\u0000message")] {
            assert!(parse_event(value.as_bytes()).is_err());
        }
    }

    #[test]
    fn controller_results_keep_large_ids_and_do_not_use_social_ids() {
        assert!(matches!(parse_event(br#"{"type":"controller_result","request_id":18446744073709551615,"ok":true,"message":""}"#),
            Ok(LoaderEvent::ControllerResult { id: u64::MAX, ok: true, message }) if message.is_empty()));
        assert!(matches!(parse_event(br#"{"type":"controller_result","request_id":0,"ok":false,"message":"Unavailable"}"#),
            Ok(LoaderEvent::ControllerResult { id: 0, ok: false, .. })));
        for invalid in [
            r#"{"type":"controller_result","id":"1","ok":true,"message":""}"#,
            r#"{"type":"controller_result","request_id":1,"message":""}"#,
            r#"{"type":"controller_result","request_id":-1,"ok":true,"message":""}"#,
            r#"{"type":"controller_result","request_id":1,"ok":true,"message":"bad\u0000value"}"#,
        ] { assert!(parse_event(invalid.as_bytes()).is_err()); }
    }


    fn open_client(name: &str) -> std::fs::File {
        std::fs::OpenOptions::new().read(true).write(true).open(name).unwrap()
    }

    #[test]
    fn base64_round_trips_every_tail_length() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 5) as u8).collect();
            let mut text = Vec::new();
            base64_into(&mut text, &data);
            assert_eq!(text.len(), crate::b64::encoded_len(n));
            assert_eq!(base64_decode(std::str::from_utf8(&text).unwrap()).unwrap(), data);
        }
        let mut text = Vec::new();
        base64_into(&mut text, b"Man");
        assert_eq!(text, b"TWFu");
        assert!(base64_decode("TW=u").is_none());
        assert!(base64_decode("TWF").is_none());
        // Non-canonical tails (stray low bits) are refused like the loader refuses them.
        assert!(base64_decode("Zh==").is_none());
        assert!(base64_decode("Zm9=").is_none());
    }

    #[test]
    fn loader_lines_parse_and_tolerate_unknown_fields() {
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true}"#).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, caps: Caps { native_auth: true, native_social: true, ..Caps::default() } });
        // A pre-round-1 loader (16e26f3): no viewer_check, so it never asks for the W2U.
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true,"native_batch":true}"#).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, caps: Caps { native_auth: true, native_social: true, native_batch: true, ..Caps::default() } });
        // crystal-loader proto::hello_line.
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true,"native_batch":true,"viewer_check":true}"#).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, caps: Caps { native_auth: true, native_social: true, native_batch: true, viewer_check: true, ..Caps::default() } });
        assert_eq!(parse_event(br#"{"type":"native_session_state","game":false}"#).unwrap(), LoaderEvent::NativeSessionState(false));
        assert!(parse_event(br#"{"type":"native_session_state","game":1}"#).is_err());
        assert!(parse_event(br#"{"type":"native_session_state"}"#).is_err());
        assert!(parse_event(br#"{"type":"native_control_result","action":"delete_profile","code":0}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"native_control_result","action":"sign_out","code":-256}"#).unwrap(), LoaderEvent::NativeControlResult { action: "sign_out".into(), code: -256 });
        assert_eq!(parse_event(br#"{"type":"native_control_result","action":"groups","code":1}"#).unwrap(), LoaderEvent::NativeControlResult { action: "groups".into(), code: 1 });
        assert_eq!(parse_event(br#"{"type":"native_own_portrait","portrait":8191}"#).unwrap(), LoaderEvent::NativeOwnPortrait(8191));
        assert!(parse_event(br#"{"type":"native_own_portrait","portrait":8192}"#).is_err());
        assert!(parse_event(br#"{"type":"native_own_portrait"}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"native_sign_out_complete","code":0}"#).unwrap(), LoaderEvent::NativeSignOutComplete(0));
        assert_eq!(parse_event(br#"{"type":"native_sign_out_complete","code":-2046}"#).unwrap(), LoaderEvent::NativeSignOutComplete(-2046));
        assert!(parse_event(br#"{"type":"native_sign_out_complete"}"#).is_err());
        assert!(parse_event(br#"{"type":"native_sign_out_complete","code":7}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"extra":true}"#).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, caps: Caps::default() });
        assert_eq!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"native_auth":true}"#).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, caps: Caps { native_auth: true, ..Caps::default() } });
        assert_eq!(parse_event(br#"{"type":"authenticated"}"#).unwrap(),LoaderEvent::Authenticated);
        assert_eq!(parse_event(br#"{"type":"native_pol_status","code":-1025}"#).unwrap(),LoaderEvent::NativeStatus(-1025));
        assert!(parse_event(br#"{"type":"native_pol_status","code":1}"#).is_err());
        assert!(parse_event(br#"{"type":"native_friends","payload":"AQID"}"#).is_err());
        assert_eq!(parse_event(br#"{"type":"started","pid":1234}"#).unwrap(), LoaderEvent::Started { pid: Some(1234) });
        // The launcher always signs in natively; the loader never sends the legacy presence line.
        assert_eq!(parse_event(br#"{"type":"presence","payload":"AQID"}"#).unwrap(), LoaderEvent::Unknown);
        assert_eq!(parse_event(br#"{"type":"exit","code":-14}"#).unwrap(), LoaderEvent::Exit(-14));
        assert_eq!(parse_event(br#"{"type":"native_game_ended","code":-14}"#).unwrap(),
            LoaderEvent::NativeGameEnded { code: -14, game_returned: false, game_id: 0 });
        assert_eq!(parse_event(br#"{"type":"native_game_ended","code":-14,"game_returned":true}"#).unwrap(),
            LoaderEvent::NativeGameEnded { code: -14, game_returned: true, game_id: 0 });
        assert_eq!(parse_event(br#"{"type":"native_game_ended","code":-14,"game_returned":true,"game_id":2}"#).unwrap(),
            LoaderEvent::NativeGameEnded { code: -14, game_returned: true, game_id: 2 });
        assert!(parse_event(br#"{"type":"native_game_ended","code":-14,"game_returned":"true"}"#).is_err());
        assert_eq!(
            parse_event(b"{\"type\":\"error\",\"message\":\"FFXiMain.dll\\nmissing\"}\r").unwrap(),
            LoaderEvent::Error("FFXiMain.dll missing".into())
        );
        assert_eq!(parse_event(br#"{"type":"future-thing"}"#).unwrap(), LoaderEvent::Unknown);
        // A notice is one history line, whole (the FFXIDB one is longer than an error's limit).
        let long = format!("FFXIDB minimap settings protection is off: {}.", "x".repeat(300));
        assert_eq!(parse_event(format!("{{\"type\":\"notice\",\"message\":\"{long}\"}}").as_bytes()).unwrap(), LoaderEvent::Notice(long));
        assert_eq!(parse_event(b"{\"type\":\"notice\",\"message\":\"two\\nlines\"}").unwrap(), LoaderEvent::Notice("two lines".into()));
        assert_eq!(parse_event(br#"{"type":"notice"}"#).unwrap(), LoaderEvent::Unknown);
        assert_eq!(parse_event(br#"{"type":"notice","message":"  "}"#).unwrap(), LoaderEvent::Unknown);
        assert!(parse_event(b"not json").is_err());
        assert!(parse_event(br#"{"type":"hello","pid":1}"#).is_err());
    }

    #[test]
    fn native_friend_locations_are_optional_and_bounded() {
        use crate::wire::{FRIENDS_LEN as BLOB_LEN,FRIENDS_MAGIC as MAGIC};
        let mut blob=vec![0;BLOB_LEN]; blob[..8].copy_from_slice(MAGIC);
        let encoded = |b: &[u8]| { let mut out=Vec::new(); base64_into(&mut out,b); String::from_utf8(out).unwrap() };
        let line=serde_json::json!({"type":"native_friends","payload":encoded(&blob)});
        assert_eq!(parse_event(line.to_string().as_bytes()).unwrap(),LoaderEvent::NativeFriends { blob:blob.clone(),locations:None });
        let mut locations=vec![0;8+300*96]; locations[..8].copy_from_slice(b"XIPOLG\x01\0");
        let line=serde_json::json!({"type":"native_friends","payload":encoded(&blob),"locations":encoded(&locations)});
        assert_eq!(parse_event(line.to_string().as_bytes()).unwrap(),LoaderEvent::NativeFriends { blob:blob.clone(),locations:Some(locations.clone()) });
        locations.pop();
        let line=serde_json::json!({"type":"native_friends","payload":encoded(&blob),"locations":encoded(&locations)});
        assert!(parse_event(line.to_string().as_bytes()).is_err());
    }

    /// The loader's lines from pol-spec 07c (round-1 contract), as crystal-loader proto.rs writes them.
    #[test]
    fn round1_viewer_check_and_main_menu_events_parse() {
        assert_eq!(parse_event(br#"{"type":"native_viewer_check"}"#).unwrap(), LoaderEvent::NativeViewerCheck);
        assert_eq!(parse_event(br#"{"type":"native_main_menu","code":1,"returned":false}"#).unwrap(), LoaderEvent::NativeMainMenu { code: 1, returned: false });
        assert_eq!(parse_event(br#"{"type":"native_main_menu","code":-2046,"returned":true}"#).unwrap(), LoaderEvent::NativeMainMenu { code: -2046, returned: true });
        assert!(parse_event(br#"{"type":"native_main_menu","returned":false}"#).is_err());
    }

    /// pol-spec 07d §1: the hello's `park` and the `parked` line, as crystal-loader proto.rs writes them.
    #[test]
    fn round2_park_hello_and_parked_events_parse() {
        let hello = br#"{"type":"hello","v":1,"pid":1234,"native_auth":true,"native_social":true,"native_batch":true,"viewer_check":true,"park":true}"#;
        assert_eq!(parse_event(hello).unwrap(), LoaderEvent::Hello { v: 1, pid: 1234, caps: Caps { native_auth: true, native_social: true, native_batch: true, viewer_check: true, park: true, ..Caps::default() } });
        assert!(matches!(parse_event(br#"{"type":"hello","v":1,"pid":1234,"windower_private":true}"#).unwrap(),
            LoaderEvent::Hello { caps: Caps { windower_private: true, native_auth: false, .. }, .. }));
        assert_eq!(parse_event(br#"{"type":"parked","pid":1234,"polcore_ms":431}"#).unwrap(), LoaderEvent::Parked { polcore_ms: Some(431) });
        assert_eq!(parse_event(br#"{"type":"parked","pid":1234}"#).unwrap(), LoaderEvent::Parked { polcore_ms: None });
    }

    #[test]
    fn social_results_validate_correlation_actions_and_payload_bounds() {
        assert_eq!(parse_event(br#"{"type":"native_social_ready"}"#).unwrap(), LoaderEvent::NativeSocialReady);
        assert_eq!(parse_event(br#"{"type":"native_mail_changed"}"#).unwrap(), LoaderEvent::NativeMailChanged);
        let result = br#"{"type":"native_social_result","id":"0123456789abcdef0123456789abcdef","action":"send","code":-7189}"#;
        assert!(matches!(parse_event(result), Ok(LoaderEvent::NativeSocialResult { code:-7189, payload:None, .. })));
        let s = std::str::from_utf8(result).unwrap();
        assert!(parse_event(s.replace("send", "unknown").as_bytes()).is_err());
        assert!(parse_event(s.replace("0123456789abcdef0123456789abcdef", "bad-id").as_bytes()).is_err());
        assert!(parse_event(s.replace("-7189", "2147483648").as_bytes()).is_err());
    }

    #[test]
    fn pipe_name_is_random_and_local() {
        let a = LoaderPipe::create().unwrap();
        let b = LoaderPipe::create().unwrap();
        assert_ne!(a.name(), b.name());
        let suffix = a.name().strip_prefix(r"\\.\pipe\crystal-loader-").unwrap();
        assert_eq!(suffix.len(), 32);
        assert!(suffix.bytes().all(|c| c.is_ascii_hexdigit()));
        // Single instance: a second client cannot open the same pipe while one is connected.
        let name = a.name().to_owned();
        let _first = open_client(&name);
        assert!(std::fs::OpenOptions::new().read(true).write(true).open(&name).is_err());
    }

    #[test]
    fn a_terminated_oversized_line_cannot_bypass_the_transport_limit() {
        let mut pipe = LoaderPipe::create().unwrap();
        pipe.buffer = vec![b' '; MAX_LINE + 1];
        pipe.buffer.push(b'\n');
        assert_eq!(pipe.read_line(None).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(parse_event(&pipe.buffer).is_err());
        pipe.buffer = vec![b' '; MAX_LINE];
        pipe.buffer.push(b'\n');
        assert_eq!(pipe.read_line(None).unwrap().unwrap().len(), MAX_LINE);
    }

    #[test]
    fn native_command_wakes_a_silent_pipe_and_retains_timeout_fallback() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let (finish, finished) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let _file = open_client(&name);
            finished.recv().unwrap();
        });
        pipe.accept(Duration::from_secs(3), || Ok(())).unwrap();
        let (listener, signal) = crate::session_worker::CommandWake::test_listener();
        pipe.listen_for_commands(Some(listener));
        let notifier = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signal();
        });
        let error = pipe.next_event(Some(Instant::now() + Duration::from_secs(2))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        notifier.join().unwrap();
        let error = pipe.next_event(Some(Instant::now() + Duration::from_millis(30))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "missing notifications retain polling fallback");
        finish.send(()).unwrap();
        client.join().unwrap();
    }

    #[test]
    fn native_command_wake_preserves_partial_lines_and_completion_races() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let (finish, finished) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            file.write_all(br#"{"type":"ex"#).unwrap();
            finished.recv().unwrap();
            file.write_all(b"it\",\"code\":0}\n").unwrap();
        });
        pipe.accept(Duration::from_secs(3), || Ok(())).unwrap();
        let mut chunk = [0; 4096];
        let count = pipe.read_some(&mut chunk, Some(Instant::now() + Duration::from_secs(2))).unwrap();
        pipe.buffer.extend_from_slice(&chunk[..count]);
        let partial = pipe.buffer.clone();
        let (listener, signal) = crate::session_worker::CommandWake::test_listener();
        pipe.listen_for_commands(Some(listener));
        signal();
        assert_eq!(pipe.read_line(Some(Instant::now() + Duration::from_secs(2))).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(pipe.buffer, partial);
        // CancelIoEx may race a successful partial read; finish retains that notification.
        pipe.command_wake_pending = true;
        assert_eq!(pipe.read_line(Some(Instant::now() + Duration::from_secs(2))).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(pipe.buffer, partial);
        finish.send(()).unwrap();
        assert_eq!(pipe.next_event(Some(Instant::now() + Duration::from_secs(2))).unwrap().unwrap(), LoaderEvent::Exit(0));
        client.join().unwrap();
        assert_eq!(pipe.next_event(None).unwrap().unwrap(), LoaderEvent::Closed);
    }

    /// An in-process fake loader: hello, read the session, then report events.
    #[test]
    fn fake_loader_exchanges_lines_end_to_end() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let file = open_client(&name);
            let mut writer = file.try_clone().unwrap();
            writeln!(writer, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
            let mut reader = BufReader::new(file);
            let mut session = String::new();
            reader.read_line(&mut session).unwrap();
            writeln!(writer, r#"{{"type":"started","pid":{}}}"#, std::process::id()).unwrap();
            writeln!(writer, r#"{{"type":"exit","code":0}}"#).unwrap();
            session
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let me = std::env::current_exe().unwrap();
        assert_eq!(pipe.handshake(&me, Duration::from_secs(10)).unwrap(), std::process::id());
        let line = Zeroizing::new(b"{\"type\":\"session\",\"v_key\":\"paWlpaWlpaWlpaWlpaWlpQ==\"}\n".to_vec());
        pipe.send(&line, Instant::now() + Duration::from_secs(10)).unwrap();
        drop(line);
        let deadline = Some(Instant::now() + Duration::from_secs(10));
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Started { pid: Some(std::process::id()) });
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Exit(0));
        let session = client.join().unwrap();
        let value: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(value["type"], "session");
        assert_eq!(base64_decode(value["v_key"].as_str().unwrap()).unwrap(), [0xA5; 16]);
        // The client closed: the pipe reports the end.
        assert_eq!(pipe.next_event(deadline).unwrap().unwrap(), LoaderEvent::Closed);
    }

    #[test]
    fn hello_with_another_pid_is_rejected_before_the_session() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            writeln!(file, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id() ^ 0x40).unwrap();
            let mut rest = Vec::new();
            let _ = std::io::Read::read_to_end(&mut file, &mut rest);
            rest
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let error = pipe.handshake(&std::env::current_exe().unwrap(), Duration::from_secs(10)).unwrap_err();
        assert!(error.contains("not the one that said hello"), "{error}");
        drop(pipe);
        assert!(client.join().unwrap().is_empty(), "nothing may be sent to an impostor");
    }

    #[test]
    fn hello_from_another_image_is_rejected() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            writeln!(file, r#"{{"type":"hello","v":1,"pid":{}}}"#, std::process::id()).unwrap();
            file
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let error = pipe
            .handshake(Path::new(r"C:\nowhere\crystal-loader.exe"), Duration::from_secs(10))
            .unwrap_err();
        assert!(error.contains("not C:\\nowhere\\crystal-loader.exe"), "{error}");
        drop(client.join().unwrap());
    }

    #[test]
    fn newer_protocol_is_rejected() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            writeln!(file, r#"{{"type":"hello","v":2,"pid":{}}}"#, std::process::id()).unwrap();
            file
        });
        pipe.accept(Duration::from_secs(10), || Ok(())).unwrap();
        let error = pipe.handshake(&std::env::current_exe().unwrap(), Duration::from_secs(10)).unwrap_err();
        assert!(error.contains("protocol v2"), "{error}");
        drop(client.join().unwrap());
    }

    #[test]
    fn accept_times_out_and_check_can_abort() {
        let mut pipe = LoaderPipe::create().unwrap();
        let started = Instant::now();
        assert!(matches!(pipe.accept(Duration::from_millis(300), || Ok(())), Err(AcceptError::TimedOut)));
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut calls = 0;
        let result = pipe.accept(Duration::from_secs(30), || {
            calls += 1;
            if calls >= 2 { Err("injector exited".into()) } else { Ok(()) }
        });
        assert!(matches!(result, Err(AcceptError::Check(m)) if m == "injector exited"));
        // The pipe still works after a cancelled connect.
        let name = pipe.name().to_owned();
        let _client = open_client(&name);
        pipe.accept(Duration::from_secs(5), || Ok(())).unwrap();
    }

    #[test]
    fn silent_client_times_out_waiting_for_hello() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let _client = open_client(&name);
        pipe.accept(Duration::from_secs(5), || Ok(())).unwrap();
        let error = pipe.handshake(&std::env::current_exe().unwrap(), Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("did not say hello"), "{error}");
    }

    #[test]
    fn overlong_line_is_refused() {
        let mut pipe = LoaderPipe::create().unwrap();
        let name = pipe.name().to_owned();
        let client = std::thread::spawn(move || {
            let mut file = open_client(&name);
            let _ = file.write_all(&vec![b'x'; MAX_LINE + 8192]);
        });
        pipe.accept(Duration::from_secs(5), || Ok(())).unwrap();
        let error = pipe.read_line(Some(Instant::now() + Duration::from_secs(10))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(pipe);
        let _ = client.join();
    }
}
