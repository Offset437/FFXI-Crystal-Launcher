//! Live controller changes run in FFXI's verified keyboard frame. The stock
//! config-tool sequence is suspend background input, reload, then resume it.
//! The following stock pad frame releases/recreates the device when required.
//! A result is sent only after a later frame observes the new values and the
//! recreation flag cleared. No external memory writes or synthetic keys.

use std::{sync::{mpsc, Mutex}, thread, time::{Duration, Instant}};
use windows_sys::Win32::{
    Foundation::HWND,
    System::{SystemInformation::GetTickCount64,
        Threading::GetCurrentThreadId},
};
use crate::{ffxi_settings::{FfxiSettings, SettingValue}, game_layout::{controls::{self, CONFIG_MEMBER, CONFIG_READY, CONFIG_SIZE, CONTROLLER_CODE, DELIMITERS, SLOT_MAP}, Controller, Resolution}, log, sigscan::Pattern};

const LIMIT: Duration = Duration::from_secs(4);
const UNAVAILABLE: &str = "Live controller settings are unavailable for this game session; saved changes apply after restarting it.";
const EXPIRED: &str = "The controller update could not be confirmed before the request expired; saved changes apply after restarting the game.";
const EXITED: &str = "The game closed before the controller update could be confirmed.";
// game_layout's controller group finds every function, global, table and
// vtable below by signature (any client build). Reload takes (this, initial:
// u32) and returns with ret 4; suspend takes (keyboard, suspended: u32), also
// ret 4. The native WndProc calls reload with false; config launch/finish
// call suspend. The entire parsers and pad recreation branch are re-checked
// live before each use, not just their prologues: their global addresses and
// field offsets are used below.

use crate::mem::{read, read_into as read_bytes};

fn pointer(address: usize) -> Option<usize> { crate::mem::pointer(address, 0x10000) }

/// Re-reads every scanned controller site through `read` (RVA reads of the
/// module at `base`): each locator's signature, the operands this module
/// relies on relocated to `base`, the call targets, and the parser tables.
fn code_valid(base: usize, c: &Controller, mut read: impl FnMut(usize, &mut [u8]) -> bool) -> bool {
    let mut code = Vec::new();
    for (name, &rva) in CONTROLLER_CODE.iter().zip(&c.code) {
        let Some(pattern) = controls::pattern(name).and_then(Pattern::parse) else { return false; };
        let mut bytes = vec![0u8; pattern.len()];
        if !read(rva, &mut bytes) || !pattern.matches(&bytes, 0) { return false; }
        code.push(bytes);
    }
    let [reload, slots, mode, _, _, _, _, _, _, tool_finish, factory, ready, _, tool_frame, tool_suspend] = &code[..] else { return false; };
    let operand = |bytes: &[u8], at: usize, rva: usize| crate::sigscan::operand_at(base, bytes, at, rva);
    let call = |bytes: &[u8], at: usize, site: usize, target: usize| crate::auto_login::call_lands(bytes, site, at, target);
    let (finish_rva, ready_rva, suspend_rva) = (c.code[9], c.code[11], c.code[14]);
    // The two parser tables the binding parser's operands point at.
    let target = |bytes: &[u8], at: usize| bytes.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes)
        .and_then(|va| (va as usize).checked_sub(base));
    let mut map = [0u8; SLOT_MAP.len() * 4];
    let mut delimiters = [0u8; 3];
    let tables = target(slots, 0x4C).is_some_and(|rva| read(rva, &mut map))
        && target(slots, 0x27).is_some_and(|rva| read(rva, &mut delimiters))
        && map.chunks(4).zip(SLOT_MAP).all(|(bytes, want)| bytes == want.to_le_bytes())
        && delimiters[..2] == *DELIMITERS && delimiters[2] == 0;
    tables
        && operand(reload, 0x66, c.keyboard) && operand(reload, 0xC5, c.flags) && operand(slots, 0x33, c.bindings)
        && operand(mode, 0x11C, c.guid) && operand(tool_frame, 2, c.config_tool) && operand(factory, 0x1B, c.config)
        && operand(tool_suspend, 2, c.keyboard)
        && call(tool_finish, 0x5E, finish_rva, c.suspend) && call(ready, 0xC, ready_rva, c.reload)
        && call(tool_suspend, 0xC, suspend_rva, c.suspend)
}

