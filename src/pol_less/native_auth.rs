//! Credentials travel once over the existing private, PID-verified pipe. The
//! game host's installed POL core performs and owns the actual authentication.
use super::{pipe, SignedIn};
use crate::i18n::{t, tf};
use crate::{
    backend::{self, Account},
    friends::{self, Snapshot},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc, Mutex,
    },
};
use zeroize::Zeroizing;

struct Input {
    pol_id: Zeroizing<String>,
    pol_password: Zeroizing<String>,
    se_id: Zeroizing<String>,
    se_secret: Zeroizing<[u8; 20]>,
    /// The code typed on the account's row, or empty.
    otp: Zeroizing<String>,
    /// With no code typed, the account's authenticator makes one when the credentials go out.
    authenticator: Option<crate::totp::Authenticator>,
    key: String,
    handle_names: Zeroizing<String>,
}
pub(super) struct NativeAuth {
    input: Mutex<Option<Input>>,
    directory: PathBuf,
    snapshot: Mutex<Snapshot>,
    control_errors: Mutex<BTreeMap<&'static str, String>>,
    cache_dirty: AtomicBool,
    status: AtomicI32,
    social_only: bool,
    /// A Play whose game start waits for the sign-in and the version check (see `SignedIn`).
    start_pending: AtomicBool,
    /// The session line's `viewer_gaps` (pol-spec 07c §4): true only for a sign-in-only
    /// session started as a pre-sign-in, never for a cold Play or a batch's sign-in.
    viewer_gaps: bool,
    keep_session: bool,
    auto_away_minutes: u8,
    auto_login: super::AutoLogin,
    game_running: Arc<AtomicBool>,
    /// `native_social_ready` came since the last hand-over to FFXI: a connection that
    /// recovers (`native_pol_status` back to 0) is at that main menu again (07d §5.5).
    social_seen: AtomicBool,
    /// The one `native_play_pending` went to the loader (07d §4.3).
    play_pending_sent: AtomicBool,
}
impl NativeAuth {
    pub fn new(
        account: &Account,
        otp: Zeroizing<String>,
        directory: PathBuf,
    ) -> Result<Self, String> {
        let pol_id = Zeroizing::new(account.pol_id.to_ascii_uppercase());
        if pol_id.len() != 8
            || !pol_id.as_bytes()[..4].iter().all(u8::is_ascii_uppercase)
            || !pol_id.as_bytes()[4..].iter().all(u8::is_ascii_digit)
        {
            return Err(t("Enter a valid eight-character PlayOnline ID.").into());
        }
        if account.pol_password.is_empty()
            || account.pol_password.len() > 15
            || !account
                .pol_password
                .bytes()
                .all(|b| (32..=126).contains(&b))
        {
            return Err(
                t("PlayOnline passwords must contain 1–15 printable ASCII characters.").into(),
            );
        }
        if account.se_id.is_empty()
            || account.se_id.len() > 16
            || !account
                .se_id
                .bytes()
                .all(|b| b.is_ascii() && !b.is_ascii_control())
        {
            return Err(t("Square Enix IDs must contain 1–16 ASCII characters.").into());
        }
        if account.se_password.is_empty()
            || account.se_password.len() > 512
            || account.se_password.contains('\0')
        {
            return Err(t("Enter a valid Square Enix password.").into());
        }
        if !otp.is_empty() && (otp.len() != 6 || !otp.bytes().all(|b| b.is_ascii_digit())) {
            return Err(t("Enter a six-digit one-time code, or leave it empty.").into());
        }
        // A typed code wins: it is the user's answer after an authenticator code was refused.
        let authenticator = if otp.is_empty() && !account.totp_key.is_empty() {
            Some(crate::totp::Authenticator::new(&account.totp_key, &directory)
                .ok_or(t("The authenticator key can only contain the letters A–Z and the digits 2–7."))?)
        } else {
            None
        };
        let utf16 = Zeroizing::new(account.se_password.encode_utf16().collect::<Vec<_>>());
        let se_secret =
            crystal_pol::credential::EditorSecret::from_utf16(&utf16).into_native_secret();
        Ok(Self {
            input: Mutex::new(Some(Input {
                pol_id,
                pol_password: Zeroizing::new(account.pol_password.clone()),
                se_id: Zeroizing::new(account.se_id.clone()),
                se_secret,
                otp,
                authenticator,
                key: backend::account_key(account),
                handle_names: Zeroizing::new(account.pol_handle_names.clone()),
            })),
            directory,
            snapshot: Mutex::new(Snapshot::default()),
            control_errors: Mutex::new(BTreeMap::new()),
            cache_dirty: AtomicBool::new(false),
            status: AtomicI32::new(0),
            social_only: false,
            start_pending: AtomicBool::new(false),
            keep_session: false, viewer_gaps: false,
            auto_away_minutes: super::default_auto_away(),
            auto_login: super::AutoLogin::default(),
            game_running: Arc::new(AtomicBool::new(true)),
            social_seen: AtomicBool::new(false),
            play_pending_sent: AtomicBool::new(false),
        })
    }
    pub fn set_social_only(&mut self, social_only: bool) {
        self.social_only = social_only;
        self.game_running.store(!social_only, Ordering::Release);
    }
    /// The session line says `start_game: false` and the worker starts FFXI after its own
    /// version check. `game_running` stays true meanwhile: the start is under way.
    pub fn set_deferred_start(&mut self, deferred: bool) {
        self.start_pending.store(deferred, Ordering::Release);
    }
    pub fn set_viewer_gaps(&mut self, gaps: bool) { self.viewer_gaps = gaps; }
    pub fn set_keep_session(&mut self, keep: bool) { self.keep_session = keep; }
    /// 0 = off, else one of the Viewer's 10..60-minute choices.
    pub fn set_auto_away(&mut self, minutes: u8) {
        self.auto_away_minutes = if super::AUTO_AWAY_CHOICES.contains(&minutes) { minutes } else { super::default_auto_away() };
    }
    pub fn set_auto_login(&mut self, choice: super::AutoLogin) {
        self.auto_login = choice.normalized();
    }
    fn publish(&self, snapshot: Snapshot) -> Result<usize, String> {
        let count = snapshot.entries.len();
        // Native events are authoritative even when an observer temporarily prevents cache
        // replacement. Keep one-shot idle/ready/results in memory so a later push can heal it.
        let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        *current = snapshot;
        self.cache_dirty.store(true, Ordering::Release);
        crate::session_worker::publish_friends(&self.directory, &current)?;
        self.cache_dirty.store(false, Ordering::Release);
        Ok(count)
    }
}
impl SignedIn for NativeAuth {
    fn social_only(&self) -> bool { self.social_only }
    fn start_pending(&self) -> bool { self.start_pending.load(Ordering::Acquire) }
    fn idle_first(&self) -> bool { self.social_only || self.start_pending() }
    fn deferred_start_done(&self, blocked: Option<&str>) {
        self.start_pending.store(false, Ordering::Release);
        if let Some(message) = blocked {
            // Same as the idle report the pending start had held back, plus the reason.
            self.game_running.store(false, Ordering::Release);
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            snapshot.game_running = Some(false);
            snapshot.session_error = Some(message.to_owned());
            self.control_errors.lock().unwrap_or_else(|e| e.into_inner()).insert("start_game", message.to_owned());
            snapshot.social.ready = false;
            let _ = self.publish(snapshot);
        }
    }
    fn game_running(&self) -> Option<Arc<AtomicBool>> { Some(self.game_running.clone()) }
    fn native_session_state(&self, game: bool) -> Result<(), String> {
        // Until a Play's start is decided, the idle report must not look like a closed game.
        if !game && self.start_pending() { return Ok(()); }
        if game { self.social_seen.store(false, Ordering::Release); }
        self.control_errors.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.game_running.store(game, Ordering::Release);
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.game_running = Some(game);
        snapshot.session_error = None;
        snapshot.social.ready = false;
        self.publish(snapshot).map(|_| ())
    }
    fn native_own_mode(&self, mode: i32) -> Result<(), String> {
        if self.status.load(Ordering::Acquire) < 0 { return Ok(()); }
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.own_mode = Some(mode);
        self.publish(snapshot).map(|_| ())
    }
    fn native_own_portrait(&self, portrait: u16) -> Result<(), String> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if snapshot.own_portrait == Some(portrait) { return Ok(()); }
        snapshot.own_portrait = Some(portrait);
        self.publish(snapshot).map(|_| ())
    }
    fn native_control_result(&self, action: &str, code: i32) -> Result<(), String> {
        let category = match action {
            "visible" | "away" | "invisible" => "presence",
            "groups" => "groups",
            "start_game" => "start_game",
            "sign_out" => "sign_out",
            "portrait" => "portrait",
            _ => return Err(t("Invalid native POL control action.").into()),
        };
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut errors = self.control_errors.lock().unwrap_or_else(|e| e.into_inner());
        if code < 0 && code != -261 {
            let args: [(&str, &dyn std::fmt::Display); 1] = [("code", &code)];
            let message = match category {
                // Put Handle List went through, but the record read back holds another portrait.
                "portrait" if code == crate::wire::PORTRAIT_NOT_SAVED => t("Portrait not saved").to_owned(),
                "start_game" => tf("PlayOnline could not start the game ({code}).", &args),
                "sign_out" => tf("PlayOnline could not sign out ({code}).", &args),
                "presence" => tf("PlayOnline could not change your online status ({code}).", &args),
                "portrait" => tf("PlayOnline could not change your portrait ({code}).", &args),
                _ => tf("PlayOnline could not change your group status ({code}).", &args),
            };
            errors.insert(category, message);
        } else {
            errors.remove(category);
        }
        // Account and group jobs complete independently. A successful sibling
        // must not hide a failure that still needs the user's attention.
        snapshot.session_error = (!errors.is_empty()).then(|| errors.values().cloned().collect::<Vec<_>>().join(" "));
        drop(errors);
        self.publish(snapshot).map(|_| ())
    }
    fn take_native_control(&self) -> Option<(crate::session_worker::NativeControl, bool)> {
        crate::session_worker::take_native_control(&self.directory)
    }
    fn take_startup_sign_out(&self) -> bool {
        self.social_only && !self.start_pending() && !self.play_pending_sent.load(Ordering::Acquire)
            && crate::session_worker::take_startup_sign_out(&self.directory)
    }
    fn take_game_shutdown(&self) -> Option<Vec<u8>> {
        crate::session_worker::game_shutdown::take(&self.directory)
    }
    fn game_shutdown_result(&self, id: u64, game: u64, ok: bool, message: &str) -> Result<(), String> {
        crate::session_worker::game_shutdown::publish(&self.directory, id, game, ok, message)
    }
    fn native_game_ended(&self, game: u64, returned: bool) -> Result<(), String> {
        if self.keep_session && returned && game != 0 {
            self.native_session_state(false)?;
            crate::session_worker::game_shutdown::ended(&self.directory, game)?;
        }
        Ok(())
    }
    fn take_controller_settings(&self) -> Option<Vec<u8>> {
        crate::session_worker::take_controller_settings(&self.directory)
    }
    fn controller_result(&self, request_id: u64, ok: bool, message: &str) -> Result<(), String> {
        crate::session_worker::publish_controller_result(&self.directory, request_id, ok, message)
    }
    fn take_native_batch(&self) -> Option<Vec<u8>> {
        crate::session_worker::take_native_batch(&self.directory)
    }
    fn native_command_listener(&self) -> Option<crate::session_worker::CommandWake> {
        crate::session_worker::listen_for_commands()
    }
    fn take_play_pending(&self) -> bool {
        // Consume the note either way; only a pre-sign-in has Viewer pauses to skip.
        let noted = crate::session_worker::take_play_pending(&self.directory);
        let first = noted && !self.play_pending_sent.swap(true, Ordering::AcqRel);
        first && self.viewer_gaps
    }
    fn take_native_activity(&self) -> bool {
        // Consume the note either way; only an idle Viewer session uses it.
        let noted = crate::session_worker::take_native_activity(&self.directory);
        noted && self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).game_running == Some(false)
    }
    fn take_native_social(&self) -> Option<Zeroizing<Vec<u8>>> {
        {
            let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            if snapshot.social.busy || !snapshot.social.ready || snapshot.game_running != Some(false) { return None; }
        }
        let (request, _lock) = crate::session_worker::take_social(&self.directory)?;
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let previous_social = snapshot.social.clone();
        snapshot.social.begin(request.id.clone(), &request.request);
        let line = request.line();
        if let Err(error) = &line {
            snapshot.social.busy = false;
            snapshot.social.error = Some(error.clone());
        }
        if let Err(error) = self.publish(snapshot) {
            // This is an outgoing intent, not a native completion: nothing was delivered.
            // Undo busy/pending state while retaining the actual session's readiness.
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            snapshot.social = previous_social;
            snapshot.social.error = Some(error);
            let _ = self.publish(snapshot);
            return None;
        }
        line.ok()
    }
    fn native_social_ready(&self) -> Result<(), String> {
        self.social_seen.store(true, Ordering::Release);
        if self.status.load(Ordering::Acquire) < 0 { return Ok(()); }
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.social = crate::social::State { ready: true, ..Default::default() };
        self.publish(snapshot).map(|_| ())
    }
    fn retry_native_publication(&self) -> Result<bool, String> {
        if !self.cache_dirty.load(Ordering::Acquire) { return Ok(false); }
        let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        // A newer native event may already have published while this caller waited.
        if !self.cache_dirty.load(Ordering::Acquire) { return Ok(false); }
        crate::session_worker::publish_friends(&self.directory, &snapshot)?;
        self.cache_dirty.store(false, Ordering::Release);
        Ok(true)
    }
    fn publication_pending(&self) -> bool {
        self.cache_dirty.load(Ordering::Acquire)
    }
    fn native_social_result(&self, id: &str, action: &str, code: i32, payload: Option<&[u8]>) -> Result<(), String> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // A word this launcher does not know answers no request of its own.
        let Some(action) = crate::social::Action::parse(action) else { return Ok(()) };
        if let Err(error) = snapshot.social.apply(id, action, code, payload) {
            snapshot.social.busy = false;
            snapshot.social.error = Some(error);
        }
        self.publish(snapshot).map(|_| ())
    }
    fn native_mail_changed(&self) -> Result<(), String> {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // A previous idle period must not mark a game-owned or ended session.
        if !snapshot.social.ready || snapshot.game_running != Some(false) { return Ok(()); }
        snapshot.social.inbox_changed = true;
        self.publish(snapshot).map(|_| ())
    }
    fn session_line(&self, install_dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
        let install = install_dir
            .to_str()
            .ok_or(t("The game folder's path is not valid Unicode."))?;
        // Retention uses this same native owner and its original session services.
        let pol = crate::discovery::find(crate::discovery::Product::PlayOnline, None);
        let auto_login = serde_json::to_string(&self.auto_login.normalized())
            .map_err(|_| t("Could not encode character login settings."))?;
        let pol = serde_json::to_string(&pol.as_ref().map(|p| p.to_string_lossy().into_owned()))
            .map_err(|_| t("Could not encode the PlayOnline folder."))?;
        let head=format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"start_game\":{},\"keep_session\":{},\"viewer_gaps\":{},\"auto_away_minutes\":{},\"auto_login\":{},\"pol_install_dir\":{},\"install_dir\":{},\"native_auth\":{{", !self.idle_first(), self.keep_session, self.viewer_gaps, self.auto_away_minutes, auto_login, pol, serde_json::to_string(install).map_err(|_| t("Could not encode the game folder."))?);
        self.line_with_credentials(head)
    }
    /// pol-spec 07d §1.2: what a parked host gets at Play instead of `session`: the per-Play
    /// flags and the credentials, nothing the `park` line already gave (region, folders,
    /// auto-login, FFXI settings). One use, like `session_line`.
    fn credentials_line(&self) -> Result<Zeroizing<Vec<u8>>, String> {
        let head = format!("{{\"type\":\"credentials\",\"start_game\":{},\"keep_session\":{},\"viewer_gaps\":{},\"auto_away_minutes\":{},\"native_auth\":{{",
            !self.idle_first(), self.keep_session, self.viewer_gaps, self.auto_away_minutes);
        self.line_with_credentials(head)
    }
    fn receive_native_friends(&self, blob: &[u8], locations: Option<&[u8]>) -> Result<usize, String> {
        let mut snapshot = friends::native_snapshot(blob)?;
        if let Some(locations) = locations { friends::apply_native_locations(&mut snapshot, locations)?; }
        let previous = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        // The core's cache is the only friend source (the Viewer has no
        // manual refresh); a connection error stays until the connection returns.
        snapshot.error.clone_from(&previous.error);
        snapshot.game_running = previous.game_running;
        snapshot.session_error.clone_from(&previous.session_error);
        snapshot.own_mode = previous.own_mode;
        snapshot.own_portrait = previous.own_portrait;
        snapshot.social.clone_from(&previous.social);
        drop(previous);
        // The native observer and COM owner publish on different threads. A
        // snapshot already queued at connection loss can still arrive here;
        // only a healthy status event may make its live presence valid again.
        if self.status.load(Ordering::Acquire) < 0 {
            for friend in &mut snapshot.entries {
                friend.online = None;
                friend.location = None;
            }
        }
        self.publish(snapshot)
    }
    fn native_status(&self, code: i32) {
        let previous = self.status.swap(code, Ordering::AcqRel);
        if code < 0 || previous < 0 {
            let mut snapshot = self
                .snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if code < 0 {
                snapshot.own_mode = None;
                for friend in &mut snapshot.entries {
                    friend.online = None;
                    friend.location = None;
                }
                snapshot.social.ready = false;
                snapshot.social.busy = false;
                snapshot.social.id = None;
                snapshot.social.action = None;
                snapshot.social.pending_header = None;
            } else if self.social_seen.load(Ordering::Acquire) && snapshot.game_running == Some(false) {
                // Recovered at the main menu it had reached: Play and messages work again
                // without waiting for a `native_social_ready` that already came (08 §4 item 1).
                snapshot.social.ready = true;
            }
            snapshot.error = (code < 0).then(|| t("The native PlayOnline connection ended.").into());
            let _ = self.publish(snapshot);
        }
    }
    fn sign_out(self) -> String {
        self.native_status(-1025);
        "The native PlayOnline game-host session ended.".into()
    }
}

