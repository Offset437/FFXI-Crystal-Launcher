//! Build-only native login diagnostics. Forward unchanged socket calls and
//! record counts/timing, never hostnames, addresses, credentials or buffers.
//! IAT wrappers and originals are process-lived: stopping recording does not
//! race native workers by restoring imports or invalidating their call targets.
//! Diagnostics never write socket options.

use std::{
    ffi::c_void,
    sync::{atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}, mpsc::{self, SyncSender}, Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError},
    Networking::WinSock::{FD_SET, TIMEVAL, HOSTENT, SOCKADDR, SOCKET, INVALID_SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_ERROR, WSAEWOULDBLOCK, WSAGetLastError, WSASetLastError,
        getsockopt, WSAIoctl, IPPROTO_TCP, SIO_TCP_INFO, TCP_INFO_v0, TCP_NODELAY, TCPSTATE_ESTABLISHED},
    System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Memory::{VirtualProtect, PAGE_READWRITE},
        Threading::GetCurrentThreadId,
    },
};

// The Winsock imports traced, in wrapper order. Each is found in polcore's
// import table by the address Windows bound it to, so any build's layout works,
// and an entry another module already redirected is never chained through.
const IMPORTS: [&str; 8] = ["socket", "connect", "gethostbyname", "send", "recv", "getsockopt", "closesocket", "select"];
static ORIGINAL: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];
static STATE: OnceLock<State> = OnceLock::new();
const QUEUE: usize = 512;
const TCP_POLL_US: u64 = 20_000;

