//! Read-only, per-process lobby diagnostics for the resolved game layout.
//! Only state codes are sampled. No packets, tickets, names or account IDs.

use std::{sync::mpsc, thread, time::{Duration, Instant}};
use crate::{game_layout::{self, Lobby, Resolution}, log};

// Fields of the lobby controller ([LOBBY]) and the objects it points to.
// game_layout extracts each from the game's own code and requires these exact
// values; a build that changed one switches its readers off.
pub(crate) const LOBBY_TCP: usize = 0x13690;
pub(crate) const LOBBY_REPLY: usize = 0x13698;
pub(crate) const LOBBY_ERROR: usize = 0x136AC;
/// 100ef790: the character count; 0x8c-byte entries follow it (100efca0).
pub(crate) const LOBBY_CHARACTER_COUNT: usize = 0x13820;
pub(crate) const LOBBY_CHARACTERS: usize = 0x13824;
pub(crate) const LOBBY_CHARACTER_SLOTS: usize = 16;
pub(crate) const TCP_OPERATION: usize = 0x4F1A8;
pub(crate) const TCP_CONTEXT: usize = 0x4F1AC;
pub(crate) const TCP_SOCKET_STATE: usize = 0x2F0F8;
pub(crate) const TCP_SOCKET_ERROR: usize = 0x2F0FC;
pub(crate) const CONTEXT_PROTOCOL_ERROR: usize = 0x13C;
pub(crate) const MAIN_STATE: usize = 0x88;
pub(crate) const MAIN_STEP: usize = 0x0C;
/// 100db170 RequestState rejects any larger state (cmp eax,0xCB; ja).
const MAIN_STATE_MAX: u32 = 0xCB;
pub(crate) const OP_CONNECT: u32 = 2;
pub(crate) const OP_LOGIN: u32 = 3;
pub(crate) const OP_CHARACTER_LIST: u32 = 4;
pub(crate) const OP_CHARACTER_SELECT: u32 = 5;
pub(crate) const OP_CLOSE: u32 = 8;
/// tcp_process dispatches operations 1..=21.
pub(crate) const OP_MAX: u32 = 21;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const TITLE_INTERVAL: Duration = Duration::from_millis(100);

use crate::mem::read;

fn valid_layout(base: usize, lobby: &Lobby, getter: [u8; 9]) -> bool {
    // Live 100ef920: mov eax,[LOBBY]; test eax,eax; je ..., with the relocated
    // operand (the branch distance is not checked, as in the scan).
    getter[0] == 0xA1 && getter[5..8] == [0x85, 0xC0, 0x74]
        && base.checked_add(lobby.ptr).and_then(|v| u32::try_from(v).ok())
            == Some(u32::from_le_bytes(getter[1..5].try_into().unwrap()))
}

/// The lobby controller global, only when the scan resolved it and the live
/// getter still loads exactly that relocated address.
pub(crate) fn verified_lobby(layout: &Resolution) -> Result<Lobby, String> {
    let lobby = layout.lobby.clone()?;
    let getter = layout.base.checked_add(lobby.get_error).and_then(read::<[u8; 9]>).ok_or("unreadable lobby getter")?;
    if !valid_layout(layout.base, &lobby, getter) {
        return Err("live lobby getter differs from the scanned image".into());
    }
    Ok(lobby)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    operation: u32,
    socket_state: i32,
    socket_error: i32,
    reply_state: i32,
    lobby_error: i32,
    protocol_error: i32,
}

fn snapshot(base: usize, lobby_rva: usize) -> Option<Snapshot> {
    let lobby = read::<u32>(base + lobby_rva)? as usize;
    if lobby == 0 { return None; }
    let tcp = read::<u32>(lobby + LOBBY_TCP)? as usize;
    if tcp == 0 { return None; }
    let operation = read::<u32>(tcp + TCP_OPERATION)?;
    let context = read::<u32>(tcp + TCP_CONTEXT)? as usize;
    if operation > OP_MAX || context == 0 { return None; }
    let reply_state = read(lobby + LOBBY_REPLY)?;
    // tcp_process writes only 0 (idle/done), 1 (pending) or -1 (failed); any
    // other value is a stale or foreign object.
    if !(-1..=1).contains(&reply_state) { return None; }
    Some(Snapshot {
        operation,
        socket_state: read(tcp + TCP_SOCKET_STATE)?,
        socket_error: read(tcp + TCP_SOCKET_ERROR)?,
        reply_state,
        lobby_error: read(lobby + LOBBY_ERROR)?,
        protocol_error: read(context + CONTEXT_PROTOCOL_ERROR)?,
    })
}

