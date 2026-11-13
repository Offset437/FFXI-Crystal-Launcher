//! The loader <-> launcher wire format: one JSON
//! object per `\n`-terminated line. Error messages produced here never quote
//! the input, because the session line carries secrets.

use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::b64;
use crate::ffxi_settings::{FfxiSettings, SettingValue};
use crate::json::{self, JStr, Value};

pub const PROTOCOL_VERSION: i64 = crate::wire::PROTOCOL as i64;
pub use crate::wire::{json_string, MAX_LINE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Jp,
    Us,
    Eu,
}

impl Region {
    /// The region index selects the polcore CLSID/IID pair, the
    /// registry root and `SetAreaCode`.
    pub fn area_code(self) -> i32 {
        match self {
            Region::Jp => 0,
            Region::Us => 1,
            Region::Eu => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Region::Jp => "JP",
            Region::Us => "US",
            Region::Eu => "EU",
        }
    }

    /// EU and JP sign-ins are experimental: their region values are logged
    /// for testers (game.rs `log_region_values`).
    pub fn experimental(self) -> bool { self != Region::Us }
}

/// The install's `POL_UCS_AREA_KBN` (slot 1309's argument, app 1001a1ea):
/// two ASCII digits. US sends none (the loader keeps its verified "01");
/// EU and JP must send the value the launcher read from their env.dat.
fn parse_ucs_area(value: Option<&Value<'_>>, region: Region) -> Result<Option<String>, ProtoError> {
    match (value, region) {
        (None, Region::Us) => Ok(None),
        (Some(_), Region::Us) => Err(ProtoError::Field("ucs_area", "only an EU or JP session carries it")),
        (None, _) => Err(ProtoError::Missing("ucs_area")),
        (Some(value), _) => {
            let raw = plain_str(Some(value), "ucs_area")?.ok_or(ProtoError::Field("ucs_area", "must be a string"))?;
            if raw.len() != 2 || !raw.iter().all(u8::is_ascii_digit) {
                return Err(ProtoError::Field("ucs_area", "must be two digits"));
            }
            Ok(Some(String::from_utf8(raw.to_vec()).expect("ASCII digits")))
        }
    }
}

pub use crate::wire::{FRIENDS_LEN, FRIEND_RECORD_LEN, FRIEND_SLOTS};

pub enum ServiceEvent { NativeStartGame, NativeBatchGame(String), NativeSignOut, NativeStatus { mode: i32, groups: bool }, NativePortrait(u16), NativeActivity, NativeSocial(NativeSocialRequest), ViewerCheck(ViewerCheckResult), NativePlayPending, ControllerSettings { id: u64, settings: FfxiSettings, deadline_ms: u64 }, GameShutdown { id: u64, game: u64, deadline_ms: u64 } }

/// The launcher's answer to `native_viewer_check`: the Viewer's own W2U
/// (:54000) verdict for this Connect (pol-spec 07a w2u-at-connect, 07c §1).
/// The launcher sends it only after the W2U connection is closed. Outdated,
/// Failed and NetworkNotReady stop the sign-in before Open(1) with the
/// launcher's (localized) text; Unreachable (sent, no usable reply) signs in
/// anyway (owner 2026-10-12, a7550a6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerCheckResult {
    Current,
    Outdated(String),
    Failed(String),
    Unreachable(String),
    /// `reason:"network"`: resolution or the local connect failed instantly on
    /// every attempt of the launcher's capped retry (pol-spec 07d §2.3): nothing
    /// reached Square Enix and Open(1) would only end in -19 (08 §1 row 8).
    NetworkNotReady(String),
}

pub enum NativeSocialRequest {
    Inbox { id: String, folder: u8, page: u32 },
    /// The Viewer's user check: list, download, save and acknowledge new mail, then Inbox page 0.
    Check { id: String },
    Read { id: String, header: [u8; 0x48] },
    Send { id: String, alternate: bool, slot: u16, uid: u64, subject: String, message: String },
    Remove { id: String, alternate: bool, slot: u16, uid: u64 },
}
impl NativeSocialRequest {
    pub fn id(&self) -> &str { match self { Self::Inbox { id, .. } | Self::Check { id } | Self::Read { id, .. } | Self::Send { id, .. } | Self::Remove { id, .. } => id } }
    pub fn action(&self) -> &'static str { match self { Self::Inbox { .. } => "inbox", Self::Check { .. } => "check", Self::Read { .. } => "read", Self::Send { .. } => "send", Self::Remove { .. } => "remove" } }
}
impl Drop for NativeSocialRequest {
    fn drop(&mut self) {
        if let Self::Send { subject, message, .. } = self { subject.zeroize(); message.zeroize(); }
    }
}

fn social_request(root: &Value<'_>) -> Result<NativeSocialRequest, ProtoError> {
    if let Value::Object(fields) = root {
        if first_duplicate(fields, &["id", "action", "header", "alternate", "slot", "uid", "subject", "message", "folder", "page"]).is_some() {
            return Err(ProtoError::Field("native_social", "duplicate request field"));
        }
    }
    let id = root.get("id").and_then(Value::as_jstr).and_then(|s| s.plain()).ok_or(ProtoError::Missing("id"))?;
    if id.len() != 32 || !id.iter().all(u8::is_ascii_hexdigit) { return Err(ProtoError::Field("id", "expected request identity")); }
    let id = String::from_utf8(id.to_vec()).unwrap();
    let action = root.get("action").and_then(Value::as_jstr).ok_or(ProtoError::Missing("action"))?;
    if action.eq_str("inbox") {
        let folder = match root.get("folder") { None => 0, Some(v) => v.as_i64().filter(|n| (0..=2).contains(n)).ok_or(ProtoError::Field("folder", "invalid native folder"))? as u8 };
        let page = match root.get("page") { None => 0, Some(v) => v.as_i64().filter(|n| (0..=4096).contains(n)).ok_or(ProtoError::Field("page", "invalid inbox page"))? as u32 };
        return Ok(NativeSocialRequest::Inbox { id, folder, page });
    }
    if action.eq_str("check") {
        return Ok(NativeSocialRequest::Check { id });
    }
    if action.eq_str("read") {
        let raw = plain_str(root.get("header"), "header")?.ok_or(ProtoError::Missing("header"))?;
        let mut header = [0; 0x48];
        b64::decode_into(raw, &mut header).map_err(|_| ProtoError::Field("header", "expected native header"))?;
        return Ok(NativeSocialRequest::Read { id, header });
    }
    let alternate = match root.get("alternate") { Some(Value::Bool(b)) => *b, _ => return Err(ProtoError::Field("alternate", "expected boolean")) };
    let slot = root.get("slot").and_then(Value::as_i64).filter(|s| *s >= 0 && *s < if alternate { 100 } else { 200 }).ok_or(ProtoError::Field("slot", "outside native catalog"))? as u16;
    let uid = nonzero_u64(root.get("uid")).ok_or(ProtoError::Field("uid", "expected native identity"))?;
    if action.eq_str("remove") { return Ok(NativeSocialRequest::Remove { id, alternate, slot, uid }); }
    if action.eq_str("send") {
        let text = |field, limit| {
            let s = root.get(field).and_then(Value::as_jstr).ok_or(ProtoError::Missing(field))?.decode().map_err(ProtoError::Json)?;
            if s.len() > limit || s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) { return Err(ProtoError::Field(field, "invalid message text")); }
            Ok(s)
        };
        let subject = text("subject", 512)?;
        let message = text("message", 16384)?;
        if message.trim().is_empty() { return Err(ProtoError::Field("message", "empty message")); }
        return Ok(NativeSocialRequest::Send { id, alternate, slot, uid, subject, message });
    }
    Err(ProtoError::Field("action", "unknown social operation"))
}

/// `{"type":"native_viewer_check_result","ok":true}` or `"ok":false` with
/// `"reason"` (`outdated` | `failed` | `unreachable`) and `"error"` (text).
fn viewer_check_result(root: &Value<'_>) -> Result<ViewerCheckResult, ProtoError> {
    match root.get("ok") {
        Some(Value::Bool(true)) => Ok(ViewerCheckResult::Current),
        Some(Value::Bool(false)) => {
            let error = root.get("error").and_then(Value::as_jstr).and_then(|s| s.decode().ok()).unwrap_or_default();
            let reason = root.get("reason").and_then(Value::as_jstr).ok_or(ProtoError::Missing("reason"))?;
            if reason.eq_str("outdated") { Ok(ViewerCheckResult::Outdated(error)) }
            else if reason.eq_str("failed") { Ok(ViewerCheckResult::Failed(error)) }
            else if reason.eq_str("unreachable") { Ok(ViewerCheckResult::Unreachable(error)) }
            else if reason.eq_str("network") { Ok(ViewerCheckResult::NetworkNotReady(error)) }
            else { Err(ProtoError::Field("reason", "unknown Viewer check verdict")) }
        }
        _ => Err(ProtoError::Field("ok", "must be a boolean")),
    }
}

