//! Retail login and services owned by the installed, verified POL core, driven
//! the way the stock PlayOnline Viewer (app.dll on pol.exe's main thread)
//! drives it, so the servers see the Viewer's calls in the Viewer's order.
//! No cipher, endpoint, successful-result or credential-getter replacements.
//! Evidence: pol-spec/01-bootstrap-signin.md, 02-member-settings.md and
//! 03-lifecycle.md (app.dll/polcore addresses are cited inline).

use crate::{
    b64, events, log,
    native_jobs::{self, Policy, Request, FRAME},
    native_profile,
    proto::{self, NativeCredentials},
};
use std::{
    ffi::{c_void, CStr},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering},
        mpsc, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

// Commands are accepted only while the COM owner is idle. A click during
// GameStart must never become a queued second game or sign out beneath FFXI.
const GAME: u8 = 0;
const SOCIAL: u8 = 1;
const START_GAME: u8 = 2;
const SIGN_OUT: u8 = 3;
const VISIBLE: u8 = 4;
const AWAY: u8 = 5;
const INVISIBLE: u8 = 6;
const SOCIAL_REQUEST: u8 = 7;
/// Change the selected handle's portrait to [`PORTRAIT_REQUEST`].
const PORTRAIT: u8 = 8;
/// Held by one request from its claim until the owner takes it (or the request is refused):
/// a second request can never overwrite a value a running PORTRAIT command has yet to read.
static PORTRAIT_REQUEST: AtomicU16 = AtomicU16::new(NO_PORTRAIT);
/// No portrait is waiting (above `wire::PORTRAIT_MASK`, so `set_own` refuses it).
const NO_PORTRAIT: u16 = u16::MAX;
/// Status command flag: the dialog's "apply to groups" (app 10031b2b).
const GROUPS: u8 = 0x10;
static CONTROL: AtomicU8 = AtomicU8::new(GAME);
/// A Play that arrived while a user command (social request, status change)
/// ran: answered 0 and taken as START_GAME when that command returns, the way
/// the Viewer's Play follows a dialog the user already applied (07c §6 S1 b).
static PLAY_LATCH: AtomicBool = AtomicBool::new(false);

const SIGNING_IN: u8 = 0;
const CANCELLING_SIGN_IN: u8 = 1;
const STARTUP_FINISHED: u8 = 2;
#[cfg(not(test))]
static STARTUP_STATE: AtomicU8 = AtomicU8::new(SIGNING_IN);
#[cfg(test)]
thread_local! { static STARTUP_STATE: AtomicU8 = const { AtomicU8::new(STARTUP_FINISHED) }; }

fn with_startup_state<T>(f: impl FnOnce(&AtomicU8) -> T) -> T {
    #[cfg(not(test))]
    { f(&STARTUP_STATE) }
    #[cfg(test)]
    { STARTUP_STATE.with(f) }
}

fn startup_cancelled() -> bool {
    with_startup_state(|state| state.load(Ordering::Acquire) == CANCELLING_SIGN_IN)
}

/// Enter idle before ending startup, so a sign-out racing this transition is
/// either a startup cancellation or an ordinary idle command.
fn finish_startup() -> bool {
    with_startup_state(|state| state.swap(STARTUP_FINISHED, Ordering::AcqRel) != CANCELLING_SIGN_IN)
}

/// The launcher-protocol refusal: the command was not accepted now.
pub(crate) const BUSY: i32 = -256;
/// `native_pol_status`: the member session is gone (or not connected).
const SESSION_LOST: i32 = -1025;

fn control_result(action: &str, code: i32) {
    events::send(format!("{{\"type\":\"native_control_result\",\"action\":\"{action}\",\"code\":{code}}}\n"));
}
fn pol_status(code: i32) {
    events::send(format!("{{\"type\":\"native_pol_status\",\"code\":{code}}}\n"));
}

fn cancel_startup(state: &AtomicU8, pending_play: bool) -> Option<i32> {
    if state.load(Ordering::Acquire) == STARTUP_FINISHED { return None; }
    if pending_play { return Some(BUSY); }
    match state.compare_exchange(SIGNING_IN, CANCELLING_SIGN_IN, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) | Err(CANCELLING_SIGN_IN) => Some(0),
        Err(_) => None,
    }
}