fn operation_name(operation: u32) -> &'static str {
    match operation {
        0 => "idle",
        OP_CONNECT => "connect",
        OP_LOGIN => "lobby login (0x26)",
        OP_CHARACTER_LIST => "character list (0x1f)",
        OP_CHARACTER_SELECT => "character selection (0x07)",
        OP_CLOSE => "close connection",
        _ => "other",
    }
}

fn network_step(base: usize, main_rva: usize) -> Option<(u32, u32)> {
    let main = read::<u32>(base + main_rva)? as usize;
    if main == 0 { return None; }
    // Live 10016580 passes this pointer directly to 100daf30, which calls
    // 100da750 with the same this pointer (state at +0x88, step at +0x0c).
    let state = read(main + MAIN_STATE)?;
    (state <= MAIN_STATE_MAX).then_some(())?;
    Some((state, read(main + MAIN_STEP)?))
}

/// Starts before GameStart and joins before game interfaces are released.
/// Sampling never changes or calls into the game. Four small samples/second;
/// logs only changes and a single 10s stall notice for an outstanding request.
pub(crate) struct Observer {
    stop: mpsc::Sender<()>,
    worker: thread::JoinHandle<()>,
}

impl Observer {
    pub(crate) fn start() -> Option<Self> {
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new().name("crystal-lobby-observer".into()).spawn(move || {
            let mut base = None;
            // (lobby global, main manager global) when diagnostics are available.
            let mut sampler = None;
            let mut title = None;
            let mut title_seen = false;
            let mut last_title = None;
            let started = Instant::now();
            let mut last = None;
            let mut changed = Instant::now();
            let mut stalled = false;
            loop {
                let watching_title = !title_seen && (base.is_none() || title.is_some())
                    && started.elapsed() < Duration::from_secs(180);
                let interval = if watching_title { TITLE_INTERVAL } else { SAMPLE_INTERVAL };
                if receiver.recv_timeout(interval) != Err(mpsc::RecvTimeoutError::Timeout) { break; }
                if base.is_none() {
                    // None until FFXiMain is loaded and initialized; keep polling.
                    let Some(layout) = game_layout::ffximain() else { continue; };
                    base = Some(layout.base);
                    sampler = match verified_lobby(layout).and_then(|lobby| layout.diagnostics.clone().map(|d| (lobby.ptr, d.main))) {
                        Ok(globals) => { log!("lobby: per-process state diagnostics enabled"); Some(globals) }
                        Err(reason) => { log!("lobby: state diagnostics unavailable: {reason}"); None }
                    };
                    title = match crate::auto_login::TitleProbe::new(layout) {
                        Ok(probe) => Some(probe),
                        Err(reason) => { log!("launch timing: title observation unavailable: {reason}"); None }
                    };
                    if sampler.is_none() && title.is_none() { break; }
                }
                let image = base.unwrap();
                if watching_title {
                    if let Some(probe) = &title {
                        let observed = probe.observe();
                        if last_title != Some(observed) {
                            crate::boot_timing::checkpoint(observed.menu);
                            log!("launch timing: title observer menu={} menu_live={} window={}",
                                observed.menu, observed.menu_live, observed.window);
                            last_title = Some(observed);
                        }
                        if observed.ready() {
                            title_seen = true;
                            log!("launch timing: FFXI title menu ready in visible window host_clock_us={}", crate::startup_timing::host_micros_now().unwrap_or(0));
                        }
                    }
                }
                let Some((lobby_rva, main_rva)) = sampler else {
                    if !watching_title { break; }
                    continue;
                };
                let Some(current) = snapshot(image, lobby_rva) else { continue; };
                if last != Some(current) {
                    log!("lobby: {} op={} socket_state={} socket_error={} reply_state={} lobby_error={} protocol_error={} network_step={:?}",
                        operation_name(current.operation), current.operation, current.socket_state, current.socket_error,
                        current.reply_state, current.lobby_error, current.protocol_error, network_step(image, main_rva));
                    last = Some(current); changed = Instant::now(); stalled = false;
                } else if (OP_CONNECT..=OP_CHARACTER_SELECT).contains(&current.operation) && !stalled && changed.elapsed() >= Duration::from_secs(10) {
                    log!("lobby: still waiting for {} after {:.1}s; network_step={:?}", operation_name(current.operation), changed.elapsed().as_secs_f32(), network_step(image, main_rva));
                    stalled = true;
                }
            }
        }).ok()?;
        Some(Self { stop, worker })
    }