pub fn parse_service_event(line: &[u8]) -> Result<Option<ServiceEvent>, ProtoError> {
    if line.len() > MAX_LINE { return Err(ProtoError::LineTooLong); }
    let root = json::parse(strip_eol(line)).map_err(ProtoError::Json)?;
    if let Value::Object(fields) = &root {
        if !exactly_once(fields, &["type"]) {
            return Err(ProtoError::Field("type", "command type must be unambiguous"));
        }
    }
    let t = root.get("type").and_then(Value::as_jstr).ok_or(ProtoError::Missing("type"))?;
    if t.eq_str("game_shutdown") {
        let Value::Object(fields) = &root else { return Err(ProtoError::Field("type", "invalid shutdown command")); };
        if fields.len() != 4 || !exactly_once(fields, &["type", "request_id", "game_id", "deadline_ms"]) {
            return Err(ProtoError::Field("type", "shutdown command fields must be unambiguous"));
        }
        let number = |name| nonzero_u64(root.get(name));
        let id = number("request_id").ok_or(ProtoError::Field("request_id", "invalid shutdown identity"))?;
        let game = number("game_id").ok_or(ProtoError::Field("game_id", "invalid game identity"))?;
        let deadline_ms = number("deadline_ms").ok_or(ProtoError::Field("deadline_ms", "invalid shutdown deadline"))?;
        return Ok(Some(ServiceEvent::GameShutdown { id, game, deadline_ms }));
    }
    if t.eq_str("controller_settings") {
        let Value::Object(fields) = &root else { return Err(ProtoError::Field("settings", "invalid controller command")); };
        if !exactly_once(fields, &["type", "request_id", "deadline_ms", "settings"]) {
            return Err(ProtoError::Field("settings", "controller command fields must be unambiguous"));
        }
        let id = nonzero_u64(root.get("request_id")).ok_or(ProtoError::Field("request_id", "must be a nonzero unsigned integer"))?;
        let deadline_ms = nonzero_u64(root.get("deadline_ms")).ok_or(ProtoError::Field("deadline_ms", "must be a nonzero unsigned integer"))?;
        let Some(Value::Object(fields)) = root.get("settings") else { return Err(ProtoError::Missing("settings")); };
        if fields.iter().any(|(_, value)| !matches!(value, Value::Str(_))) {
            return Err(ProtoError::Field("settings", "controller values must be strings"));
        }
        let settings = parse_ffxi_settings(fields)?;
        if !(2..=3).contains(&settings.len()) || settings.get("padmode000").is_none() || settings.get("padsin000").is_none()
            || settings.names().any(|name| !crate::ffxi_settings::is_controller_key(name)) {
            return Err(ProtoError::Field("settings", "only controller settings are allowed"));
        }
        return Ok(Some(ServiceEvent::ControllerSettings { id, settings, deadline_ms }));
    }
    if t.eq_str("native_social") { return Ok(Some(ServiceEvent::NativeSocial(social_request(&root)?))); }
    if t.eq_str("native_start_game") { return Ok(Some(ServiceEvent::NativeStartGame)); }
    if t.eq_str("native_batch_game") {
        let token = plain_str(root.get("token"), "token")?.ok_or(ProtoError::Missing("token"))?;
        if token.len() != 61 || !token.iter().all(|b| b.is_ascii_hexdigit() || *b == b'-') { return Err(ProtoError::Field("token", "invalid game-start gate")); }
        return Ok(Some(ServiceEvent::NativeBatchGame(String::from_utf8(token.to_vec()).map_err(|_| ProtoError::Field("token", "invalid gate"))?)));
    }
    if t.eq_str("native_sign_out") { return Ok(Some(ServiceEvent::NativeSignOut)); }
    if t.eq_str("native_viewer_check_result") { return viewer_check_result(&root).map(|r| Some(ServiceEvent::ViewerCheck(r))); }
    if t.eq_str("native_activity") { return Ok(Some(ServiceEvent::NativeActivity)); }
    if t.eq_str("native_portrait") {
        let portrait = root.get("portrait").and_then(Value::as_i64).filter(|p| (0..=i64::from(crate::wire::PORTRAIT_MASK)).contains(p))
            .ok_or(ProtoError::Field("portrait", "must be a 13-bit portrait ID"))?;
        return Ok(Some(ServiceEvent::NativePortrait(portrait as u16)));
    }
    if t.eq_str("native_play_pending") { return Ok(Some(ServiceEvent::NativePlayPending)); }
    for (name, mode) in [("native_visible", 0), ("native_away", 1), ("native_invisible", 4)] {
        if t.eq_str(name) {
            // The status dialog's "apply to groups" (app 10031b2b); stock default on.
            let groups = match root.get("groups") {
                None => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(ProtoError::Field("groups", "must be a boolean")),
            };
            return Ok(Some(ServiceEvent::NativeStatus { mode, groups }));
        }
    }
    // A well-formed line of a type this loader does not know is ignored.
    Ok(None)
}

#[derive(Debug)]
pub struct Session {
    pub region: Region,
    /// EU/JP only: the install's `POL_UCS_AREA_KBN` (`parse_ucs_area`).
    pub ucs_area: Option<String>,
    /// FFXI install folder as the launcher sees it; `None` if empty/absent.
    pub install_dir: Option<String>,
    /// Explicit discovered POL installation, including portable installations.
    pub pol_install_dir: Option<String>,
    /// Credentials for a genuine sign-in by the installed core; never
    /// formatted or written to disk.
    pub native_auth: NativeCredentials,
    pub start_game: bool,
    pub keep_session: bool,
    /// Restore the Viewer's machine-paced UI gaps (op7 → 374 ≈1.05 s, mail
    /// list → publish ≈1.6 s; pol-spec 07a stock-faithful-pacing). Sent true
    /// only for a sign-in-only (pre-sign-in) session, never on a cold Play
    /// (owner decision 2026-10-12). Absent = false.
    pub viewer_gaps: bool,
    /// Viewer auto-Away minutes: 0 off, else 10..=60; absent = 10 (app table
    /// 10490d00, default index 0).
    pub auto_away_minutes: u32,
    /// Optional foreground-only navigation of the real game login menus.
    pub auto_login: AutoLogin,
    /// FFXI settings this process reads instead of the registry (`ffxi_settings`); `None`
    /// when absent or null.
    pub ffxi_settings: Option<FfxiSettings>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoLogin {
    pub enabled: bool,
    /// One-based position in FFXI's character list; never silently clamped.
    pub character_slot: u8,
}

impl Default for AutoLogin {
    fn default() -> Self { Self { enabled: false, character_slot: 1 } }
}

fn parse_auto_login(value: Option<&Value<'_>>) -> Result<AutoLogin, ProtoError> {
    let Some(value) = value else { return Ok(AutoLogin::default()); };
    let Value::Object(fields) = value else { return Err(ProtoError::Field("auto_login", "must be an object")); };
    if first_duplicate(fields, &["enabled", "character_slot"]).is_some() {
        return Err(ProtoError::Field("auto_login", "duplicate field"));
    }
    let enabled = match value.get("enabled") {
        None => false,
        Some(Value::Bool(enabled)) => *enabled,
        _ => return Err(ProtoError::Field("auto_login", "enabled must be a boolean")),
    };
    let character_slot = match value.get("character_slot") {
        None => 1,
        Some(value) => value.as_i64().filter(|slot| (1..=16).contains(slot))
            .ok_or(ProtoError::Field("auto_login", "character_slot must be 1-16"))? as u8,
    };
    Ok(AutoLogin { enabled, character_slot })
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct NativeCredentials {
    pub pol_id: [u8; 9],
    pub pol_password: [u8; 16],
    pub se_id: [u8; 17],
    pub se_secret: [u8; 20],
    pub otp: Option<[u8; 6]>,
    pub profile_key: String,
    pub handle_names: String,
}

impl fmt::Debug for NativeCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("NativeCredentials([redacted])") }
}