#[derive(Clone, Copy)]
struct Errors { win32: u32, winsock: i32 }
impl Errors {
    fn capture() -> Self { unsafe { Self { win32: GetLastError(), winsock: WSAGetLastError() } } }
    fn restore(self) { unsafe { WSASetLastError(self.winsock); SetLastError(self.win32); } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op { Socket(i32, i32, i32), Connect, Dns, Send, Recv, Error, Close, Select, Tcp }
impl Op { fn is_socket(self) -> bool { matches!(self, Self::Socket(..)) } }
#[derive(Clone, Copy, Debug)]
enum Direction { Read, Write }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Summary { Ready, Close, Stop, Reused }
#[derive(Clone, Copy, Debug, Default)]
struct Readiness {
    polls: u64, pending: u64, ready: u64, errors: u64,
    last_poll: Option<u64>, first_ready: Option<u64>, pending_since: Option<u64>,
    max_gap_us: u64, max_call_us: u64, transitions: u64,
}
impl Readiness {
    fn poll(&mut self, at: u64, duration: u64, ready: bool, error: bool) -> bool {
        if let Some(last) = self.last_poll.replace(at) { self.max_gap_us = self.max_gap_us.max(at.saturating_sub(last)); }
        self.max_call_us = self.max_call_us.max(duration);
        self.polls += 1;
        if error { self.errors += 1; return false; }
        if !ready {
            self.pending += 1;
            self.pending_since.get_or_insert(at);
            return false;
        }
        self.ready += 1;
        let transition = self.first_ready.is_none() || self.pending_since.is_some();
        self.first_ready.get_or_insert(at);
        if transition { self.transitions += 1; }
        // At most eight readiness-transition records per direction/socket;
        // close/stop still report every accumulated poll. Never log each poll.
        transition && self.transitions <= 8
    }
}
#[derive(Clone, Copy, Debug)]
struct ReadinessRecord { direction: Direction, reason: Summary, stats: Readiness }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TcpReason { Connected, Send, Recv, Pending }
// Fields of these query records are read only by Debug (the log line).
#[allow(dead_code)]
#[derive(Clone, Copy, Default, Debug)]
struct TcpInfo {
    state: i32, mss: u32, connection_ms: u64, rtt_us: u32, min_rtt_us: u32,
    bytes_out: u64, bytes_in: u64, in_flight: u32, cwnd: u32, snd_wnd: u32, rcv_wnd: u32,
    retrans: u32, fast_retrans: u32, timeouts: u32, syn_retrans: u8,
}
impl From<TCP_INFO_v0> for TcpInfo {
    fn from(i: TCP_INFO_v0) -> Self {
        Self { state: i.State, mss: i.Mss, connection_ms: i.ConnectionTimeMs, rtt_us: i.RttUs, min_rtt_us: i.MinRttUs,
            bytes_out: i.BytesOut, bytes_in: i.BytesIn, in_flight: i.BytesInFlight, cwnd: i.Cwnd, snd_wnd: i.SndWnd, rcv_wnd: i.RcvWnd,
            retrans: i.BytesRetrans, fast_retrans: i.FastRetrans, timeouts: i.TimeoutEpisodes, syn_retrans: i.SynRetrans }
    }
}
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct QueryStatus { result: i32, error: i32, bytes: u32, duration_us: u64 }
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct NoDelayQuery { status: QueryStatus, value: Option<bool> }
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
struct InfoQuery { status: QueryStatus, value: Option<TcpInfo> }
#[derive(Clone, Copy, Debug)]
struct TcpQueries { nodelay: Option<NoDelayQuery>, info: Option<InfoQuery> }
type QueryTcp = fn(SOCKET, bool, bool) -> TcpQueries;

// Providers can report a one-byte Boolean despite the caller supplying a
// four-byte buffer. Decode only these known widths from initialized storage.
fn socket_bool(result: i32, length: i32, value: i32) -> Option<bool> {
    if result != 0 { return None; }
    match length {
        1 => Some(value.to_ne_bytes()[0] != 0),
        4 => Some(value != 0),
        _ => None,
    }
}
fn query_nodelay(socket: SOCKET) -> NoDelayQuery {
    let (mut value, mut length) = (0i32, std::mem::size_of::<i32>() as i32);
    let start = Instant::now();
    let result = unsafe { getsockopt(socket, IPPROTO_TCP, TCP_NODELAY, (&mut value as *mut i32).cast(), &mut length) };
    let error = if result == SOCKET_ERROR { unsafe { WSAGetLastError() } } else { 0 };
    NoDelayQuery { status: QueryStatus { result, error, bytes: length.max(0) as u32, duration_us: start.elapsed().as_micros() as u64 },
        value: socket_bool(result, length, value) }
}

// These are synchronous, read-only queries on the existing socket, using only
// fixed stack buffers. They never read SO_ERROR (which could consume it).
fn query_tcp(socket: SOCKET, nodelay: bool, info: bool) -> TcpQueries {
    let nodelay = nodelay.then(|| query_nodelay(socket));
    let info = info.then(|| {
        let version = 0u32;
        let mut value = TCP_INFO_v0::default();
        let mut bytes = 0;
        let start = Instant::now();
        let result = unsafe { WSAIoctl(socket, SIO_TCP_INFO, (&version as *const u32).cast(), std::mem::size_of::<u32>() as u32,
            (&mut value as *mut TCP_INFO_v0).cast(), std::mem::size_of::<TCP_INFO_v0>() as u32, &mut bytes, std::ptr::null_mut(), None) };
        let error = if result == SOCKET_ERROR { unsafe { WSAGetLastError() } } else { 0 };
        InfoQuery { status: QueryStatus { result, error, bytes, duration_us: start.elapsed().as_micros() as u64 },
            value: (result == 0 && bytes as usize == std::mem::size_of::<TCP_INFO_v0>()).then(|| value.into()) }
    });
    TcpQueries { nodelay, info }
}

#[derive(Clone, Copy, Debug)]
struct TcpBaseline { at_us: u64, info: TcpInfo, coverage: u64 }
#[derive(Clone, Copy, Default)]
struct TcpTracking {
    created: bool, connected: bool, nodelay_queried: bool, nodelay: Option<bool>, info_disabled: bool, querying: bool,
    accepted: u64, returned: u64, creation_coverage: u64, last_sample_us: Option<u64>, baseline: Option<TcpBaseline>,
}
#[derive(Clone, Copy, Debug)]
struct TcpRecord {
    reason: TcpReason, accepted: u64, returned: u64, nodelay: Option<bool>, queries: TcpQueries,
    baseline: Option<TcpBaseline>, coherent: bool, coverage: u64, out_delta: Option<u64>, in_delta: Option<u64>,
    unsent_estimate: Option<u64>,
}
impl TcpTracking {
    fn report(self, reason: TcpReason, queries: TcpQueries, coherent: bool, coverage: u64) -> TcpRecord {
        let mut r = TcpRecord { reason, accepted: self.accepted, returned: self.returned, nodelay: self.nodelay, queries,
            baseline: self.baseline, coherent, coverage, out_delta: None, in_delta: None, unsent_estimate: None };
        if let (Some(base), Some(info)) = (self.baseline, queries.info.and_then(|q| q.value)) {
            r.out_delta = info.bytes_out.checked_sub(base.info.bytes_out);
            r.in_delta = info.bytes_in.checked_sub(base.info.bytes_in);
            // BytesOut is a TCP-stack counter, not a wire timestamp. Only offer
            // an estimate with a pre-traffic baseline, complete observed calls,
            // stable counters, no concurrent native call, and no retransmits.
            // An incompatible counter stays unknown instead of being clamped.
            if coherent && coverage == base.coverage && info.state == TCPSTATE_ESTABLISHED
                && info.connection_ms >= base.info.connection_ms && info.retrans == base.info.retrans
                && info.fast_retrans == base.info.fast_retrans && info.timeouts == base.info.timeouts && info.syn_retrans == base.info.syn_retrans
                && r.in_delta.is_some_and(|n| n >= self.returned) {
                r.unsent_estimate = r.out_delta.filter(|n| u64::from(info.in_flight) <= *n).and_then(|n| self.accepted.checked_sub(n));
            }
        }
        r
    }
}
#[derive(Clone, Copy)]
struct TcpRequest { socket: SOCKET, generation: u64, reason: TcpReason }

#[derive(Clone, Copy, Debug)]
struct Record {
    op: Op, at_us: u64, duration_us: u64, thread: u32,
    generation: u64, requested: i32, result: i64, error: i32,
    port: Option<u16>, socket_error: Option<i32>,
    readiness: Option<ReadinessRecord>,
    tcp: Option<TcpRecord>,
}
#[derive(Clone, Copy)]
struct Handle {
    socket: SOCKET, generation: u64, readiness: [Readiness; 2], tcp: TcpTracking,
}
struct Handles { entries: [Option<Handle>; 64], next: u64 }
impl Handles {
    fn new() -> Self { Self { entries: [None; 64], next: 1 } }
    fn generation(&mut self, socket: SOCKET, op: Op, success: bool) -> Option<u64> {
        if socket == INVALID_SOCKET || op == Op::Dns { return Some(0); }
        let found = self.entries.iter().position(|h| h.is_some_and(|h| h.socket == socket));
        let index = found.or_else(|| self.entries.iter().position(Option::is_none))?;
        if found.is_none() || op.is_socket() {
            self.entries[index] = Some(Handle { socket, generation: self.next, readiness: [Readiness::default(); 2],
                tcp: TcpTracking { created: op.is_socket(), ..TcpTracking::default() },
            });
            self.next += 1;
        }
        let generation = self.entries[index]?.generation;
        if op == Op::Close && success { self.entries[index] = None; }
        Some(generation)
    }
}
struct State {
    enabled: AtomicBool, done: AtomicBool, epoch: Instant,
    sender: SyncSender<Record>, handles: Mutex<Handles>, lost: AtomicU64,
    coverage: AtomicU64, native_active: AtomicUsize, native_epoch: AtomicU64, query_tcp: QueryTcp,
}
impl State {
    fn new(sender: SyncSender<Record>) -> Self {
        Self { enabled: AtomicBool::new(false), done: AtomicBool::new(false), epoch: Instant::now(), sender,
            handles: Mutex::new(Handles::new()), lost: AtomicU64::new(0), coverage: AtomicU64::new(0),
            native_active: AtomicUsize::new(0), native_epoch: AtomicU64::new(0), query_tcp }
    }
    fn stop(&self) {
        if self.enabled.swap(false, Ordering::AcqRel) {
            if let Ok(handles) = self.handles.try_lock() {
                for handle in handles.entries.iter().flatten() { self.summarize(*handle, Summary::Stop); }
            } else { self.lost.fetch_add(1, Ordering::Relaxed); }
        }
        self.done.store(true, Ordering::Release);
    }
    fn enqueue(&self, record: Record) {
        if self.sender.try_send(record).is_err() { self.lost.fetch_add(1, Ordering::Relaxed); }
    }
    fn readiness_record(&self, handle: Handle, index: usize, reason: Summary, at: u64, thread: u32, error: i32) {
        let stats = handle.readiness[index];
        if stats.polls == 0 { return; }
        self.enqueue(Record { op: Op::Select, at_us: at, duration_us: 0, thread,
            generation: handle.generation, requested: 0, result: 0, error, port: None, socket_error: None,
            readiness: Some(ReadinessRecord { direction: if index == 0 { Direction::Read } else { Direction::Write }, reason, stats }), tcp: None });
    }
    fn summarize(&self, handle: Handle, reason: Summary) {
        for index in 0..2 { self.readiness_record(handle, index, reason, self.epoch.elapsed().as_micros() as u64, 0, 0); }
    }
    fn record(&self, socket: SOCKET, mut record: Record) -> Option<TcpRequest> {
        if !self.enabled.load(Ordering::Acquire) { return None; }
        // Never wait behind another native thread or the file writer. Losing
        // metadata is preferable to stalling a socket; the writer reports it.
        let Ok(mut handles) = self.handles.try_lock() else {
            self.lost.fetch_add(1, Ordering::Relaxed); self.coverage.fetch_add(1, Ordering::SeqCst); return None;
        };
        if !self.enabled.load(Ordering::Acquire) { return None; }
        if record.op.is_socket() || record.op == Op::Close && record.result >= 0 {
            if let Some(handle) = handles.entries.iter().flatten().find(|h| h.socket == socket) {
                self.summarize(*handle, if record.op.is_socket() { Summary::Reused } else { Summary::Close });
            }
        }
        let Some(generation) = handles.generation(socket, record.op, record.result >= 0) else {
            self.lost.fetch_add(1, Ordering::Relaxed); self.coverage.fetch_add(1, Ordering::SeqCst); return None;
        };
        record.generation = generation;
        self.enqueue(record);
        let handle = handles.entries.iter_mut().flatten().find(|h| h.socket == socket)?;
        if record.op.is_socket() { handle.tcp.creation_coverage = self.coverage.load(Ordering::SeqCst); }
        let reason = match record.op {
            Op::Connect if record.result == 0 => { handle.tcp.connected = true; TcpReason::Connected }
            Op::Error if record.result == 0 && record.socket_error == Some(0) => { handle.tcp.connected = true; TcpReason::Connected }
            Op::Send | Op::Recv if record.result > 0 => {
                let total = if record.op == Op::Send { &mut handle.tcp.accepted } else { &mut handle.tcp.returned };
                if let Some(n) = total.checked_add(record.result as u64) { *total = n; }
                else { self.coverage.fetch_add(1, Ordering::SeqCst); }
                if record.op == Op::Send { TcpReason::Send } else { TcpReason::Recv }
            }
            _ => return None,
        };
        Some(TcpRequest { socket, generation, reason })
    }
    fn selected(&self, input: &[Option<Sockets>; 2], output: &[Option<Sockets>; 2], result: i32, at: u64, duration: u64, thread: u32, error: i32) -> [Option<TcpRequest>; 64] {
        let mut pending = [None; 64];
        let mut count = 0;
        if !self.enabled.load(Ordering::Acquire) { return pending; }
        let Ok(mut handles) = self.handles.try_lock() else { self.lost.fetch_add(1, Ordering::Relaxed); return pending; };
        if !self.enabled.load(Ordering::Acquire) { return pending; }
        for index in 0..2 {
            let Some(input) = &input[index] else { self.lost.fetch_add(1, Ordering::Relaxed); continue; };
            if result > 0 && output[index].is_none() { self.lost.fetch_add(1, Ordering::Relaxed); continue; }
            for (position, &socket) in input.values[..input.count].iter().enumerate() {
                if socket == INVALID_SOCKET || input.values[..position].contains(&socket) { continue; }
                if handles.generation(socket, Op::Select, true).is_none() { self.lost.fetch_add(1, Ordering::Relaxed); continue; }
                let handle = handles.entries.iter_mut().flatten().find(|h| h.socket == socket).unwrap();
                let ready = result > 0 && output[index].as_ref().is_some_and(|set| set.values[..set.count].contains(&socket));
                if handle.readiness[index].poll(at, duration, ready, result < 0) {
                    self.readiness_record(*handle, index, Summary::Ready, at, thread, error);
                }
                if ready { handle.readiness[index].pending_since = None; }
                if result >= 0 && !ready && (handle.tcp.connected || handle.tcp.accepted > 0 || handle.tcp.returned > 0)
                    && !pending[..count].iter().flatten().any(|r: &TcpRequest| r.generation == handle.generation) {
                    pending[count] = Some(TcpRequest { socket, generation: handle.generation, reason: TcpReason::Pending });
                    count += 1;
                }
            }
        }
        pending
    }
    fn sample_tcp(&self, request: TcpRequest, thread: u32, at: u64) {
        if !self.enabled.load(Ordering::Acquire) { return; }
        let snapshot = {
            let Ok(mut handles) = self.handles.try_lock() else { self.lost.fetch_add(1, Ordering::Relaxed); return; };
            if !self.enabled.load(Ordering::Acquire) { return; }
            let Some(handle) = handles.entries.iter_mut().flatten().find(|h| h.socket == request.socket && h.generation == request.generation) else { return; };
            let t = &mut handle.tcp;
            if t.querying || request.reason == TcpReason::Pending && t.last_sample_us.is_some_and(|last| at.saturating_sub(last) < TCP_POLL_US) { return; }
            let nodelay = t.connected && !t.nodelay_queried;
            if !nodelay && (t.info_disabled || request.reason == TcpReason::Connected && t.nodelay_queried) { return; }
            let snapshot = *t;
            t.nodelay_queried |= nodelay;
            t.last_sample_us = Some(at);
            t.querying = true;
            (snapshot, nodelay)
        };
        // Never hold even the try-acquired handle lock across a Winsock query.
        // Concurrent calls, lost accounting, and handle reuse invalidate only
        // interpretation; original calls keep running with their native args.
        let coverage = self.coverage.load(Ordering::SeqCst);
        let native_epoch = self.native_epoch.load(Ordering::SeqCst);
        let idle = self.native_active.load(Ordering::SeqCst) == 0;
        let start = Instant::now();
        let queries = (self.query_tcp)(request.socket, snapshot.1, !snapshot.0.info_disabled);
        let duration_us = start.elapsed().as_micros() as u64;
        let coherent = idle && self.native_active.load(Ordering::SeqCst) == 0
            && native_epoch == self.native_epoch.load(Ordering::SeqCst) && coverage == self.coverage.load(Ordering::SeqCst);
        let Ok(mut handles) = self.handles.try_lock() else {
            // The reservation stays set on a contended completion, suppressing
            // further diagnostic queries for this generation rather than waiting.
            self.lost.fetch_add(1, Ordering::Relaxed); return;
        };
        let Some(handle) = handles.entries.iter_mut().flatten().find(|h| h.socket == request.socket && h.generation == request.generation) else { return; };
        let t = &mut handle.tcp;
        t.querying = false;
        if !self.enabled.load(Ordering::Acquire) { return; }
        if let Some(q) = queries.nodelay { t.nodelay = q.value; }
        if queries.info.is_some_and(|q| q.value.is_none()) { t.info_disabled = true; }
        let coherent = coherent && t.accepted == snapshot.0.accepted && t.returned == snapshot.0.returned;
        if let Some(info) = queries.info.and_then(|q| q.value) {
            if t.baseline.is_none() && request.reason == TcpReason::Connected && t.created && t.accepted == 0 && t.returned == 0
                && coherent && coverage == t.creation_coverage && info.state == TCPSTATE_ESTABLISHED && info.in_flight == 0 {
                t.baseline = Some(TcpBaseline { at_us: at, info, coverage });
            }
        }
        let tcp = t.report(request.reason, queries, coherent, coverage);
        self.enqueue(Record { op: Op::Tcp, at_us: at, duration_us, thread, generation: request.generation,
            requested: 0, result: 0, error: 0, port: None, socket_error: None, readiness: None, tcp: Some(tcp) });
    }
}

pub struct Recording;
impl Recording {
    /// The caller has validated this mapped polcore build and retains it until
    /// native teardown. No hook is installed unless every expected import fits.
    pub unsafe fn install(base: usize, size: usize) -> Result<Self, String> {
        if STATE.get().is_some() { return Err("already initialized".into()); }
        if std::mem::size_of::<usize>() != 4 { return Err("requires x86".into()); }
        let slots = crate::iat::import_slots(base, "WS2_32.dll");
        let module_name: Vec<u16> = "WS2_32.dll\0".encode_utf16().collect();
        let module = GetModuleHandleW(module_name.as_ptr());
        if module.is_null() { return Err("Winsock is not loaded".into()); }
        let mut entries = [std::ptr::null_mut(); 8];
        let mut originals = [0; 8];
        for (index, name) in IMPORTS.iter().enumerate() {
            let bytes: Vec<u8> = name.bytes().chain(Some(0)).collect();
            let system = GetProcAddress(module, bytes.as_ptr()).map_or(0, |f| f as usize);
            if system == 0 { return Err(format!("Winsock has no {name}")); }
            let found = |s: &&crate::iat::ImportSlot| {
                let at = s.slot as usize;
                at >= base && at.checked_add(4).is_some_and(|end| end <= base.saturating_add(size)) && s.slot.read() == system
            };
            // Exactly one entry bound to the system function; none means another
            // module redirected it (never chain through an unknown hook).
            let mut matching = slots.iter().filter(found);
            let (Some(slot), None) = (matching.next(), matching.next()) else {
                return Err(format!("unsupported {name} import target"));
            };
            entries[index] = slot.slot;
            originals[index] = system;
        }
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        STATE.set(State::new(sender)).map_err(|_| "already initialized")?;
        let state = STATE.get().unwrap();
        if std::thread::Builder::new().name("native-socket-log".into()).spawn(move || {
            loop {
                if !state.done.load(Ordering::Acquire) && state.epoch.elapsed() >= Duration::from_secs(60) {
                    state.stop();
                }
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(r) => if let Some(select) = r.readiness {
                        let s = select.stats;
                        crate::log!("native POL readiness: {:?} {:?} +{}us socket={} polls={} pending={} ready={} errors={} transitions={} first_ready_us={:?} last_poll_us={:?} pending_since_us={:?} max_gap_us={} max_call_us={}",
                            select.direction, select.reason, r.at_us, r.generation, s.polls, s.pending, s.ready, s.errors, s.transitions,
                            s.first_ready, s.last_poll, s.pending_since, s.max_gap_us, s.max_call_us);
                    } else if let Some(t) = r.tcp {
                        crate::log!("native POL TCP: {:?} +{}us duration={}us thread={} socket={} send_accepted={} recv_returned={} nodelay={:?} baseline_us={:?} baseline_out={:?} baseline_in={:?} coherent={} coverage={} out_delta={:?} in_delta={:?} unsent_estimate={:?} nodelay_query={:?} info_query={:?}",
                            t.reason, r.at_us, r.duration_us, r.thread, r.generation, t.accepted, t.returned, t.nodelay,
                            t.baseline.map(|b| b.at_us), t.baseline.map(|b| b.info.bytes_out), t.baseline.map(|b| b.info.bytes_in),
                            t.coherent, t.coverage, t.out_delta, t.in_delta, t.unsent_estimate, t.queries.nodelay, t.queries.info);
                    } else { crate::log!("native POL socket: {:?} +{}us duration={}us thread={} socket={} requested={} result={} error={} port={:?} so_error={:?}",
                        r.op, r.at_us, r.duration_us, r.thread, r.generation, r.requested, r.result, r.error, r.port, r.socket_error); },
                    Err(mpsc::RecvTimeoutError::Timeout) if !state.done.load(Ordering::Acquire) => continue,
                    Err(_) => break,
                }
            }
            crate::log!("native POL socket trace stopped; metadata records lost={}", state.lost.load(Ordering::Relaxed));
        }).is_err() { state.stop(); return Err("cannot start metadata writer".into()); }
        // The eight entries lie in polcore's import address table. Publish every
        // original before replacing any slot; workers may enter a wrapper immediately.
        let start = entries.iter().map(|e| *e as usize).min().unwrap();
        let length = entries.iter().map(|e| *e as usize).max().unwrap() + 4 - start;
        let mut protection = 0;
        if VirtualProtect(start as _, length, PAGE_READWRITE, &mut protection) == 0 {
            state.stop(); return Err("cannot write socket imports".into());
        }
        for (target, original) in ORIGINAL.iter().zip(originals) { target.store(original, Ordering::Release); }
        let wrappers = [socket as *const () as usize, connect as *const () as usize, dns as *const () as usize,
            send as *const () as usize, recv as *const () as usize, socket_error as *const () as usize, close as *const () as usize, select as *const () as usize];
        for (slot, wrapper) in entries.into_iter().zip(wrappers) { AtomicUsize::from_ptr(slot).store(wrapper, Ordering::Release); }
        let mut ignored = 0;
        if VirtualProtect(start as _, length, protection, &mut ignored) == 0 {
            state.stop(); return Err("cannot restore socket import page protection; forwarding remains valid".into());
        }
        state.enabled.store(true, Ordering::Release);
        crate::log!("native POL socket metadata recording enabled; TCP_INFO v0 pending interval={}us; stack counters are not wire timestamps; payloads omitted",
            TCP_POLL_US);
        Ok(Self)
    }
    pub fn stop(&self) { if let Some(state) = STATE.get() { state.stop(); } }
}
impl Drop for Recording { fn drop(&mut self) { self.stop(); } }

/// Read only fixed, non-payload scalars. Invalid/native-racing pointers simply
/// omit the optional field; never dereference application memory in Rust.
fn read<const N: usize>(pointer: *const c_void) -> Option<[u8; N]> {
    crate::mem::read::<[u8; N]>(pointer as usize)
}
fn port(address: *const SOCKADDR, length: i32) -> Option<u16> {
    if length < 4 { return None; }
    let b = read::<4>(address.cast())?;
    matches!(u16::from_ne_bytes([b[0], b[1]]), 2 | 23).then(|| u16::from_be_bytes([b[2], b[3]]))
}
#[derive(Clone, Copy)]
struct Sockets { values: [SOCKET; 64], count: usize }
fn read_fd_set(pointer: *const FD_SET) -> Option<Sockets> {
    let mut set = Sockets { values: [0; 64], count: 0 };
    if pointer.is_null() { return Some(set); }
    set.count = u32::from_ne_bytes(read::<4>(pointer.cast())?) as usize;
    if set.count > set.values.len() { return None; }
    if set.count != 0 {
        let address = (pointer as usize).checked_add(std::mem::offset_of!(FD_SET, fd_array))?;
        if !crate::mem::read_into(address, &mut set.values[..set.count]) {
            return None;
        }
    }
    Some(set)
}
unsafe fn original<F: Copy>(index: usize) -> F { std::mem::transmute_copy(&ORIGINAL[index].load(Ordering::Acquire)) }

struct NativeCall<'a>(&'a State);
impl<'a> NativeCall<'a> {
    fn begin(state: &'a State) -> Self {
        state.native_active.fetch_add(1, Ordering::SeqCst);
        state.native_epoch.fetch_add(1, Ordering::SeqCst);
        Self(state)
    }
}
impl Drop for NativeCall<'_> {
    fn drop(&mut self) { self.0.native_active.fetch_sub(1, Ordering::SeqCst); }
}