    pub(crate) fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OLD: Lobby = game_layout::BUILD_6A995428.lobby;

    #[test]
    fn layout_rejects_the_old_pointer_and_supports_relocation() {
        let getter = |base: u32, offset: u32| {
            let mut bytes = [0xA1, 0, 0, 0, 0, 0x85, 0xC0, 0x74, 0x1E];
            bytes[1..5].copy_from_slice(&(base + offset).to_le_bytes()); bytes
        };
        for base in [0x1000_0000, 0x1800_0000] {
            assert!(valid_layout(base as usize, &OLD, getter(base, OLD.ptr as u32)));
            assert!(!valid_layout(base as usize, &OLD, getter(base, 0x004D_F7B0)));
            assert!(!valid_layout(base as usize + 0x1_0000, &OLD, getter(base, OLD.ptr as u32)), "unrelocated operand");
        }
        assert!(!valid_layout(0x1000_0000, &OLD, [0;9]));
    }

    #[test]
    fn unresolved_or_unreadable_layouts_are_never_sampled() {
        let unsupported = Resolution::unsupported(0x1000_0000, "test");
        assert!(verified_lobby(&unsupported).unwrap_err().contains("unsupported module: test"));
        assert_eq!(read::<u32>(0), None);
    }

    #[test]
    fn operation_labels_follow_the_resolved_codes() {
        assert_eq!(operation_name(0), "idle");
        assert_eq!(operation_name(OP_CONNECT), "connect");
        assert_eq!(operation_name(OP_CHARACTER_SELECT), "character selection (0x07)");
        assert_eq!(operation_name(OP_MAX), "other");
    }

    #[test]
    #[cfg(target_pointer_width = "32")]
    fn network_state_uses_the_direct_manager_pointer() {
        let mut manager = [0u32; 0x90 / 4];
        manager[0x88 / 4] = 9;
        manager[0x0C / 4] = 4;
        let root = manager.as_ptr() as u32;
        let main_rva = game_layout::BUILD_6A995428.diagnostics.main;
        let base = (&root as *const u32 as usize).checked_sub(main_rva).unwrap();
        assert_eq!(network_step(base, main_rva), Some((9, 4)));
        manager[0x88 / 4] = MAIN_STATE_MAX + 1;
        std::hint::black_box(&manager);
        assert_eq!(network_step(base, main_rva), None, "RequestState never sets a larger state");
    }

    #[test]
    #[cfg(target_pointer_width = "32")]
    fn samples_require_a_live_connection_and_a_reply_protocol_value() {
        let mut lobby = vec![0u8; 0x13700];
        let mut tcp = vec![0u8; 0x4F200];
        let mut context = vec![0u8; 0x140];
        let put = |bytes: &mut Vec<u8>, at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        put(&mut tcp, TCP_OPERATION, OP_LOGIN);
        put(&mut tcp, TCP_CONTEXT, context.as_mut_ptr() as u32);
        put(&mut tcp, TCP_SOCKET_STATE, 6);
        put(&mut context, CONTEXT_PROTOCOL_ERROR, 0x64);
        put(&mut lobby, LOBBY_TCP, tcp.as_ptr() as u32);
        put(&mut lobby, LOBBY_REPLY, 1);
        let root = lobby.as_ptr() as u32;
        let base = (&root as *const u32 as usize).checked_sub(OLD.ptr).unwrap();
        let sample = snapshot(base, OLD.ptr).unwrap();
        assert_eq!((sample.operation, sample.socket_state, sample.reply_state, sample.protocol_error), (OP_LOGIN, 6, 1, 0x64));
        put(&mut lobby, LOBBY_REPLY, 7);
        assert_eq!(snapshot(base, OLD.ptr), None, "not a reply value: stale or foreign object");
        put(&mut lobby, LOBBY_REPLY, u32::MAX);
        assert!(snapshot(base, OLD.ptr).is_some());
        put(&mut tcp, TCP_OPERATION, OP_MAX + 1);
        assert_eq!(snapshot(base, OLD.ptr), None);
        put(&mut tcp, TCP_OPERATION, OP_LOGIN);
        put(&mut tcp, TCP_CONTEXT, 0);
        assert_eq!(snapshot(base, OLD.ptr), None);
    }
}