#[derive(Clone, Copy)]
struct Layout { base: usize, window: usize, controller: Controller }
impl Layout {
    /// The scanned groups this needs, then a live re-check. Every refusal
    /// names its reason (logged by the caller); the user sees UNAVAILABLE.
    fn resolve(resolution: &Resolution) -> Result<Self, String> {
        let group = |what: &str, reason: &String| format!("{what} unavailable: {reason}");
        let controller = *resolution.controller.as_ref().map_err(|e| group("controller code", e))?;
        let window = resolution.menus.as_ref().map_err(|e| group("login menus (game window)", e))?.window;
        let layout = Self { base: resolution.base, window, controller };
        layout.verify().then_some(layout).ok_or_else(|| "the scanned controller code differs in live memory".to_string())
    }

    fn verify(self) -> bool {
        code_valid(self.base, &self.controller, |rva, out| read_bytes(self.base + rva, out))
    }

    fn context(self, data: *mut u8) -> Option<Context> {
        let keyboard = crate::auto_login::frame_keyboard(self.base, self.controller.keyboard, data)?;
        let window = read::<u32>(self.base + self.window)? as usize as HWND;
        crate::auto_login::owned_window(window, unsafe { GetCurrentThreadId() })
            .then_some(Context { keyboard, window: window as usize })
    }

    fn flags(self) -> Option<[u8; 15]> { read(self.base + self.controller.flags) }

    fn applied(self, expected: &Expected) -> bool {
        let Some(flags) = self.flags() else { return false; };
        let Some(bindings) = read::<[i8; 32]>(self.base + self.controller.bindings) else { return false; };
        let Some(guid) = read::<[u8; 16]>(self.base + self.controller.guid) else { return false; };
        expected.matches(&flags, &bindings, &guid)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Context { keyboard: usize, window: usize }

#[derive(Clone, Debug, PartialEq, Eq)]
struct Expected { mode: [u8; 6], bindings: Vec<i8>, guid: Option<[u8; 16]> }
impl Expected {
    fn parse(settings: &FfxiSettings) -> Result<Self, &'static str> {
        const INVALID: &str = "The live request contains invalid controller settings.";
        if settings.names().any(|name| !crate::ffxi_settings::is_controller_key(name)) {
            return Err(INVALID);
        }
        let text = |name| match settings.get(name) { Some(SettingValue::Text(value)) if value.is_ascii() && value.len() < 256 => Ok(value.as_str()), _ => Err(INVALID) };
        let list = |value: &str| -> Result<Vec<i32>, &'static str> {
            value.split(',').map(|part| part.trim().parse::<i32>().map_err(|_| INVALID)).collect()
        };
        let mode = list(text("padmode000")?)?;
        let bindings = list(text("padsin000")?)?;
        if !(6..=32).contains(&mode.len()) || mode.iter().any(|flag| !(0..=1).contains(flag))
            || !(27..=32).contains(&bindings.len()) || bindings.iter().any(|id| !(-41..=41).contains(id)) {
            return Err(INVALID);
        }
        let guid = if settings.get("padguid000").is_some() {
            Some(parse_guid(text("padguid000")?).ok_or(INVALID)?)
        } else { None };
        Ok(Self { mode: std::array::from_fn(|index| mode[index] as u8),
            bindings: bindings.into_iter().map(|id| id as i8).collect(), guid })
    }

    fn matches(&self, flags: &[u8; 15], bindings: &[i8; 32], guid: &[u8; 16]) -> bool {
        // Mode order: enabled, rumble, sliders, hat, background, XInput.
        [0, 5, 2, 1, 3, 4].into_iter().enumerate().all(|(i, offset)| flags[offset] == self.mode[i])
            && flags[12] == 0 && flags[14] == 0
            // Stock skips binding parsing while the pad is disabled. The
            // retained process-local string is read when it is enabled again.
            && (self.mode[0] == 0 || bindings[..self.bindings.len()] == self.bindings)
            && self.guid.as_ref().is_none_or(|expected| expected == guid)
    }
}

fn parse_guid(text: &str) -> Option<[u8; 16]> {
    // The stock 19060 reader calls CLSIDFromString, which requires braces.
    let text = text.strip_prefix('{')?.strip_suffix('}')?;
    if text.len() != 36 || [8, 13, 18, 23].iter().any(|&i| text.as_bytes()[i] != b'-') { return None; }
    let hex: String = text.chars().filter(|&ch| ch != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) { return None; }
    let mut out = [0u8; 16];
    for (i, value) in out.iter_mut().enumerate() { *value = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?; }
    out[..4].reverse(); out[4..6].reverse(); out[6..8].reverse();
    Some(out)
}