/// Capture before any diagnostic APIs, restore before the real call, and
/// restore its post-call state after all metadata work (including failed reads).
fn forward<R: Copy>(op: Op, socket: SOCKET, requested: i32, call: impl FnOnce() -> R,
    result: impl FnOnce(R) -> i64, details: impl FnOnce(R) -> (Option<u16>, Option<i32>)) -> R {
    let Some(state) = STATE.get().filter(|s| s.enabled.load(Ordering::Acquire)) else { return call(); };
    forward_with(state, op, socket, requested, call, result, details)
}
fn forward_with<R: Copy>(state: &State, op: Op, socket: SOCKET, requested: i32, call: impl FnOnce() -> R,
    result: impl FnOnce(R) -> i64, details: impl FnOnce(R) -> (Option<u16>, Option<i32>)) -> R {
    let before = Errors::capture();
    let native = NativeCall::begin(state);
    let start = Instant::now();
    let thread = unsafe { GetCurrentThreadId() };
    before.restore();
    let value = call();
    let after = Errors::capture();
    let end = Instant::now();
    let result = result(value);
    // Nonblocking pending is normal. Connect's initial would-block is useful
    // because it marks the beginning of a connection; suppress send/recv noise.
    let request = if !(matches!(op, Op::Send | Op::Recv) && result == SOCKET_ERROR as i64 && after.winsock == WSAEWOULDBLOCK) {
        let (port, socket_error) = details(value);
        let socket = if op.is_socket() { if result < 0 { INVALID_SOCKET } else { result as SOCKET } } else { socket };
        state.record(socket, Record { op, at_us: start.duration_since(state.epoch).as_micros() as u64,
            duration_us: end.duration_since(start).as_micros() as u64, thread, generation: 0, requested, result,
            error: if result < 0 { after.winsock } else { 0 }, port, socket_error, readiness: None, tcp: None })
    } else { None };
    drop(native);
    if let Some(request) = request { state.sample_tcp(request, thread, state.epoch.elapsed().as_micros() as u64); }
    after.restore();
    value
}