impl NativeAuth {
    /// `head` (ending in `"native_auth":{`) plus the one-use credentials and `}}\n`.
    fn line_with_credentials(&self, head: String) -> Result<Zeroizing<Vec<u8>>, String> {
        let input = self
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or(t("Native POL credentials were already handed to the game host."))?;
        // An authenticator account's code is made here, as the credentials leave for the host:
        // the same six digits a typed code would be, in the same field.
        let generated = input.authenticator.as_ref().map(crate::totp::Authenticator::code);
        let otp = generated.as_ref().unwrap_or(&input.otp);
        // Preallocate all secret fields, preventing discarded allocation copies.
        let fields: [(&str, &[u8]); 6] = [
            ("pol_id", input.pol_id.as_bytes()),
            ("pol_password", input.pol_password.as_bytes()),
            ("se_id", input.se_id.as_bytes()),
            ("se_secret", &*input.se_secret),
            ("otp", otp.as_bytes()),
            ("handle_names", input.handle_names.as_bytes()),
        ];
        let capacity = head.len()
            + fields
                .iter()
                .map(|(k, v)| k.len() + 6 + v.len().div_ceil(3) * 4)
                .sum::<usize>()
            + 80;
        let mut line = Zeroizing::new(Vec::with_capacity(capacity));
        line.extend_from_slice(head.as_bytes());
        for (index, (name, value)) in fields.iter().enumerate() {
            if index != 0 {
                line.push(b',');
            }
            line.push(b'"');
            line.extend_from_slice(name.as_bytes());
            line.extend_from_slice(b"\":");
            if *name == "otp" && value.is_empty() {
                line.extend_from_slice(b"null");
            } else {
                line.push(b'"');
                pipe::base64_into(&mut line, value);
                line.push(b'"');
            }
        }
        line.extend_from_slice(b",\"profile_key\":\"");
        line.extend_from_slice(input.key.as_bytes());
        line.extend_from_slice(b"\"}}\n");
        debug_assert!(line.len() <= capacity);
        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_commands_use_the_live_game_channel_without_changing_social_state() {
        #[link(name = "kernel32")]
        unsafe extern "system" { fn GetTickCount64() -> u64; }
        let fixture = Fixture::new();
        fixture.auth.native_session_state(true).unwrap();
        let directory = &fixture.auth.directory;
        let before = std::fs::read(directory.join("friends.json")).unwrap();
        let cache: serde_json::Value = serde_json::from_slice(&before).unwrap();
        std::fs::write(directory.join("session.json"), serde_json::to_vec(&serde_json::json!({
            "worker": cache["worker"], "state": {"Progress": "Fixture game"},
        })).unwrap()).unwrap();
        fixture.auth.controller_result(0, true, "Ready.").unwrap();
        std::fs::write(directory.join("controller-settings.json"), serde_json::to_vec(&serde_json::json!({
            "worker": cache["worker"], "request_id": 17, "deadline_ms": unsafe { GetTickCount64() } + 1000,
            "settings": {"padmode000": "1,0,0,0,0,1", "padsin000": "0,1,2,3"},
        })).unwrap()).unwrap();
        let frame: serde_json::Value = serde_json::from_slice(&fixture.auth.take_controller_settings().unwrap()).unwrap();
        assert_eq!(frame["type"], "controller_settings");
        assert_eq!(frame["request_id"], 17);
        assert!(fixture.auth.take_controller_settings().is_none());
        fixture.auth.controller_result(17, true, "Applied.").unwrap();
        let result: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.join("controller-settings-result-17.json")).unwrap()).unwrap();
        assert_eq!(result["worker"], cache["worker"]);
        assert_eq!(std::fs::read(directory.join("friends.json")).unwrap(), before);
    }

    /// 07d §5.5 step 4 (08 §4 item 1): -1025 then 0 at the main menu brings `social.ready`
    /// back, so a Play there no longer waits 90 s and fails; never without a prior ready.
    #[test]
    fn a_recovered_connection_restores_main_menu_readiness_only_after_a_ready() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_status(-1025);
        fixture.auth.native_status(0);
        assert!(!fixture.cached().social.ready, "no native_social_ready yet: still signing in");
        fixture.auth.native_social_ready().unwrap();
        assert!(fixture.cached().social.ready);
        fixture.auth.native_status(-1025);
        let lost = fixture.cached();
        assert!(!lost.social.ready && lost.error.is_some());
        fixture.auth.native_status(0);
        let back = fixture.cached();
        assert!(back.social.ready && back.error.is_none() && back.game_running == Some(false));
        // Handed to FFXI: that readiness belonged to the earlier main menu.
        fixture.auth.native_session_state(true).unwrap();
        fixture.auth.native_status(-1025);
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_status(0);
        assert!(!fixture.cached().social.ready);
    }

    /// 07d §4.3: one `native_play_pending` per pre-sign-in; the note is consumed either way.
    #[test]
    fn a_play_pending_note_reaches_only_a_pre_sign_in_once() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.auth.directory).unwrap();
        let directory = fixture.auth.directory.clone();
        let note = || crate::session_worker::save_play_pending_for_test(&directory);
        note();
        assert!(!fixture.auth.take_play_pending(), "a cold Play has no Viewer pauses");
        assert!(!fixture.auth.take_play_pending(), "consumed");
        assert!(fixture.auth.play_pending_sent.load(Ordering::Acquire), "remember the Play even without Viewer pauses");
        std::fs::remove_dir_all(&fixture.auth.directory).unwrap();
        let mut fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.auth.directory).unwrap();
        fixture.auth.viewer_gaps = true;
        let directory = fixture.auth.directory.clone();
        let note = || crate::session_worker::save_play_pending_for_test(&directory);
        note();
        assert!(fixture.auth.take_play_pending());
        note();
        assert!(!fixture.auth.take_play_pending(), "once per session");
        let _ = std::fs::remove_dir_all(&fixture.auth.directory);
    }

    #[test]
    fn session_lifetime_survives_friend_updates_and_reports_control_failures() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_own_mode(4).unwrap();
        assert!(!fixture.auth.game_running.load(Ordering::Acquire));
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().game_running, Some(false));
        assert_eq!(fixture.cached().own_mode, Some(4));
        fixture.auth.native_control_result("start_game", -256).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().session_error.as_deref(), Some("PlayOnline could not start the game (-256)."));
        fixture.auth.native_session_state(true).unwrap();
        assert!(fixture.auth.game_running.load(Ordering::Acquire));
        assert!(fixture.cached().session_error.is_none());
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().game_running, Some(true));
        assert_eq!(fixture.cached().own_mode, Some(4));
    }

    #[test]
    fn presence_and_group_results_keep_independent_failures() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_control_result("visible", -2046).unwrap();
        fixture.auth.native_control_result("groups", -261).unwrap();
        assert_eq!(fixture.cached().session_error.as_deref(), Some("PlayOnline could not change your online status (-2046)."));
        fixture.auth.native_control_result("groups", -263).unwrap();
        fixture.auth.native_control_result("away", 1).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().session_error.as_deref(), Some("PlayOnline could not change your group status (-263)."));
        let event = pipe::parse_event(br#"{"type":"native_control_result","action":"groups","code":1}"#).unwrap();
        let mut log = super::super::SessionLog::default();
        assert!(matches!(super::super::handle_event(event, &fixture.auth, &mut log), super::super::Step::Continue));
        assert!(fixture.cached().session_error.is_none());
        fixture.auth.native_control_result("groups", -263).unwrap();
        fixture.auth.native_session_state(true).unwrap();
        fixture.auth.native_control_result("visible", 1).unwrap();
        assert!(fixture.cached().session_error.is_none());
    }

    /// A portrait change the record read back doesn't hold says so in a short note; the portrait
    /// shown is the one the loader read back, and the rest of the session is untouched.
    #[test]
    fn a_portrait_the_read_back_does_not_confirm_is_not_saved() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_own_mode(4).unwrap();
        fixture.auth.native_own_portrait(42).unwrap();
        let mut log = super::super::SessionLog::default();
        for line in [&br#"{"type":"native_own_portrait","portrait":42}"#[..], br#"{"type":"native_control_result","action":"portrait","code":-5121}"#] {
            let event = pipe::parse_event(line).unwrap();
            assert!(matches!(super::super::handle_event(event, &fixture.auth, &mut log), super::super::Step::Continue));
        }
        let cached = fixture.cached();
        assert_eq!(cached.session_error.as_deref(), Some("Portrait not saved"));
        assert_eq!((cached.own_portrait, cached.own_mode, cached.game_running), (Some(42), Some(4), Some(false)));
        // Another failure keeps its own wording; a later success clears the note.
        fixture.auth.native_control_result("portrait", -7).unwrap();
        assert_eq!(fixture.cached().session_error.as_deref(), Some("PlayOnline could not change your portrait (-7)."));
        fixture.auth.native_control_result("portrait", 1).unwrap();
        assert!(fixture.cached().session_error.is_none());
    }

    #[test]
    fn friend_pushes_preserve_social_results_and_new_viewer_sessions_clear_them() {
        let fixture = Fixture::new(); fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        assert!(fixture.cached().social.ready);
        let id = "0123456789abcdef0123456789abcdef";
        let mut snapshot = fixture.cached(); snapshot.social.begin(id.into(), &crate::social::Request::Inbox { folder:0, page:0 });
        fixture.auth.publish(snapshot).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap(); assert!(fixture.cached().social.busy);
        fixture.auth.native_social_result(id, "inbox", 1, Some(&[0; 12 + 32 * 72])).unwrap();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap(); assert!(!fixture.cached().social.busy);
        fixture.auth.native_session_state(true).unwrap(); assert!(!fixture.cached().social.ready);
        fixture.auth.native_session_state(false).unwrap(); fixture.auth.native_social_ready().unwrap();
        assert!(fixture.cached().social.id.is_none());
        fixture.auth.native_status(-1025); assert!(!fixture.cached().social.ready);
    }

    #[test]
    fn failed_cache_replacement_cannot_discard_native_idle_or_one_shot_readiness() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(true).unwrap();
        let observer = fixture.deny_cache_replacement();
        assert!(fixture.auth.native_session_state(false).is_err());
        assert!(fixture.auth.native_social_ready().is_err());
        let native = fixture.auth.snapshot.lock().unwrap().clone();
        assert_eq!(native.game_running, Some(false));
        assert!(native.social.ready && !native.social.busy);
        // The observer still sees its old file; it must not become our source of truth.
        assert_eq!(fixture.cached().game_running, Some(true));
        assert!(!fixture.cached().social.ready);
        drop(observer);
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        let healed = fixture.cached();
        assert_eq!(healed.game_running, Some(false));
        assert!(healed.social.ready && !healed.social.busy);
        assert_eq!(healed.entries.len(), 1);
    }

    #[test]
    fn failed_readiness_publication_recovers_without_another_native_event() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        let observer = fixture.deny_cache_replacement();
        // Keep the real Windows reader open through the entire bounded replacement retry.
        assert!(fixture.auth.native_social_ready().is_err());
        assert!(!fixture.cached().social.ready);
        assert!(fixture.auth.retry_native_publication().is_err());
        drop(observer);
        assert!(fixture.auth.retry_native_publication().unwrap());
        let recovered = fixture.cached();
        assert_eq!(recovered.game_running, Some(false));
        assert!(recovered.social.ready && !recovered.social.busy);
        let _observer = fixture.deny_cache_replacement();
        assert!(!fixture.auth.retry_native_publication().unwrap(), "a successful retry clears pending work and performs no second write");
    }

    #[test]
    fn failed_outgoing_social_publication_preserves_ready_without_sticking_busy() {
        let fixture = Fixture::new();
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        let cache: serde_json::Value = serde_json::from_slice(
            &std::fs::read(fixture.auth.directory.join("friends.json")).unwrap(),
        ).unwrap();
        let request_path = fixture.auth.directory.join("native-social.json");
        let submit = |id: &str| {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "worker": cache["worker"], "id": id, "request": crate::social::Request::Check {},
            })).unwrap();
            std::fs::write(&request_path, bytes).unwrap();
        };
        submit("0123456789abcdef0123456789abcdef");
        let observer = fixture.deny_cache_replacement();
        assert!(fixture.auth.take_native_social().is_none(), "failed publication must not send the request");
        assert!(!request_path.exists());
        let native = fixture.auth.snapshot.lock().unwrap().clone();
        assert!(native.social.ready && !native.social.busy);
        assert!(native.social.id.is_none() && native.social.error.is_some());
        drop(observer);
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert!(fixture.cached().social.ready && !fixture.cached().social.busy);
        assert!(fixture.cached().social.error.is_some());
        submit("1123456789abcdef0123456789abcdef");
        let line = fixture.auth.take_native_social().expect("the next request remains usable");
        let sent: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(sent["type"], "native_social");
        assert_eq!(sent["id"], "1123456789abcdef0123456789abcdef");
        assert!(fixture.cached().social.busy);
    }

    #[test]
    fn incoming_mail_marks_only_the_live_idle_session_and_survives_friend_pushes() {
        let fixture = Fixture::new();
        fixture.auth.native_mail_changed().unwrap();
        assert!(!fixture.auth.snapshot.lock().unwrap().social.inbox_changed);
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        fixture.auth.native_mail_changed().unwrap();
        assert!(fixture.cached().social.inbox_changed);
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert!(fixture.cached().social.inbox_changed);
        let id = "0123456789abcdef0123456789abcdef";
        let mut snapshot = fixture.cached();
        snapshot.social.begin(id.into(), &crate::social::Request::Inbox { folder:0, page:0 });
        fixture.auth.publish(snapshot).unwrap();
        fixture.auth.native_social_result(id, "inbox", -515, None).unwrap();
        assert!(fixture.cached().social.inbox_changed);
        fixture.auth.native_social_result(id, "inbox", 1, Some(&[0; 12 + 32 * 72])).unwrap();
        assert!(!fixture.cached().social.inbox_changed);
        fixture.auth.native_session_state(true).unwrap();
        fixture.auth.native_mail_changed().unwrap();
        assert!(!fixture.cached().social.inbox_changed);
        fixture.auth.native_session_state(false).unwrap();
        fixture.auth.native_social_ready().unwrap();
        fixture.auth.native_status(-1025);
        fixture.auth.native_mail_changed().unwrap();
        assert!(!fixture.cached().social.inbox_changed);
    }

    struct Fixture { auth: NativeAuth }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "crystal-native-refresh-{}", backend::unique_id().unwrap()
            ));
            Self { auth: NativeAuth {
                input: Mutex::new(None), directory,
                snapshot: Mutex::new(Snapshot::default()),
                control_errors: Mutex::new(BTreeMap::new()),
                cache_dirty: AtomicBool::new(false),
                status: AtomicI32::new(0), social_only: false, start_pending: AtomicBool::new(false), keep_session: false, viewer_gaps: false, auto_away_minutes: 10, auto_login: super::super::AutoLogin::default(), game_running: Arc::new(AtomicBool::new(true)),
                social_seen: AtomicBool::new(false), play_pending_sent: AtomicBool::new(false),
            } }
        }
        fn cached(&self) -> Snapshot {
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(self.auth.directory.join("friends.json")).unwrap()
            ).unwrap();
            serde_json::from_value(value["snapshot"].clone()).unwrap()
        }
        fn deny_cache_replacement(&self) -> std::fs::File {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(self.auth.directory.join("friends.json")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // This fixture owns only its unique absolute temporary directory.
            let _ = std::fs::remove_dir_all(&self.auth.directory);
        }
    }
    fn friend_blob() -> Vec<u8> {
        use crystal_pol::friends::{BLOB_LEN, MAGIC};
        let mut blob = vec![0; BLOB_LEN];
        blob[..8].copy_from_slice(MAGIC);
        blob[8 + 0x98] = 1;
        blob[8 + 0xa0..8 + 0xa5].copy_from_slice(b"Alice");
        blob[8 + 0x10..8 + 0x18].copy_from_slice(&123u64.to_le_bytes());
        blob[8 + 8..8 + 16].copy_from_slice(&((1u64 << 11) | (1u64 << 33)).to_le_bytes());
        blob
    }

    #[test]
    fn disconnection_clears_presence_without_losing_the_list() {
        let fixture = Fixture::new();
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        fixture.auth.native_status(-1025);
        let disconnected = fixture.cached();
        assert_eq!(disconnected.entries[0].name, "Alice");
        assert_eq!(disconnected.entries[0].online, None);
        assert_eq!(disconnected.entries[0].location, None);
        assert!(disconnected.error.is_some());
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        fixture.auth.native_own_mode(4).unwrap();
        fixture.auth.native_social_ready().unwrap();
        let queued_after_loss = fixture.cached();
        assert_eq!(queued_after_loss.entries[0].online, None);
        assert_eq!(queued_after_loss.entries[0].location, None);
        assert_eq!(queued_after_loss.own_mode, None);
        assert!(!queued_after_loss.social.ready);
        assert!(queued_after_loss.error.is_some());
        fixture.auth.native_status(0);
        assert!(fixture.cached().error.is_none());
        assert_eq!(fixture.cached().entries[0].online, None);
        // Recovery needs a new event even when the native bytes are unchanged.
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().entries[0].online, Some(true));
    }

    #[test]
    fn game_locations_publish_with_presence_and_clear_when_no_longer_available() {
        let fixture = Fixture::new();
        let mut locations = vec![0; 8 + 300 * 96];
        locations[..8].copy_from_slice(b"XIPOLG\x01\0");
        locations[8..13].copy_from_slice(b"Alice");
        locations[24..38].copy_from_slice(b"Bastok Markets");
        fixture.auth.receive_native_friends(&friend_blob(), Some(&locations)).unwrap();
        assert_eq!(fixture.cached().entries[0].location.as_deref(), Some("Alice · Bastok Markets"));
        fixture.auth.receive_native_friends(&friend_blob(), None).unwrap();
        assert_eq!(fixture.cached().entries[0].location.as_deref(), Some("FINAL FANTASY XI"));
        fixture.auth.receive_native_friends(&friend_blob(), Some(&locations)).unwrap();
        fixture.auth.native_status(-1025);
        assert_eq!(fixture.cached().entries[0].location, None);
    }

    #[test]
    fn credentials_are_one_use_and_never_become_session_keys() {
        let account = Account {
            label: "test".into(),
            mode: backend::AccountMode::Retail,
            server: String::new(),
            pol_id: "abcd1234".into(),
            pol_password: "fake pol pass".into(),
            se_id: "fakeuser".into(),
            se_password: "fake SE pass".into(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
            pol_handle_names: "PublicOne, PublicTwo".into(),
            totp_key: String::new(),
        };
        let mut auth =
            NativeAuth::new(&account, Zeroizing::new("123456".into()), PathBuf::new()).unwrap();
        assert_eq!(auth.auto_away_minutes, 10, "the Viewer's default");
        auth.set_auto_away(25);
        assert_eq!(auth.auto_away_minutes, 10, "only the Viewer's choices");
        auth.set_auto_away(0);
        auth.set_social_only(true);
        auth.set_auto_login(super::super::AutoLogin { enabled: true, character_slot: 3 });
        let line = auth.session_line(Path::new(r"C:\Games\FFXI")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert!(value.get("auth_code").is_none());
        assert!(value.get("v_key").is_none());
        assert_eq!(value["start_game"], false);
        assert_eq!(value["keep_session"], false);
        assert_eq!(value["auto_away_minutes"], 0);
        assert_eq!(value["auto_login"]["enabled"], true);
        assert_eq!(value["auto_login"]["character_slot"], 3);
        let credentials = &value["native_auth"];
        assert_eq!(pipe::base64_decode(credentials["handle_names"].as_str().unwrap()).unwrap(), b"PublicOne, PublicTwo");
        assert_eq!(
            pipe::base64_decode(credentials["pol_id"].as_str().unwrap()).unwrap(),
            b"ABCD1234"
        );
        assert_eq!(
            pipe::base64_decode(credentials["se_secret"].as_str().unwrap())
                .unwrap()
                .len(),
            20
        );
        assert_eq!(
            pipe::base64_decode(credentials["otp"].as_str().unwrap()).unwrap(),
            b"123456"
        );
        assert!(!String::from_utf8_lossy(&line).contains("fake SE pass"));
        assert!(auth.session_line(Path::new(r"C:\Games\FFXI")).is_err());
    }

    /// An account with an authenticator key and no typed code sends a code made from the key as
    /// the credentials leave (recording its time step, never the code); a typed code wins, and
    /// nothing else in the line changes.
    #[test]
    fn an_authenticator_key_fills_in_the_code_when_the_credentials_go_out() {
        let directory = std::env::temp_dir().join(format!("crystal-authenticator-{}", backend::unique_id().unwrap()));
        let key = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        let mut account = play_account();
        account.totp_key = key.into();
        let sent = |otp: &str| {
            let auth = NativeAuth::new(&account, Zeroizing::new(otp.into()), directory.clone()).unwrap();
            assert!(crate::totp::recorded_step(&directory).is_none() || !otp.is_empty(), "no code is made before the line");
            let line = auth.credentials_line().unwrap();
            let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
            assert!(!String::from_utf8_lossy(&line).contains(key), "the key never leaves");
            String::from_utf8(pipe::base64_decode(value["native_auth"]["otp"].as_str().unwrap()).unwrap()).unwrap()
        };
        let code = sent("");
        let step = crate::totp::recorded_step(&directory).expect("the step is recorded");
        assert_eq!(code, crate::totp::code_at(key, step).as_str());
        assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()));
        let ledger = std::fs::read_to_string(directory.join("authenticator-step")).unwrap();
        assert!(!ledger.contains(&code), "the record holds the step, not the code");
        assert_eq!(sent("654321"), "654321", "a typed code wins");
        assert_eq!(crate::totp::recorded_step(&directory), Some(step), "and makes no code");
        // An invalid saved key never reaches the host.
        account.totp_key = "not a key".into();
        assert!(NativeAuth::new(&account, Zeroizing::new(String::new()), directory.clone()).is_err());
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn play_account() -> Account {
        Account {
            label: "test".into(),
            mode: backend::AccountMode::Retail,
            server: String::new(),
            pol_id: "abcd1234".into(),
            pol_password: "fake pol pass".into(),
            se_id: "fakeuser".into(),
            se_password: "fake SE pass".into(),
            region: 0,
            launch_with: None,
            ashita_boot_ini: None,
            windower_profile: None,
            pol_handle_names: String::new(),
            totp_key: String::new(),
        }
    }

    #[test]
    fn a_play_signs_in_idle_and_holds_the_idle_report_until_its_start_is_decided() {
        let directory = std::env::temp_dir().join(format!("crystal-deferred-{}", backend::unique_id().unwrap()));
        let start_game = |deferred: bool| {
            let mut auth = NativeAuth::new(&play_account(), Zeroizing::new(String::new()), directory.clone()).unwrap();
            auth.set_deferred_start(deferred);
            let line = auth.session_line(Path::new(r"C:\Games\FFXI")).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
            assert_eq!(value["keep_session"], false, "retention is off by default");
            value["start_game"].clone()
        };
        assert_eq!(start_game(false), true, "a plain launch still tells the loader to start the game");
        assert_eq!(start_game(true), false, "a Play signs in idle; the worker starts the game after its check");

        let mut auth = NativeAuth::new(&play_account(), Zeroizing::new(String::new()), directory.clone()).unwrap();
        auth.set_deferred_start(true);
        assert!(auth.idle_first() && auth.start_pending() && !auth.social_only());
        // The loader's idle report does not read as a closed game while the start is pending.
        auth.native_session_state(false).unwrap();
        assert!(auth.game_running.load(Ordering::Acquire));
        // The check refused the start: the session is idle, with the reason as its error.
        auth.deferred_start_done(Some("Update required: installed 1, official server 2."));
        assert!(!auth.game_running.load(Ordering::Acquire) && !auth.start_pending());
        let snapshot = auth.snapshot.lock().unwrap().clone();
        assert_eq!(snapshot.game_running, Some(false));
        assert!(snapshot.session_error.as_deref().unwrap().starts_with("Update required"));
        // Afterwards the game's own return to the Viewer is an ordinary idle report.
        auth.native_session_state(true).unwrap();
        auth.native_session_state(false).unwrap();
        assert!(!auth.game_running.load(Ordering::Acquire));
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// pol-spec 07c §4: the loader restores the Viewer's pauses only for a pre-sign-in;
    /// never for a cold Play, nor a batch's own sign-in (social-only, gaps off).
    /// pol-spec 07d §1.2: a parked host's `credentials` line has the session line's credentials
    /// and per-Play flags, none of the host fields the `park` line carried, and is one-use.
    #[test]
    fn the_credentials_line_carries_only_credentials_and_play_flags_once() {
        let directory = std::env::temp_dir().join(format!("crystal-credentials-{}", backend::unique_id().unwrap()));
        let make = || {
            let mut auth = NativeAuth::new(&play_account(), Zeroizing::new("123456".into()), directory.clone()).unwrap();
            auth.set_deferred_start(true);
            auth.set_auto_away(20);
            auth
        };
        let session: serde_json::Value = serde_json::from_slice(&make().session_line(Path::new(r"C:\Games\FFXI")).unwrap()).unwrap();
        let auth = make();
        let line = auth.credentials_line().unwrap();
        assert!(line.ends_with(b"}}\n"));
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["type"], "credentials");
        assert_eq!(value["native_auth"], session["native_auth"]);
        for flag in ["start_game", "keep_session", "viewer_gaps", "auto_away_minutes"] {
            assert_eq!(value[flag], session[flag], "{flag}");
        }
        for host in ["v", "region", "lang", "install_dir", "pol_install_dir", "auto_login", "ffxi_settings"] {
            assert!(value.get(host).is_none(), "{host} belongs in the park line");
        }
        assert!(auth.credentials_line().is_err() && auth.session_line(Path::new(r"C:\Games\FFXI")).is_err(), "one use");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn normal_and_parked_sessions_forward_the_retention_choice_without_retaining_credentials() {
        let directory = std::env::temp_dir().join(format!("crystal-retention-{}", backend::unique_id().unwrap()));
        for keep in [false, true] {
            for parked in [false, true] {
                let mut auth = NativeAuth::new(&play_account(), Zeroizing::new(String::new()), directory.clone()).unwrap();
                auth.set_keep_session(keep);
                let line = if parked { auth.credentials_line() } else { auth.session_line(Path::new(r"C:\Games\FFXI")) }.unwrap();
                let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
                assert_eq!(value["keep_session"], keep);
                assert!(auth.credentials_line().is_err() && auth.session_line(Path::new(r"C:\Games\FFXI")).is_err());
            }
        }
    }

    #[test]
    fn only_a_pre_sign_in_asks_for_the_viewer_pauses() {
        let directory = std::env::temp_dir().join(format!("crystal-gaps-{}", backend::unique_id().unwrap()));
        for (social_only, gaps, expected) in [(true, true, true), (true, false, false), (false, true, false), (false, false, false)] {
            let plan = super::super::Plan { tool: super::super::Tool::Windower, settings: Default::default(), social_only,
                auto_away_minutes: 10, auto_login: Default::default(), keep_session: false, viewer_gaps: gaps, trigger: Default::default() };
            let mut auth = NativeAuth::new(&play_account(), Zeroizing::new(String::new()), directory.clone()).unwrap();
            super::super::configure(&mut auth, &plan);
            let value: serde_json::Value = serde_json::from_slice(&auth.session_line(Path::new(r"C:\Games\FFXI")).unwrap()).unwrap();
            assert_eq!(value["viewer_gaps"], expected, "social_only {social_only}, gaps {gaps}");
            assert_eq!(value["start_game"], false, "every launch signs in idle");
        }
        // Older window processes send no field: a cold Play.
        let plan: super::super::Plan = serde_json::from_value(serde_json::json!({"tool": super::super::Tool::Windower, "settings": super::super::ToolSettings::default(), "social_only": true})).unwrap();
        assert!(!plan.viewer_gaps);
        let _ = std::fs::remove_dir_all(&directory);
    }}