fn transition(flags: &[u8; 15], expected: &Expected, stock_config: u8) -> Result<(), &'static str> {
    if [0, 1, 2, 3, 4, 5, 12, 13, 14].iter().any(|&offset| flags[offset] > 1) { return Err(UNAVAILABLE); }
    if stock_config > 1 { return Err(UNAVAILABLE); }
    if stock_config != 0 || flags[14] != 0 { return Err("Close the game's controller configuration window before applying live changes."); }
    if flags[4] != 0 && expected.mode[5] == 0 {
        // 83D0 sets +C9A when XInput is enabled, but never clears it when
        // disabled. Reusing that path would keep detecting the pad as XInput.
        return Err("Turning XInput off requires restarting the game; the new setting has been saved.");
    }
    Ok(())
}

struct Request { id: u64, settings: FfxiSettings, expected: Expected, expires: Instant, deadline_ms: u64 }
impl Request {
    fn new(id: u64, settings: FfxiSettings, deadline_ms: u64, now: Instant, ticks: u64) -> Result<Self, &'static str> {
        if deadline_ms <= ticks { return Err(EXPIRED); }
        let expected = Expected::parse(&settings)?;
        let duration = LIMIT.min(Duration::from_millis(deadline_ms - ticks));
        Ok(Self { id, settings, expected, expires: now + duration, deadline_ms })
    }
    fn expired(&self, now: Instant, ticks: u64) -> bool { self.expires <= now || self.deadline_ms <= ticks }
}
struct Waiting { request: Request, context: Context }
#[derive(Default)]
struct State { layout: Option<Layout>, unsupported: bool, pending: Option<Request>, waiting: Option<Waiting> }
static STATE: Mutex<Option<State>> = Mutex::new(None);

fn result(id: u64, value: Result<(), &'static str>) {
    match value {
        Ok(()) => crate::events::send(crate::proto::controller_result(id, true, "")),
        Err(error) => crate::events::send(crate::proto::controller_result(id, false, error)),
    }
}

impl State {
    fn cancel(&mut self, error: &'static str) {
        if let Some(request) = self.pending.take() { result(request.id, Err(error)); }
        if let Some(waiting) = self.waiting.take() { result(waiting.request.id, Err(error)); }
    }
    fn expire(&mut self, now: Instant, ticks: u64) {
        if self.pending.as_ref().is_some_and(|request| request.expired(now, ticks)) {
            result(self.pending.take().unwrap().id, Err(EXPIRED));
        }
        if self.waiting.as_ref().is_some_and(|waiting| waiting.request.expired(now, ticks)) {
            result(self.waiting.take().unwrap().request.id, Err(EXPIRED));
        }
    }
}

/// Called by the pipe reader. One pending update is retained, so dragging or
/// repeated edits cannot build an unbounded queue of stale game mutations.
pub(crate) fn submit(id: u64, settings: FfxiSettings, deadline_ms: u64) {
    let request = match Request::new(id, settings, deadline_ms, Instant::now(), unsafe { GetTickCount64() }) {
        Ok(value) => value, Err(error) => { result(id, Err(error)); return; }
    };
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut().filter(|state| !state.unsupported) else { result(id, Err(UNAVAILABLE)); return; };
    if let Some(old) = state.pending.replace(request) {
        result(old.id, Err("A newer controller change replaced this request."));
    }
}

/// The shared GetKeyboardState wrapper calls this after the real API, before
/// the stock pad frame. Wrong buffers/threads never consume a request.
pub(crate) fn on_input(data: *mut u8) {
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else { return; };
    if state.pending.is_none() && state.waiting.is_none() { return; }
    if crate::events::pipe_closed() { state.cancel(EXITED); return; }
    let Some(layout) = state.layout else { return; };
    let Some(context) = layout.context(data) else { return; };
    state.expire(Instant::now(), unsafe { GetTickCount64() });
    if let Some(waiting) = state.waiting.as_ref() {
        if waiting.context != context {
            result(state.waiting.take().unwrap().request.id, Err(EXITED));
        } else if layout.applied(&waiting.request.expected) {
            result(state.waiting.take().unwrap().request.id, Ok(()));
        } else {
            return;
        }
    }
    let Some(request) = state.pending.take() else { return; };
    match apply(layout, context, &request) {
        Ok(()) => state.waiting = Some(Waiting { request, context }),
        Err(error) => result(request.id, Err(error)),
    }
}