/// Play was requested (START_GAME accepted or latched). From then on no new
/// attempt starts (no auto-Away, no status/mail retry, no new mail transfer)
/// and in-flight tickets finish without a cancel (07a presign-no-cancel-freeze 1).
#[cfg(not(test))]
pub(crate) fn play_requested() -> bool { play_requested_in(&CONTROL, &PLAY_LATCH) }
/// START_GAME is set alone (never with GROUPS); a latched Play waits behind a
/// running user command and counts already.
fn play_requested_in(control: &AtomicU8, latch: &AtomicBool) -> bool {
    control.load(Ordering::Acquire) == START_GAME || latch.load(Ordering::Acquire)
}
// Tests drive the owner on their own thread and leave the globals alone,
// unless a test serialised on CONTROL opts in to the shipped predicate.
#[cfg(test)]
thread_local! {
    static TEST_PLAY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REAL_CONTROL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
pub(crate) fn play_requested() -> bool {
    TEST_PLAY.with(|p| p.get()) || REAL_CONTROL.with(|r| r.get()) && play_requested_in(&CONTROL, &PLAY_LATCH)
}
#[cfg(test)]
pub(crate) struct TestPlay;
#[cfg(test)]
impl TestPlay {
    pub(crate) fn requested() -> Self { TEST_PLAY.with(|p| p.set(true)); Self }
    /// This thread reads the real CONTROL/PLAY_LATCH (hold CONTROL_TEST).
    pub(crate) fn real_control() -> Self { REAL_CONTROL.with(|r| r.set(true)); Self }
}
#[cfg(test)]
impl Drop for TestPlay {
    fn drop(&mut self) { TEST_PLAY.with(|p| p.set(false)); REAL_CONTROL.with(|r| r.set(false)); }
}

/// FINAL FANTASY XI's content id (InstallFolder\0001), the app id the Viewer
/// stores with 1175 before handing off (app 101a0611, [esi+0x27c]).
const FFXI_APP: usize = 1;
/// The Viewer application id it publishes on return (app 101cf603).
const VIEWER_APP: i32 = 1000;
/// app 1001b13f: the auto-away check runs when the gated counter exceeds 3600
/// stock frames: 112.5 s at the 1.18.15e Viewer's cold 31.25 ms frame (5625
/// frames of 20 ms), about 61 s at the 1.18.15f Viewer's ~17 ms frame (3060);
/// the installed app.dll decides (pol-spec 08 §4 item 7, 07d §6 S1 D).
fn auto_away_frames() -> u32 { native_jobs::stock_frames(3600) }
/// 101983fb polls 817 at least 100 ms apart (696 clock) and never pumps.
const CLOSE_POLL: Duration = Duration::from_millis(100);
/// The Viewer polls without a limit; Crystal logs and stops after this.
const CLOSE_LIMIT: Duration = Duration::from_secs(10);
/// Sign-in has no Viewer timeout (the user cancels). Crystal's cancel uses the
/// Viewer's own abort (807) after this, or when the launcher goes away.
const SIGN_IN_LIMIT: Duration = Duration::from_secs(120);
const ABORT_LIMIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const SLOTS_FULL: i32 = -256;
/// The four sign-in/return lists in the Viewer's start order (app 10040650,
/// 10040822): friends, handles, characters, groups.
const LISTS: [(&str, usize, usize); 4] =
    [("friends", 163, 164), ("handles", 171, 172), ("characters", 175, 176), ("groups", 383, 384)];

/// Longest wait for the launcher's W2U answer. It must cover the launcher's
/// worst case, patch.rs VIEWER_CHECK_BUDGET = 32.115 s: the network-not-ready
/// retry (NETWORK_WAIT 12 s), its fresh lookup (≤ CHECK_TIMEOUT 10 s), then one
/// full CHECK_TIMEOUT exchange from the answer and the stock close (≤ 0.115 s);
/// pol-spec 07d §3. On expiry the sign-in fails before Open(1); no :51240 ever
/// goes out without a W2U.
pub(crate) const VIEWER_CHECK_LIMIT: Duration = Duration::from_secs(35);
/// Stock :54000 close → :51240 SYN is 39-62 ms (r1 46.856 → 46.895, r2
/// 18.917 → 18.979): Open(1) starts no earlier than this after the answer.
pub(crate) const VIEWER_CHECK_FLOOR: Duration = Duration::from_millis(40);
/// The launcher's answer to this sign-in's `native_viewer_check` and when it
/// arrived (the W2U close; the floor counts from here), taken by the owner
/// thread while it pumps frames (07c §1).
static VIEWER_CHECK: std::sync::Mutex<Option<(proto::ViewerCheckResult, Instant)>> = std::sync::Mutex::new(None);
/// Reply-reader thread: store the answer; only the owner acts on it.
pub fn viewer_check_result(result: proto::ViewerCheckResult) {
    *VIEWER_CHECK.lock().unwrap_or_else(|e| e.into_inner()) = Some((result, Instant::now()));
}
fn take_viewer_check() -> Option<(proto::ViewerCheckResult, Instant)> {
    #[cfg(test)]
    if let Some(answer) = tests::viewer_check_answer() { return answer; }
    VIEWER_CHECK.lock().unwrap_or_else(|e| e.into_inner()).take()
}
const VIEWER_CHECK_SILENT: &str = "PlayOnline Viewer version check did not answer.";

/// `native_play_pending`: a Play is queued behind this pre-sign-in (pol-spec
/// 07d §4.3). Not a START_GAME: it only ends the not-yet-elapsed Viewer gaps,
/// since the cold path has none (08 §4 item 5).
static PLAY_PENDING: AtomicBool = AtomicBool::new(false);
/// Reply-reader thread.
pub fn note_play_pending() {
    #[cfg(test)]
    TEST_PLAY_PENDING.with(|p| p.set(true));
    if !PLAY_PENDING.swap(true, Ordering::AcqRel) { log!("native POL: a Play is queued; remaining Viewer gaps are skipped"); }
}
/// `viewer_gap` returns at its next frame once this (or `play_requested`) is true.
#[cfg(not(test))]
pub(crate) fn play_pending() -> bool { PLAY_PENDING.load(Ordering::Acquire) }
// Tests note a pending Play on their own owner thread only.
#[cfg(test)]
thread_local! { static TEST_PLAY_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) fn play_pending() -> bool { TEST_PLAY_PENDING.with(|p| p.get()) }

pub fn request_start_game() {
    control_result("start_game", accept_play(&CONTROL, &PLAY_LATCH));
}
static START_GATE: std::sync::Mutex<Option<crate::start_gate::Participant>> = std::sync::Mutex::new(None);
pub fn request_batch_game(token: &str) {
    let mut gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let code = match crate::start_gate::Participant::open(token) {
        Ok(participant) if gate.is_none() => {
            // Publish the gate before the owner can observe START_GAME.
            *gate = Some(participant);
            let code = accept_play(&CONTROL, &PLAY_LATCH);
            if code != 0 { *gate = None; }
            code
        }
        _ => BUSY,
    };
    control_result("start_game", code);
}
pub fn wait_batch_game() -> Result<(), String> {
    let gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(gate) = gate { gate.wait()?; }
    Ok(())
}
pub fn request_sign_out() {
    let code = with_startup_state(|state| cancel_startup(state, play_pending()))
        .unwrap_or_else(|| accept_control(&CONTROL, &PLAY_LATCH, SIGN_OUT));
    control_result("sign_out", code);
}
pub fn request_status(mode: i32, groups: bool) {
    let flag = if groups { GROUPS } else { 0 };
    match mode {
        0 => request_control(VISIBLE | flag, "visible"),
        1 => request_control(AWAY | flag, "away"),
        4 => request_control(INVISIBLE | flag, "invisible"),
        _ => {},
    }
}
/// A portrait the user picked. Refused at once, without a core call, until
/// the portrait field is confirmed (`wire::PORTRAIT_FIELD_CONFIRMED`).
pub fn request_portrait(portrait: u16) {
    if !crate::wire::PORTRAIT_FIELD_CONFIRMED || portrait > crate::wire::PORTRAIT_MASK {
        return control_result("portrait", crate::native_portrait::NOT_CONFIRMED);
    }
    control_result("portrait", claim_portrait(&PORTRAIT_REQUEST, portrait, || accept_control(&CONTROL, &PLAY_LATCH, PORTRAIT)));
}
/// Claims the empty slot for `portrait` (published before the owner can observe PORTRAIT),
/// then asks for the command; a refused command gives the slot back. A slot still held by an
/// accepted command the owner has not read yet refuses at once, leaving that value intact.
fn claim_portrait(slot: &AtomicU16, portrait: u16, accept: impl FnOnce() -> i32) -> i32 {
    if slot.compare_exchange(NO_PORTRAIT, portrait, Ordering::AcqRel, Ordering::Acquire).is_err() { return BUSY; }
    let code = accept();
    if code != 0 { slot.store(NO_PORTRAIT, Ordering::Release); }
    code
}
fn request_control(command: u8, name: &str) {
    control_result(name, accept_control(&CONTROL, &PLAY_LATCH, command));
}
/// Every non-Play command starts only from the idle Viewer with no Play queued.
fn accept_control(control: &AtomicU8, latch: &AtomicBool, command: u8) -> i32 {
    if latch.load(Ordering::Acquire) { return BUSY; }
    if control.compare_exchange(SOCIAL, command, Ordering::AcqRel, Ordering::Acquire).is_ok() { 0 } else { BUSY }
}
/// Play from the idle Viewer, or latched behind a running user command. Never
/// during a game, a sign-out or a second Play.
fn accept_play(control: &AtomicU8, latch: &AtomicBool) -> i32 {
    if latch.load(Ordering::Acquire) { return BUSY; }
    if control.compare_exchange(SOCIAL, START_GAME, Ordering::AcqRel, Ordering::Acquire).is_ok() { return 0; }
    let user_command = matches!(control.load(Ordering::Acquire) & !GROUPS, SOCIAL_REQUEST | VISIBLE | AWAY | INVISIBLE | PORTRAIT);
    if user_command && latch.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() { 0 } else { BUSY }
}
/// The owner finished a user command: a latched Play starts now.
fn release_command(control: &AtomicU8, latch: &AtomicBool) {
    control.store(if latch.swap(false, Ordering::AcqRel) { START_GAME } else { SOCIAL }, Ordering::Release);
}
/// A Play latched just as the command was released (the requester saw the
/// command, the owner had already released it): take it from the idle Viewer.
fn take_latched_play(control: &AtomicU8, latch: &AtomicBool) {
    if latch.load(Ordering::Acquire) && control.compare_exchange(SOCIAL, START_GAME, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        latch.store(false, Ordering::Release);
    }
}

/// Launcher-window input counts as Viewer input (owner decision 2): it resets
/// the idle clock the auto-away check reads (app [obj+0x2c0], 10060039).
pub fn note_activity() {
    LAST_INPUT_MS.store(clock_ms(), Ordering::Release);
}
static CLOCK: OnceLock<Instant> = OnceLock::new();
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
fn clock_ms() -> u64 {
    CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64
}
fn idle_time() -> Duration {
    Duration::from_millis(clock_ms().saturating_sub(LAST_INPUT_MS.load(Ordering::Acquire)))
}

pub fn request_social(request: proto::NativeSocialRequest) {
    let mut pending = crate::native_social::REQUEST.lock().unwrap_or_else(|e| e.into_inner());
    if pending.is_some() || accept_control(&CONTROL, &PLAY_LATCH, SOCIAL_REQUEST) != 0 {
        crate::native_social::result(&request, BUSY, None);
    } else { *pending = Some(request); }
}

/// The core's common function table, checked by `pol_core::check_table`.
/// Every call names the number of word arguments it passes; debug builds
/// (every test) assert that it is the count `pol_core::SERVICES` records for
/// that slot, so the validated registry and the calls cannot drift apart.
#[derive(Clone)]
pub(crate) struct Api(Box<[usize]>);
impl Api {
    /// A table `pol_core::check_table` accepted.
    pub(crate) fn new(table: &[usize]) -> Self { Self(table.into()) }
    #[cfg(test)]
    pub(crate) fn test_table(table: &[usize]) -> Self { Self(table.into()) }
    /// cdecl entry `slot`, called with `args` word arguments as `F`.
    pub(crate) unsafe fn f<F: Copy>(&self, slot: usize, args: u8) -> F {
        debug_assert_eq!(crate::pol_core::service(slot).and_then(|s| s.args), Some(args),
            "service {slot} is called with {args} arguments but pol_core::SERVICES records otherwise");
        std::mem::transmute_copy(&self.0[slot])
    }
    /// The table's entry `slot`, handed back to the core as a callback.
    pub(crate) fn entry(&self, slot: usize) -> usize {
        debug_assert!(crate::pol_core::service(slot).is_some(), "entry {slot} is not in pol_core::SERVICES");
        self.0[slot]
    }
    pub(crate) fn zero(&self, slot: usize) -> i32 {
        unsafe { self.f::<unsafe extern "C" fn() -> i32>(slot, 0)() }
    }
    pub(crate) fn one(&self, slot: usize, argument: usize) -> i32 {
        unsafe { self.f::<unsafe extern "C" fn(usize) -> i32>(slot, 1)(argument) }
    }
    pub(crate) fn two(&self, slot: usize, a: usize, b: usize) {
        // Credential/string setters have no return value.
        unsafe { self.f::<unsafe extern "C" fn(usize, usize)>(slot, 2)(a, b) }
    }
    /// cdecl call with up to six word arguments (signed values as two's
    /// complement). The caller pops, so void entries just return garbage eax.
    pub(crate) fn call(&self, slot: usize, a: &[usize]) -> i32 {
        self.word(slot, a) as i32
    }
    /// Same, for entries returning a pointer.
    pub(crate) fn word(&self, slot: usize, a: &[usize]) -> usize {
        type W = usize;
        unsafe {
            match *a {
                [] => self.f::<unsafe extern "C" fn() -> W>(slot, 0)(),
                [p] => self.f::<unsafe extern "C" fn(W) -> W>(slot, 1)(p),
                [p, q] => self.f::<unsafe extern "C" fn(W, W) -> W>(slot, 2)(p, q),
                [p, q, r] => self.f::<unsafe extern "C" fn(W, W, W) -> W>(slot, 3)(p, q, r),
                [p, q, r, s] => self.f::<unsafe extern "C" fn(W, W, W, W) -> W>(slot, 4)(p, q, r, s),
                [p, q, r, s, t] => self.f::<unsafe extern "C" fn(W, W, W, W, W) -> W>(slot, 5)(p, q, r, s, t),
                [p, q, r, s, t, u] => self.f::<unsafe extern "C" fn(W, W, W, W, W, W) -> W>(slot, 6)(p, q, r, s, t, u),
                _ => unreachable!("no common-table entry takes more than six arguments here"),
            }
        }
    }
    fn se(&self, credentials: Option<&NativeCredentials>) {
        let (mode, name, secret, otp) = credentials.map_or((0, 0, 0, 0), |c| {
            (
                if c.otp.is_some() { 2 } else { 1 },
                c.se_id.as_ptr() as usize,
                c.se_secret.as_ptr() as usize,
                c.otp.as_ref().map_or(0, |o| o.as_ptr() as usize),
            )
        });
        unsafe {
            self.f::<unsafe extern "C" fn(i32, usize, usize, usize)>(1502, 4)(mode, name, secret, otp)
        }
    }
    /// ChangeMyStatus (376 = the same code as 195) with the Viewer's tuple
    /// (world/handle, char, flag, mode, field5, app); -2 keeps a field.
    fn begin_status(&self, tuple: [i32; 6]) -> i32 {
        self.call(376, &tuple.map(|v| v as usize))
    }
    fn restore_viewer_policy(&self) -> Result<(), String> {
        // Stock POL keeps this scalar at zero through game backup. Windower's
        // supported Hook.dll calls common entry 1075(1) after loading the core.
        // Besides viewer limits, that makes native delete reject a missing old
        // backup (-5326), preventing the first upload. Use the original setter
        // to restore stock behavior; native transfer and errors remain intact.
        let before = self.zero(1074);
        if before != 0 {
            unsafe { self.f::<unsafe extern "C" fn(i32)>(1075, 1)(0) };
        }
        if self.zero(1074) != 0 {
            return Err("The native PlayOnline viewer policy could not be restored.".into());
        }
        log!("native POL viewer policy {before}->0");
        Ok(())
    }
}

/// Runs a status-job request (376/377) under the Viewer's Normal policy.
pub(crate) fn status_request(api: &Api, name: &'static str, tuple: [i32; 6], frame: &mut dyn FnMut() -> bool) -> i32 {
    native_jobs::run(api, Request { name, start: &mut || api.begin_status(tuple), poll: 377, policy: Policy::Normal }, frame)
}

// --- Viewer host callbacks (pol-spec 01 §2.1 #13/#15/#17) -------------------

extern "C" {
    // The CRT heap: app.dll registers its CRT malloc/free/realloc (1019b8fe).
    // The core frees profile memory through these while it lives.
    fn malloc(size: usize) -> *mut c_void;
    fn free(block: *mut c_void);
    fn realloc(block: *mut c_void, size: usize) -> *mut c_void;
}
/// 871 entries 0 and 2 (app 100a60cd, 100a606c): queue member-session text
/// into the Viewer's chat window ring ([105050a4]); without a chat window they
/// do nothing. The core ignores their result (10017ef0).
unsafe extern "C" fn no_chat_window(_: usize, _: usize, _: usize, _: usize, _: usize) {}
/// 871 entry 5, the 288 receiver: app 10008ec7 (`ret`) before sign-in and
/// 1005657d (local UI redraws only) after it.
unsafe extern "C" fn no_service_view(_: usize, _: usize) {}
/// 1149 (app 1019b63c, 1019acf0): inactivity warning/expiry dialogs. The
/// inactivity timer is never armed here (the Viewer's 1001cfb7(0) default;
/// Crystal never calls 1140(1)), so neither can fire.
unsafe extern "C" fn no_inactivity_warning() {}
unsafe extern "C" fn no_inactivity_expiry() {}

/// The Viewer's auto-Away (app 1001b0f3 / 1001b16c), counted per idle owner
/// frame while signed in, between games and Visible.
pub(crate) struct AutoAway {
    minutes: u32,
    frames: u32,
}
impl AutoAway {
    pub(crate) fn new(minutes: u32) -> Self { Self { minutes, frames: 0 } }
    /// One gated frame; true when the Viewer would set Away now. The counter
    /// advances only on gated frames and resets after every check.
    pub(crate) fn frame(&mut self, visible: bool, idle: Duration) -> bool {
        if self.minutes == 0 || !visible {
            return false;
        }
        self.frames += 1;
        if self.frames <= auto_away_frames() {
            return false;
        }
        self.frames = 0;
        idle.as_millis() > u128::from(self.minutes) * 60_000
    }
}

/// Manual and automatic presence share the Viewer's b4c status job. A newer
/// change replaces it, cancelling only an actually pending status ticket.
#[derive(Default)]
struct ViewerStatus {
    pending: Option<PendingStatus>,
}
struct PendingStatus {
    mode: i32,
    automatic: bool,
    job: native_jobs::Job,
}
impl ViewerStatus {
    fn queue(&mut self, api: &Api, mode: i32, automatic: bool) {
        self.cancel(api);
        self.pending = Some(PendingStatus { mode, automatic, job: native_jobs::Job::new() });
    }
    fn step(&mut self, api: &Api) {
        let Some(pending) = &mut self.pending else { return; };
        let mode = pending.mode;
        let mut start = || api.begin_status([-2, -2, 1, mode, 0, -2]);
        let request = &mut Request { name: "status-change", start: &mut start, poll: 377, policy: Policy::Normal };
        let Some(code) = pending.job.advance(api, request) else { return; };
        let action = match mode { 0 => "visible", 1 => "away", _ => "invisible" };
        log!("native POL {action} status mode={mode} automatic={} result={code}", pending.automatic);
        // After Play a failure is only logged: the worker would hold it as a
        // session error until attach_game's state line clears it, and a batch
        // release reads that window as a dead session.
        if code >= 0 || code == -261 || !play_requested() {
            control_result(action, code);
        }
        self.pending = None;
    }
    fn in_flight(&self) -> bool { self.pending.as_ref().is_some_and(|p| p.job.in_flight()) }
    /// Once Play is requested: poll an in-flight ticket to its end (or its 60 s
    /// policy cancel); a change not in flight (queued or waiting to retry) is
    /// dropped unsent, since no new attempt may start. Logged, not reported (see `step`).
    fn settle_step(&mut self, api: &Api) {
        if self.in_flight() { return self.step(api); }
        if let Some(pending) = self.pending.take() {
            log!("native POL status mode={} automatic={} not resent: Play requested", pending.mode, pending.automatic);
        }
    }
    fn cancel(&mut self, api: &Api) {
        if let Some(mut pending) = self.pending.take() { pending.job.cancel(api); }
    }
    /// The handoff takes the job without a cancel: the settle loop has already
    /// polled any ticket to its end, so none is left that 337 would abort.
    fn retire(&mut self) {
        if let Some(pending) = self.pending.take() {
            if pending.job.in_flight() { log!("error: native POL status ticket still in flight at the handoff; not cancelled"); }
        }
    }
}

/// Lives inside the same scope as the COM core: the Viewer's host state and
/// its signed-in member session. Drop before releasing the core; it runs the
/// Viewer's sign-out and close teardown (03 §5).
pub struct NativeSession {
    api: Api,
    viewer: PathBuf,
    area: i32,
    /// The game language 421 falls back to (app 102787d6 → 1528).
    language: u32,
    auto_away: AutoAway,
    viewer_status: ViewerStatus,
    social: crate::native_social::Service,
    pushes: crate::native_social::PushDelivery,
    /// Profile list header returned by 825.
    profiles: usize,
    /// 932/914 started the file worker and option.bin mutex.
    files: bool,
    /// A member home is selected (834() >= 0).
    home: bool,
    /// 819/824/profile list/1149 are registered (app 1019b874).
    registered: bool,
    /// Member session reached 30 with 838 == 3 and has not been lost.
    signed_in: bool,
    /// Open(0) succeeded; member-login failure still requires native close.
    network_opened: bool,
    /// GameStart returned; the Viewer's restart is still due.
    returned: bool,
    /// Session `viewer_gaps`: a sign-in-only session restores the Viewer's
    /// ~1.05 s (op7 → 374) and ~1.6 s (mail → publish) UI gaps, as owner
    /// frames. Never on a cold Play (07b decision 2, 07c §5.4).
    viewer_gaps: bool,
    /// This sign-in has asked for its W2U; one per sign-in, and Crystal runs
    /// one session per host process (07c §1.1).
    viewer_checked: bool,
}

impl NativeSession {
    fn new(api: Api, viewer: &Path, area: i32, auto_away_minutes: u32) -> Self {
        Self {
            api,
            viewer: viewer.to_path_buf(),
            area,
            language: u32::from(area != 0),
            auto_away: AutoAway::new(auto_away_minutes),
            viewer_status: Default::default(),
            social: Default::default(),
            pushes: Default::default(),
            profiles: 0,
            files: false,
            home: false,
            registered: false,
            signed_in: false,
            network_opened: false,
            returned: false,
            viewer_gaps: false,
            viewer_checked: false,
        }
    }

    /// From the session line's `viewer_gaps`; set before `sign_in`.
    pub fn set_viewer_gaps(&mut self, gaps: bool) { self.viewer_gaps = gaps; }

    /// pol.exe → app.dll startup on a fresh core, before any 815/814 (A1/A2/A8).
    ///
    /// # Safety
    /// `table` is the core's live function table that `pol_core::check_table`
    /// accepted (`_checked`). The caller keeps that core alive through this
    /// object's lifetime; `viewer_c` and `regkey` must outlive it (the core
    /// keeps the pointers).
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bootstrap(
        table: &[usize],
        _checked: &crate::pol_core::Table,
        viewer: &Path,
        viewer_c: &'static CStr,
        regkey: &'static CStr,
        area: i32,
        language: u32,
        auto_away_minutes: u32,
    ) -> Result<Self, String> {
        let mut session = Self::new(Api::new(table), viewer, area, auto_away_minutes);
        session.language = language;
        session.api.restore_viewer_policy()?;
        session.bootstrap_core(viewer_c, regkey)?;
        // 10050fa2 → 10057896: notification titles and texts.
        install_message_texts(&session.api, viewer)?;
        log!("native POL host registered like the Viewer (824/871/1149, profile list loaded)");
        Ok(session)
    }

    fn bootstrap_core(&mut self, viewer_c: &CStr, regkey: &CStr) -> Result<(), String> {
        let api = self.api.clone();
        self.host_paths(viewer_c, regkey);
        api.call(870, &[usize::MAX]); // 10199783: no member
        api.zero(932); // 100192d6 (919 = 932): file worker
        api.zero(914); // 100192e1: option.bin manager mutex
        self.files = true;
        self.region_settings();
        self.register_callbacks(); // 1019b8b9, fresh Viewer only
        self.register_host() // 1019b8e2..1019b929
    }

    /// The game language first, then the Viewer root and registry key.
    /// app 102787d6: 1528(102afa80(region)) — JP 0, US 1, EU the registry
    /// language. FFXi.dll/FFXiMain read it through 421, which caches its first
    /// answer; 125's path code (core 10048e1f/10048edc/100491be) already asks
    /// 421, so 1528 must come before 125 or FFXI starts in Japanese.
    fn host_paths(&self, viewer_c: &CStr, regkey: &CStr) {
        self.api.call(1528, &[self.language as usize]);
        self.api.call(125, &[viewer_c.as_ptr() as usize]); // 1001a6ea (1456 → 125)
        self.api.call(367, &[regkey.as_ptr() as usize]); // 100192bb
    }

    /// app 1001a175: 965(region != 0), 1527(region == 0), 1309(POL_UCS_AREA_KBN
    /// "01" from data\utf\env.dat, US). 420 is never called by the US Viewer.
    fn region_settings(&self) {
        self.api.call(965, &[usize::from(self.area != 0)]);
        self.api.call(1527, &[usize::from(self.area == 0)]);
        self.api.call(1309, &[c"01".as_ptr() as usize]);
    }

    /// 871 as app 1019b8b9 / 10050fd9: entry 1 is the core's own dispatcher
    /// (table[334]) — the only route for server pushes on the member session —
    /// and entry 3 is the core's 401, which the Viewer's 100a608e calls first.
    /// Entry 4 is the Viewer's mail receiver (app 10056b05); it accepts pushes
    /// from here until the hand-off to a game, since 1032 gates its calls.
    fn register_callbacks(&self) {
        let set = [
            no_chat_window as *const () as usize,
            self.api.entry(334),
            no_chat_window as *const () as usize,
            self.api.entry(401),
            crate::native_social::mail_arrived as *const () as usize,
            no_service_view as *const () as usize,
        ];
        self.api.call(871, &[set.as_ptr() as usize]);
        crate::native_social::set_listening(true);
    }

    /// app 1019b874 (after 871): 819(0), 824(CRT heap), the profile list from
    /// usr\all\inet_w.bin (101a0f48), 1149(callbacks). Redone on return.
    fn register_host(&mut self) -> Result<(), String> {
        self.api.call(819, &[0]);
        self.api.call(824, &[malloc as *const () as usize, free as *const () as usize, realloc as *const () as usize]);
        self.registered = true;
        let file = native_profile::read_file(&self.viewer);
        self.profiles = native_profile::load(&self.api, file.as_deref())?.0;
        self.api.call(1149, &[no_inactivity_warning as *const () as usize, no_inactivity_expiry as *const () as usize]);
        Ok(())
    }

    /// app 1019869a, preceded by 828 (101a09db): free the profile list, then
    /// unregister the heap, error callback, connection callbacks and 1149.
    fn unregister_host(&mut self) {
        if !self.registered {
            return;
        }
        self.api.zero(828);
        self.api.call(824, &[0, 0, 0]);
        self.api.call(819, &[0]);
        self.api.zero(872); // 871 set → core defaults (331/288 ← 0)
        self.api.call(1149, &[0, 0]);
        self.registered = false;
        self.profiles = 0;
    }

    /// Sign in the member like a Viewer user: select the member, choose the
    /// LAN connection, open the network, then the member session (A3/A4/A5).
    pub fn sign_in(&mut self, credentials: NativeCredentials) -> Result<(), String> {
        // Local storage must work before spending a login/OTP.
        let home = claim_home(&self.viewer, &credentials.profile_key)?;
        note_activity();
        native_jobs::restart_request_clock();
        let result = self.sign_in_core(home, &credentials);
        drop(credentials);
        result
    }

    fn sign_in_core(&mut self, home: u32, c: &NativeCredentials) -> Result<(), String> {
        if startup_cancelled() { return Err(SIGN_IN_CANCELLED.into()); }
        self.select_member(home, c);
        let name = native_profile::selected_name(&self.api, self.profiles)?;
        native_profile::connect(&self.api, &name)?; // connect screen 10199d02
        // The cold stock probe (2026-10-06, [+0x99414]) shows Open(0) to the
        // network level (838 == 2), then Open(1) for member login. Mode 1 skips
        // the core's own state-40 status request that Open(3) would add; the
        // live captures contain none. 1502 is set before member login.
        self.open(0, 2)?; // sets network_opened once the network is open
        self.connect_viewer_check()?;
        self.api.se(Some(c));
        self.open(1, 3)?;
        let member_login = owner_frames();
        self.signed_in = true;
        log!("native POL member session signed in");
        // The live stock probe shows delivery switched on right after sign-in,
        // before the status load; then 101aa269 → 10050fd9: 871 again, the
        // status load alone, and its success handler (10040650) queues the
        // four lists. No status request of our own at sign-in (A6).
        self.api.call(1032, &[1]);
        let api = self.api.clone();
        // Stock op7 end → 374 SYN: 1.049 s (r2), 1.07 s (r1); 07a stock-faithful-pacing.
        self.viewer_gap(&api, "member login → status", member_login, OP7_TO_STATUS_FRAMES)?;
        self.register_callbacks();
        let status_started = Instant::now();
        let status = {
            let mut cadence = StartupCadence::new("status");
            native_jobs::run(&api, Request { name: "status", start: &mut || api.zero(374), poll: 375, policy: Policy::Login }, &mut || cadence.frame(&api, &mut self.pushes))
        };
        log!("native POL status load result {status} in {:.2}s", status_started.elapsed().as_secs_f64());
        if !native_jobs::succeeded(status) {
            return Err(startup_stop(LAUNCHER_CLOSED).unwrap_or_else(|| format!("Native PlayOnline status loading failed ({status}).")));
        }
        let results = {
            let mut cadence = StartupCadence::new("lists");
            cadence.frame(&api, &mut self.pushes); // the queued lists start on the next pump tick
            run_concurrent(&api, &LISTS, &mut || cadence.frame(&api, &mut self.pushes))
        };
        if let Some((name, result)) = LISTS.iter().zip(&results).map(|((n, ..), r)| (n, *r)).find(|(_, r)| !native_jobs::succeeded(*r)) {
            return Err(startup_stop(LAUNCHER_CLOSED).unwrap_or_else(|| format!("Native PlayOnline {name} loading failed ({result}).")));
        }
        crate::native_handles::ensure(&api, &c.handle_names, &mut || owner_frame(&api, &mut self.pushes))
            .map_err(|error| startup_stop(LAUNCHER_CLOSED).unwrap_or(error))?;
        crate::native_portrait::probe_own(&api); // local reads only (190, 173)
        events::send("{\"type\":\"authenticated\"}\n".into());
        log!("native POL authenticated; all game service callbacks are original");
        // Main menu entry: the mail list, then the Viewer presence about a
        // second later (live capture: 496-byte list, then the 120-byte status).
        let mail_started = Instant::now();
        // The list's timeouts use wall time, so it can advance at startup
        // cadence. Downloads and acknowledgements still count Viewer frames.
        let listed = self.social.social_entry_with_list_frame(&api, STARTUP_FRAME,
            &mut |interval| owner_frame_at(&api, &mut self.pushes, interval));
        log!("native POL startup mail completed in {:.2}s", mail_started.elapsed().as_secs_f64());
        // A Viewer closed or cancelled here publishes no presence (376) only to
        // cancel it (337) ahead of the sign-out.
        if owner_stopping(&api) {
            return Err(startup_stop(LAUNCHER_CLOSED).unwrap_or_else(||
                format!("Native PlayOnline connection failed during sign-in (POL result {}).", api.zero(1103))));
        }
        // Stock mail response → publish SYN: 1.606 s (r2), ~1.63 s (r1) at sign-in,
        // counted from the list response so new-mail transfers fall inside it (07c §5.4).
        self.viewer_gap(&api, "mail → presence", listed, MAIL_TO_PUBLISH_FRAMES)?;
        let presence_started = Instant::now();
        let published = {
            let mut cadence = StartupCadence::new("presence");
            Self::publish_viewer(&api, &mut || cadence.frame(&api, &mut self.pushes))
        };
        log!("native POL startup presence completed in {:.2}s", presence_started.elapsed().as_secs_f64());
        events::send(proto::main_menu_line(published, false));
        Ok(())
    }

    /// One of the Viewer's frame-counted screen transitions, only for a
    /// sign-in-only session: idle owner frames (818, pushes; never a bare
    /// sleep) until `gap` frames have run since owner frame `since` (pol-spec
    /// 08 §4 item 5: the Viewer counts frames, not time). Once a Play is
    /// queued or requested the rest is skipped at the next frame: a cold Play
    /// has no gaps (07d §4.3). A stopping owner fails the sign-in here, before
    /// the next request starts: a Viewer closed during the transition opens
    /// neither 374 nor 376, so none is started to be cancelled (337) just
    /// ahead of the sign-out.
    fn viewer_gap(&mut self, api: &Api, stage: &str, since: u64, gap: u32) -> Result<(), String> {
        if !self.viewer_gaps { return Ok(()); }
        let started = Instant::now();
        let mut frames = 0;
        while owner_frames().saturating_sub(since) < u64::from(gap) {
            if play_requested() || play_pending() {
                log!("native POL Viewer gap {stage} skipped after {frames} frame(s): a Play is queued");
                return Ok(());
            }
            if !owner_frame(api, &mut self.pushes) {
                log!("native POL Viewer gap {stage} stopped after {frames} frame(s)");
                if let Some(stop) = startup_stop(LAUNCHER_CLOSED) { return Err(stop); }
                return Err(format!("Native PlayOnline connection failed during sign-in (POL result {}).", api.zero(1103)));
            }
            frames += 1;
        }
        log!("native POL Viewer gap {stage}: {frames} frame(s), {:.2}s", started.elapsed().as_secs_f64());
        Ok(())
    }

    /// The Viewer's own version check inside Connect (app 1029c725 → 102a63fa):
    /// stock sends W2U :54000 after Open(0) and closes it 39-62 ms before the
    /// :51240 member SYN (pol-spec 07a w2u-at-connect F3; 07c §1). The launcher
    /// runs it and answers after its close. Owner frames (window messages,
    /// one 818, pushes; no 815, no request) keep the core alive meanwhile.
    /// An outdated Viewer or a check that could not be sent fails the sign-in
    /// before 1502/Open(1) (the existing post-Open(0) teardown). An unanswered
    /// server does not: the sign-in goes on, like the FFXI check (owner,
    /// 2026-10-12).
    fn connect_viewer_check(&mut self) -> Result<(), String> {
        if std::mem::replace(&mut self.viewer_checked, true) {
            return Err("The PlayOnline Viewer version check already ran for this sign-in.".into());
        }
        if let Some(stop) = startup_stop(VIEWER_CHECK_SILENT) { return Err(stop); }
        let api = self.api.clone();
        *VIEWER_CHECK.lock().unwrap_or_else(|e| e.into_inner()) = None; // only this request's answer counts
        events::send(proto::viewer_check_line());
        let asked = Instant::now();
        let mut frames = 0;
        let (answer, arrived) = loop {
            if let Some(answer) = take_viewer_check() { break answer; }
            if waited(asked, frames, VIEWER_CHECK_LIMIT) {
                log!("error: native POL Viewer check: no answer after {:.1}s", asked.elapsed().as_secs_f64());
                return Err(VIEWER_CHECK_SILENT.into());
            }
            if !owner_frame(&api, &mut self.pushes) { return Err(self.stopped_owner(&api)); }
            frames += 1;
        };
        log!("native POL Viewer check answered after {:.2}s: {answer:?}", asked.elapsed().as_secs_f64());
        let error = match answer {
            proto::ViewerCheckResult::Current => None,
            proto::ViewerCheckResult::Outdated(e) => Some((e, "PlayOnline Viewer needs an update before signing in.")),
            proto::ViewerCheckResult::Failed(e) => Some((e, "PlayOnline Viewer version check failed.")),
            // The launcher's capped retry found no network (07d §2.3): Open(1) would end in -19.
            proto::ViewerCheckResult::NetworkNotReady(e) => Some((e, "The network is not ready, so PlayOnline did not sign in.")),
            proto::ViewerCheckResult::Unreachable(_) => {
                log!("note: native POL Viewer check got no answer from the server; signing in anyway");
                None
            }
        };
        if let Some((text, fallback)) = error {
            return Err(if text.is_empty() { fallback.into() } else { text });
        }
        // Stock W2U FIN → :51240 SYN 39-62 ms: frames until the floor has passed.
        let mut frames = 0;
        while !waited(arrived, frames, VIEWER_CHECK_FLOOR) {
            if !owner_frame(&api, &mut self.pushes) { return Err(self.stopped_owner(&api)); }
            frames += 1;
        }
        Ok(())
    }

    /// An owner frame stopped a sign-in wait: the launcher is gone or the core
    /// latched a pump error.
    fn stopped_owner(&self, api: &Api) -> String {
        if let Some(stop) = startup_stop(VIEWER_CHECK_SILENT) { return stop; }
        format!("Native PlayOnline connection failed before member login (POL result {}).", api.zero(1103))
    }

    /// Member select (app 101a425f): 870 → 851 → 852 → 886(encoded "") → 853 → 891.
    /// That 891 fails like stock (-11267): Viewer-made members have no profile name.
    fn select_member(&mut self, home: u32, c: &NativeCredentials) {
        let api = &self.api;
        api.call(870, &[home as usize]);
        self.home = true;
        let mut id = Zeroizing::new([0u8; 8]);
        api.two(854, id.as_mut_ptr() as usize, c.pol_id.as_ptr() as usize);
        api.one(851, id.as_ptr() as usize);
        let mut password = Zeroizing::new([0u8; 17]);
        api.two(856, password.as_mut_ptr() as usize, c.pol_password.as_ptr() as usize);
        api.two(852, password.as_ptr() as usize, 0);
        let mut none = [0u8; native_profile::NAME_LEN];
        api.call(858, &[none.as_mut_ptr() as usize, c"".as_ptr() as usize]);
        api.call(886, &[none.as_ptr() as usize]);
        let mut host = [0u8; 0x42];
        api.two(868, host.as_mut_ptr() as usize, c"ci000.pol.com".as_ptr() as usize);
        api.one(853, host.as_ptr() as usize);
        let code = api.zero(891);
        if code != native_profile::PROFILE_NOT_FOUND {
            log!("note: member-select profile load returned {code} (stock: -11267)");
        }
    }

    /// Sign-in object (app 1019cb52): Open(mode, 835()), then one 818 and one
    /// 815 per frame (102797f0, 1019ce64) until state 30 with 838 == `linked`
    /// (2 network, 3 member). 31..38 are transitional; failure is a negative
    /// state, reported with 823 (D18).
    fn open(&mut self, mode: usize, linked: i32) -> Result<(), String> {
        if startup_cancelled() { return Err(SIGN_IN_CANCELLED.into()); }
        let name = if mode == 0 { "open-network" } else { "open-member" };
        let mut cadence = StartupCadence::new(name);
        let api = &self.api.clone();
        let connection = api.word(835, &[]);
        // 07d §4.7: start/end, one attempt, one poll per 815.
        let mut request = native_jobs::RequestLog::start(name);
        request.attempts = 1;
        let code = api.call(814, &[mode, connection]);
        request.result = code;
        if code < 0 {
            return Err(format!("Native PlayOnline login could not start (mode {mode}, POL result {code})."));
        }
        let started = Instant::now();
        let mut aborted: Option<Instant> = None;
        let mut previous = i32::MIN;
        loop {
            cadence.tick();
            pump_idle_messages();
            api.zero(818);
            let state = api.zero(815);
            (request.polls, request.result) = (request.polls + 1, state);
            if state != previous {
                log!("native POL sign-in state {state} (mode {mode}, +{:.2}s)", started.elapsed().as_secs_f64());
                events::send(format!("{{\"type\":\"native_auth_stage\",\"code\":{state}}}\n"));
                previous = state;
            }
            match connection_result(state, || api.zero(838), linked) {
                Some(Ok(())) => {
                    // The network is open and needs the native close, also
                    // when the abort below came too late to fail the Open.
                    if mode == 0 { self.network_opened = true; }
                    if aborted.is_none() { return Ok(()); }
                    return Err(startup_stop(LAUNCHER_CLOSED)
                        .unwrap_or_else(|| format!("Native PlayOnline login timed out at state {state}.")));
                }
                Some(Err(())) => {
                    let error = api.zero(823); // non-consuming native error getter
                    return Err(open_failure(mode, state, error, aborted.is_some()));
                }
                None => {}
            }
            match aborted {
                // The Viewer's cancel button: 807 makes the core fail the login.
                None if events::pipe_closed() || startup_cancelled() || started.elapsed() >= SIGN_IN_LIMIT => {
                    log!("native POL sign-in cancelled at state {state}");
                    api.zero(807);
                    aborted = Some(Instant::now());
                }
                Some(at) if at.elapsed() >= ABORT_LIMIT => {
                    return Err(format!("Native PlayOnline login timed out at state {state}."));
                }
                _ => {}
            }
            pause_owner(STARTUP_FRAME);
        }
    }

    /// Before GameStart, mirror the Viewer's launch and exit (B3, 03 §3.1).
    /// Retire the owned status job before the game takes its place, without a
    /// cancel: 337/328/199 would stamp the core's 10 s request freeze over the
    /// game's first status (07a presign-no-cancel-freeze). Its ticket, if any,
    /// was polled to its end by `settle_for_play`. False, and nothing handed
    /// over, when the launcher's startup sign-out came first.
    pub fn attach_game(&mut self) -> bool {
        CONTROL.store(GAME, Ordering::Release);
        if !finish_startup() { return false; }
        let api = self.api.clone();
        self.viewer_status.retire();
        // Answer messages pushed during the last idle frames before the game
        // owns the core; later pushes go to FFXI's 288 receiver.
        if self.signed_in {
            // No retry-capable mail cycle and no pump between this flush and
            // 872 removing the callback. A push cannot land in a disabled sink.
            self.pushes.flush_pushed(&api, true);
        }
        let dropped = self.pushes.abandon_receipts();
        if dropped > 0 {
            log!("native POL {dropped} mail receipt(s) not accepted before the game; dropped like the Viewer's");
        }
        // Play (app 1019fe84 / 1019fec7): clear the arrival flag, hold delivery
        // off only around the version check, then back on. The live probe
        // shows 1032 at 1 throughout the game, so FFXI's 288 receiver gets
        // pushes during play; the launcher runs that check before this call.
        api.call(1305, &[0]);
        api.call(1032, &[1]);
        // Launch state 8 (101a05ea): disarm inactivity, launched app, exit code.
        api.call(1140, &[0]);
        api.call(1175, &[FFXI_APP]);
        api.call(969, &[0]);
        // Viewer exit: 101aa2ac (463), then 1019869a (828/824/819/872/1149).
        api.zero(463);
        self.unregister_host();
        crate::native_social::set_listening(false);
        events::send("{\"type\":\"native_session_state\",\"game\":true}\n".into());
        true
    }

    /// GameStart has returned and all game callbacks/observers have joined.
    /// FFXI's 288 receiver lives in FFXiMain; clear it before anything pumps
    /// (LOCAL; the restarted Viewer's 871 replaces it anyway).
    pub fn detach_game(&mut self) {
        self.api.call(288, &[0]);
        self.returned = true;
    }

    /// pol.exe's next Viewer instance reusing the signed-in core (B4, 03 §3.4):
    /// startup registrations, 1032(1), post-login 871, the four lists without
    /// 374, the app-1000 publish, then the social entry. Ok(false): the owner
    /// is stopping, so the rest of the return starts no request.
    fn return_from_game(&mut self) -> Result<bool, String> {
        note_activity(); // 101abaf8 / 1019fde5 refresh the last-input stamp
        native_jobs::restart_request_clock();
        self.region_settings(); // 1001a175
        if self.api.zero(838) != 3 {
            self.signed_in = false;
            self.network_opened = false;
            pol_status(SESSION_LOST);
            return Err("PlayOnline is no longer signed in after the game. Sign in again to reconnect.".into());
        }
        self.register_host()?; // 1019b874 (871 is skipped at startup)
        self.api.call(1032, &[1]); // 101aaebe
        self.register_callbacks(); // 101aa269 → 10050fd9
        let api = self.api.clone();
        let mut frame = || owner_frame(&api, &mut self.pushes);
        let results = run_concurrent(&api, &LISTS, &mut frame);
        for ((name, ..), result) in LISTS.iter().zip(&results) {
            log!("native POL {name} reload result {result}");
        }
        if owner_stopping(&api) { return Ok(false); }
        // As at sign-in: the mail list first, then the Viewer presence.
        // (Return-path gaps, 0.9-1.06 s, are not restored: 07c §8.4.)
        self.social.social_entry(&api, &mut frame);
        if owner_stopping(&api) { return Ok(false); }
        let published = Self::publish_viewer(&api, &mut frame);
        events::send(proto::main_menu_line(published, true));
        Ok(true)
    }

    /// app 101cf603 → 10041342(1000) with no Viewer-own handle (10019224):
    /// a selected character is cleared with the publish; otherwise publish
    /// only if the application is not already the Viewer. Returns the 376/377
    /// result, or 0 when nothing needed publishing (07c §2).
    fn publish_viewer(api: &Api, frame: &mut dyn FnMut() -> bool) -> i32 {
        let tuple = if api.zero(190) >= 0 {
            [-2, -1, 1, -2, 0, VIEWER_APP]
        } else if api.zero(207) as u16 as i32 != VIEWER_APP {
            [-2, -2, 1, -2, 0, VIEWER_APP]
        } else {
            log!("native POL Viewer presence already published");
            return 0;
        };
        let code = status_request(api, "presence", tuple, frame);
        log!("native POL Viewer presence result {code}");
        code
    }

    /// The idle Viewer between games (03 §1): commands, auto-Away, the social
    /// idle work and the per-frame 1103 loss check, one 818 per frame.
    pub fn wait_for_game(&mut self) -> Result<bool, String> {
        // The launcher is gone: sign out without the restarted Viewer's requests.
        if events::pipe_closed() {
            return Ok(false);
        }
        if std::mem::take(&mut self.returned) && !self.return_from_game()? {
            if events::pipe_closed() { return Ok(false); }
            let error = self.api.zero(1103);
            return Err(self.connection_lost(error));
        }
        PLAY_LATCH.store(false, Ordering::Release);
        CONTROL.store(SOCIAL, Ordering::Release);
        if !finish_startup() {
            CONTROL.store(GAME, Ordering::Release);
            return Ok(false);
        }
        events::send("{\"type\":\"native_session_state\",\"game\":false}\n".into());
        events::send("{\"type\":\"native_social_ready\"}\n".into());
        let observer = self.observe_friends()?;
        let api = self.api.clone();
        let result = loop {
            if crate::native_social::take_mail_changed() {
                events::send("{\"type\":\"native_mail_changed\"}\n".into());
            }
            let control = CONTROL.load(Ordering::Acquire);
            match control & !GROUPS {
                SOCIAL => take_latched_play(&CONTROL, &PLAY_LATCH),
                SOCIAL_REQUEST => {
                    let request = crate::native_social::REQUEST.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(request) = request {
                        self.social.execute_with(&self.api, request,
                            &mut || idle_owner_frame(&api, &mut self.pushes, &mut self.auto_away, &mut self.viewer_status, idle_time()));
                    }
                    release_command(&CONTROL, &PLAY_LATCH);
                }
                PORTRAIT => {
                    let portrait = PORTRAIT_REQUEST.swap(NO_PORTRAIT, Ordering::AcqRel);
                    let code = crate::native_portrait::set_own(&api, portrait,
                        &mut || idle_owner_frame(&api, &mut self.pushes, &mut self.auto_away, &mut self.viewer_status, idle_time()));
                    control_result("portrait", code);
                    release_command(&CONTROL, &PLAY_LATCH);
                }
                START_GAME => break self.settle_for_play(),
                SIGN_OUT => break Ok(false),
                action @ (VISIBLE | AWAY | INVISIBLE) => {
                    let mode = match action { VISIBLE => 0, AWAY => 1, _ => 4 };
                    Self::user_status(&api, mode, control & GROUPS != 0, &mut self.viewer_status,
                        &mut |status| idle_owner_frame(&api, &mut self.pushes, &mut self.auto_away, status, idle_time()));
                    release_command(&CONTROL, &PLAY_LATCH);
                }
                _ => {},
            }
            if events::pipe_closed() { break Ok(false); }
            let error = self.api.zero(1103);
            if error < 0 {
                break Err(self.connection_lost(error));
            }
            let mut frame = || idle_owner_frame(&api, &mut self.pushes, &mut self.auto_away, &mut self.viewer_status, idle_time());
            self.social.idle_frame(&self.api, &mut frame); // no mail retry once Play is requested
            frame();
        };
        CONTROL.store(GAME, Ordering::Release);
        PLAY_LATCH.store(false, Ordering::Release);
        drop(observer);
        // Play hands off with no cancel; sign-out and a stopping owner keep the
        // Viewer's own cancel point (Drop repeats it harmlessly).
        if !matches!(result, Ok(true)) { self.viewer_status.cancel(&api); }
        result
    }

    /// Play from the idle Viewer: launch states 3/6/7 (app 101a0269, 03 §3.1)
    /// wait for the status and commit jobs to go idle, then hand off with no
    /// status call and no cancel. Here that is the owned status job (auto-Away
    /// or a user change); social requests, commits and mail cycles run inside
    /// their own nested owner loops and have finished before START_GAME is read.
    /// Idle frames keep running (818, pushes); nothing new starts. The 60 s
    /// Normal-policy cancel still fires for a stuck ticket. Ok(false): the
    /// launcher went away while waiting.
    fn settle_for_play(&mut self) -> Result<bool, String> {
        let api = self.api.clone();
        let started = Instant::now();
        let mut frames = 0u32;
        while self.viewer_status.in_flight() {
            let status = &mut self.viewer_status;
            let running = owner_frame_with(&api, &mut self.pushes, FRAME, &mut || status.settle_step(&api));
            frames += 1;
            if !running {
                if events::pipe_closed() { return Ok(false); }
                let error = api.zero(1103);
                return Err(self.connection_lost(error));
            }
        }
        if frames > 0 {
            log!("native POL Play waited {frames} frame(s) ({:.2}s) for the in-flight status job", started.elapsed().as_secs_f64());
        }
        Ok(true)
    }

    /// One idle frame of the Viewer's OnUpdate auto-Away (app 1001b0f3): gated
    /// on Visible (206() == 0); when due and idle long enough, 10040d4f(1)
    /// sends (-2,-2,1,1,0,-2). No group push and no automatic return (03 §1.3).
    fn auto_away_tick(api: &Api, auto_away: &mut AutoAway, status: &mut ViewerStatus, idle: Duration) {
        if auto_away.frame(api.zero(206) == 0, idle) { status.queue(api, 1, true); }
    }

    /// Status dialog apply queues ChangeMyStatus and, when requested and a
    /// group is active, ChgAllGrpStatus in the same frame (app 10031ba9..bc1).
    /// Both retain independent Normal-policy completion, timeout and retries.
    fn user_status(api: &Api, mode: i32, groups: bool, status: &mut ViewerStatus, frame: &mut dyn FnMut(&mut ViewerStatus) -> bool) {
        let with_groups = groups && api.zero(385) > 0;
        status.queue(api, mode, false);
        status.step(api);
        if with_groups {
            let mut running = true;
            let code = native_jobs::run(api, Request {
                name: "groups-status", start: &mut || api.call(404, &[mode as usize]), poll: 405, policy: Policy::Normal,
            }, &mut || { running = frame(status); running });
            log!("native POL groups status mode={mode} result={code}");
            control_result("groups", code);
            if !running { status.cancel(api); return; }
        }
        while status.pending.is_some() {
            if !frame(status) { status.cancel(api); break; }
        }
    }

    /// app 101984da: the latched pump error. Local sign-out only: 101aa28e,
    /// cleared flags, 101aa2ac (463), 434; no 816/817/836, no automatic
    /// re-login (B8).
    fn connection_lost(&mut self, error: i32) -> String {
        log!("native POL connection lost (POL result {error})");
        pol_status(SESSION_LOST);
        self.signed_in = false;
        self.network_opened = false;
        self.viewer_status.cancel(&self.api);
        crate::native_social::set_listening(false);
        // Keep what already arrived; the lost session can't take receipts.
        self.pushes.flush_pushed(&self.api, false);
        crate::native_social::discard_pushed();
        self.api.zero(463);
        self.api.zero(434);
        format!("PlayOnline session ended (POL result {error}). Sign in again to reconnect.")
    }

    /// Closing a command is separate from accepting it. Only 817 reaching 7
    /// proves native sign-out completed; an error or timeout retains its result.
    fn close_member(&mut self) -> Option<i32> {
        if !self.signed_in && !self.network_opened { return None; }
        self.signed_in = false;
        self.network_opened = false;
        log!("native POL sign-out starting");
        self.api.call(816, &[1]);
        let started = Instant::now();
        let result = loop {
            let state = self.api.zero(817);
            if state == 7 {
                log!("native POL sign-out completed (state 7)");
                break 0;
            }
            if state < 0 {
                log!("error: native POL sign-out failed (state {state})");
                break state;
            }
            if started.elapsed() >= CLOSE_LIMIT {
                log!("error: native POL sign-out still at state {state} after {}s; closing anyway", CLOSE_LIMIT.as_secs());
                break native_jobs::TIMED_OUT;
            }
            std::thread::sleep(CLOSE_POLL);
        };
        self.api.zero(463);
        self.api.zero(434);
        Some(result)
    }

    /// Observe native snapshots through locked getters only (167/169/206/207/
    /// 838 and friend_locations). Never requests, pumps or consumes
    /// notifications, so it may run during play.
    /// Keep the signed-in owner responsive while another Windower launcher
    /// completes its profile save, before this process calls GameStart.
    pub(crate) fn startup_frame(&mut self) -> bool {
        owner_frame(&self.api, &mut self.pushes)
    }

    pub fn observe_friends(&self) -> Result<FriendObserver, String> {
        let api = self.api.clone();
        let (stop, stopping) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("native-pol-friends".into())
            .spawn(move || run_friend_observer(api, stopping, events::send))
            .map_err(|_| "Could not observe native POL friends.")?;
        Ok(FriendObserver {
            stop,
            thread: Some(thread),
        })
    }
}

/// One Viewer frame for request drivers: window messages, one 818, one frame
/// of sleep. False when the owner must stop: launcher gone or a latched pump
/// error (1103, peeked like the Viewer's frame tail 101984da).
pub(crate) fn owner_frame(api: &Api, pushes: &mut crate::native_social::PushDelivery) -> bool {
    owner_frame_at(api, pushes, FRAME)
}

fn idle_owner_frame(api: &Api, pushes: &mut crate::native_social::PushDelivery, auto_away: &mut AutoAway, status: &mut ViewerStatus, idle: Duration) -> bool {
    owner_frame_with(api, pushes, FRAME, &mut || {
        // Play is pending: finish in-flight work only; no auto-Away, no retry.
        if play_requested() { return status.settle_step(api); }
        NativeSession::auto_away_tick(api, auto_away, status, idle);
        status.step(api);
    })
}

fn owner_frame_at(api: &Api, pushes: &mut crate::native_social::PushDelivery, interval: Duration) -> bool {
    owner_frame_with(api, pushes, interval, &mut || {})
}

// Owner frames run so far on this (the owner) thread; the Viewer's
// frame-counted pauses measure from here.
thread_local! { static OWNER_FRAMES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
pub(crate) fn owner_frames() -> u64 { OWNER_FRAMES.with(|f| f.get()) }
#[cfg(test)]
pub(crate) fn count_owner_frame_for_test() { OWNER_FRAMES.with(|f| f.set(f.get() + 1)); }

fn owner_frame_with(api: &Api, pushes: &mut crate::native_social::PushDelivery, interval: Duration, work: &mut dyn FnMut()) -> bool {
    OWNER_FRAMES.with(|f| f.set(f.get() + 1));
    pump_idle_messages();
    api.zero(818);
    work();
    pushes.flush_pushed(api, api.zero(838) == 3);
    pause_owner(interval);
    !owner_stopping(api)
}

/// The owner must stop: the launcher is gone, a startup sign-out was accepted,
/// or the core latched a pump error (1103). No new request starts then.
pub(crate) fn owner_stopping(api: &Api) -> bool {
    events::pipe_closed() || startup_cancelled() || api.zero(1103) < 0
}

const SIGN_IN_CANCELLED: &str = "PlayOnline sign-in was cancelled.";
const LAUNCHER_CLOSED: &str = "The launcher closed the session during PlayOnline sign-in.";

/// Why a stopped sign-in ended, when it was not a POL failure: a startup
/// sign-out, or a closed launcher (`closed` names that stage).
fn startup_stop(closed: &str) -> Option<String> {
    if startup_cancelled() { Some(SIGN_IN_CANCELLED.into()) }
    else if events::pipe_closed() { Some(closed.into()) }
    else { None }
}

/// The launcher's startup sign-out ended this sign-in (read before the
/// session's Drop finishes startup).
pub(crate) fn sign_in_cancelled() -> bool { startup_cancelled() }

// Initial open/status/list/presence polls, including the wall-clock mail list,
// step one 818 per Viewer frame (07b decision 2; 07a stock-faithful-pacing:
// 1 ms turnarounds are below any stock timing).
const STARTUP_FRAME: Duration = FRAME;
/// The Viewer's screen transitions on a sign-in-only session (07a
/// stock-faithful-pacing), counted in frames like the Viewer counts them
/// (08 §4 item 5): op7 end → 374 (stock 1.05 s ≈ 34 frames of 31.25 ms) and
/// mail response → publish (1.6 s ≈ 51 frames). At FRAME that is ≈0.69 s and
/// ≈1.03 s, both timings a stock Viewer produces (07d §6 S1 D).
const OP7_TO_STATUS_FRAMES: u32 = 34;
const MAIL_TO_PUBLISH_FRAMES: u32 = 51;

/// `limit` has passed since `since`, measured on the wall clock or as `frames`
/// owner frames of FRAME. A real frame never takes less than FRAME, so live
/// this is the wall clock; tests that capture the sleeps count frames.
fn waited(since: Instant, frames: u32, limit: Duration) -> bool {
    since.elapsed() >= limit || FRAME * frames >= limit
}

fn pause_owner(interval: Duration) {
    #[cfg(test)]
    if tests::capture_pause(interval) { return; }
    std::thread::sleep(interval);
}

/// One summary per startup stage, including failed stages. Gaps are between
/// pump entries, so they include scheduler/native work as well as our sleep.
struct StartupCadence {
    stage: &'static str,
    started: Instant,
    previous: Option<Instant>,
    pumps: u64,
    max_gap: Duration,
}
impl StartupCadence {
    fn new(stage: &'static str) -> Self {
        Self { stage, started: Instant::now(), previous: None, pumps: 0, max_gap: Duration::ZERO }
    }
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(previous) = self.previous.replace(now) {
            self.max_gap = self.max_gap.max(now.duration_since(previous));
        }
        self.pumps += 1;
    }
    fn frame(&mut self, api: &Api, pushes: &mut crate::native_social::PushDelivery) -> bool {
        self.tick();
        owner_frame_at(api, pushes, STARTUP_FRAME)
    }
}
impl Drop for StartupCadence {
    fn drop(&mut self) {
        log!("native POL startup cadence: {} interval={}ms pumps={} max_gap={:.2}ms elapsed={:.2}s",
            self.stage, STARTUP_FRAME.as_millis(), self.pumps,
            self.max_gap.as_secs_f64() * 1000.0, self.started.elapsed().as_secs_f64());
    }
}

/// Concurrent requests under the Login policy, as the Viewer's job runner
/// starts several queued jobs in one tick (1003feb5): all start in list order,
/// then each frame polls them in order. A full table restarts the allocation
/// on the next frame; no
/// timeout or retry; only a stopping owner cancels its own pending tickets.
/// Each job writes its own request lines (07d §4.7; attempts include the
/// full-table reissues).
fn run_concurrent(api: &Api, jobs: &[(&'static str, usize, usize)], frame: &mut dyn FnMut() -> bool) -> Vec<i32> {
    let started = Instant::now();
    let mut starts: Vec<_> = jobs.iter().map(|&(_, start, _)| move || api.zero(start)).collect();
    let mut requests: Vec<_> = starts.iter_mut().zip(jobs).map(|(start, &(name, _, poll))|
        Request { name, start, poll, policy: Policy::Login }).collect();
    let mut max_frame = Duration::ZERO;
    let results = native_jobs::run_all(api, &mut requests, &mut || {
        let frame_started = Instant::now();
        let running = frame();
        max_frame = max_frame.max(frame_started.elapsed());
        running
    });
    let took = started.elapsed();
    log!("native POL concurrent loads completed in {:.2}s (longest owner frame {:.0}ms)", took.as_secs_f64(), max_frame.as_secs_f64() * 1000.0);
    // The four lists, start → last end (07d §4.5).
    if jobs.len() == LISTS.len() {
        if let Some(note) = native_jobs::possible_loss("lists", took, native_jobs::CLEAN_LISTS) { log!("{note}"); }
    }
    results
}

/// Keep this STA responsive to COM and injected-tool window messages between
/// games. GameStart owns message dispatch during play. Its WM_QUIT ends that
/// game, while explicit POL sign-out is handled by the session owner above.
pub(crate) fn pump_idle_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT};
    let mut message: MSG = unsafe { std::mem::zeroed() };
    for _ in 0..32 {
        if unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } == 0 { break; }
        if message.message != WM_QUIT {
            unsafe { TranslateMessage(&message); DispatchMessageW(&message); }
        }
    }
}