unsafe extern "system" fn socket(af: i32, kind: i32, protocol: i32) -> SOCKET {
    forward(Op::Socket(af, kind, protocol), INVALID_SOCKET, 0, || original::<unsafe extern "system" fn(i32, i32, i32) -> SOCKET>(0)(af, kind, protocol),
        |r| if r == INVALID_SOCKET { -1 } else { r as i64 }, |_| (None, None))
}
unsafe extern "system" fn connect(s: SOCKET, address: *const SOCKADDR, length: i32) -> i32 {
    forward(Op::Connect, s, 0,
        || original::<unsafe extern "system" fn(SOCKET, *const SOCKADDR, i32) -> i32>(1)(s, address, length),
        i64::from, |_| (port(address, length), None))
}

unsafe extern "system" fn dns(name: *const u8) -> *mut HOSTENT {
    forward(Op::Dns, INVALID_SOCKET, 0, || original::<unsafe extern "system" fn(*const u8) -> *mut HOSTENT>(2)(name),
        |r| if r.is_null() { -1 } else { 1 }, |_| (None, None))
}
unsafe extern "system" fn send(s: SOCKET, buffer: *const u8, length: i32, flags: i32) -> i32 {
    forward(Op::Send, s, length, || original::<unsafe extern "system" fn(SOCKET, *const u8, i32, i32) -> i32>(3)(s, buffer, length, flags),
        i64::from, |_| (None, None))
}
unsafe extern "system" fn recv(s: SOCKET, buffer: *mut u8, length: i32, flags: i32) -> i32 {
    forward(Op::Recv, s, length, || original::<unsafe extern "system" fn(SOCKET, *mut u8, i32, i32) -> i32>(4)(s, buffer, length, flags),
        i64::from, |_| (None, None))
}
unsafe extern "system" fn socket_error(s: SOCKET, level: i32, option: i32, value: *mut u8, length: *mut i32) -> i32 {
    let call = || original::<unsafe extern "system" fn(SOCKET, i32, i32, *mut u8, *mut i32) -> i32>(5)(s, level, option, value, length);
    if level != SOL_SOCKET || option != SO_ERROR { return call(); }
    forward(Op::Error, s, 0, call, i64::from, |r| {
        let error = (r == 0).then(|| read::<4>(length.cast())).flatten().filter(|b| i32::from_ne_bytes(*b) >= 4)
            .and_then(|_| read::<4>(value.cast())).map(i32::from_ne_bytes);
        (None, error)
    })
}
unsafe extern "system" fn close(s: SOCKET) -> i32 {
    forward(Op::Close, s, 0, || original::<unsafe extern "system" fn(SOCKET) -> i32>(6)(s), i64::from, |_| (None, None))
}