fn parse_native_credentials(value: &Value<'_>) -> Result<NativeCredentials, ProtoError> {
    const KEY: &str = "native_auth";
    let Value::Object(fields) = value else { return Err(ProtoError::Field(KEY, "must be an object")); };
    if first_duplicate(fields, &["pol_id", "pol_password", "se_id", "se_secret", "otp", "profile_key", "handle_names"]).is_some() {
        return Err(ProtoError::Field(KEY, "duplicate credential field"));
    }
    let mut out = NativeCredentials {
        pol_id: [0; 9], pol_password: [0; 16], se_id: [0; 17],
        se_secret: [0; 20], otp: None, profile_key: String::new(), handle_names: String::new(),
    };
    let decode = |name: &'static str, out: &mut [u8], min: usize, max: usize| -> Result<usize, ProtoError> {
        let raw = plain_str(value.get(name), KEY)?.ok_or(ProtoError::Field(KEY, "missing credential field"))?;
        let len = b64::decoded_len(raw).filter(|n| (min..=max).contains(n))
            .ok_or(ProtoError::Field(KEY, "invalid credential length"))?;
        b64::decode_into(raw, &mut out[..len]).map_err(|_| ProtoError::Field(KEY, "invalid credential encoding"))?;
        Ok(len)
    };
    decode("pol_id", &mut out.pol_id, 8, 8)?;
    let password_len = decode("pol_password", &mut out.pol_password, 1, 15)?;
    let se_len = decode("se_id", &mut out.se_id, 1, 16)?;
    decode("se_secret", &mut out.se_secret, 20, 20)?;
    if !out.pol_id[..4].iter().all(u8::is_ascii_uppercase) || !out.pol_id[4..8].iter().all(u8::is_ascii_digit)
        || !out.pol_password[..password_len].iter().all(|b| (32..=126).contains(b))
        || !out.se_id[..se_len].iter().all(|b| b.is_ascii() && !b.is_ascii_control()) {
        return Err(ProtoError::Field(KEY, "invalid credential characters"));
    }
    if let Some(value) = value.get("otp") {
        if !matches!(value, Value::Null) {
            let mut otp = zeroize::Zeroizing::new([0; 6]);
            decode("otp", &mut *otp, 6, 6)?;
            if !otp.iter().all(u8::is_ascii_digit) { return Err(ProtoError::Field(KEY, "invalid one-time code")); }
            out.otp = Some(*otp);
        }
    }
    let key = plain_str(value.get("profile_key"), KEY)?.ok_or(ProtoError::Field(KEY, "missing profile key"))?;
    if key.len() != 40 || !key.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) {
        return Err(ProtoError::Field(KEY, "invalid profile key"));
    }
    out.profile_key = std::str::from_utf8(key).unwrap().into();
    if value.get("handle_names").is_some() {
        let mut bytes = zeroize::Zeroizing::new([0; 512]);
        let length = decode("handle_names", &mut *bytes, 0, 512)?;
        let text = std::str::from_utf8(&bytes[..length]).map_err(|_| ProtoError::Field(KEY, "invalid handle name encoding"))?;
        if text.chars().any(char::is_control) { return Err(ProtoError::Field(KEY, "invalid handle name characters")); }
        out.handle_names = text.into();
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Json(json::Error),
    NotAnObject,
    WrongType,
    UnsupportedVersion(i64),
    Missing(&'static str),
    Field(&'static str, &'static str),
    DuplicateField(&'static str),
    LineTooLong,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Json(e) => write!(f, "{e}"),
            ProtoError::NotAnObject => write!(f, "session line is not a JSON object"),
            ProtoError::WrongType => write!(f, "expected a \"session\" message"),
            ProtoError::UnsupportedVersion(v) => write!(f, "unsupported session version {v} (loader speaks v{PROTOCOL_VERSION})"),
            ProtoError::Missing(k) => write!(f, "session is missing \"{k}\""),
            ProtoError::Field(k, why) => write!(f, "session field \"{k}\": {why}"),
            ProtoError::DuplicateField(k) => write!(f, "session field \"{k}\" appears twice"),
            ProtoError::LineTooLong => write!(f, "line longer than {MAX_LINE} bytes"),
        }
    }
}

/// Parses and validates the launcher's `session` line (without its `\n`).
pub fn parse_session(line: &[u8]) -> Result<Session, ProtoError> {
    if line.len() > MAX_LINE {
        return Err(ProtoError::LineTooLong);
    }
    let line = strip_eol(line);
    let root = json::parse(line).map_err(ProtoError::Json)?;
    // A known key given twice is ambiguous; refuse rather than guess.
    for known in [&["type", "v"][..], &HOST_FIELDS, &SECRET_FIELDS, &PLAY_FIELDS] {
        duplicate_known(&root, known)?;
    }
    let get = |k: &'static str| root.get(k);

    match get("type").and_then(Value::as_jstr) {
        Some(t) if t.eq_str("session") => {}
        Some(_) => return Err(ProtoError::WrongType),
        None => return Err(ProtoError::Missing("type")),
    }
    let version = get("v").ok_or(ProtoError::Missing("v"))?.as_i64().ok_or(ProtoError::Field("v", "not an integer"))?;
    if version != PROTOCOL_VERSION {
        return Err(ProtoError::UnsupportedVersion(version));
    }

    let region = match plain_str(get("region"), "region")? {
        Some(b"US") => Region::Us,
        Some(b"EU") => Region::Eu,
        Some(b"JP") => Region::Jp,
        Some(_) => return Err(ProtoError::Field("region", "must be US, EU or JP")),
        None => return Err(ProtoError::Missing("region")),
    };
    let ucs_area = parse_ucs_area(get("ucs_area"), region)?;
    // Accepted and checked, not used: the game language comes from the
    // Viewer's registry (registry::viewer_language).
    match get("lang") {
        Some(Value::Str(s)) => { s.decode().map_err(|_| ProtoError::Field("lang", "bad string"))?; }
        Some(Value::Null) | None => {}
        Some(_) => return Err(ProtoError::Field("lang", "not a string")),
    }
    let install_dir = match get("install_dir") {
        Some(Value::Str(s)) => Some(s.decode().map_err(|_| ProtoError::Field("install_dir", "bad string"))?).filter(|s| !s.is_empty()),
        Some(Value::Null) | None => None,
        Some(_) => return Err(ProtoError::Field("install_dir", "not a string")),
    };
    let pol_install_dir = match get("pol_install_dir") {
        Some(Value::Str(s)) => {
            let path = s.decode().map_err(|_| ProtoError::Field("pol_install_dir", "bad string"))?;
            if path.is_empty() || path.len() > 32_767 || path.contains('\0') {
                return Err(ProtoError::Field("pol_install_dir", "invalid folder path"));
            }
            Some(path)
        }
        Some(Value::Null) | None => None,
        Some(_) => return Err(ProtoError::Field("pol_install_dir", "not a string")),
    };

    // Every session is a native sign-in by the installed core. Launcher-made
    // session keys and records (the retired offline path) are refused.
    let native_auth = get("native_auth").filter(|value| !matches!(value, Value::Null)).ok_or(ProtoError::Missing("native_auth"))?;
    if LEGACY_FIELDS.iter().any(|k| get(k).is_some()) {
        return Err(ProtoError::Field("native_auth", "requires a session without supplied session keys or records"));
    }
    let native_auth = parse_native_credentials(native_auth)?;

    let ffxi_settings = match get("ffxi_settings") {
        Some(Value::Null) | None => None,
        Some(Value::Object(fields)) => Some(parse_ffxi_settings(fields)?),
        Some(_) => return Err(ProtoError::Field("ffxi_settings", "must be an object or null")),
    };

    let flag = |key, default| match get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(ProtoError::Field(key, "must be a boolean")),
    };
    let start_game = flag("start_game", true)?;
    let keep_session = flag("keep_session", false)?;
    let viewer_gaps = flag("viewer_gaps", false)?;
    let auto_away_minutes = auto_away_minutes(get("auto_away_minutes"))?;
    let auto_login = parse_auto_login(get("auto_login"))?;
    Ok(Session { region, ucs_area, install_dir, pol_install_dir, native_auth, start_game, keep_session, viewer_gaps, auto_away_minutes, auto_login, ffxi_settings })
}

/// Viewer auto-Away minutes: 0 off, else 10..=60; absent = 10.
fn auto_away_minutes(value: Option<&Value<'_>>) -> Result<u32, ProtoError> {
    let Some(value) = value else { return Ok(10) };
    value.as_i64().filter(|m| *m == 0 || (10..=60).contains(m))
        .map(|m| m as u32).ok_or(ProtoError::Field("auto_away_minutes", "must be 0 or 10-60"))
}

// --- round 2: the parked warm host (pol-spec 07d §1; 07a parked-warm-host) ---

/// Set when this loader can park: `park` line → polcore created (COM, paths,
/// SetAreaCode/SetParamInit, function table) → wait without a timeout for the
/// `credentials` line → the normal sign-in. Advertised in the hello; the
/// launcher sends `park` only to a loader that says so.
pub const PARK: bool = true;

/// The `park` line: everything the host needs before polcore creation and
/// nothing secret. Never carries credentials, keys, records or the per-Play
/// flags (those come in `credentials`).
#[derive(Debug)]
pub struct Park {
    pub region: Region,
    pub ucs_area: Option<String>,
    pub install_dir: Option<String>,
    pub pol_install_dir: Option<String>,
    pub auto_login: AutoLogin,
    pub ffxi_settings: Option<FfxiSettings>,
}

/// The `credentials` line sent to a parked host at Play: the native
/// credentials (OTP included) and the per-Play session flags.
pub struct Unpark {
    pub native_auth: NativeCredentials,
    pub start_game: bool,
    pub keep_session: bool,
    pub viewer_gaps: bool,
    pub auto_away_minutes: u32,
}

impl fmt::Debug for Unpark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("Unpark([redacted])") }
}

/// The first launcher line after the hello.
#[derive(Debug)]
pub enum FirstLine {
    /// Today's direct `session` line (every older launcher, every cold launch).
    Session(Session),
    /// A credential-less park (round 2).
    Park(Park),
    WindowerStartup(WindowerStartup),
}

pub struct WindowerStartup { pub hook:String, pub nonce:String, pub data:zeroize::Zeroizing<Vec<u8>> }
impl fmt::Debug for WindowerStartup {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result{f.write_str("WindowerStartup([private configuration])")}}

impl Park {
    /// The session a direct `session` line with the same fields gives, so the
    /// parked path continues exactly like the cold one (07d §1.4).
    pub fn into_session(self, unpark: Unpark) -> Session {
        let Unpark { native_auth, start_game, keep_session, viewer_gaps, auto_away_minutes } = unpark;
        Session {
            region: self.region, ucs_area: self.ucs_area, install_dir: self.install_dir, pol_install_dir: self.pol_install_dir,
            native_auth, start_game, keep_session, viewer_gaps, auto_away_minutes,
            auto_login: self.auto_login, ffxi_settings: self.ffxi_settings,
        }
    }
}