/// Read the native cache again when its connection returns, even if its bytes
/// match the last connected snapshot. The launcher invalidates presence on loss.
/// Read-only: no request, cancel, pump or notification slot is ever called.
fn run_friend_observer(api: Api, stopping: mpsc::Receiver<()>, send: impl Fn(String)) {
    let mut before = Zeroizing::new(Vec::new());
    let mut locations_before = None;
    let mut game_friends = crate::friend_locations::GameFriends::default();
    let mut location_counts = None;
    let mut own_presence = None;
    let mut health = None;
    let mut portrait_probed = false;
    loop {
        let catalog = crate::native_social::CATALOG_ACCESS.lock().unwrap_or_else(|e| e.into_inner());
        let connected = api.zero(838) == 3;
        let connection_changed = health.replace(connected) != Some(connected);
        if connection_changed {
            send(format!("{{\"type\":\"native_pol_status\",\"code\":{}}}\n", if connected { 0 } else { SESSION_LOST }));
        }
        if connected {
            // Original locked getters; the word-returning class getter
            // leaves its upper return bits unspecified on x86.
            let presence = (api.zero(206), api.zero(207) as u16);
            if own_presence != Some(presence) || connection_changed {
                log!("native POL own presence: mode={} application={}", presence.0, presence.1);
                own_presence = Some(presence);
                send(format!("{{\"type\":\"native_own_presence\",\"mode\":{}}}\n", presence.0));
            }
            let mut blob = Zeroizing::new(vec![0u8; proto::FRIENDS_LEN]);
            blob[..8].copy_from_slice(crate::wire::FRIENDS_MAGIC);
            for index in 0..proto::FRIEND_SLOTS {
                let (getter, index_in_table) = if index < 200 {
                    (167, index)
                } else {
                    (169, index - 200)
                };
                let out = &mut blob[8 + index * proto::FRIEND_RECORD_LEN..]
                    [..proto::FRIEND_RECORD_LEN];
                api.two(getter, index_in_table, out.as_mut_ptr() as usize);
            }
            if !std::mem::replace(&mut portrait_probed, true) {
                log!("{}", crate::native_portrait::probe_friends(&blob));
            }
            let locations = game_friends.snapshot(&blob);
            if *blob != *before || locations != locations_before || connection_changed {
                let active = blob[8..].as_chunks::<{ proto::FRIEND_RECORD_LEN }>().0.iter().filter(|r| {
                    let flags = u64::from_le_bytes(r[8..16].try_into().unwrap());
                    r[0x98] & 1 != 0 && flags & (1 << 11) != 0 && (flags >> 33) & 0x3ff == 1
                }).count();
                let mut counts = [active, 0, 0, 0];
                if let Some(extra) = &locations {
                    for r in extra[8..].as_chunks::<{ crate::friend_locations::RECORD_LEN }>().0 {
                        counts[1] += usize::from(r[0] != 0);
                        counts[2] += usize::from(r[16] != 0);
                        counts[3] += usize::from(r[80] != 0);
                    }
                }
                if location_counts != Some(counts) {
                    log!("native friend locations: active_ffxi={} characters={} areas={} other_worlds={}", counts[0], counts[1], counts[2], counts[3]);
                    location_counts = Some(counts);
                }
                let extra = locations.as_ref().map_or(String::new(), |b| format!(",\"locations\":\"{}\"", b64::encode(b)));
                send(format!(
                    "{{\"type\":\"native_friends\",\"payload\":\"{}\"{extra}}}\n",
                    b64::encode(&blob),
                ));
                before = blob;
                locations_before = locations;
            }
        }
        drop(catalog);
        match stopping.recv_timeout(Duration::from_millis(500)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Drop for NativeSession {
    /// Viewer close (03 §5): synchronous sign-out 101983fb when signed in, then
    /// 101aa2d4 (1502 clear), 1019869a (host unregistration) and 10019464
    /// (920 file worker stop, 915 option.bin mutex). No 852 (B9).
    fn drop(&mut self) {
        // Cancellation stops startup work; teardown still pumps and signs out.
        let cancelled = !finish_startup();
        log!("native POL cleanup: cancelling Viewer status and finishing push delivery");
        self.viewer_status.cancel(&self.api);
        crate::native_social::set_listening(false);
        if self.home {
            // Store (and while still signed in, answer) the last pushes first.
            let receipts = self.signed_in && self.api.zero(838) == 3;
            self.pushes.flush_pushed(&self.api, receipts);
        }
        crate::native_social::discard_pushed();
        let sign_out = self.close_member();
        log!("native POL cleanup: clearing member credentials and host registration");
        if self.home {
            self.api.se(None);
        }
        self.unregister_host();
        if self.files {
            log!("native POL cleanup: stopping file worker");
            self.api.zero(920);
            log!("native POL cleanup: closing options storage");
            self.api.zero(915);
        }
        log!("native POL cleanup finished");
        // A launcher startup sign-out that landed before any Open succeeded has
        // nothing to close: report it as the completed sign-out it asked for,
        // so the launcher records a sign-out rather than a startup exit.
        if let Some(code) = sign_out.or(cancelled.then_some(0)) {
            events::send(format!("{{\"type\":\"native_sign_out_complete\",\"code\":{code}}}\n"));
        }
    }
}

pub struct FriendObserver {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for FriendObserver {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The words for an Open the core failed, naming the stage, because the launcher
/// learns from them (src/launch_log.rs `sign_in_refused`): only the member login
/// (mode 1, after the code is set with 1502) can be a verdict on the account,
/// such as a password or a one-time code it wanted. The network-level Open
/// (mode 0) runs before any credential is sent, and a login cancelled here (too
/// slow, or the launcher went away) never reached a verdict. "POL result {n})"
/// stays in every form: -19 there is the launcher's network failure.
fn open_failure(mode: usize, state: i32, error: i32, cancelled: bool) -> String {
    if cancelled {
        format!("Native PlayOnline login was cancelled at state {state} (POL result {error}).")
    } else if mode == 0 {
        format!("Native PlayOnline network connection failed (state {state}, POL result {error}).")
    } else {
        format!("Native PlayOnline member login failed (state {state}, POL result {error}).")
    }
}

/// The sign-in tick (app 1019ce44): 30 finishes the Open when 838 shows the
/// expected link; negative states fail; 31..38 (and 39) keep polling.
fn connection_result(state: i32, linked: impl FnOnce() -> i32, expected: i32) -> Option<Result<(), ()>> {
    if state == 30 {
        Some(if linked() == expected { Ok(()) } else { Err(()) })
    } else if state < 0 {
        Some(Err(()))
    } else {
        None
    }
}

/// Reserve disjoint POL message stores, without adopting official or foreign
/// folders. The ownership marker is created atomically; no hash-modulo aliasing.
fn claim_home(viewer: &Path, key: &str) -> Result<u32, String> {
    if !viewer.is_absolute() {
        return Err("PlayOnline's message root must be an absolute path.".into());
    }
    if key.len() != 40
        || !key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Invalid native POL profile key.".into());
    }
    let public = viewer.join("pub");
    fs::create_dir_all(&public).map_err(|_| "Could not create PlayOnline message storage.")?;
    let expected = format!("Crystal POL profile v1\n{key}\n");
    let owned = |path: &Path| {
        fs::metadata(path)
            .ok()
            .is_some_and(|m| m.is_file() && m.len() == expected.len() as u64)
            && fs::read_to_string(path).ok().as_deref() == Some(&expected)
    };
    // Prefer an existing owned store even when a lower-numbered store is free.
    let existing = (50..100).find(|&slot| {
        let (home, private) = slot_dirs(viewer, &public, slot);
        owned(&home.join(MARKER)) && (!private.exists() || owned(&private.join(MARKER)))
    });
    let slot = if let Some(slot) = existing {
        slot
    } else {
        // Preserve any uncertain contents; never adopt or recursively delete.
        (50..100).find(|&slot| {
            let (home, private) = slot_dirs(viewer, &public, slot);
            !private.exists() && fs::create_dir(&home).is_ok() && write_marker(&home.join(MARKER), &expected).is_ok()
        }).ok_or("No unused PlayOnline message store is available in home50–home99.")?
    };
    let (home, private) = slot_dirs(viewer, &public, slot);
    if !private.exists() {
        fs::create_dir_all(&private)
            .map_err(|_| "Could not prepare the account's PlayOnline profile folder.")?;
        write_marker(&private.join(MARKER), &expected)
            .map_err(|_| "Could not reserve the account's PlayOnline profile folder.")?;
    } else if !owned(&private.join(MARKER)) {
        return Err("The PlayOnline profile folder belongs to another client.".into());
    }
    for relative in [
        "msg/s/a",
        "msg/s/b",
        "msg/r/a",
        "msg/r/b",
        "mail/s/a",
        "mail/s/b",
        "mail/r/a",
        "mail/r/b",
        "mail/uidl",
        "mail/adr",
        "hidden",
        "log",
        "open/GreetingCards",
        "open/SaveFiles",
        "open/ScreenShots/FinalFantasyXI",
        "open/ContentsData/FinalFantasyXI",
        "open/etc",
    ] {
        fs::create_dir_all(home.join(relative))
            .map_err(|_| "Could not prepare the account's PlayOnline message folders.")?;
    }
    Ok(slot)
}

/// The ownership marker in each claimed message store and profile folder.
const MARKER: &str = ".crystal-profile";

/// (pub\homeNN, usr\homeNN) for a member home.
fn slot_dirs(viewer: &Path, public: &Path, slot: u32) -> (PathBuf, PathBuf) {
    let name = format!("home{slot:02}");
    (public.join(&name), viewer.join("usr").join(name))
}

/// Create the marker atomically (never over an existing file) and flush it.
fn write_marker(path: &Path, expected: &str) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(expected.as_bytes())?;
    file.sync_all()
}

// The viewer's resource IDs, app.dll 10057911..10057a11. Read the installed
// localization instead of substituting approximate message text.
const TITLE_IDS: [u16; 10] = [
    0x46c9, 0x46d0, 0x46d1, 0x46d2, 0x46d3, 0x46d4, 0x46d6, 0x46d8, 0x46dc, 0x46da,
];
const TEXT_IDS: [u16; 10] = [
    0x46e6, 0x46ea, 0x46ec, 0x4ab8, 0x3bad, 0x46d7, 0x46d9, 0x46dd, 0x46db, 0x46d5,
];

/// A notification string as the Viewer hands it to the core: 1302 texts lose
/// every CR first (app 10057a46); 1072 titles are passed unchanged.
fn viewer_text(setter: usize, mut text: Vec<u8>) -> Vec<u8> {
    if setter == 1302 {
        text.retain(|&b| b != b'\r');
    }
    text
}

fn install_message_texts(api: &Api, viewer: &Path) -> Result<(), String> {
    let path = viewer.join("viewer/data/common/StringTable.bin");
    let size = fs::metadata(&path)
        .map_err(|_| "PlayOnline's message localization is missing.")?
        .len();
    if size > 2_000_000 {
        return Err("PlayOnline's message localization is too large.".into());
    }
    let raw = fs::read(path).map_err(|_| "Could not read PlayOnline's message localization.")?;
    let decoded = decode_pex(&raw).ok_or("PlayOnline's message localization is invalid.")?;
    for (setter, ids, max) in [(1072, &TITLE_IDS, 127), (1302, &TEXT_IDS, 255)] {
        for (index, &id) in ids.iter().enumerate() {
            let text = resource_text(&decoded, id)
                .ok_or("PlayOnline's message localization is incomplete or unsupported.")?;
            let mut terminated = viewer_text(setter, text);
            if terminated.len() > max {
                return Err(
                    "PlayOnline's message localization exceeds its native field size.".into(),
                );
            }
            terminated.push(0);
            api.two(setter, index, terminated.as_ptr() as usize);
        }
    }
    Ok(())
}

/// Installed-core ABI check: host registration, the real profile list (read
/// only), member select and connection choice with synthetic credentials.
/// Never calls 814/815/816/818 or any request; nothing reaches the network.
/// # Safety
/// `table` is an installed, verified core's live table at `base`.
#[cfg(test)]
pub unsafe fn smoke_prepare(table: &[usize], base: usize, viewer: &Path) {
    // The internal-global assertions below were read from the 1.18.15e core
    // by hand; other builds run the same calls without them. Production
    // never reads a core global.
    let legacy = crate::pol_core::Layout::live(base).is_ok_and(|l| l.timestamp == 0x4E4E_5917);
    let at = |offset: usize| (base + offset) as *const usize;
    let mut session = NativeSession::new(Api(table.into()), viewer, 1, 10);
    // Reproduce the injector's setting in this test's own core, then restore
    // the stock value through the production function. No remote operation.
    let before = table.to_vec();
    session.api.f::<unsafe extern "C" fn(i32)>(1075, 1)(1);
    assert_eq!(session.api.zero(1074), 1);
    session.api.restore_viewer_policy().unwrap();
    assert_eq!(session.api.zero(1074), 0);
    session.api.restore_viewer_policy().unwrap();
    assert_eq!(session.api.zero(1074), 0);
    assert_eq!(table, before, "restoring native policy must preserve all callbacks");
    install_message_texts(&session.api, viewer).unwrap();
    // FFXi.dll/FFXiMain read their language from 421, which falls back to the
    // 1528 value while 420 is unset and caches its first answer. Run the
    // production host calls (125's path code reads 421 itself), then check
    // FFXI would get English for the US Viewer.
    // The core keeps these pointers, as production's leaked strings.
    let root: &'static CStr = Box::leak(std::ffi::CString::new(viewer.to_str().unwrap()).unwrap().into_boxed_c_str());
    let key: &'static CStr = Box::leak(std::ffi::CString::new("SOFTWARE\\PlayOnlineUS").unwrap().into_boxed_c_str());
    session.host_paths(root, key);
    assert_eq!(session.api.zero(421), 1, "FFXI must start in English for the US Viewer");

    // Host registration as app 1019b874, the installed inet_w.bin through
    // 825/826 and its 884 validation, then the built-in default for comparison.
    session.register_callbacks();
    if legacy { assert_eq!(*at(0x99424), table[334], "871 routes pushes to the core dispatcher"); }
    session.api.call(819, &[0]);
    session.api.call(824, &[malloc as *const () as usize, free as *const () as usize, realloc as *const () as usize]);
    session.registered = true;
    let file = native_profile::read_file(viewer).expect("installed usr\\all\\inet_w.bin");
    let (header, from_file) = native_profile::load(&session.api, Some(&file)).unwrap();
    assert!(from_file, "the installed profile file must pass the Viewer's validation");
    if legacy { assert_eq!(*at(0x9926c), header); }
    let snapshot = |api: &Api, header: usize| {
        let record = api.word(849, &[0]);
        let mut bytes = std::slice::from_raw_parts(header as *const u8, native_profile::HEADER_LEN).to_vec();
        bytes.extend_from_slice(std::slice::from_raw_parts(record as *const u8, native_profile::RECORD_LEN));
        bytes
    };
    let installed = snapshot(&session.api, header);
    session.api.zero(828);
    let (default_header, from_file) = native_profile::load(&session.api, None).unwrap();
    assert!(!from_file);
    let built = snapshot(&session.api, default_header);
    assert_eq!(installed[..native_profile::HEADER_LEN], built[..native_profile::HEADER_LEN], "default header");
    assert_eq!(installed[native_profile::HEADER_LEN..], built[native_profile::HEADER_LEN..], "default WindowsNetWork record");
    session.profiles = default_header;
    session.api.call(1149, &[no_inactivity_warning as *const () as usize, no_inactivity_expiry as *const () as usize]);

    // Member select (stock failure), then the connect screen's profile load.
    let credentials = NativeCredentials {
        pol_id: *b"ABCD1234\0",
        pol_password: *b"fake password\0\0\0",
        se_id: *b"synthetic\0\0\0\0\0\0\0\0",
        se_secret: [0x51; 20],
        otp: Some(*b"123456"),
        profile_key: "a".repeat(40),
        handle_names: String::new(),
    };
    session.select_member(50, &credentials);
    if legacy { assert_eq!(*(at(0x992e0) as *const u32), 50); }
    assert_eq!(session.api.zero(891), native_profile::PROFILE_NOT_FOUND, "Viewer members name no profile");
    let name = native_profile::selected_name(&session.api, session.profiles).unwrap();
    native_profile::connect(&session.api, &name).unwrap();
    if legacy { assert_eq!(*(at(0x99c80) as *const i32), 1, "connection kind LAN"); }
    assert_eq!(*(session.api.word(835, &[]) as *const i32), 1, "Open argument kind LAN");
    session.api.se(Some(&credentials));
    if legacy {
        assert_eq!(*(at(0xaaa98) as *const u32), 2, "native SE + OTP mode");
        assert_eq!(std::slice::from_raw_parts(at(0xaaaad) as *const u8, 20), &[0xae; 20], "native transformed secret retained");
    }
    assert_ne!(session.api.zero(838), 3);
    // Close teardown without a member session: 1502 clear, 828/824/819/872/1149.
    session.home = true;
    session.files = false; // smoke_message_files owns this test's file worker
    drop(session);
    if legacy {
        assert_eq!(*(at(0xaaa98) as *const u32), 0, "native credentials cleared");
        assert_eq!(*at(0x9926c), 0, "profile list freed");
        assert_eq!(*at(0x9925c), 0, "host heap unregistered");
        assert_eq!(*at(0x99424), table[334], "872 restores the core's defaults");
    }
}

/// # Safety
/// `table` is an installed, verified core's live table at `base`.
#[cfg(test)]
pub unsafe fn smoke_message_files(table: &[usize]) {
    let api = Api(table.into());
    let root = std::env::temp_dir().join(format!(
        "crystal-native-mail-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let slot = claim_home(&root, &"a".repeat(40)).unwrap();
    let root_c = std::ffi::CString::new(root.to_str().unwrap()).unwrap();
    api.one(125, root_c.as_ptr() as usize);
    api.one(870, slot as usize);
    api.zero(932);
    let mut directory = [0u8; 512];
    let get_path = api.f::<unsafe extern "C" fn(i32, *mut u8, i32, i32) -> i32>(126, 4);
    let length = get_path(6, directory.as_mut_ptr(), 512, 0);
    assert!(length > 0 && length < 512);
    let directory =
        std::path::PathBuf::from(std::str::from_utf8(&directory[..length as usize]).unwrap());
    assert_eq!(directory, root.join(format!("pub/home{slot:02}/msg/s/a")));
    let path = directory.join("offline-test.bin");
    let file = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    let open = api.f::<unsafe extern "C" fn(*const u8, u32) -> i32>(921, 2);
    let ticket = open(file.as_ptr().cast(), 0x0e); // FFXI's failing friend-request mode
    assert!(ticket >= 0, "native message-file open failed: {ticket}");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = api.one(922, ticket as usize);
    while ready == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        ready = api.one(922, ticket as usize);
    }
    assert_eq!(ready, 1, "native message-file open completion");
    let bytes = b"Crystal native message storage check";
    let write = api.f::<unsafe extern "C" fn(i32, *const u8, u32) -> i32>(928, 3);
    assert!(write(ticket, bytes.as_ptr(), bytes.len() as u32) >= 0);
    let poll = api.f::<unsafe extern "C" fn(i32, *mut u32) -> i32>(929, 2);
    let mut written = 0;
    let mut ready = poll(ticket, &mut written);
    while ready == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        ready = poll(ticket, &mut written);
    }
    assert_eq!(ready, 1, "native message-file write completion");
    assert_eq!(written, bytes.len() as u32);
    api.one(923, ticket as usize);
    api.zero(920); // the Viewer's close stop (= 933); 923 only enqueues its close
    crate::native_social::smoke_read_file(table, file.as_bytes_with_nul(), bytes);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    // Test-owned absolute temporary root only; contains no account data.
    fs::remove_dir_all(root).unwrap();
}

fn decode_pex(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < 16 || &raw[..4] != b"PEX\0" {
        return None;
    }
    let n = u32::from_le_bytes(raw[4..8].try_into().ok()?) as usize;
    if !(16..=2_000_000).contains(&n) {
        return None;
    }
    let mut bit = 128usize;
    let mut bits = |count: usize| -> Option<usize> {
        let mut value = 0;
        for shift in 0..count {
            value |= usize::from((raw.get(bit / 8)? >> (bit % 8)) & 1) << shift;
            bit += 1;
        }
        Some(value)
    };
    let mut out = vec![0; 4096];
    out.reserve(n);
    while out.len() < 4096 + n {
        if bits(1)? == 0 {
            out.push(bits(8)? as u8);
        } else {
            let offset = bits(12)?;
            let length = bits(4)? + 2;
            let start = out.len() - 4096 + offset;
            if start >= out.len() || out.len() + length > 4096 + n {
                return None;
            }
            for index in 0..length {
                out.push(*out.get(start + index)?);
            }
        }
    }
    Some(out.split_off(4096))
}

fn resource_text(decoded: &[u8], id: u16) -> Option<Vec<u8>> {
    let mut at = 16;
    while at + 4 <= decoded.len() {
        let size = u16::from_le_bytes(decoded[at..at + 2].try_into().ok()?) as usize;
        let key = u16::from_le_bytes(decoded[at + 2..at + 4].try_into().ok()?);
        if size < 4 || !size.is_multiple_of(2) || at.checked_add(size)? > decoded.len() {
            return None;
        }
        if key == id {
            let units: Vec<u16> = decoded[at + 4..at + size]
                .as_chunks::<2>().0.iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect();
            let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
            // Verified US templates are ASCII. Refuse unsupported encodings rather
            // than corrupting a native sprintf template or replacing its text.
            if units[..end].iter().any(|u| *u > 127) {
                return None;
            }
            return Some(units[..end].iter().map(|u| *u as u8).collect());
        }
        at += size;
    }
    None
}

#[cfg(test)]
mod tests;