type SelectFn = unsafe extern "system" fn(i32, *mut FD_SET, *mut FD_SET, *mut FD_SET, *const TIMEVAL) -> i32;
unsafe extern "system" fn select(nfds: i32, readfds: *mut FD_SET, writefds: *mut FD_SET, exceptfds: *mut FD_SET, timeout: *const TIMEVAL) -> i32 {
    let call = || original::<SelectFn>(7)(nfds, readfds, writefds, exceptfds, timeout);
    let Some(state) = STATE.get().filter(|s| s.enabled.load(Ordering::Acquire)) else { return call(); };
    select_with(state, readfds, writefds, call)
}
fn select_with(state: &State, readfds: *const FD_SET, writefds: *const FD_SET, call: impl FnOnce() -> i32) -> i32 {
    let before = Errors::capture();
    // select overwrites its fd_sets. Snapshot input membership first; otherwise
    // pending sockets disappear and cannot be counted. Read handles only.
    let input = [read_fd_set(readfds), read_fd_set(writefds)];
    let start = Instant::now();
    let thread = unsafe { GetCurrentThreadId() };
    before.restore();
    let result = call();
    let after = Errors::capture();
    let end = Instant::now();
    // Error outputs are undefined; a zero result means no input socket ready.
    let output = if result > 0 { [read_fd_set(readfds), read_fd_set(writefds)] } else { [None, None] };
    let pending = state.selected(&input, &output, result, start.duration_since(state.epoch).as_micros() as u64,
        end.duration_since(start).as_micros() as u64, thread, after.winsock);
    for request in pending.into_iter().flatten() { state.sample_tcp(request, thread, state.epoch.elapsed().as_micros() as u64); }
    after.restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};
    use windows_sys::Win32::Networking::WinSock::WSAEOPNOTSUPP;

    #[derive(Default)]
    struct MockQueries { calls: Vec<(SOCKET, bool, bool)>, info: VecDeque<InfoQuery> }
    thread_local! { static QUERIES: RefCell<MockQueries> = RefCell::new(MockQueries::default()); }
    fn unsupported() -> InfoQuery {
        InfoQuery { status: QueryStatus { result: SOCKET_ERROR, error: WSAEOPNOTSUPP, bytes: 0, duration_us: 5 }, value: None }
    }
    fn info_query(info: TcpInfo) -> InfoQuery {
        InfoQuery { status: QueryStatus { result: 0, error: 0, bytes: std::mem::size_of::<TCP_INFO_v0>() as u32, duration_us: 3 }, value: Some(info) }
    }
    fn fake_queries(socket: SOCKET, nodelay: bool, info: bool) -> TcpQueries {
        let queries = QUERIES.with_borrow_mut(|q| {
            q.calls.push((socket, nodelay, info));
            TcpQueries { nodelay: nodelay.then_some(NoDelayQuery {
                status: QueryStatus { result: 0, error: 0, bytes: 4, duration_us: 2 }, value: Some(false) }),
                info: info.then(|| q.info.pop_front().unwrap_or_else(unsupported)) }
        });
        unsafe { WSASetLastError(54321); }
        queries
    }
    fn test_state(sender: SyncSender<Record>) -> State {
        QUERIES.with_borrow_mut(|q| *q = MockQueries::default());
        State { query_tcp: fake_queries, ..State::new(sender) }
    }
    fn query_values(values: impl IntoIterator<Item = InfoQuery>) {
        QUERIES.with_borrow_mut(|q| q.info.extend(values));
    }
    fn tcp_info(out: u64, input: u64, flight: u32) -> TcpInfo {
        TcpInfo { state: TCPSTATE_ESTABLISHED, bytes_out: out, bytes_in: input, in_flight: flight, connection_ms: 10,
            mss: 1460, rtt_us: 250_000, min_rtt_us: 240_000, cwnd: 14600, snd_wnd: 8192, rcv_wnd: 8192, ..TcpInfo::default() }
    }

    unsafe fn mark() { WSASetLastError(12345); }
    unsafe extern "system" fn fake_socket(a: i32, k: i32, p: i32) -> SOCKET {
        assert_eq!((a, k, p), (2, 1, 6)); mark(); 19
    }
    unsafe extern "system" fn fake_connect(s: SOCKET, p: *const SOCKADDR, n: i32) -> i32 {
        assert_eq!((s, p as usize, n), (19, 1, 16)); mark(); -1
    }
    unsafe extern "system" fn fake_dns(p: *const u8) -> *mut HOSTENT {
        assert_eq!(p as usize, 1); mark(); 1usize as *mut HOSTENT
    }
    unsafe extern "system" fn fake_send(s: SOCKET, p: *const u8, n: i32, f: i32) -> i32 {
        assert_eq!((s, p as usize, n, f), (19, 1, 73, 7)); mark(); 11
    }
    unsafe extern "system" fn fake_recv(s: SOCKET, p: *mut u8, n: i32, f: i32) -> i32 {
        assert_eq!((s, p as usize, n, f), (19, 2, 80, 3)); mark(); 17
    }
    unsafe extern "system" fn fake_error(s: SOCKET, l: i32, o: i32, p: *mut u8, n: *mut i32) -> i32 {
        assert_eq!((s, l, o, *n), (19, SOL_SOCKET, SO_ERROR, 4));
        p.cast::<i32>().write(0); mark(); 0
    }
    unsafe extern "system" fn fake_close(s: SOCKET) -> i32 { assert_eq!(s, 19); mark(); 0 }
    unsafe extern "system" fn fake_select(n: i32, r: *mut FD_SET, w: *mut FD_SET, e: *mut FD_SET, t: *const TIMEVAL) -> i32 {
        assert_eq!((n, r as usize, w as usize, e as usize, t as usize), (99, 1, 2, 3, 4));
        mark(); SOCKET_ERROR
    }
    fn errors_unchanged() {
        let e = Errors::capture();
        assert_eq!((e.win32, e.winsock), (12345, 12345));
    }

    #[test]
    fn nodelay_decodes_provider_boolean_widths_and_rejects_incomplete_results() {
        assert_eq!(socket_bool(0, 1, 0), Some(false));
        assert_eq!(socket_bool(0, 1, 1), Some(true));
        assert_eq!(socket_bool(0, 1, 0x100), Some(false));
        assert_eq!(socket_bool(0, 4, 0), Some(false));
        assert_eq!(socket_bool(0, 4, 0x100), Some(true));
        for width in [-1, 0, 2, 3, 5, i32::MAX] {
            assert_eq!(socket_bool(0, width, 1), None);
        }
        for width in [1, 4] { assert_eq!(socket_bool(SOCKET_ERROR, width, 1), None); }
    }

    #[test]
    fn all_abi_wrappers_forward_invalid_payload_pointers_without_reading_them() {
        let (tx, rx) = mpsc::sync_channel(16);
        assert!(STATE.set(test_state(tx)).is_ok());
        let state = STATE.get().unwrap();
        state.enabled.store(true, Ordering::Release);
        let targets = [fake_socket as *const () as usize, fake_connect as *const () as usize,
            fake_dns as *const () as usize, fake_send as *const () as usize, fake_recv as *const () as usize,
            fake_error as *const () as usize, fake_close as *const () as usize, fake_select as *const () as usize];
        for (slot, target) in ORIGINAL.iter().zip(targets) { slot.store(target, Ordering::Release); }
        unsafe {
            assert_eq!(socket(2, 1, 6), 19); errors_unchanged();
            assert_eq!(connect(19, 1usize as _, 16), -1); errors_unchanged();
            assert_eq!(dns(1usize as _) as usize, 1); errors_unchanged();
            assert_eq!(send(19, 1usize as _, 73, 7), 11); errors_unchanged();
            assert_eq!(recv(19, 2usize as _, 80, 3), 17); errors_unchanged();
            let (mut value, mut length) = (-1i32, 4);
            assert_eq!(socket_error(19, SOL_SOCKET, SO_ERROR, (&mut value as *mut i32).cast(), &mut length), 0);
            errors_unchanged(); assert_eq!(value, 0);
            assert_eq!(select(99, 1usize as _, 2usize as _, 3usize as _, 4usize as _), SOCKET_ERROR); errors_unchanged();
            assert_eq!(close(19), 0); errors_unchanged();
        }
        let all: Vec<_> = rx.try_iter().collect();
        let records: Vec<_> = all.iter().filter(|r| r.tcp.is_none()).collect();
        assert_eq!(records.len(), 7);
        assert_eq!(records[5].socket_error, Some(0));
        assert_eq!(records[3].generation, records[0].generation);
        // INFO failed on the first real positive send. The native SO_ERROR
        // success still permits one NODELAY read, with no extra SO_ERROR call.
        QUERIES.with_borrow(|q| assert_eq!(q.calls, [(19, false, true), (19, true, false)]));
        Recording.stop();
        // No slot/original is invalidated by stop or by dropping the guard.
        drop(Recording);
        unsafe { assert_eq!(send(19, 1usize as _, 73, 7), 11); }
        errors_unchanged();
        unsafe { assert_eq!(select(99, 1usize as _, 2usize as _, 3usize as _, 4usize as _), SOCKET_ERROR); }
        errors_unchanged();
        assert!(rx.try_recv().is_err());
        assert!(ORIGINAL.iter().all(|original| original.load(Ordering::Acquire) != 0));
    }

    #[test]
    fn forwarded_result_and_error_survive_failed_metadata_read() {
        let (tx, rx) = mpsc::sync_channel(2);
        let state = test_state(tx);
        state.enabled.store(true, Ordering::Release);
        unsafe { WSASetLastError(12345); }
        let incoming = Errors::capture();
        let result = forward_with(&state, Op::Connect, 9, 0, || {
            let seen = Errors::capture();
            assert_eq!((seen.win32, seen.winsock), (incoming.win32, incoming.winsock));
            unsafe { WSASetLastError(WSAEWOULDBLOCK); }
            SOCKET_ERROR
        }, i64::from, |_| (port(std::ptr::null(), 16), None));
        let seen = Errors::capture();
        assert_eq!(result, SOCKET_ERROR);
        assert_eq!((seen.win32, seen.winsock), (WSAEWOULDBLOCK as u32, WSAEWOULDBLOCK));
        let record = rx.try_recv().unwrap();
        assert_eq!(record.error, WSAEWOULDBLOCK);
        assert!(record.port.is_none());
    }

    #[test]
    fn pending_noise_is_omitted_and_queue_overflow_is_counted() {
        let (tx, rx) = mpsc::sync_channel(1);
        let state = test_state(tx);
        state.enabled.store(true, Ordering::Release);
        forward_with(&state, Op::Recv, 4, 80, || { unsafe { WSASetLastError(WSAEWOULDBLOCK) }; -1 }, i64::from, |_| panic!("pending needs no metadata"));
        assert!(rx.try_recv().is_err());
        for _ in 0..3 { forward_with(&state, Op::Send, 4, 0, || 0, i64::from, |_| (None, None)); }
        assert_eq!(rx.try_recv().unwrap().result, 0);
        assert_eq!(state.lost.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn stopping_recording_preserves_forwarding_and_inflight_completion() {
        let (tx, rx) = mpsc::sync_channel(2);
        let state = test_state(tx);
        state.enabled.store(true, Ordering::Release);
        assert_eq!(forward_with(&state, Op::Recv, 7, 24, || { state.stop(); 24 }, i64::from, |_| (None, None)), 24);
        assert!(rx.try_recv().is_err());
        assert!(state.done.load(Ordering::Acquire));
        assert_eq!(forward_with(&state, Op::Close, 7, 0, || 0, i64::from, |_| (None, None)), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reused_handles_get_new_generations_and_optional_ports_are_guarded() {
        let mut handles = Handles::new();
        assert_eq!(handles.generation(8, Op::Socket(2, 1, 0), true), Some(1));
        assert_eq!(handles.generation(8, Op::Connect, false), Some(1));
        assert_eq!(handles.generation(8, Op::Close, true), Some(1));
        assert_eq!(handles.generation(8, Op::Socket(2, 1, 0), true), Some(2));
        let address = [2u8, 0, 0xc8, 0x14];
        assert_eq!(port(address.as_ptr().cast(), 4), Some(51220));
        assert_eq!(port(address.as_ptr().cast(), 2), None);
        assert_eq!(port(std::ptr::null(), 16), None);
    }

    fn fd_set(sockets: &[SOCKET]) -> FD_SET {
        let mut set = FD_SET { fd_count: sockets.len() as u32, fd_array: [0; 64] };
        set.fd_array[..sockets.len()].copy_from_slice(sockets);
        set
    }
    fn readiness_state() -> (State, mpsc::Receiver<Record>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let state = test_state(tx);
        state.enabled.store(true, Ordering::Release);
        (state, rx)
    }

    #[test]
    fn select_snapshots_input_before_output_mutation_and_preserves_errors() {
        let (state, rx) = readiness_state();
        let mut reads = fd_set(&[11, 12]);
        let mut writes = fd_set(&[11]);
        unsafe { mark(); }
        // Original select clears pending sockets from both output sets.
        let result = select_with(&state, &reads, &writes, || {
            errors_unchanged();
            reads.fd_count = 0;
            writes.fd_count = 0;
            unsafe { mark(); }
            0
        });
        assert_eq!(result, 0);
        errors_unchanged();
        assert!(rx.try_recv().is_err(), "pending polls produce no records");
        reads = fd_set(&[11, 12]);
        writes = fd_set(&[11]);
        assert_eq!(select_with(&state, &reads, &writes, || {
            errors_unchanged();
            reads = fd_set(&[12]);
            writes.fd_count = 0;
            unsafe { mark(); }
            1
        }), 1);
        errors_unchanged();
        assert_eq!(reads.fd_array[0], 12, "the original's output remains intact");
        let record = rx.try_recv().unwrap();
        let summary = record.readiness.unwrap();
        assert_eq!(summary.reason, Summary::Ready);
        assert_eq!((summary.stats.polls, summary.stats.pending, summary.stats.ready), (2, 1, 1));
        assert!(summary.stats.first_ready.is_some() && summary.stats.pending_since.is_some());
        assert!(rx.try_recv().is_err());
        let handles = state.handles.lock().unwrap();
        let pending = handles.entries.iter().flatten().find(|h| h.socket == 11).unwrap();
        assert_eq!((pending.readiness[0].pending, pending.readiness[1].pending), (2, 2));
    }

    #[test]
    fn select_guarded_reads_bound_counts_and_forward_invalid_pointers() {
        assert_eq!(read_fd_set(std::ptr::null()).unwrap().count, 0);
        assert!(read_fd_set(1usize as _).is_none());
        let oversized = FD_SET { fd_count: 65, fd_array: [0; 64] };
        assert!(read_fd_set(&oversized).is_none());
        let valid = fd_set(&[1, 2, 3]);
        let copied = read_fd_set(&valid).unwrap();
        assert_eq!(&copied.values[..copied.count], &[1, 2, 3]);
        let (state, rx) = readiness_state();
        unsafe { mark(); }
        assert_eq!(select_with(&state, 1usize as _, &valid, || {
            errors_unchanged(); // guarded reads did not leak their last error
            unsafe { WSASetLastError(10022); }
            SOCKET_ERROR
        }), SOCKET_ERROR);
        assert_eq!(Errors::capture().winsock, 10022);
        assert_eq!(Errors::capture().win32, 10022);
        assert_eq!(state.lost.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
        assert!(state.handles.lock().unwrap().entries.iter().flatten().all(|h| h.readiness[1].errors == 1));
    }

    #[test]
    fn readiness_transitions_are_bounded_and_close_stop_summarize_all_polls() {
        let (state, rx) = readiness_state();
        let set = read_fd_set(&fd_set(&[17])).unwrap();
        let input = [Some(set), Some(Sockets { values: [0; 64], count: 0 })];
        for cycle in 0..20 {
            state.selected(&input, &[None, None], 0, cycle * 100, 2, 1, 0);
            state.selected(&input, &input, 1, cycle * 100 + 40, 3, 1, 0);
        }
        let ready: Vec<_> = rx.try_iter().collect();
        assert_eq!(ready.len(), 8, "bounded transitions, not per-poll logging");
        forward_with(&state, Op::Close, 17, 0, || 0, i64::from, |_| (None, None));
        let summary = rx.try_recv().unwrap().readiness.unwrap();
        assert_eq!(summary.reason, Summary::Close);
        assert_eq!((summary.stats.polls, summary.stats.pending, summary.stats.ready, summary.stats.transitions), (40, 20, 20, 20));
        assert_eq!((summary.stats.max_gap_us, summary.stats.max_call_us), (60, 3));
        assert_eq!(summary.stats.first_ready, Some(40));
        let closed = rx.try_recv().unwrap();
        assert_eq!(closed.op, Op::Close);
        state.selected(&input, &[None, None], 0, 3000, 1, 1, 0);
        state.stop();
        let stopped = rx.try_recv().unwrap();
        assert_eq!(stopped.readiness.unwrap().reason, Summary::Stop);
        assert_ne!(stopped.generation, closed.generation, "reused handles start new stats");
        state.stop();
        assert!(rx.try_recv().is_err(), "stopping twice emits no duplicate summaries");
        assert_eq!(select_with(&state, 1usize as _, 2usize as _, || { unsafe { mark(); } 0 }), 0);
        errors_unchanged();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn readiness_capacity_contention_and_inflight_stop_never_block_forwarding() {
        let (state, rx) = readiness_state();
        let mut reads = Sockets { values: [0; 64], count: 64 };
        let mut writes = reads;
        for i in 0..64 { reads.values[i] = i + 1; writes.values[i] = i + 65; }
        state.selected(&[Some(reads), Some(writes)], &[None, None], 0, 1, 0, 1, 0);
        assert_eq!(state.handles.lock().unwrap().entries.iter().flatten().count(), 64);
        assert_eq!(state.lost.load(Ordering::Relaxed), 64);
        let guard = state.handles.lock().unwrap();
        state.selected(&[Some(reads), None], &[None, None], 0, 2, 0, 1, 0);
        assert_eq!(state.lost.load(Ordering::Relaxed), 65);
        drop(guard);
        assert_eq!(select_with(&state, std::ptr::null(), std::ptr::null(), || { state.stop(); unsafe { mark(); } 0 }), 0);
        errors_unchanged();
        assert_eq!(rx.try_iter().count(), 64, "one final read summary per tracked socket");
    }

    fn observed(state: &State, op: Op, socket: SOCKET, requested: i32, result: i32, socket_error: Option<i32>) {
        unsafe { mark(); }
        assert_eq!(forward_with(state, op, socket, requested, || { errors_unchanged(); unsafe { mark(); } result },
            i64::from, |_| (None, socket_error)), result);
        errors_unchanged();
    }

    #[test]
    fn tcp_uses_actual_partial_returns_and_a_pre_traffic_baseline() {
        let (state, rx) = readiness_state();
        query_values([tcp_info(100, 50, 0), tcp_info(140, 50, 40), tcp_info(140, 50, 40), tcp_info(164, 90, 0)].map(info_query));
        observed(&state, Op::Socket(2, 1, 0), 23, 0, 23, None);
        observed(&state, Op::Connect, 23, 0, SOCKET_ERROR, None);
        observed(&state, Op::Error, 23, 0, 0, Some(10035));
        QUERIES.with_borrow(|q| assert!(q.calls.is_empty(), "pending is not a positive connection"));
        observed(&state, Op::Error, 23, 0, 0, Some(0));
        observed(&state, Op::Send, 23, 73, 40, None);
        observed(&state, Op::Send, 23, 64, 24, None);
        observed(&state, Op::Send, 23, 80, SOCKET_ERROR, None);
        observed(&state, Op::Send, 23, 80, 0, None);
        observed(&state, Op::Recv, 23, 80, SOCKET_ERROR, None);
        observed(&state, Op::Recv, 23, 80, 17, None);
        let tcp: Vec<_> = rx.try_iter().filter_map(|r| r.tcp).collect();
        assert_eq!(tcp.len(), 4);
        assert_eq!(tcp[0].reason, TcpReason::Connected);
        assert_eq!((tcp[0].baseline.unwrap().info.bytes_out, tcp[0].baseline.unwrap().info.bytes_in), (100, 50));
        assert_eq!((tcp[1].accepted, tcp[1].out_delta, tcp[1].unsent_estimate), (40, Some(40), Some(0)));
        assert_eq!((tcp[2].accepted, tcp[2].out_delta, tcp[2].unsent_estimate), (64, Some(40), Some(24)));
        assert_eq!((tcp[3].accepted, tcp[3].returned, tcp[3].in_delta, tcp[3].unsent_estimate), (64, 17, Some(40), Some(0)));
        assert!(tcp.iter().all(|t| t.coherent && t.nodelay == Some(false)));
        QUERIES.with_borrow(|q| assert_eq!(q.calls, [(23, true, true), (23, false, true), (23, false, true), (23, false, true)]));
        assert_eq!(state.lost.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tcp_pending_samples_are_rate_limited_and_select_keeps_errors_and_sets() {
        let (state, rx) = readiness_state();
        query_values(std::iter::repeat_n(info_query(tcp_info(0, 0, 0)), 8));
        observed(&state, Op::Socket(2, 1, 0), 31, 0, 31, None);
        observed(&state, Op::Connect, 31, 0, 0, None);
        let generation = {
            let mut handles = state.handles.lock().unwrap();
            let h = handles.entries.iter_mut().flatten().find(|h| h.socket == 31).unwrap();
            h.tcp.last_sample_us = Some(0);
            h.generation
        };
        rx.try_iter().for_each(drop);
        let set = read_fd_set(&fd_set(&[31])).unwrap();
        for at in (0..100_000).step_by(1000) {
            let pending = state.selected(&[Some(set), Some(set)], &[None, None], 0, at, 1, 1, 0);
            assert_eq!(pending.iter().flatten().count(), 1, "one sample per socket even in both sets");
            for request in pending.into_iter().flatten() { state.sample_tcp(request, 1, at); }
        }
        let samples: Vec<_> = rx.try_iter().collect();
        assert_eq!(samples.iter().map(|r| r.at_us).collect::<Vec<_>>(), [20_000, 40_000, 60_000, 80_000]);
        assert!(samples.iter().all(|r| r.generation == generation && r.tcp.unwrap().reason == TcpReason::Pending));
        // A positive native call is sampled immediately, independently of the
        // pending-poll timer. The select path restores both incoming/outgoing
        // errors even when its query deliberately changes them.
        observed(&state, Op::Send, 31, 9, 1, None);
        {
            let mut handles = state.handles.lock().unwrap();
            handles.entries.iter_mut().flatten().find(|h| h.socket == 31).unwrap().tcp.last_sample_us = None;
        }
        let mut reads = fd_set(&[31]);
        unsafe { mark(); }
        assert_eq!(select_with(&state, &reads, std::ptr::null(), || {
            errors_unchanged(); reads.fd_count = 0; unsafe { mark(); } 0
        }), 0);
        errors_unchanged();
        assert_eq!(reads.fd_count, 0);
        QUERIES.with_borrow(|q| {
            assert_eq!(q.calls.len(), 7);
            assert_eq!(q.calls.iter().filter(|(_, nodelay, _)| *nodelay).count(), 1);
        });
    }

    #[test]
    fn tcp_failed_or_short_queries_stop_until_a_new_socket_generation() {
        let (state, rx) = readiness_state();
        observed(&state, Op::Socket(2, 1, 0), 41, 0, 41, None);
        observed(&state, Op::Connect, 41, 0, 0, None);
        observed(&state, Op::Send, 41, 90, 12, None);
        observed(&state, Op::Recv, 41, 80, 7, None);
        observed(&state, Op::Error, 41, 0, 0, Some(0));
        let generation = state.handles.lock().unwrap().entries[0].unwrap().generation;
        state.sample_tcp(TcpRequest { socket: 41, generation, reason: TcpReason::Pending }, 1, 100_000);
        QUERIES.with_borrow(|q| assert_eq!(q.calls, [(41, true, true)]));
        let first = rx.try_iter().find_map(|r| r.tcp).unwrap();
        let failure = first.queries.info.unwrap();
        assert_eq!((failure.status.result, failure.status.error, failure.status.bytes, failure.status.duration_us), (-1, WSAEOPNOTSUPP, 0, 5));
        assert!(first.baseline.is_none() && first.unsent_estimate.is_none());
        let counts = state.handles.lock().unwrap().entries[0].unwrap().tcp;
        assert_eq!((counts.accepted, counts.returned), (12, 7));
        observed(&state, Op::Close, 41, 0, 0, None);
        observed(&state, Op::Socket(2, 1, 0), 41, 0, 41, None);
        query_values([InfoQuery { status: QueryStatus { result: 0, error: 0, bytes: 8, duration_us: 4 }, value: None }]);
        observed(&state, Op::Connect, 41, 0, 0, None);
        observed(&state, Op::Send, 41, 8, 8, None);
        let second = rx.try_iter().find(|r| r.tcp.is_some()).unwrap();
        assert_ne!(second.generation, generation);
        assert_eq!(second.tcp.unwrap().queries.info.unwrap().status.bytes, 8);
        assert!(second.tcp.unwrap().baseline.is_none());
        QUERIES.with_borrow(|q| assert_eq!(q.calls.len(), 2, "no repeated unsupported or malformed queries"));
    }

    #[test]
    fn tcp_backlog_stays_unknown_without_complete_coherent_counter_evidence() {
        let baseline = TcpBaseline { at_us: 0, info: tcp_info(100, 50, 0), coverage: 0 };
        let tracking = TcpTracking { accepted: 104, baseline: Some(baseline), ..TcpTracking::default() };
        let valid = tcp_info(180, 50, 40);
        let report = |t: TcpTracking, info: TcpInfo, coherent, coverage| t.report(TcpReason::Send,
            TcpQueries { nodelay: None, info: Some(info_query(info)) }, coherent, coverage);
        assert_eq!(report(tracking, valid, true, 0).unsent_estimate, Some(24));
        assert!(report(TcpTracking { baseline: None, ..tracking }, valid, true, 0).unsent_estimate.is_none());
        assert!(report(tracking, valid, false, 0).unsent_estimate.is_none());
        assert!(report(tracking, valid, true, 1).unsent_estimate.is_none());
        for info in [TcpInfo { bytes_out: 99, ..valid }, TcpInfo { bytes_out: 205, ..valid }, TcpInfo { bytes_in: 49, ..valid },
            TcpInfo { in_flight: 81, ..valid }, TcpInfo { retrans: 1, ..valid }, TcpInfo { fast_retrans: 1, ..valid },
            TcpInfo { timeouts: 1, ..valid }, TcpInfo { syn_retrans: 1, ..valid }, TcpInfo { connection_ms: 9, ..valid },
            TcpInfo { state: 0, ..valid }] {
            assert!(report(tracking, info, true, 0).unsent_estimate.is_none(), "{info:?}");
        }
        assert!(report(TcpTracking { returned: 1, ..tracking }, valid, true, 0).unsent_estimate.is_none());
    }

    #[test]
    fn tcp_missing_calls_and_active_calls_prevent_a_false_baseline() {
        let (state, rx) = readiness_state();
        query_values([info_query(tcp_info(0, 0, 0)); 4]);
        observed(&state, Op::Socket(2, 1, 0), 51, 0, 51, None);
        let held = state.handles.lock().unwrap();
        observed(&state, Op::Send, 51, 40, 40, None);
        drop(held);
        assert_eq!(state.coverage.load(Ordering::SeqCst), 1);
        observed(&state, Op::Connect, 51, 0, 0, None);
        assert!(rx.try_iter().find_map(|r| r.tcp).unwrap().baseline.is_none());
        observed(&state, Op::Close, 51, 0, 0, None);
        observed(&state, Op::Socket(2, 1, 0), 51, 0, 51, None);
        let active = NativeCall::begin(&state);
        observed(&state, Op::Connect, 51, 0, 0, None);
        drop(active);
        let t = rx.try_iter().find_map(|r| r.tcp).unwrap();
        assert!(!t.coherent && t.baseline.is_none() && t.unsent_estimate.is_none());
        // Successful I/O alone has no pre-traffic baseline, even if the kernel
        // counter is lower than accepted bytes and there were no dropped logs.
        observed(&state, Op::Send, 61, 80, 64, None);
        let t = rx.try_iter().find_map(|r| r.tcp).unwrap();
        assert_eq!(t.accepted, 64);
        assert!(t.baseline.is_none() && t.unsent_estimate.is_none());
    }
}