/// Session keys and records from the retired offline path: refused everywhere.
const LEGACY_FIELDS: [&str; 5] = ["v_key", "auth_code", "account_id", "pol_records", "content_ids"];
/// Secret or session-key fields: never in a `park` line, never in `credentials`
/// except `native_auth`.
const SECRET_FIELDS: [&str; 6] = ["v_key", "auth_code", "account_id", "pol_records", "content_ids", "native_auth"];
/// Per-Play flags: only in `credentials` (or a direct `session`).
const PLAY_FIELDS: [&str; 4] = ["start_game", "keep_session", "viewer_gaps", "auto_away_minutes"];
/// Host fields: only in `park` (or a direct `session`).
const HOST_FIELDS: [&str; 7] = ["region", "lang", "install_dir", "pol_install_dir", "ffxi_settings", "auto_login", "ucs_area"];

/// Parses the first launcher line: `session` (unchanged) or `park`.
pub fn parse_first_line(line: &[u8]) -> Result<FirstLine, ProtoError> {
    if line.len() > MAX_LINE { return Err(ProtoError::LineTooLong); }
    let root = json::parse(strip_eol(line)).map_err(ProtoError::Json)?;
    match root.get("type").and_then(Value::as_jstr) {
        Some(t) if t.eq_str("park") => parse_park(&root).map(FirstLine::Park),
        Some(t) if t.eq_str("windower_startup") => {
            duplicate_known(&root,&["type","v","hook","nonce","data"])?;
            for field in SECRET_FIELDS.into_iter().chain(HOST_FIELDS).chain(PLAY_FIELDS) {
                if root.get(field).is_some(){return Err(ProtoError::Field(field,"never part of Windower startup"));}
            }
            let version=root.get("v").and_then(Value::as_i64).ok_or(ProtoError::Missing("v"))?;
            if version!=PROTOCOL_VERSION{return Err(ProtoError::UnsupportedVersion(version));}
            let text=|key|root.get(key).and_then(Value::as_jstr).ok_or(ProtoError::Missing(key))?.decode().map_err(|_|ProtoError::Field(key,"invalid text"));
            let hook=text("hook")?;let nonce=text("nonce")?;let data=zeroize::Zeroizing::new(text("data")?);
            if hook.len()>32767||hook.contains('\0')||!std::path::Path::new(&hook).is_absolute(){return Err(ProtoError::Field("hook","invalid path"));}
            if crate::windower_bootstrap_contract::private_names(&nonce).is_none(){return Err(ProtoError::Field("nonce","invalid startup nonce"));}
            if b64::decoded_len(data.as_bytes())!=Some(crate::windower_bootstrap_contract::SETTINGS_LEN){return Err(ProtoError::Field("data","invalid startup record length"));}
            let mut bytes=zeroize::Zeroizing::new(vec![0;crate::windower_bootstrap_contract::SETTINGS_LEN]);
            b64::decode_into(data.as_bytes(),&mut bytes).map_err(|_|ProtoError::Field("data","invalid encoding"))?;
            crate::windower_bootstrap_contract::validate(&bytes).map_err(|_|ProtoError::Field("data","invalid startup record"))?;
            Ok(FirstLine::WindowerStartup(WindowerStartup{hook,nonce,data:bytes}))
        }
        _ => parse_session(line).map(FirstLine::Session),
    }
}

fn duplicate_known(root: &Value<'_>, known: &[&'static str]) -> Result<(), ProtoError> {
    let Value::Object(fields) = root else { return Err(ProtoError::NotAnObject) };
    first_duplicate(fields, known).map_or(Ok(()), |k| Err(ProtoError::DuplicateField(k)))
}

fn count(fields: &[(JStr<'_>, Value<'_>)], name: &str) -> usize {
    fields.iter().filter(|(key, _)| key.eq_str(name)).count()
}

/// The first of `names` that appears more than once.
fn first_duplicate<'a>(fields: &[(JStr<'_>, Value<'_>)], names: &[&'a str]) -> Option<&'a str> {
    names.iter().copied().find(|name| count(fields, name) > 1)
}

/// Every one of `names` appears exactly once (none missing, none repeated).
fn exactly_once(fields: &[(JStr<'_>, Value<'_>)], names: &[&str]) -> bool {
    names.iter().all(|name| count(fields, name) == 1)
}

/// A nonzero unsigned 64-bit JSON number (identities and deadlines).
fn nonzero_u64(value: Option<&Value<'_>>) -> Option<u64> {
    match value { Some(Value::Number(n)) => n.parse().ok().filter(|u| *u != 0), _ => None }
}

fn parse_park(root: &Value<'_>) -> Result<Park, ProtoError> {
    duplicate_known(root, &["type", "v"])?;
    duplicate_known(root, &HOST_FIELDS)?;
    let get = |k: &'static str| root.get(k);
    if let Some(k) = SECRET_FIELDS.into_iter().find(|&k| get(k).is_some()) {
        return Err(ProtoError::Field(k, "never part of a park line"));
    }
    if let Some(k) = PLAY_FIELDS.into_iter().find(|&k| get(k).is_some()) {
        return Err(ProtoError::Field(k, "belongs in the credentials line"));
    }
    let version = get("v").ok_or(ProtoError::Missing("v"))?.as_i64().ok_or(ProtoError::Field("v", "not an integer"))?;
    if version != PROTOCOL_VERSION { return Err(ProtoError::UnsupportedVersion(version)); }
    let region = match plain_str(get("region"), "region")? {
        Some(b"US") => Region::Us,
        Some(b"EU") => Region::Eu,
        Some(b"JP") => Region::Jp,
        Some(_) => return Err(ProtoError::Field("region", "must be US, EU or JP")),
        None => return Err(ProtoError::Missing("region")),
    };
    let ucs_area = parse_ucs_area(get("ucs_area"), region)?;
    let text = |k: &'static str| -> Result<Option<String>, ProtoError> {
        match get(k) {
            Some(Value::Str(s)) => Ok(Some(s.decode().map_err(|_| ProtoError::Field(k, "bad string"))?).filter(|s| !s.is_empty())),
            Some(Value::Null) | None => Ok(None),
            Some(_) => Err(ProtoError::Field(k, "not a string")),
        }
    };
    text("lang")?; // checked, not used (see parse_session)
    let install_dir = text("install_dir")?;
    let pol_install_dir = text("pol_install_dir")?;
    if pol_install_dir.as_ref().is_some_and(|p| p.len() > 32_767 || p.contains('\0')) {
        return Err(ProtoError::Field("pol_install_dir", "invalid folder path"));
    }
    let ffxi_settings = match get("ffxi_settings") {
        Some(Value::Null) | None => None,
        Some(Value::Object(fields)) => Some(parse_ffxi_settings(fields)?),
        Some(_) => return Err(ProtoError::Field("ffxi_settings", "must be an object or null")),
    };
    let auto_login = parse_auto_login(get("auto_login"))?;
    Ok(Park { region, ucs_area, install_dir, pol_install_dir, auto_login, ffxi_settings })
}

/// Parses the `credentials` line a parked host waits for (no timeout).
pub fn parse_credentials(line: &[u8]) -> Result<Unpark, ProtoError> {
    if line.len() > MAX_LINE { return Err(ProtoError::LineTooLong); }
    let root = json::parse(strip_eol(line)).map_err(ProtoError::Json)?;
    duplicate_known(&root, &["type"])?;
    duplicate_known(&root, &PLAY_FIELDS)?;
    duplicate_known(&root, &SECRET_FIELDS)?;
    let get = |k: &'static str| root.get(k);
    match get("type").and_then(Value::as_jstr) {
        Some(t) if t.eq_str("credentials") => {}
        Some(_) => return Err(ProtoError::WrongType),
        None => return Err(ProtoError::Missing("type")),
    }
    if let Some(k) = SECRET_FIELDS.into_iter().filter(|&k| k != "native_auth").find(|&k| get(k).is_some()) {
        return Err(ProtoError::Field(k, "never part of a credentials line"));
    }
    if let Some(k) = HOST_FIELDS.into_iter().find(|&k| get(k).is_some()) {
        return Err(ProtoError::Field(k, "belongs in the park line"));
    }
    let native_auth = parse_native_credentials(get("native_auth").ok_or(ProtoError::Missing("native_auth"))?)?;
    let flag = |key, default| match get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(ProtoError::Field(key, "must be a boolean")),
    };
    let auto_away_minutes = auto_away_minutes(get("auto_away_minutes"))?;
    let (start_game, keep_session) = (flag("start_game", true)?, flag("keep_session", false)?);
    Ok(Unpark { native_auth, start_game, keep_session, viewer_gaps: flag("viewer_gaps", false)?, auto_away_minutes })
}

/// `ffxi_settings`: value name -> JSON number (`REG_DWORD`, -2^31..2^32-1) or string (`REG_SZ`).
fn parse_ffxi_settings(fields: &[(JStr<'_>, Value<'_>)]) -> Result<FfxiSettings, ProtoError> {
    const KEY: &str = "ffxi_settings";
    let mut entries = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        let name = name.decode().map_err(|_| ProtoError::Field(KEY, "bad value name"))?;
        let value = match value {
            Value::Number(_) => {
                let n = value.as_i64().ok_or(ProtoError::Field(KEY, "numbers must be integers"))?;
                if !(i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&n) {
                    return Err(ProtoError::Field(KEY, "numbers must fit in 32 bits"));
                }
                SettingValue::Dword(n as u32)
            }
            Value::Str(s) => SettingValue::Text(s.decode().map_err(|_| ProtoError::Field(KEY, "bad string"))?),
            _ => return Err(ProtoError::Field(KEY, "values must be numbers or strings")),
        };
        entries.push((name, value));
    }
    FfxiSettings::new(entries).map_err(|why| ProtoError::Field(KEY, why))
}