fn apply(layout: Layout, context: Context, request: &Request) -> Result<(), &'static str> {
    if request.expired(Instant::now(), unsafe { GetTickCount64() }) { return Err(EXPIRED); }
    if !layout.verify() { return Err(UNAVAILABLE); }
    let flags = layout.flags().ok_or(UNAVAILABLE)?;
    // The config UI sets +4 before launching its asynchronous worker, which
    // suspends input later. Checking only the suspended flag would race it.
    let stock_config = pointer(layout.base + layout.controller.config_tool).and_then(|object| read::<u8>(object + 4)).ok_or(UNAVAILABLE)?;
    transition(&flags, &request.expected, stock_config)?;
    let config = pointer(layout.base + layout.controller.config).ok_or(UNAVAILABLE)?;
    // Exact constructor/registry ownership and object extents. The keyboard
    // pointer is also the currently executing frame's +298 buffer owner.
    let c = layout.controller;
    if read::<u32>(config) != Some((layout.base + c.config_vtable) as u32)
        || read::<u32>(config + CONFIG_MEMBER) != Some((layout.base + c.member_vtable) as u32)
        || read::<u32>(config + CONFIG_READY) != Some(1)
        || read::<[u8; CONFIG_SIZE]>(config).is_none()
        || read::<[u8; 0xDC4]>(context.keyboard).is_none()
        || pointer(layout.base + c.keyboard) != Some(context.keyboard) {
        return Err(UNAVAILABLE);
    }
    let key = read::<u32>(config + 0x90).filter(|key| *key != 0).ok_or(UNAVAILABLE)? as usize;
    #[cfg(target_arch = "x86")]
    {
        // The sender's deadline uses the same Windows boot clock in both
        // processes. Recheck after validation, immediately before mutation.
        if request.expired(Instant::now(), unsafe { GetTickCount64() }) { return Err(EXPIRED); }
        crate::ffxi_settings::update_controller_values(&request.settings, key)?;
        type Method = unsafe extern "thiscall" fn(usize, u32);
        // Exact bodies/ret 4 ABIs are checked immediately above. These are
        // the game's own official config calls on the current input thread.
        unsafe {
            let suspend: Method = std::mem::transmute(layout.base + c.suspend);
            let reload: Method = std::mem::transmute(layout.base + c.reload);
            suspend(context.keyboard, 1);
            reload(config, 0);
            suspend(context.keyboard, 0);
        }
        Ok(())
    }
    #[cfg(not(target_arch = "x86"))]
    { let _ = (key, request); Err(UNAVAILABLE) }
}