fn plain_str<'a>(v: Option<&Value<'a>>, key: &'static str) -> Result<Option<&'a [u8]>, ProtoError> {
    match v {
        None => Ok(None),
        Some(Value::Str(s)) => s.plain().map(Some).ok_or(ProtoError::Field(key, "unexpected escape")),
        Some(_) => Err(ProtoError::Field(key, "not a string")),
    }
}

fn strip_eol(mut line: &[u8]) -> &[u8] {
    while let [rest @ .., b'\n' | b'\r'] = line {
        line = rest;
    }
    line
}

// --- loader -> launcher lines (each ends in '\n') ---

/// `viewer_check`: this loader asks for the sign-in's W2U at Connect
/// (`native_viewer_check`, 07c §1). The launcher sends no W2U of its own, so
/// it refuses a native sign-in from a loader without it. `park`: it accepts a
/// `park` line before credentials (07d §1). `windower_private`: it accepts a
/// credential-free `windower_startup` line before the session or park.
pub fn hello_line(pid: u32) -> String {
    format!("{{\"type\":\"hello\",\"v\":{PROTOCOL_VERSION},\"pid\":{pid},\"native_auth\":true,\"native_social\":true,\"native_batch\":true,\"viewer_check\":true,\"startup_sign_out\":true,\"park\":{PARK},\"windower_private\":true}}\n")
}

/// The park reached its end point: polcore created, SetAreaCode/SetParamInit
/// done, function table validated; nothing networked. `polcore_ms` is the
/// instance creation time (polcore's second-boundary wait, 08 §2). Sent once,
/// then the host waits for `credentials` with no timeout (07d §1.3).
pub fn parked_line(pid: u32, polcore_ms: u64) -> String {
    format!("{{\"type\":\"parked\",\"pid\":{pid},\"polcore_ms\":{polcore_ms}}}\n")
}

pub fn started_line(pid: u32) -> String {
    format!("{{\"type\":\"started\",\"pid\":{pid}}}\n")
}

/// Local controller acknowledgement. Request zero advertises the scoped live service.
pub fn game_shutdown_result(id: u64, game: u64, ok: bool, message: &str) -> String {
    format!("{{\"type\":\"game_shutdown_result\",\"request_id\":{id},\"game_id\":{game},\"ok\":{ok},\"message\":{}}}\n", json_string(message))
}

pub fn controller_result(id: u64, ok: bool, message: &str) -> String {
    format!("{{\"type\":\"controller_result\",\"request_id\":{id},\"ok\":{ok},\"message\":{}}}\n", json_string(message))
}

pub fn exit_line(code: i32) -> String {
    format!("{{\"type\":\"exit\",\"code\":{code}}}\n")
}

pub fn error_line(message: &str) -> String {
    format!("{{\"type\":\"error\",\"message\":{}}}\n", json_string(message))
}

/// One line for the launcher's launch history: a protection this host could
/// not provide, and why. Never an error (the game goes on) and never user
/// data. An older launcher ignores the type.
pub fn notice_line(message: &str) -> String {
    format!("{{\"type\":\"notice\",\"message\":{}}}\n", json_string(message))
}

/// Ask the launcher for the Viewer's W2U (:54000) check of this Connect: sent
/// once per sign-in, after Open(0) reached 838 == 2 and before Open(1)
/// (pol-spec 07a w2u-at-connect F3/F4, 07c §1). The answer is
/// `native_viewer_check_result` (`ViewerCheckResult`).
pub fn viewer_check_line() -> String {
    "{\"type\":\"native_viewer_check\"}\n".into()
}