pub(crate) struct Worker { stop: mpsc::Sender<()>, thread: Option<thread::JoinHandle<()>> }
impl Worker {
    pub(crate) fn start() -> Option<Self> {
        if !cfg!(target_arch = "x86") || !crate::ffxi_settings::active() { result(0, Err(UNAVAILABLE)); return None; }
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() { return None; }
        *guard = Some(State::default());
        let (stop, receive) = mpsc::channel();
        let spawned = thread::Builder::new().name("crystal-controller-settings".into()).spawn(move || {
            let mut hook = None;
            loop {
                if receive.recv_timeout(Duration::from_millis(100)) != Err(mpsc::RecvTimeoutError::Timeout) { break; }
                let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
                let Some(state) = guard.as_mut() else { break; };
                if crate::events::pipe_closed() { state.cancel(EXITED); break; }
                state.expire(Instant::now(), unsafe { GetTickCount64() });
                if state.layout.is_some() || state.unsupported { continue; }
                drop(guard);
                let Some(resolution) = crate::game_layout::ffximain() else { continue; };
                if !crate::ffxi_settings::controller_ready() { continue; }
                let prepared = Layout::resolve(resolution).and_then(|layout| {
                    crate::auto_login::input_hook(resolution).map(|hook| (layout, hook))
                        .map_err(|reason| format!("game input could not be prepared: {reason}"))
                });
                let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
                let Some(state) = guard.as_mut() else { break; };
                match prepared {
                    Ok((layout, lease)) => { state.layout = Some(layout); hook = Some(lease); result(0, Ok(())); }
                    Err(reason) => {
                        state.unsupported = true;
                        state.cancel(UNAVAILABLE);
                        result(0, Err(UNAVAILABLE));
                        log!("note: live controller settings unavailable: {reason}");
                    }
                }
            }
            drop(hook);
        });
        match spawned {
            Ok(thread) => Some(Self { stop, thread: Some(thread) }),
            Err(_) => { *guard = None; result(0, Err(UNAVAILABLE)); None }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Cancel synchronously before waiting for the worker. A callback that
        // already holds the lock completes first; no callback can use retired
        // settings after this point, including on another GameStart cycle.
        if let Some(mut state) = STATE.lock().unwrap_or_else(|e| e.into_inner()).take() { state.cancel(EXITED); }
        result(0, Err(EXITED));
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: &str, bindings: &str) -> FfxiSettings {
        FfxiSettings::new(vec![("padmode000".into(), SettingValue::Text(mode.into())),
            ("padsin000".into(), SettingValue::Text(bindings.into()))]).unwrap()
    }
    fn bindings(count: usize, id: i32) -> String { vec![id.to_string(); count].join(",") }

    #[test]
    fn native_parser_preserves_extended_slots_and_rejects_unsafe_raw_values() {
        let extra = bindings(28, -41);
        assert_eq!(Expected::parse(&settings("1,1,0,0,1,0", &extra)).unwrap().bindings, vec![-41; 28]);
        assert!(Expected::parse(&settings("1,1,0,0,1,0,1,0", &bindings(32, 41))).is_ok());
        assert!(Expected::parse(&settings("1,1,0,0,1,0,2", &extra)).is_err(), "unknown mode tail is not silently dropped");
        assert!(Expected::parse(&settings("1,1,0,0,1,0", &format!("{},42", bindings(27, 1)))).is_err(),
            "unsafe binding tail is preserved by the launcher but never passed to the native parser");
        for value in [bindings(26, 0), bindings(33, 0), bindings(27, 128), bindings(27, i32::MIN), format!("{},", bindings(27, 1))] {
            assert!(Expected::parse(&settings("1,1,0,0,1,0", &value)).is_err());
        }
        assert!(Expected::parse(&settings("1,1,0,0,2,0", &extra)).is_err());
        let bad = FfxiSettings::new(vec![("0001".into(), SettingValue::Dword(1920))]).unwrap();
        assert!(Expected::parse(&bad).is_err());
        assert_eq!(parse_guid("{00112233-4455-6677-8899-AABBCCDDEEFF}"),
            Some([0x33,0x22,0x11,0,0x55,0x44,0x77,0x66,0x88,0x99,0xAA,0xBB,0xCC,0xDD,0xEE,0xFF]));
        assert!(parse_guid("00112233-4455-6677-8899-AABBCCDDEEFF").is_none());
        assert!(parse_guid("bad").is_none());
    }

    #[test]
    fn acknowledgement_requires_background_restored_and_recreation_consumed() {
        let expected = Expected::parse(&settings("1,1,0,0,1,1", &bindings(27, 3))).unwrap();
        let mut flags = [0u8; 15];
        for offset in [0, 3, 4, 5, 13] { flags[offset] = 1; }
        assert!(expected.matches(&flags, &[3;32], &[0;16]));
        flags[12] = 1;
        assert!(!expected.matches(&flags, &[3;32], &[0;16]), "setter return is not a completed recreation");
        flags[12] = 0; flags[3] = 0;
        assert!(!expected.matches(&flags, &[3;32], &[0;16]), "reload alone leaves background disabled");
        flags[3] = 1; flags[14] = 1;
        assert!(!expected.matches(&flags, &[3;32], &[0;16]));
        flags[14] = 0;
        assert!(!expected.matches(&flags, &[4;32], &[0;16]));
    }

    #[test]
    fn unsafe_xinput_disable_and_an_open_stock_config_window_are_refused() {
        let direct = Expected::parse(&settings("1,1,0,0,1,0", &bindings(27, 3))).unwrap();
        let mut flags = [0u8;15]; flags[4] = 1;
        assert!(transition(&flags, &direct, 0).unwrap_err().contains("restarting"));
        flags[4] = 0; flags[14] = 1;
        assert!(transition(&flags, &direct, 0).unwrap_err().contains("configuration window"));
        flags[14] = 0;
        assert!(transition(&flags, &direct, 1).unwrap_err().contains("configuration window"),
            "the stock worker may still be starting before it suspends input");
        assert!(transition(&flags, &direct, 0).is_ok());
    }

    #[test]
    fn expired_or_retired_requests_cannot_apply_on_a_later_frame() {
        let config = settings("1,1,0,0,0,1", &bindings(27, 3));
        let request = |id| Request { id, settings: config.clone(), expected: Expected::parse(&config).unwrap(), expires: Instant::now(), deadline_ms: u64::MAX };
        let mut state = State { pending: Some(request(1)), waiting: Some(Waiting { request: request(2), context: Context { keyboard: 1, window: 2 } }), ..State::default() };
        let events = crate::events::capture(|| state.expire(Instant::now() + LIMIT, 0));
        assert!(state.pending.is_none() && state.waiting.is_none());
        assert_eq!(events.len(), 2);
        state.pending = Some(request(3));
        let events = crate::events::capture(|| state.cancel(EXITED));
        assert!(state.pending.is_none());
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn sender_deadline_is_not_extended_when_a_queued_request_arrives_late() {
        let config = settings("1,1,0,0,0,1", &bindings(27, 3));
        let now = Instant::now();
        assert!(matches!(Request::new(1, config.clone(), 1000, now, 1000), Err(EXPIRED)));
        let request = Request::new(2, config, 1500, now, 1000).unwrap();
        assert!(request.expired(now, 1500), "elapsed IPC time is not a new four-second lease");
        assert_eq!(request.expires, now + Duration::from_millis(500));
        let layout = Layout { base: 0, window: 0, controller: crate::game_layout::BUILD_6AC37846.controller };
        let request = Request { deadline_ms: 0, ..request };
        assert_eq!(apply(layout, Context { keyboard: 0, window: 0 }, &request), Err(EXPIRED),
            "an expired request returns before any game read, registry mutation or native call");
    }

    #[test]
    #[ignore = "reads the two local unpacked FFXiMain images; no game launch"]
    fn controller_code_matches_both_clients_and_rejects_changed_code() {
        let Some(root) = std::env::var_os("CRYSTAL_FFXIMAIN_DUMPS").map(std::path::PathBuf::from) else { return; };
        for (name, known) in [("6A995428", &crate::game_layout::BUILD_6A995428), ("6AC37846", &crate::game_layout::BUILD_6AC37846)] {
            let bytes = std::fs::read(root.join(format!("FFXiMain-{name}.img"))).unwrap();
            let c = known.controller;
            let check = |image: &[u8], base: usize| code_valid(base, &c, |rva, out| {
                let Some(source) = image.get(rva..rva + out.len()) else { return false; };
                out.copy_from_slice(source);
                true
            });
            // The dump is relocated for its preferred base.
            assert!(check(&bytes, 0x1000_0000), "{name}");
            assert!(!check(&bytes, 0x1800_0000), "{name}: operands are relocated for another base");
            for (what, at) in [("suspend's ret 4", c.suspend + 0x51), ("a reload stack displacement", c.reload + 2),
                ("the slot map", c.code[1] + 0x4C)] {
                let mut changed = bytes.clone();
                if what == "the slot map" {
                    let map = u32::from_le_bytes(changed[at..at + 4].try_into().unwrap()) as usize - 0x1000_0000;
                    changed[map] ^= 1;
                } else {
                    changed[at] ^= 1;
                }
                assert!(!check(&changed, 0x1000_0000), "{name}: changed {what} is refused");
            }
        }
    }

    #[test]
    #[ignore = "requires CRYSTAL_AUTO_LOGIN_DLL; loads DLL only, never starts FFXI"]
    fn installed_game_controller_layout_is_verified_without_starting_game() {
        use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
        let path = std::env::var("CRYSTAL_AUTO_LOGIN_DLL").unwrap();
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let base = unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) } as usize;
        assert_ne!(base, 0);
        let resolution = crate::game_layout::resolve_module(base).unwrap();
        assert!(Layout::resolve(&resolution).is_ok());
        let first = crate::auto_login::input_hook(&resolution).unwrap();
        let second = crate::auto_login::input_hook(&resolution).unwrap();
        drop(first); drop(second);
    }
}