/// The Viewer reached its main menu: the app-1000 presence publish (376/377,
/// `publish_viewer`) has completed, after sign-in (`returned` false) or after
/// a game (`returned` true). `code` is the publish job's result, 0 when no
/// publish was needed. Sent before the idle report (`native_session_state`
/// false, `native_social_ready`); the launcher starts W20 (:53001) only
/// after it (07c §2).
pub fn main_menu_line(code: i32, returned: bool) -> String {
    format!("{{\"type\":\"native_main_menu\",\"code\":{code},\"returned\":{returned}}}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_shutdown_protocol_is_fixed_scoped_and_unambiguous() {
        let line = r#"{"type":"game_shutdown","request_id":18446744073709551615,"game_id":7,"deadline_ms":1000}"#;
        assert!(matches!(parse_service_event(line.as_bytes()), Ok(Some(ServiceEvent::GameShutdown { id:u64::MAX,game:7,deadline_ms:1000 }))));
        for line in [
            line.replace("\"game_id\":7", "\"game_id\":0"),
            line.replace("\"game_id\":7", "\"game_id\":7,\"game_id\":8"),
            line.replace("\"request_id\":18446744073709551615", "\"request_id\":-1"),
            line.replace("\"deadline_ms\":1000", "\"deadline_ms\":1000,\"command\":\"/say unsafe\""),
        ] { assert!(parse_service_event(line.as_bytes()).is_err()); }
        let reply=game_shutdown_result(1,7,true,"Requested");
        let value=json::parse(reply.trim().as_bytes()).unwrap();
        assert_eq!(value.get("game_id"),Some(&Value::Number("7")));
    }

    #[test]
    fn controller_commands_are_correlated_and_cannot_change_other_settings() {
        let command = r#"{"type":"controller_settings","request_id":18446744073709551615,"deadline_ms":123456,"settings":{"padmode000":"1,1,0,0,1,1","padsin000":"1,2,3"}}"#;
        assert!(matches!(parse_service_event(command.as_bytes()),
            Ok(Some(ServiceEvent::ControllerSettings { id: u64::MAX, deadline_ms: 123456, settings })) if settings.len() == 2));
        for (index, invalid) in [
            command.replace("18446744073709551615", "0"),
            command.replace("18446744073709551615", "-1"),
            command.replace("18446744073709551615", "1.5"),
            command.replace("18446744073709551615", "18446744073709551616"),
            command.replace("123456", "0"),
            command.replace("123456", "-1"),
            command.replace("\"padmode000\"", "\"0001\""),
            command.replace("\"padsin000\":\"1,2,3\"", "\"padsin000\":42"),
            command.replace("\"settings\":", "\"request_id\":1,\"settings\":"),
            command.replace("\"settings\":", "\"deadline_ms\":999,\"settings\":"),
            command.replace("\"settings\":", "\"type\":\"native_start_game\",\"settings\":"),
            command.replace("\"padsin000\":", "\"padmode000\":\"1,1,1,1,1,1\",\"padsin000\":"),
        ].into_iter().enumerate() { assert!(parse_service_event(invalid.as_bytes()).is_err(), "invalid controller case {index}"); }
        let line = controller_result(u64::MAX, false, "Cannot apply \"settings\".");
        let reply = json::parse(line.trim().as_bytes()).unwrap();
        assert_eq!(reply.get("request_id"), Some(&Value::Number("18446744073709551615")));
        assert_eq!(reply.get("ok"), Some(&Value::Bool(false)));
        assert_eq!(reply.get("message").unwrap().as_jstr().unwrap().decode().unwrap(), "Cannot apply \"settings\".");
    }
    #[test]
    fn native_social_actions_preserve_text_and_u64_ids_and_reject_ambiguous_requests() {
        let send = br#"{"type":"native_social","id":"0123456789abcdef0123456789abcdef","action":"send","alternate":false,"slot":199,"uid":18446744073709551615,"subject":"quoted\"subject","message":"line\nline"}"#;
        let Some(ServiceEvent::NativeSocial(request)) = parse_service_event(send).unwrap() else { panic!("missing action") };
        let NativeSocialRequest::Send { slot, uid, subject, message, .. } = &request else { panic!("wrong action") };
        assert_eq!(*slot, 199); assert_eq!(*uid, u64::MAX); assert_eq!(subject, "quoted\"subject"); assert_eq!(message, "line\nline");
        let text = std::str::from_utf8(send).unwrap();
        for bad in [text.replace("199", "200"), text.replace("false", "true"), text.replace("18446744073709551615", "0"),
            text.replace("\"send\"", "\"send\",\"action\":\"remove\""), text.replace("line\\nline", "line\\u0007line"), text.replace("0123456789abcdef0123456789abcdef", "bad-id")] {
            assert!(parse_service_event(bad.as_bytes()).is_err());
        }
        let header = b64::encode(&[0; 72]);
        let read = format!("{{\"type\":\"native_social\",\"id\":\"0123456789abcdef0123456789abcdef\",\"action\":\"read\",\"header\":\"{header}\"}}");
        assert!(matches!(parse_service_event(read.as_bytes()), Ok(Some(ServiceEvent::NativeSocial(NativeSocialRequest::Read { .. })))));
        assert!(parse_service_event(read.replace(&header, "AA==").as_bytes()).is_err());
    }

    fn native_fields() -> String {
        format!("\"native_auth\":{{\"pol_id\":\"{}\",\"pol_password\":\"{}\",\"se_id\":\"{}\",\"se_secret\":\"{}\",\"otp\":\"{}\",\"profile_key\":\"{}\"}}",
            b64::encode(b"ABCD1234"),b64::encode(b"fake password"),b64::encode(b"fakeuser"),b64::encode(&[0x51;20]),b64::encode(b"123456"),"a".repeat(40))
    }

    #[test]
    fn windower_startup_configuration_is_bounded_and_separate_from_credentials(){
        let data=crate::windower_bootstrap_contract::tests::synthetic("Separate profile",1600);
        let line=format!("{{\"type\":\"windower_startup\",\"v\":1,\"hook\":\"C:\\\\Windower\\\\Hook.dll\",\"nonce\":\"0123456789abcdef01234567\",\"data\":\"{}\"}}",b64::encode(&data));
        let parsed=parse_first_line(line.as_bytes()).unwrap();
        let FirstLine::WindowerStartup(startup)=&parsed else{panic!("wrong configuration kind")};
        assert_eq!(&*startup.data,&data);
        assert!(!format!("{parsed:?}").contains("Separate profile"));
        for extra in ["\"native_auth\":{}","\"v_key\":\"secret\"","\"start_game\":true"]{
            let altered=format!("{},{} }}",line.trim_end_matches('}'),extra);
            assert!(parse_first_line(altered.as_bytes()).is_err());
        }
        assert!(parse_first_line(line.replace("0123456789abcdef01234567","bad").as_bytes()).is_err());
        assert!(parse_first_line(line.replace(&b64::encode(&data),"AA==").as_bytes()).is_err());
    }

    #[test]
    fn native_session_is_bounded_exclusive_and_secret_safe() {
        let native=native_fields();
        let parsed=parse_session(line(&native).as_bytes()).unwrap();
        let social = parse_session(line(&format!("{native},\"start_game\":false,\"keep_session\":true")).as_bytes()).unwrap();
        assert!(!social.start_game && social.keep_session);
        let credentials=&parsed.native_auth;
        assert_eq!(&credentials.pol_id,b"ABCD1234\0");
        assert_eq!(credentials.otp,Some(*b"123456"));
        assert_eq!(credentials.se_secret,[0x51;20]);
        assert!(!format!("{parsed:?}").contains("fake password"));
        assert!(parse_session(line(&format!("{native},{}",keys())).as_bytes()).is_err());
        assert!(parse_session(line(&native).replace("\"US\"","\"EU\"").as_bytes()).is_err());
        for (old,new) in [(b64::encode(b"ABCD1234"),b64::encode(b"ABCD123\0")),
            (b64::encode(&[0x51;20]),b64::encode(&[0x51;21])),
            (b64::encode(b"123456"),b64::encode(b"12x456")),
            ("a".repeat(40),"../home00".into())] {
            let error=parse_session(line(&native.replace(&old,&new)).as_bytes()).unwrap_err().to_string();
            assert!(!error.contains("fake password"));assert!(!error.contains("../home00"));
        }
        let duplicate=native.replace("\"otp\":", "\"se_secret\":\"AAAA\",\"otp\":");
        assert!(parse_session(line(&duplicate).as_bytes()).is_err());
        let no_otp=native.replace(&format!("\"{}\"",b64::encode(b"123456")),"null");
        assert!(parse_session(line(&no_otp).as_bytes()).unwrap().native_auth.otp.is_none());
    }

    #[test]
    fn native_handle_names_are_optional_bounded_utf8_and_redacted() {
        let native=native_fields();
        assert!(parse_session(line(&native).as_bytes()).unwrap().native_auth.handle_names.is_empty());
        let named=native.replacen("\"profile_key\":",&format!("\"handle_names\":\"{}\",\"profile_key\":",b64::encode(b"PublicName, Other")),1);
        let parsed=parse_session(line(&named).as_bytes()).unwrap();
        assert_eq!(parsed.native_auth.handle_names,"PublicName, Other");
        assert!(!format!("{parsed:?}").contains("PublicName"));
        for bytes in [vec![b'A';513],vec![0xff],b"Name\n".to_vec()] {
            let invalid=native.replacen("\"profile_key\":",&format!("\"handle_names\":\"{}\",\"profile_key\":",b64::encode(&bytes)),1);
            assert!(parse_session(line(&invalid).as_bytes()).is_err());
        }
    }

    #[test]
    fn viewer_status_options_and_activity_parse() {
        let status = |line: &[u8]| match parse_service_event(line) { Ok(Some(ServiceEvent::NativeStatus { mode, groups })) => Ok((mode, groups)), Ok(_) => Err(()), Err(_) => Err(()) };
        assert_eq!(status(br#"{"type":"native_visible"}"#), Ok((0, true)), "the dialog applies to groups by default");
        assert_eq!(status(br#"{"type":"native_away","groups":false}"#), Ok((1, false)));
        assert_eq!(status(br#"{"type":"native_invisible","groups":true}"#), Ok((4, true)));
        assert!(parse_service_event(br#"{"type":"native_visible","groups":1}"#).is_err());
        assert!(matches!(parse_service_event(br#"{"type":"native_activity"}"#), Ok(Some(ServiceEvent::NativeActivity))));
        assert!(matches!(parse_service_event(br#"{"type":"native_portrait","portrait":8191}"#), Ok(Some(ServiceEvent::NativePortrait(8191)))));
        for bad in [&br#"{"type":"native_portrait","portrait":8192}"#[..], br#"{"type":"native_portrait","portrait":-1}"#, br#"{"type":"native_portrait"}"#] {
            assert!(parse_service_event(bad).is_err());
        }
        let native = native_fields();
        assert_eq!(parse_session(line(&native).as_bytes()).unwrap().auto_away_minutes, 10, "Viewer default");
        for (value, minutes) in [("0", 0), ("10", 10), ("35", 35), ("60", 60)] {
            assert_eq!(parse_session(line(&format!("{native},\"auto_away_minutes\":{value}")).as_bytes()).unwrap().auto_away_minutes, minutes);
        }
        for bad in ["5", "61", "-1", "10.5", "\"10\"", "null"] {
            assert!(parse_session(line(&format!("{native},\"auto_away_minutes\":{bad}")).as_bytes()).is_err(), "{bad}");
        }
        assert!(parse_session(line(&format!("{native},\"auto_away_minutes\":10,\"auto_away_minutes\":20")).as_bytes()).is_err());
        assert!(parse_session(line(&format!("{},\"auto_away_minutes\":10", keys())).as_bytes()).is_err());
    }

    fn line(fields: &str) -> String {
        format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"install_dir\":\"C:\\\\Games\\\\SquareEnix\\\\FINAL FANTASY XI\",{fields}}}\n")
    }

    fn keys() -> String {
        format!("\"v_key\":\"{}\",\"auth_code\":\"{}\"", b64::encode(&[0x11; 16]), b64::encode(&[0x22; 52]))
    }

    #[test]
    fn auto_login_defaults_are_manual_and_only_exact_slots_are_accepted() {
        let native = native_fields();
        assert_eq!(parse_session(line(&native).as_bytes()).unwrap().auto_login, AutoLogin::default());
        for slot in [1, 16] {
            let fields = format!("{native},\"auto_login\":{{\"enabled\":true,\"character_slot\":{slot}}}");
            assert_eq!(parse_session(line(&fields).as_bytes()).unwrap().auto_login,
                AutoLogin { enabled: true, character_slot: slot });
        }
        for bad in ["null", "true", r#"{"enabled":1}"#, r#"{"character_slot":0}"#,
            r#"{"character_slot":17}"#, r#"{"character_slot":1.5}"#,
            r#"{"enabled":true,"enabled":false}"#, r#"{"character_slot":1,"character_slot":2}"#] {
            let fields = format!("{native},\"auto_login\":{bad}");
            assert!(parse_session(line(&fields).as_bytes()).is_err());
        }
        let fields = format!("{},\"auto_login\":{{\"enabled\":true}}", keys());
        assert!(parse_session(line(&fields).as_bytes()).is_err(), "offline session must not send keys");
        let fields = format!("{native},\"auto_login\":{{}},\"auto_login\":{{}}");
        assert!(parse_session(line(&fields).as_bytes()).is_err());
    }

    #[test]
    fn explicit_pol_folder_is_optional_bounded_and_unambiguous() {
        let native = native_fields();
        assert!(parse_session(line(&native).as_bytes()).unwrap().pol_install_dir.is_none());
        let fields = format!("{native},\"pol_install_dir\":\"C:\\\\Games\\\\PlayOnlineViewer\"");
        assert_eq!(parse_session(line(&fields).as_bytes()).unwrap().pol_install_dir.as_deref(), Some(r"C:\Games\PlayOnlineViewer"));
        for bad in ["true", "0", r#""""#, r#""C:\\Games\u0000x""#] {
            let fields = format!("{native},\"pol_install_dir\":{bad}");
            assert!(parse_session(line(&fields).as_bytes()).is_err());
        }
        let fields = format!("{native},\"pol_install_dir\":\"{}\"", "x".repeat(32_768));
        assert!(parse_session(line(&fields).as_bytes()).is_err());
        let fields = format!("{native},\"pol_install_dir\":null,\"pol_install_dir\":null");
        assert!(parse_session(line(&fields).as_bytes()).is_err());
    }

    #[test]
    fn retired_offline_service_lines_are_ignored() {
        // The launcher sent these only for launcher-made session keys.
        for line in [format!("{{\"type\":\"session_update\",{}}}", keys()), "{\"type\":\"friends_result\",\"ok\":false}".into(),
            "{\"type\":\"session_status\",\"code\":-1038}".into(), "{\"type\":\"presence_result\",\"ok\":true,\"changed\":true}".into(),
            "{\"type\":\"future_thing\",\"x\":1}".into()] {
            assert!(matches!(parse_service_event(line.as_bytes()), Ok(None)), "{line}");
        }
        assert!(parse_service_event(br#"{"x":1}"#).is_err(), "a command names its type");
        assert!(parse_service_event(b"not json").is_err());
    }

    #[test]
    fn parses_a_contract_session() {
        let s = parse_session(line(&format!("{},\"future\":{{\"x\":[1]}}", native_fields())).as_bytes()).unwrap();
        assert_eq!(s.region, Region::Us);
        assert_eq!(s.install_dir.as_deref(), Some(r"C:\Games\SquareEnix\FINAL FANTASY XI"));
        assert!(s.start_game);
        assert!(!s.keep_session);
        for extra in ["\"start_game\":1", "\"keep_session\":null", "\"keep_session\":false,\"keep_session\":false", "\"lang\":1"] {
            assert!(parse_session(line(&format!("{},{}", native_fields(), extra)).as_bytes()).is_err(), "{extra}");
        }
        assert!(matches!(parse_service_event(br#"{"type":"native_start_game"}"#), Ok(Some(ServiceEvent::NativeStartGame))));
        assert!(matches!(parse_service_event(br#"{"type":"native_sign_out"}"#), Ok(Some(ServiceEvent::NativeSignOut))));
    }

    #[test]
    fn batch_game_command_has_only_a_bounded_gate_token() {
        let token = "0123456789abcdef0123456789abcdef-00000001-0000000000000001-00";
        let line = format!("{{\"type\":\"native_batch_game\",\"token\":\"{token}\"}}");
        assert!(matches!(parse_service_event(line.as_bytes()), Ok(Some(ServiceEvent::NativeBatchGame(value))) if value == token));
        for bad in ["", "short", "../../unexpected"] {
            assert!(parse_service_event(format!("{{\"type\":\"native_batch_game\",\"token\":\"{bad}\"}}").as_bytes()).is_err());
        }
        assert!(parse_service_event(br#"{"type":"native_batch_game"}"#).is_err());
    }

    #[test]
    fn every_session_is_a_native_sign_in_and_launcher_made_keys_are_refused() {
        let native = native_fields();
        assert_eq!(parse_session(line(&keys()).as_bytes()).unwrap_err(), ProtoError::Missing("native_auth"));
        assert_eq!(parse_session(line(&format!("{native},\"native_auth\":null")).as_bytes()).unwrap_err(), ProtoError::DuplicateField("native_auth"));
        for legacy in [keys(), "\"account_id\":\"0123456789abcdef\"".into(), "\"pol_records\":null".into(), "\"content_ids\":[1]".into()] {
            assert!(matches!(parse_session(line(&format!("{native},{legacy}")).as_bytes()), Err(ProtoError::Field("native_auth", _))), "{legacy}");
        }
        let v2 = line(&native).replace("\"v\":1", "\"v\":2");
        assert_eq!(parse_session(v2.as_bytes()).unwrap_err(), ProtoError::UnsupportedVersion(2));
        let hello = "{\"type\":\"hello\",\"v\":1,\"pid\":3}";
        assert_eq!(parse_session(hello.as_bytes()).unwrap_err(), ProtoError::WrongType);
        let region = line(&native).replace("\"region\":\"US\"", "\"region\":\"KR\"");
        assert!(matches!(parse_session(region.as_bytes()), Err(ProtoError::Field("region", _))));
        for region in ["EU", "JP"] {
            let other = line(&native).replace("\"region\":\"US\"", &format!("\"region\":\"{region}\""));
            assert_eq!(parse_session(other.as_bytes()).unwrap_err(), ProtoError::Missing("ucs_area"), "{region}");
            for legacy in [keys(), "\"pol_records\":null".into()] {
                let other = line(&format!("{native},\"ucs_area\":\"02\",{legacy}")).replace("\"region\":\"US\"", &format!("\"region\":\"{region}\""));
                assert!(matches!(parse_session(other.as_bytes()), Err(ProtoError::Field("native_auth", _))), "{region} {legacy}");
            }
        }
    }

    #[test]
    fn eu_and_jp_sessions_carry_their_ucs_area_and_us_sessions_never_do() {
        let native = native_fields();
        let us = parse_session(line(&native).as_bytes()).unwrap();
        assert_eq!((us.region, us.ucs_area.as_deref(), us.region.experimental()), (Region::Us, None, false));
        assert!(matches!(parse_session(line(&format!("{native},\"ucs_area\":\"01\"")).as_bytes()), Err(ProtoError::Field("ucs_area", _))),
            "US keeps its verified value and its unchanged line");
        for (region, expected) in [("EU", Region::Eu), ("JP", Region::Jp)] {
            let other = |area: &str| line(&format!("{native},\"ucs_area\":{area}")).replace("\"region\":\"US\"", &format!("\"region\":\"{region}\""));
            let parsed = parse_session(other("\"03\"").as_bytes()).unwrap();
            assert_eq!((parsed.region, parsed.ucs_area.as_deref(), parsed.region.experimental()), (expected, Some("03"), true));
            for bad in ["\"3\"", "\"003\"", "\"a1\"", "3", "null", "\"\\u0030\\u0031\""] {
                assert!(parse_session(other(bad).as_bytes()).is_err(), "{region} {bad}");
            }
            let twice = line(&format!("{native},\"ucs_area\":\"03\",\"ucs_area\":\"03\"")).replace("\"region\":\"US\"", &format!("\"region\":\"{region}\""));
            assert_eq!(parse_session(twice.as_bytes()).unwrap_err(), ProtoError::DuplicateField("ucs_area"));
        }
    }

    #[test]
    fn errors_never_echo_secrets() {
        let secretish = "\"v_key\":\"SECRETSECRETSECRET\",\"auth_code\":\"AAAA\"";
        let e = parse_session(line(secretish).as_bytes()).unwrap_err().to_string();
        assert!(!e.contains("SECRET"), "{e}");
        let broken = "{\"v_key\":\"SECRETSECRET\" oops}";
        let e = parse_session(broken.as_bytes()).unwrap_err().to_string();
        assert!(!e.contains("SECRET"), "{e}");
    }

    #[test]
    fn ffxi_settings_parse() {
        // What the launcher sends for a Windower profile (src/pol_less/ffxi_settings.rs).
        let native = native_fields();
        let fields = format!(
            "{native},\"ffxi_settings\":{{\"0000\":0,\"0001\":1920,\"0002\":1080,\"0028\":3212836864,\"0034\":3,\"0042\":\"text \\\"q\\\"\",\"padmode000\":\"1,0,1,0,1,0\",\"neg\":-1}}"
        );
        let s = parse_session(line(&fields).as_bytes()).unwrap();
        let fx = s.ffxi_settings.unwrap();
        assert_eq!(fx.len(), 8);
        assert_eq!(fx.get("0001"), Some(&SettingValue::Dword(1920)));
        assert_eq!(fx.get("0028"), Some(&SettingValue::Dword(0xBF80_0000)));
        assert_eq!(fx.get("0042"), Some(&SettingValue::Text("text \"q\"".into())));
        assert_eq!(fx.get("PADMODE000"), Some(&SettingValue::Text("1,0,1,0,1,0".into())));
        assert_eq!(fx.get("neg"), Some(&SettingValue::Dword(u32::MAX)));

        // Absent or null: no overrides; an empty object is allowed.
        assert!(parse_session(line(&native).as_bytes()).unwrap().ffxi_settings.is_none());
        assert!(parse_session(line(&format!("{native},\"ffxi_settings\":null")).as_bytes()).unwrap().ffxi_settings.is_none());
        assert!(parse_session(line(&format!("{native},\"ffxi_settings\":{{}}")).as_bytes()).unwrap().ffxi_settings.unwrap().is_empty());

        for bad in [
            "[1]",
            "\"x\"",
            "{\"0001\":1.5}",
            "{\"0001\":4294967296}",
            "{\"0001\":-2147483649}",
            "{\"0001\":true}",
            "{\"0001\":null}",
            "{\"0001\":[1]}",
            "{\"0001\":1,\"0001\":2}",
            "{\"Pad\":1,\"pAD\":2}",
            "{\"\":1}",
            "{\"a\\\\b\":1}",
        ] {
            let e = parse_session(line(&format!("{native},\"ffxi_settings\":{bad}")).as_bytes());
            assert!(matches!(e, Err(ProtoError::Field("ffxi_settings", _))), "{bad}: {e:?}");
        }
        let twice = line(&format!("{native},\"ffxi_settings\":{{}},\"ffxi_settings\":{{}}"));
        assert_eq!(parse_session(twice.as_bytes()).unwrap_err(), ProtoError::DuplicateField("ffxi_settings"));
    }

    #[test]
    fn viewer_check_results_follow_the_round1_contract() {
        let parse = |line: &str| match parse_service_event(line.as_bytes()) { Ok(Some(ServiceEvent::ViewerCheck(r))) => Ok(r), Ok(_) => Err(None), Err(e) => Err(Some(e)) };
        assert_eq!(parse(r#"{"type":"native_viewer_check_result","ok":true}"#), Ok(ViewerCheckResult::Current));
        assert_eq!(parse(r#"{"type":"native_viewer_check_result","ok":false,"reason":"outdated","error":"Update \"now\""}"#),
            Ok(ViewerCheckResult::Outdated("Update \"now\"".into())));
        assert_eq!(parse(r#"{"type":"native_viewer_check_result","ok":false,"reason":"failed","error":"x"}"#), Ok(ViewerCheckResult::Failed("x".into())));
        assert_eq!(parse(r#"{"type":"native_viewer_check_result","ok":false,"reason":"unreachable"}"#), Ok(ViewerCheckResult::Unreachable(String::new())));
        for bad in [r#"{"type":"native_viewer_check_result"}"#, r#"{"type":"native_viewer_check_result","ok":false}"#,
            r#"{"type":"native_viewer_check_result","ok":false,"reason":"later"}"#, r#"{"type":"native_viewer_check_result","ok":1}"#] {
            assert!(matches!(parse(bad), Err(Some(_))), "{bad}");
        }
        assert_eq!(viewer_check_line(), "{\"type\":\"native_viewer_check\"}\n");
        assert_eq!(main_menu_line(1, false), "{\"type\":\"native_main_menu\",\"code\":1,\"returned\":false}\n");
        let native = native_fields();
        assert!(!parse_session(line(&native).as_bytes()).unwrap().viewer_gaps, "absent = cold-path pacing");
        assert!(parse_session(line(&format!("{native},\"start_game\":false,\"viewer_gaps\":true")).as_bytes()).unwrap().viewer_gaps);
        assert!(parse_session(line(&format!("{native},\"viewer_gaps\":1")).as_bytes()).is_err());
        assert!(parse_session(line(&format!("{native},\"viewer_gaps\":true,\"viewer_gaps\":true")).as_bytes()).is_err());
        assert!(parse_session(line(&format!("{},\"viewer_gaps\":true", keys())).as_bytes()).is_err(), "requires native authentication");
    }

    #[test]
    fn event_lines_are_valid_json() {
        for l in [hello_line(12), started_line(12), exit_line(-14), error_line("bad \"thing\"\n\u{1}"), viewer_check_line(), main_menu_line(-2046, true)] {
            assert!(l.ends_with('\n'));
            json::parse(l.trim_end().as_bytes()).unwrap();
        }
        let hello = hello_line(77);
        let hello = json::parse(hello.trim_end().as_bytes()).unwrap();
        assert_eq!((hello.get("v"), hello.get("pid")), (Some(&json::Value::Number("1")), Some(&json::Value::Number("77"))));
        assert_eq!(hello.get("native_batch"), Some(&json::Value::Bool(true)));
        assert_eq!(hello.get("viewer_check"), Some(&json::Value::Bool(true)));
        assert_eq!(hello.get("startup_sign_out"), Some(&json::Value::Bool(true)));
    }

    // pol-spec 07d §1: the launcher's exact park line (pol_less::park::park_line).
    const PARK_LINE: &str = r#"{"type":"park","v":1,"region":"US","lang":"US","pol_install_dir":"C:\\Games\\PlayOnlineViewer","install_dir":"C:\\Games\\SquareEnix\\FINAL FANTASY XI","auto_login":{"enabled":true,"character_slot":3},"ffxi_settings":{"0001":1920,"padmode000":"0,0,0,0,1,0"}}"#;

    fn credentials_line(extra: &str) -> String {
        format!("{{\"type\":\"credentials\",\"start_game\":false,\"keep_session\":false,\"viewer_gaps\":false,\"auto_away_minutes\":20,{}{extra}}}\n", native_fields())
    }

    #[test]
    fn round2_park_and_credentials_lines_rebuild_the_direct_session() {
        let FirstLine::Park(park) = parse_first_line(PARK_LINE.as_bytes()).unwrap() else { panic!("not a park") };
        assert_eq!(park.region, Region::Us);
        assert_eq!(park.auto_login, AutoLogin { enabled: true, character_slot: 3 });
        assert_eq!(park.ffxi_settings.as_ref().map(|s| s.len()), Some(2));
        let session = park.into_session(parse_credentials(credentials_line("").as_bytes()).unwrap());
        let direct = parse_session(line(&format!("\"pol_install_dir\":\"C:\\\\Games\\\\PlayOnlineViewer\",\"auto_login\":{{\"enabled\":true,\"character_slot\":3}},\"ffxi_settings\":{{\"0001\":1920,\"padmode000\":\"0,0,0,0,1,0\"}},\"start_game\":false,\"auto_away_minutes\":20,{}", native_fields())).as_bytes()).unwrap();
        assert_eq!((session.region, &session.install_dir, &session.pol_install_dir), (direct.region, &direct.install_dir, &direct.pol_install_dir));
        assert_eq!((session.start_game, session.keep_session, session.viewer_gaps, session.auto_away_minutes, session.auto_login),
            (direct.start_game, direct.keep_session, direct.viewer_gaps, direct.auto_away_minutes, direct.auto_login));
        assert_eq!(session.ffxi_settings.as_ref().map(|s| s.len()), direct.ffxi_settings.as_ref().map(|s| s.len()));
        let (a, b) = (&session.native_auth, &direct.native_auth);
        assert_eq!((a.pol_id, a.otp, &a.profile_key), (b.pol_id, b.otp, &b.profile_key));
        // The direct session line still parses as before.
        assert!(matches!(parse_first_line(line(&native_fields()).as_bytes()), Ok(FirstLine::Session(_))));
    }

    #[test]
    fn round2_a_park_never_carries_secrets_or_play_flags_and_credentials_never_carry_host_fields() {
        let park = PARK_LINE.strip_suffix('}').unwrap();
        for extra in [format!(",{}", native_fields()), format!(",{}", keys()), ",\"account_id\":\"0123456789abcdef\"".into(),
            ",\"start_game\":false".into(), ",\"viewer_gaps\":true".into(), ",\"auto_away_minutes\":10".into()] {
            let bad = format!("{park}{extra}}}");
            let error = parse_first_line(bad.as_bytes()).unwrap_err().to_string();
            assert!(!error.contains("fake password"), "{error}");
        }
        assert!(parse_first_line(PARK_LINE.replace("\"US\",\"lang\"", "\"EU\",\"lang\"").as_bytes()).is_err(), "EU needs its ucs_area");
        let eu = PARK_LINE.replace("\"US\",\"lang\"", "\"EU\",\"ucs_area\":\"02\",\"lang\"");
        let Ok(FirstLine::Park(park)) = parse_first_line(eu.as_bytes()) else { panic!("an EU park parses") };
        assert_eq!((park.region, park.ucs_area.as_deref()), (Region::Eu, Some("02")));
        let session = park.into_session(parse_credentials(credentials_line("").as_bytes()).unwrap());
        assert_eq!((session.region, session.ucs_area.as_deref()), (Region::Eu, Some("02")));
        assert!(parse_first_line(PARK_LINE.replace("\"lang\":\"US\"", "\"lang\":\"US\",\"ucs_area\":\"01\"").as_bytes()).is_err(), "never for US");
        assert!(parse_credentials(credentials_line(",\"ucs_area\":\"02\"").as_bytes()).is_err(), "a host field");
        assert!(parse_first_line(PARK_LINE.replace("\"v\":1", "\"v\":2").as_bytes()).is_err());
        assert!(parse_first_line(PARK_LINE.replace("\"lang\":\"US\"", "\"lang\":\"US\",\"lang\":\"US\"").as_bytes()).is_err());
        for extra in [",\"region\":\"US\"", ",\"install_dir\":\"C:\\\\x\"", ",\"ffxi_settings\":{}", ",\"auto_login\":{}", ",\"pol_records\":null"] {
            assert!(parse_credentials(credentials_line(extra).as_bytes()).is_err(), "{extra}");
        }
        assert!(parse_credentials(credentials_line("").replace("credentials", "session").as_bytes()).is_err());
        assert!(parse_credentials(b"{\"type\":\"credentials\"}").is_err(), "native_auth is required");
        let defaults = parse_credentials(format!("{{\"type\":\"credentials\",{}}}", native_fields()).as_bytes()).unwrap();
        assert!(defaults.start_game && !defaults.keep_session && !defaults.viewer_gaps && defaults.auto_away_minutes == 10);
        assert!(!format!("{defaults:?}").contains("fake password"));
    }

    #[test]
    fn round2_service_lines_and_hello_follow_the_contract() {
        assert!(matches!(parse_service_event(br#"{"type":"native_play_pending"}"#), Ok(Some(ServiceEvent::NativePlayPending))));
        let parse = |line: &str| match parse_service_event(line.as_bytes()) { Ok(Some(ServiceEvent::ViewerCheck(r))) => Some(r), _ => None };
        assert_eq!(parse(r#"{"type":"native_viewer_check_result","ok":false,"reason":"network","error":"DNS: No such host is known. (11001)"}"#),
            Some(ViewerCheckResult::NetworkNotReady("DNS: No such host is known. (11001)".into())));
        let hello = hello_line(77);
        assert_eq!(json::parse(hello.trim_end().as_bytes()).unwrap().get("park"), Some(&json::Value::Bool(PARK)));
        assert_eq!(parked_line(77, 431), "{\"type\":\"parked\",\"pid\":77,\"polcore_ms\":431}\n");
    }
}
