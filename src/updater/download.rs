//! The POLP download pool (`stage`, design: docs/downloader-design.md). One queue of 64 KB
//! chunks, largest download first, is served by up to `MAX_DOWNLOAD_WORKERS` plain connections,
//! each keeping a self-sizing pipeline of requests in flight (`Pace`). Accepted chunks stream to
//! one store thread (`patch-store`) that writes them into memory (small transfers) or a
//! preallocated `.xi-part` file whose header records a durable resume frontier. Each group is
//! decoded, verified and staged as soon as its last chunk lands, while the rest keeps
//! downloading; the install itself is still one transaction after staging (`commit`).
use super::*;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::os::windows::fs::FileExt;
use std::sync::atomic::AtomicU64;

/// Requests in flight per connection at most (1 MiB of 64 KB chunks). `1` turns pipelining off:
/// every connection then keeps one request in flight, as stock does; the shared chunk queue,
/// streaming and resume still apply.
pub(super) const MAX_DEPTH: usize = 16;
/// How much a connection may have requested ahead, in its own measured delivery time. Above the
/// round trip (145 ms from the US, ~300 ms from Europe); bounds the tail, the cancel waste and
/// the out-of-order window.
const LOOKAHEAD: Duration = Duration::from_secs(1);
/// Between connection opens while unassigned work remains: 16 connections in 1.5 s, and a small
/// patch finishes on the first few.
const CONNECT_SPACING: Duration = Duration::from_millis(100);
/// Before replacing a broken connection or retrying a refused connect: doubling from the first
/// value to the second, back to the first once any chunk has completed since.
pub(super) const BREAK_BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(30));
/// A refused connect while others work holds the connection limit at the live count this long.
const REFUSAL_HOLD: Duration = Duration::from_secs(30);
/// Until one pipelined reply has completed in a run, a connection with more than one request in
/// flight that hears nothing this long is taken as a server dropping pipelined requests.
const PIPELINE_PROBATION: Duration = Duration::from_secs(5);
/// Silence on a download connection that counts as dead. A slow reply that keeps trickling is
/// never cut off (the probe saw 64 KB take 8.8 s).
const FRAME_IDLE: Duration = Duration::from_secs(30);
/// Times a chunk may be at the head of a broken connection before the stage fails.
const CHUNK_ATTEMPTS: u8 = 3;
/// A transfer this large streams to a part file; smaller ones (418 of 459 files in a routine
/// patch are one chunk) stay in memory until they are finished.
const PART_MIN: u64 = 4 * polp::CHUNK_LEN as u64;
/// New contiguous data per part file between durable frontier records (one `sync_data` each).
pub(super) const CHECKPOINT: u64 = 16 << 20;
/// Accepted frames queued for the store (~4 MiB): backpressure from disk to network.
const STORE_FRAMES: usize = 64;
/// Completed transfers queued for the supervisor; a finish backlog blocks the store, then the
/// connections, then TCP.
const COMPLETED_QUEUE: usize = 64;
/// The largest file reply frame: one 64 KB chunk, its echoed path and the header.
const FILE_FRAME_CAP: usize = 128 * 1024;
/// A part file: this magic, then the frontier (u64 LE), then the payload at `PART_HEADER`.
const PART_MAGIC: [u8; 8] = *b"XIPART\x01\0";
pub(super) const PART_HEADER: u64 = 16;

/// Groups decoded, verified and staged at once (and threads checking reusable files).
fn finish_workers() -> usize {
    (std::thread::available_parallelism().map_or(1, |n| n.get()) / 2).clamp(1, 4)
}

/// The pool's knobs. Production uses the default; tests and the live benchmark change them.
#[derive(Debug, Clone, Copy)]
pub(super) struct Tuning {
    pub connections: usize,
    pub depth: usize,
    pub spacing: Duration,
    pub probation: Duration,
    pub idle: Duration,
}
impl Default for Tuning {
    fn default() -> Self {
        Self { connections: MAX_DOWNLOAD_WORKERS, depth: MAX_DEPTH, spacing: CONNECT_SPACING,
            probation: PIPELINE_PROBATION, idle: FRAME_IDLE }
    }
}

/// What a run did on the wire, for the live benchmark and tests.
#[derive(Debug, Default)]
pub(super) struct Stats {
    /// Requests written.
    pub requests: AtomicU64,
    /// Payload bytes received.
    pub bytes: AtomicU64,
    /// Sum over replies of the requests in flight when each reply completed.
    pub in_flight: AtomicU64,
    /// Connections that broke or were refused.
    pub reconnects: AtomicU64,
    /// Runs of this stage that fell back to one request per connection.
    pub fallbacks: AtomicU64,
    /// Why each group downloaded again from 0 failed to decode or verify.
    pub redownloads: Mutex<Vec<String>>,
    /// Request-to-reply time of each request written onto an idle connection.
    pub solo: Mutex<Vec<Duration>>,
}

/// One server file: a Direct payload, or one delta of a chain.
#[derive(Debug, Clone)]
struct Transfer {
    id: u32,
    path: String,
    len: u64,
    /// Its part file, when it streams to disk (`PART_MIN`).
    part: Option<PathBuf>,
    /// Its group's index in `Downloads`.
    progress: usize,
    /// Asked one request at a time, never pipelined: a fresh download of it failed to decode or
    /// verify, so it is fetched again without the pipeline while the rest of the run keeps it.
    solo: bool,
}

/// One request. `attempts` counts the times it was at the head of a broken connection.
#[derive(Debug)]
struct Chunk { transfer: Arc<Transfer>, offset: u32, count: u32, attempts: u8 }

/// The shared chunk queue: requeued chunks first, then the next chunk of the front transfer.
/// Chunks are cut on demand, never materialised up front.
#[derive(Default)]
struct Queue {
    retry: VecDeque<Chunk>,
    /// (transfer, next offset), largest group first.
    pending: VecDeque<(Arc<Transfer>, u64)>,
}
impl Queue {
    /// The next chunk; lone: the connection has nothing in flight (a `solo` chunk needs that).
    fn pop(&mut self, alone: bool) -> Option<Chunk> {
        let solo = self.retry.front().map(|chunk| &chunk.transfer).or(self.pending.front().map(|(transfer, _)| transfer))?.solo;
        if solo && !alone { return None; }
        if let Some(chunk) = self.retry.pop_front() { return Some(chunk); }
        let (offset, count, last) = {
            let (transfer, next) = self.pending.front_mut()?;
            let offset = *next;
            let count = (transfer.len - offset).min(polp::CHUNK_LEN as u64);
            *next += count;
            (offset, count, *next >= transfer.len)
        };
        let transfer = if last { self.pending.pop_front()?.0 } else { self.pending.front()?.0.clone() };
        Some(Chunk { transfer, offset: offset as u32, count: count as u32, attempts: 0 })
    }
    fn has_work(&self) -> bool { !self.retry.is_empty() || !self.pending.is_empty() }
}

/// How much one connection may have in flight: what it delivered over the last ~1-2 s, scaled
/// to `LOOKAHEAD`. Nothing is pipelined before its first reply; a slow flow keeps one request.
/// TCP already controls each flow's rate, so this only decides how much to queue behind it.
#[derive(Debug, Clone, Copy)]
struct Pace { since: Instant, bytes: u64 }
impl Pace {
    fn new(now: Instant) -> Self { Self { since: now, bytes: 0 } }
    fn credit(&mut self, now: Instant, n: u64) {
        let age = now.saturating_duration_since(self.since);
        if age > 2 * LOOKAHEAD {
            // Forget the older half.
            self.bytes /= 2;
            self.since += age / 2;
        }
        self.bytes += n;
    }
    /// Bytes that may be in flight.
    fn window(&self, now: Instant) -> u64 {
        let age = now.saturating_duration_since(self.since).as_secs_f64().max(0.001);
        (self.bytes as f64 * LOOKAHEAD.as_secs_f64() / age) as u64
    }
}

/// Everything the connections share.
struct Shared {
    product: Product,
    server: Arc<str>,
    op: Operation,
    downloads: Downloads,
    tuning: Tuning,
    queue: Mutex<Queue>,
    /// Set by any protocol surprise on a pipeline: every connection keeps one request in flight.
    no_pipeline: AtomicBool,
    /// A pipelined reply completed in this run (ends `PIPELINE_PROBATION`).
    pipelined_ok: AtomicBool,
    /// Chunks completed (resets `BREAK_BACKOFF`).
    delivered: AtomicU64,
    stats: Arc<Stats>,
}
impl Shared {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> { self.queue.lock().unwrap_or_else(|e| e.into_inner()) }
    fn depth_cap(&self) -> usize {
        if self.no_pipeline.load(Ordering::Acquire) { 1 } else { self.tuning.depth.max(1) }
    }
    /// A connection stopped with `flight` unanswered: the head is charged an attempt (a chunk
    /// out of attempts fails the stage); the rest go back uncharged, in order.
    fn broken(&self, flight: VecDeque<(Chunk, Instant, bool)>, error: String) -> End {
        if self.op.cancelled() { return End::Fatal(error); }
        let mut chunks = flight.into_iter().map(|(chunk, ..)| chunk);
        if let Some(mut head) = chunks.next() {
            head.attempts += 1;
            if head.attempts >= CHUNK_ATTEMPTS {
                return End::Fatal(tf("{path}: {error}", &[("path", &head.transfer.path), ("error", &error)]));
            }
            self.queue().retry.extend(std::iter::once(head).chain(chunks));
        }
        End::Broken
    }
    /// A server that does not answer a pipeline as stock's server does: one request in flight
    /// on every connection from now on, and this connection's chunks go back uncharged.
    fn fall_back(&self, flight: VecDeque<(Chunk, Instant, bool)>) -> End {
        if !self.no_pipeline.swap(true, Ordering::AcqRel) { self.stats.fallbacks.fetch_add(1, Ordering::Relaxed); }
        self.queue().retry.extend(flight.into_iter().map(|(chunk, ..)| chunk));
        End::Closed
    }
}

/// How a connection ended.
#[derive(Debug)]
enum End {
    /// Out of work; closed like stock.
    Done,
    /// Closed for the fallback to one request in flight; its chunks were requeued.
    Closed,
    /// Broke; its chunks were requeued.
    Broken,
    /// The connect failed.
    Refused(String),
    /// The stage fails (a chunk out of attempts, a cancel, the store gone).
    Fatal(String),
}

/// An accepted chunk on its way to the store: the frame buffer itself, no copy.
struct Write { transfer: Arc<Transfer>, offset: u64, frame: Vec<u8>, data: Range<usize> }

/// One connection: top up the pipeline, read one reply, check it against the head request,
/// hand it to the store; until the queue is empty and nothing is in flight.
async fn connection(shared: Arc<Shared>, store: tokio::sync::mpsc::Sender<Write>) -> End {
    let op = &shared.op;
    let mut session = match Session::connect_download(&shared.server, op).await {
        Ok(session) => session,
        Err(error) if op.cancelled() => return End::Fatal(error),
        Err(error) => return End::Refused(error),
    };
    let tags = shared.product.tags();
    // (chunk, when written, written behind an unanswered request).
    let mut flight: VecDeque<(Chunk, Instant, bool)> = VecDeque::new();
    let (mut flight_bytes, mut pace) = (0u64, None::<Pace>);
    loop {
        let now = Instant::now();
        let (cap, window) = (shared.depth_cap(), pace.map_or(0, |pace| pace.window(now)));
        let mut batch = Vec::new();
        // Nothing goes behind a solo chunk.
        while flight.len() < cap && (flight.is_empty() || flight_bytes < window) && !flight.front().is_some_and(|(chunk, ..)| chunk.transfer.solo) {
            let Some(chunk) = shared.queue().pop(flight.is_empty()) else { break };
            batch.extend(polp::file_request(&tags, &chunk.transfer.path, chunk.offset, chunk.count));
            flight_bytes += chunk.count as u64;
            let pipelined = !flight.is_empty();
            flight.push_back((chunk, now, pipelined));
            shared.stats.requests.fetch_add(1, Ordering::Relaxed);
        }
        if flight.is_empty() {
            session.close().await;
            return End::Done;
        }
        if !batch.is_empty() {
            pace.get_or_insert(Pace::new(now));
            if let Err(error) = session.send(&batch, op).await { return shared.broken(flight, error); }
        }
        let probation = flight.len() > 1 && !shared.pipelined_ok.load(Ordering::Acquire);
        let idle = if probation { shared.tuning.probation } else { shared.tuning.idle };
        let frame = match session.read_frame(FILE_FRAME_CAP, idle, op, false).await {
            Ok(frame) => frame,
            Err(error) if probation && !op.cancelled() && error == t("Patch server timed out") => return shared.fall_back(flight),
            Err(error) => return shared.broken(flight, error),
        };
        let data = match polp::parse(&frame) {
            Ok(parsed) => match matches_head(parsed, &flight[0].0) {
                Ok(at) => frame.len() - at..frame.len(),
                // Not the head's reply while a pipeline was out: a server that answers
                // pipelined requests out of order or not at all.
                Err(_) if flight.len() > 1 => return shared.fall_back(flight),
                Err(error) => return shared.broken(flight, error),
            },
            Err(error) => return shared.broken(flight, error),
        };
        let (chunk, written, pipelined) = flight.pop_front().unwrap();
        let (n, now) = (chunk.count as u64, Instant::now());
        shared.stats.in_flight.fetch_add(flight.len() as u64 + 1, Ordering::Relaxed);
        shared.stats.bytes.fetch_add(n, Ordering::Relaxed);
        if pipelined {
            shared.pipelined_ok.store(true, Ordering::Release);
        } else if flight.is_empty() {
            shared.stats.solo.lock().unwrap_or_else(|e| e.into_inner()).push(now - written);
        }
        flight_bytes -= n;
        if let Some(pace) = &mut pace { pace.credit(now, n); }
        // Requests still unanswered: a close must read their replies first.
        session.broken = !flight.is_empty();
        shared.delivered.fetch_add(1, Ordering::Relaxed);
        shared.downloads.file_scoped(chunk.transfer.progress, op.clone()).advance(n, chunk.transfer.path.clone(), false);
        let write = Write { offset: chunk.offset as u64, transfer: chunk.transfer, frame, data };
        if store.send(write).await.is_err() {
            return End::Fatal(std::io::Error::from(std::io::ErrorKind::BrokenPipe).to_string());
        }
    }
}

/// A file reply's chunk length when it answers `head` (path, offset and count echoed).
fn matches_head(frame: polp::Frame<'_>, head: &Chunk) -> Result<usize, String> {
    if frame.kind != polp::kind::FILE_REPLY {
        return Err(tf("Patch server reply {kind}, expected {expected}", &[("kind", &frame.kind), ("expected", &polp::kind::FILE_REPLY)]));
    }
    let body = frame.body;
    let (offset, chunk) = polp::file_chunk(body)?;
    let name_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
    let echoed = body.get(12..12 + name_len).ok_or(t("Invalid echoed patch path"))?;
    let path = head.transfer.path.as_bytes();
    if offset != head.offset || chunk.len() != head.count as usize || echoed.split_last() != Some((&0, path))
        || body.len() != 12 + name_len + chunk.len() {
        return Err(t("Patch chunk path, offset, or size mismatch").into());
    }
    Ok(chunk.len())
}

/// A transfer's bytes while it downloads.
#[derive(Debug)]
enum Body {
    Ram(Vec<u8>),
    /// A preallocated part file; `frontier` ends the contiguous run of completed chunks from 0,
    /// `synced` is the frontier last recorded durably in the header.
    Disk { file: fs::File, path: PathBuf, frontier: u64, synced: u64 },
}
#[derive(Debug)]
struct Slot { body: Body, len: u64, done: Vec<bool>, left: u32 }

/// A completed transfer, as the store hands it on.
#[derive(Debug)]
enum Done { Ram(Vec<u8>), Part(PathBuf) }

fn write_at(file: &fs::File, mut data: &[u8], mut at: u64) -> std::io::Result<()> {
    while !data.is_empty() {
        match file.seek_write(data, at)? {
            0 => return Err(std::io::ErrorKind::WriteZero.into()),
            n => { data = &data[n..]; at += n as u64; }
        }
    }
    Ok(())
}

fn read_at(file: &fs::File, mut buffer: &mut [u8], mut at: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        match file.seek_read(buffer, at)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => { buffer = &mut buffer[n..]; at += n as u64; }
        }
    }
    Ok(())
}

/// A part file left by an earlier run: accepted when its size, magic and frontier (at most
/// `len`, on a chunk boundary or `len`) are right. Only chooses what to skip; the decoded file
/// is still verified against the manifest.
fn resume_part(path: &Path, len: u64) -> Option<(fs::File, u64)> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
    if file.metadata().ok()?.len() != PART_HEADER + len { return None; }
    let mut head = [0u8; PART_HEADER as usize];
    read_at(&file, &mut head, 0).ok()?;
    let frontier = u64::from_le_bytes(head[8..].try_into().unwrap());
    (head[..8] == PART_MAGIC && frontier <= len && (frontier % polp::CHUNK_LEN as u64 == 0 || frontier == len))
        .then_some((file, frontier))
}

impl Slot {
    /// A new transfer's slot, made by the store at its first chunk: memory below `PART_MIN`,
    /// else its part file created empty at full size, so extra disk use is bounded by the
    /// transfers in flight and a full disk fails at that file's first chunk.
    fn create(transfer: &Transfer) -> Result<Self, String> {
        let chunks = transfer.len.div_ceil(polp::CHUNK_LEN as u64) as usize;
        let Some(path) = &transfer.part else {
            let body = Body::Ram(vec![0; transfer.len as usize]);
            return Ok(Self { body, len: transfer.len, done: vec![false; chunks], left: chunks as u32 });
        };
        ensure_plain(path)?;
        let file = (|| {
            let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
            write_at(&file, &[&PART_MAGIC[..], &0u64.to_le_bytes()].concat(), 0)?;
            file.set_len(PART_HEADER + transfer.len)?;
            Ok::<_, std::io::Error>(file)
        })().map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self::disk(transfer, file, path.clone(), 0))
    }

    /// An earlier run's part file with data below its frontier (`resume_part`), and that frontier.
    fn resume(transfer: &Transfer) -> Result<Option<(Self, u64)>, String> {
        let Some(path) = &transfer.part else { return Ok(None) };
        ensure_plain(path)?;
        Ok(resume_part(path, transfer.len).filter(|(_, frontier)| *frontier > 0)
            .map(|(file, frontier)| (Self::disk(transfer, file, path.clone(), frontier), frontier)))
    }

    fn disk(transfer: &Transfer, file: fs::File, path: PathBuf, frontier: u64) -> Self {
        let chunks = transfer.len.div_ceil(polp::CHUNK_LEN as u64) as usize;
        let complete = if frontier == transfer.len { chunks } else { (frontier / polp::CHUNK_LEN as u64) as usize };
        let done = (0..chunks).map(|i| i < complete).collect();
        Self { body: Body::Disk { file, path, frontier, synced: frontier }, len: transfer.len, done, left: (chunks - complete) as u32 }
    }

    /// Store one chunk. True once every chunk is in.
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<bool, String> {
        let index = (offset / polp::CHUNK_LEN as u64) as usize;
        if self.done.get(index) != Some(&false) || offset + data.len() as u64 > self.len {
            return Ok(false);
        }
        match &mut self.body {
            Body::Ram(bytes) => bytes[offset as usize..offset as usize + data.len()].copy_from_slice(data),
            Body::Disk { file, path, .. } => write_at(file, data, PART_HEADER + offset).map_err(|e| format!("{}: {e}", path.display()))?,
        }
        self.done[index] = true;
        self.left -= 1;
        if let Body::Disk { frontier, synced, .. } = &mut self.body {
            while *frontier < self.len && self.done[(*frontier / polp::CHUNK_LEN as u64) as usize] {
                *frontier = (*frontier + polp::CHUNK_LEN as u64).min(self.len);
            }
            if *frontier - *synced >= CHECKPOINT { self.checkpoint()?; }
        }
        Ok(self.left == 0)
    }

    /// Make the data below the frontier durable, then record the frontier: a frontier on disk
    /// never covers bytes that were not flushed first.
    fn checkpoint(&mut self) -> Result<(), String> {
        let Body::Disk { file, path, frontier, synced } = &mut self.body else { return Ok(()) };
        if *frontier == *synced { return Ok(()); }
        file.sync_data().and_then(|_| write_at(file, &frontier.to_le_bytes(), 8))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        *synced = *frontier;
        Ok(())
    }

    fn take(&mut self) -> Done {
        match &mut self.body {
            Body::Ram(bytes) => Done::Ram(std::mem::take(bytes)),
            Body::Disk { path, .. } => Done::Part(path.clone()),
        }
    }
}

/// What the store tells the supervisor.
#[derive(Debug)]
enum Event { Complete { transfer: u32, body: Done }, Failed(String) }

/// The `patch-store` thread: owns every transfer's bytes, so nothing blocks a runtime worker and
/// no lock guards file state. Ends when every connection and the supervisor dropped their
/// senders, after a checkpoint of every part still open.
fn store(mut writes: tokio::sync::mpsc::Receiver<Write>, events: tokio::sync::mpsc::Sender<Event>,
    mut slots: HashMap<u32, Slot>) {
    let mut failed = false;
    while let Some(write) = writes.blocking_recv() {
        if failed { continue; }
        let id = write.transfer.id;
        let written = match slots.entry(id) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) =>
                Slot::create(&write.transfer).map(|slot| entry.insert(slot)),
        }.and_then(|slot| slot.write(write.offset, &write.frame[write.data.clone()]).map(|complete| (slot, complete)));
        match written {
            Ok((slot, true)) => {
                let body = slot.take();
                // The supervisor gone (the stage is stopping): keep the slot for the checkpoint.
                if events.blocking_send(Event::Complete { transfer: id, body }).is_ok() { slots.remove(&id); }
            }
            Ok((_, false)) => {}
            Err(error) => { failed = true; let _ = events.blocking_send(Event::Failed(error)); }
        }
    }
    for slot in slots.values_mut() { let _ = slot.checkpoint(); }
}

/// One group being downloaded.
struct Job {
    group: usize,
    action: Action,
    transfers: Vec<Arc<Transfer>>,
    bodies: Vec<Option<Done>>,
    /// Transfers not yet complete.
    open: usize,
    /// Some of its bytes came from an earlier run's part files.
    resumed: bool,
    /// Already downloaded again from 0 once after its bytes failed to decode or verify.
    restarted: bool,
    cache: PathBuf,
}

/// The server files `action` downloads for `history`, in order.
fn transfers_of(history: &FileHistory, action: &Action) -> Result<Vec<(String, u64)>, String> {
    match action {
        Action::Direct => Ok(vec![(history.current().direct.clone(), history.current().direct_len)]),
        Action::Deltas(from) => history.versions[*from..].iter()
            .map(|v| v.indirect.as_ref().map(|d| (d.path.clone(), d.len)).ok_or_else(|| t("Missing delta").to_owned()))
            .collect(),
    }
}

/// The part file of a transfer that streams to disk.
pub(super) fn part_path(staged: &Path, path: &str, len: u64) -> Option<PathBuf> {
    (len >= PART_MIN).then(|| staged.join(format!("{}.{len}.xi-part", hex_md5(path.as_bytes()))))
}

/// The verified-cache file of a group's payload.
fn cache_path(product: Product, staged: &Path, history: &FileHistory) -> PathBuf {
    let current = history.current();
    staged.join(format!("{}-{}.bin",
        hex_md5(if product == Product::Viewer { current.direct.as_bytes() } else { history.path.as_bytes() }),
        hex_md5(format!("{:?}", current.sig).as_bytes())))
}

/// A group at the start of a stage: its verified copy is reusable (`None`), or its transfers'
/// resumable part files (a slot and its frontier, each used up by the store's first write).
type Opened = Option<Vec<Option<(Slot, u64)>>>;

/// Check every group's cache and part files, `finish_workers()` at a time.
fn prepare(product: Product, staged: &Path, m: &Manifest, groups: &[StageGroup], specs: &[Vec<Arc<Transfer>>],
    op: &Operation) -> Result<Vec<Opened>, String> {
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Opened>> = groups.iter().map(|_| Mutex::new(None)).collect();
    let error = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..finish_workers().min(groups.len()) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(group) = groups.get(index) else { break };
                let result = (|| {
                    op.check_cancelled()?;
                    let history = &m.files[group.source.index];
                    let cache = cache_path(product, staged, history);
                    ensure_plain(&cache)?;
                    if cache.is_file() && signature(&cache, op)? == history.current().sig { return Ok(None); }
                    specs[index].iter().map(|transfer| Slot::resume(transfer)).collect::<Result<Vec<_>, String>>().map(Some)
                })();
                match result {
                    Ok(opened) => *results[index].lock().unwrap_or_else(|e| e.into_inner()) = opened,
                    Err(failed) => {
                        error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(failed);
                        next.store(groups.len(), Ordering::Relaxed);
                    }
                }
            });
        }
    });
    if let Some(error) = error.into_inner().unwrap_or_else(|e| e.into_inner()) { return Err(error); }
    Ok(results.into_iter().map(|cell| cell.into_inner().unwrap_or_else(|e| e.into_inner())).collect())
}

/// A completed group: decode, verify and stage it (`save`), then delete its part files. The
/// decoded bytes are verified in memory before `save` writes and flushes them, and the commit
/// verifies the staged file again before using it. Returns the decoded length.
fn finish(root: &Path, history: &FileHistory, action: &Action, bodies: Vec<Done>, cache: &Path, op: &Operation) -> Result<u64, String> {
    finish_inner(root, history, action, bodies, cache, op).map_err(|error|
        if error.contains(&history.path) { error } else { tf("{path}: {error}", &[("path", &history.path), ("error", &error)]) })
}

fn finish_inner(root: &Path, history: &FileHistory, action: &Action, bodies: Vec<Done>, cache: &Path, op: &Operation) -> Result<u64, String> {
    op.check_cancelled()?;
    let parts: Vec<PathBuf> = bodies.iter().filter_map(|body| match body { Done::Part(path) => Some(path.clone()), Done::Ram(_) => None }).collect();
    let payload = |body: Done| -> Result<Vec<u8>, String> {
        match body {
            Done::Ram(bytes) => Ok(bytes),
            Done::Part(path) => (|| {
                let mut file = fs::File::open(&path)?;
                let len = file.metadata()?.len().saturating_sub(PART_HEADER);
                let mut bytes = Vec::with_capacity(len as usize);
                std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(PART_HEADER))?;
                file.read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(bytes)
            })().map_err(|e| format!("{}: {e}", path.display())),
        }
    };
    let current = history.current();
    let mut bodies = bodies.into_iter();
    let data = match action {
        Action::Direct => decode_payload(&payload(bodies.next().ok_or(t("Missing delta"))?)?, current.sig.len as usize)?,
        Action::Deltas(from) => {
            let mut data = read_delta_base(&confined(root, &history.path)?, history.versions[from - 1].sig, op)?;
            for (version, body) in history.versions[*from..].iter().zip(bodies) {
                op.check_cancelled()?;
                data = lz::apply_indirect_bounded(&payload(body)?, &data, version.sig.len as usize)?;
                if FileSig::of(&data) != version.sig {
                    return Err(tf("{path}: delta signature mismatch", &[("path", &history.path)]));
                }
            }
            data
        }
    };
    if FileSig::of(&data) != current.sig {
        return Err(tf("{path}: downloaded signature mismatch", &[("path", &history.path)]));
    }
    op.check_cancelled()?;
    save(cache, &data)?;
    for part in parts { let _ = fs::remove_file(part); }
    Ok(data.len() as u64)
}

/// Stage `groups` (`stage_groups`) into the verified cache `staged`: reuse each verified copy,
/// download the rest through the pool, and finish each group as it completes. The first error
/// stops every connection; the stage returns only after the store and every finish have ended,
/// so nothing still writes once the updater lock is released. Returns what is ready, in group
/// order, and the decoded bytes staged. `first` is the first group's index in `downloads`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stage(product: Product, server: &str, root: &Path, staged: &Path, m: &Manifest, groups: Vec<StageGroup>,
    op: &Operation, downloads: &Downloads, first: usize, tuning: Tuning, stats: Arc<Stats>) -> Result<(Vec<Ready>, u64), String> {
    let mut next_id = 0u32;
    let mut transfer = |path: String, len: u64, group: usize, solo: bool| {
        next_id += 1;
        Arc::new(Transfer { id: next_id, part: part_path(staged, &path, len), path, len, progress: first + group, solo })
    };
    let mut specs = Vec::with_capacity(groups.len());
    for (index, group) in groups.iter().enumerate() {
        let history = &m.files[group.source.index];
        specs.push(transfers_of(history, &group.source.action)?.into_iter().map(|(path, len)| transfer(path, len, index, false)).collect::<Vec<_>>());
    }
    let (m, groups, root): (Arc<Manifest>, Arc<Vec<StageGroup>>, Arc<Path>) = (Arc::new(m.clone()), Arc::new(groups), root.into());
    let opened = {
        let (m, groups, staged, specs, op) = (m.clone(), groups.clone(), staged.to_path_buf(), specs.clone(), op.clone());
        tokio::task::spawn_blocking(move || prepare(product, &staged, &m, &groups, &specs, &op)).await.map_err(|e| e.to_string())??
    };

    let mut ready: Vec<Option<Vec<Ready>>> = vec![None; groups.len()];
    let stage_ready = |ready: &mut Vec<Option<Vec<Ready>>>, index: usize, cache: &Path| {
        let group = &groups[index];
        let sig = m.files[group.source.index].current().sig;
        ready[index] = Some(group.targets.iter().map(|target| {
            let history = &m.files[target.index];
            op.complete_update_work(UpdateWork::Staging, staging_work(history, target), None);
            (history.path.clone(), cache.to_path_buf(), sig, target.original)
        }).collect());
    };
    let (mut jobs, mut slots, mut owner, mut complete) = (Vec::new(), HashMap::new(), HashMap::new(), Vec::new());
    for ((index, opened), transfers) in opened.into_iter().enumerate().zip(specs) {
        let history = &m.files[groups[index].source.index];
        let cache = cache_path(product, staged, history);
        let Some(opened) = opened else {
            downloads.file_scoped(first + index, op.clone()).advance(u64::MAX, tf("Reusing verified {path}", &[("path", &history.path)]), true);
            stage_ready(&mut ready, index, &cache);
            continue;
        };
        let mut job = Job { group: index, action: groups[index].source.action.clone(), bodies: Vec::new(), open: 0,
            resumed: false, restarted: false, cache, transfers: Vec::new() };
        for (position, (resumed, transfer)) in opened.into_iter().zip(transfers).enumerate() {
            let (slot, frontier) = resumed.map_or((None, 0), |(slot, frontier)| (Some(slot), frontier));
            if frontier > 0 {
                // Not transferred at network speed now.
                downloads.file_scoped(first + index, op.clone()).advance(frontier, tf("Resuming {path}", &[("path", &history.path)]), true);
                job.resumed = true;
            }
            if frontier == transfer.len {
                job.bodies.push(transfer.part.clone().map(Done::Part));
            } else {
                job.bodies.push(None);
                job.open += 1;
                owner.insert(transfer.id, (jobs.len(), position));
                if let Some(slot) = slot { slots.insert(transfer.id, slot); }
            }
            job.transfers.push(transfer);
        }
        if job.open == 0 { complete.push(jobs.len()); }
        jobs.push(job);
    }
    // Largest download first (ties in plan order): heavy decoding overlaps the rest, tiny files
    // end the stage at the pipeline's request rate, and a big file spreads over every connection.
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    order.sort_by_key(|&j| std::cmp::Reverse(download_size(&m.files[groups[jobs[j].group].source.index], &jobs[j].action)));
    let mut queue = Queue::default();
    for &j in &order {
        for (transfer, body) in jobs[j].transfers.iter().zip(&jobs[j].bodies) {
            if body.is_none() {
                let frontier = slots.get(&transfer.id).map_or(0, |slot: &Slot| match slot.body { Body::Disk { frontier, .. } => frontier, Body::Ram(_) => 0 });
                queue.pending.push_back((transfer.clone(), frontier));
            }
        }
    }

    let shared = Arc::new(Shared { product, server: server.into(), op: op.clone(), downloads: downloads.clone(), tuning,
        queue: Mutex::new(queue), no_pipeline: AtomicBool::new(tuning.depth <= 1), pipelined_ok: AtomicBool::new(false),
        delivered: AtomicU64::new(0), stats });
    let (writes, store_rx) = tokio::sync::mpsc::channel::<Write>(STORE_FRAMES);
    let (event_tx, mut events) = tokio::sync::mpsc::channel::<Event>(COMPLETED_QUEUE);
    let store = std::thread::Builder::new().name("patch-store".into())
        .spawn(move || store(store_rx, event_tx, slots)).map_err(|e| e.to_string())?;
    let budget = Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_BUDGET_MIB as usize));
    let mut connections = tokio::task::JoinSet::new();
    let mut finishing = tokio::task::JoinSet::new();
    let workers = finish_workers();
    let spawn_finish = |finishing: &mut tokio::task::JoinSet<(usize, Result<u64, String>)>, jobs: &mut Vec<Job>, j: usize| {
        let job = &mut jobs[j];
        let bodies: Vec<Done> = job.bodies.iter_mut().map(|body| body.take().expect("complete transfer")).collect();
        let (m, groups, root, op, budget) = (m.clone(), groups.clone(), root.clone(), op.clone(), budget.clone());
        let (index, action, cache) = (groups[job.group].source.index, job.action.clone(), job.cache.clone());
        finishing.spawn(async move {
            let mib = download_memory_mib(&m.files[index], &action);
            let result = match budget.acquire_many_owned(mib).await {
                Ok(permit) => tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    finish(&root, &m.files[index], &action, bodies, &cache, &op)
                }).await.map_err(|e| e.to_string()).and_then(|result| result),
                Err(error) => Err(error.to_string()),
            };
            (j, result)
        });
    };
    for j in complete { spawn_finish(&mut finishing, &mut jobs, j); }

    let (mut left, mut bytes) = (jobs.len(), 0u64);
    // Half the first backoff, so the first break doubles it to `BREAK_BACKOFF.0`.
    let (mut alive, mut next_open, mut backoff, mut delivered_at_break) = (0usize, Instant::now(), BREAK_BACKOFF.0 / 2, 0u64);
    let mut held: Option<(usize, Instant)> = None;
    let result: Result<(), String> = loop {
        if left == 0 { break Ok(()); }
        let now = Instant::now();
        let limit = match held { Some((limit, until)) if now < until => limit.max(1), _ => tuning.connections.max(1) };
        let want = alive < limit && shared.queue().has_work();
        if want && now >= next_open {
            connections.spawn(connection(shared.clone(), writes.clone()));
            alive += 1;
            next_open = now + tuning.spacing;
            continue;
        }
        tokio::select! {
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                alive -= 1;
                let mut back_off = |next_open: &mut Instant| {
                    shared.stats.reconnects.fetch_add(1, Ordering::Relaxed);
                    let delivered = shared.delivered.load(Ordering::Relaxed);
                    backoff = if delivered > delivered_at_break { BREAK_BACKOFF.0 } else { (backoff * 2).min(BREAK_BACKOFF.1) };
                    delivered_at_break = delivered;
                    *next_open = (*next_open).max(Instant::now() + backoff);
                };
                match joined.unwrap_or_else(|e| End::Fatal(e.to_string())) {
                    End::Done | End::Closed => {}
                    End::Broken => back_off(&mut next_open),
                    End::Refused(error) if alive == 0 => break Err(error),
                    End::Refused(_) => {
                        held = Some((alive, Instant::now() + REFUSAL_HOLD));
                        back_off(&mut next_open);
                    }
                    End::Fatal(error) => break Err(error),
                }
            }
            Some(event) = events.recv(), if finishing.len() < workers => match event {
                Event::Failed(error) => break Err(error),
                Event::Complete { transfer, body } => {
                    let Some(&(j, position)) = owner.get(&transfer) else { continue };
                    jobs[j].bodies[position] = Some(body);
                    jobs[j].open -= 1;
                    if jobs[j].open == 0 { spawn_finish(&mut finishing, &mut jobs, j); }
                }
            },
            Some(finished) = finishing.join_next(), if !finishing.is_empty() => {
                let (j, outcome) = match finished { Ok(finished) => finished, Err(error) => break Err(error.to_string()) };
                let history = &m.files[groups[jobs[j].group].source.index];
                let progress = downloads.file_scoped(first + jobs[j].group, op.clone());
                let (retry, solo): (Vec<(String, u64)>, bool) = match outcome {
                    Ok(n) => {
                        bytes = bytes.saturating_add(n);
                        stage_ready(&mut ready, jobs[j].group, &jobs[j].cache);
                        left -= 1;
                        continue;
                    }
                    Err(_) if op.cancelled() => break Err(op.check_cancelled().unwrap_err()),
                    // A failed delta chain: the full file instead, as one Direct transfer.
                    Err(error) if matches!(jobs[j].action, Action::Deltas(_)) => {
                        progress.fallback(history.current().direct_len,
                            tf("Delta failed for {path}; using full file ({error})", &[("path", &history.path), ("error", &error)]));
                        jobs[j].action = Action::Direct;
                        (vec![(history.current().direct.clone(), history.current().direct_len)], false)
                    }
                    // Bytes that did not decode or verify: every transfer again from 0, once. Bytes
                    // from an earlier run may simply be stale; a fresh download that failed is asked
                    // again one request at a time (`solo`), while the rest of the run keeps its
                    // pipelines (a file damaged on the server fails again either way).
                    Err(error) if !jobs[j].restarted => {
                        jobs[j].restarted = true;
                        shared.stats.redownloads.lock().unwrap_or_else(|e| e.into_inner()).push(error.clone());
                        // Kept for download_concurrency_bench's diagnosis of what arrived.
                        #[cfg(test)]
                        for part in jobs[j].transfers.iter().filter_map(|t| t.part.as_ref()) { let _ = fs::copy(part, part.with_extension("xi-bad")); }
                        let again: Vec<(String, u64)> = jobs[j].transfers.iter().map(|t| (t.path.clone(), t.len)).collect();
                        progress.fallback(again.iter().map(|(_, len)| len).sum(), error);
                        (again, !jobs[j].resumed)
                    }
                    Err(error) => break Err(error),
                };
                let job = &mut jobs[j];
                job.resumed = false;
                job.transfers = retry.into_iter().map(|(path, len)| transfer(path, len, job.group, solo)).collect();
                job.bodies = job.transfers.iter().map(|_| None).collect();
                job.open = job.transfers.len();
                let mut queue = shared.queue();
                for (position, transfer) in job.transfers.iter().enumerate().rev() {
                    owner.insert(transfer.id, (j, position));
                    queue.pending.push_front((transfer.clone(), 0));
                }
            }
            _ = tokio::time::sleep_until(next_open.into()), if want => {}
            cancelled = op.cancelled_wait() => break Err(cancelled),
        }
    };
    // Shut down in an order that cannot deadlock: the store may be waiting to hand over a
    // completed transfer, the connections to hand frames to the store.
    drop(events);
    if result.is_err() { connections.abort_all(); }
    while connections.join_next().await.is_some() {}
    drop(writes);
    drop(shared);
    let _ = tokio::task::spawn_blocking(move || store.join()).await;
    while finishing.join_next().await.is_some() {}
    result?;
    Ok((ready.into_iter().flatten().flatten().collect(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(id: u32, len: u64, part: Option<PathBuf>) -> Arc<Transfer> {
        Arc::new(Transfer { id, path: format!("t{id}"), len, part, progress: 0, solo: false })
    }

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crystal-download-{label}-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_queue_serves_retries_first_then_cuts_the_front_transfer_into_chunks() {
        let chunk = polp::CHUNK_LEN as u64;
        let mut queue = Queue::default();
        queue.pending.push_back((transfer(1, 2 * chunk + 5, None), 0));
        queue.pending.push_back((transfer(2, 10, None), 0));
        let first = queue.pop(true).unwrap();
        assert_eq!((first.transfer.id, first.offset, first.count), (1, 0, polp::CHUNK_LEN));
        queue.retry.push_back(Chunk { attempts: 1, ..first });
        let order: Vec<(u32, u32, u32, u8)> = std::iter::from_fn(|| queue.pop(true))
            .map(|c| (c.transfer.id, c.offset, c.count, c.attempts)).collect();
        assert_eq!(order, [(1, 0, 0x10000, 1), (1, 0x10000, 0x10000, 0), (1, 0x20000, 5, 0), (2, 0, 10, 0)]);
        assert!(!queue.has_work());
        // A resumed transfer starts at its frontier.
        queue.pending.push_back((transfer(3, 3 * chunk, None), 2 * chunk));
        let resumed = queue.pop(true).unwrap();
        assert_eq!((resumed.offset as u64, resumed.count as u64), (2 * chunk, chunk));
        assert!(queue.pop(true).is_none());
        // A solo transfer goes only onto an idle connection.
        queue.pending.push_back((Arc::new(Transfer { solo: true, ..(*transfer(4, 10, None)).clone() }), 0));
        assert!(queue.pop(false).is_none() && queue.has_work());
        assert!(queue.pop(true).is_some_and(|chunk| chunk.transfer.solo));
    }

    #[test]
    fn the_pipeline_window_is_one_second_of_the_connections_own_delivery() {
        let start = Instant::now();
        let chunk = polp::CHUNK_LEN as u64;
        let chunks_in_flight = |pace: &Pace, now| {
            let window = pace.window(now);
            let mut n = 1u64; // the top-up always allows one into an empty pipeline
            while n * chunk < window && n < MAX_DEPTH as u64 { n += 1; }
            n
        };
        // Nothing measured: one request.
        assert_eq!(Pace::new(start).window(start), 0);
        // A normal period: 64 KB in 150 ms → 437 KB/s → 7 chunks.
        let mut pace = Pace::new(start);
        pace.credit(start + Duration::from_millis(150), chunk);
        assert_eq!(chunks_in_flight(&pace, start + Duration::from_millis(150)), 7);
        // A slow period: 15 KB/s keeps one chunk in flight.
        let mut slow = Pace::new(start);
        slow.credit(start + Duration::from_secs(4), 60_000);
        assert!(slow.window(start + Duration::from_secs(4)) < chunk);
        // Tiny files: 24-byte replies reach 16 requests by the third round (145 ms each).
        let (mut tiny, mut depth, mut at) = (Pace::new(start), 1u64, start);
        for _ in 0..3 {
            at += Duration::from_millis(145);
            for _ in 0..depth { tiny.credit(at, 24); }
            let window = tiny.window(at);
            depth = 1;
            while depth * 24 < window && depth < MAX_DEPTH as u64 { depth += 1; }
        }
        assert_eq!(depth, MAX_DEPTH as u64);
        // A 3 s gap halves the history.
        let mut gap = Pace::new(start);
        gap.credit(start + Duration::from_millis(100), 1000);
        gap.credit(start + Duration::from_secs(3), 0);
        assert_eq!((gap.bytes, gap.since), (500, start + Duration::from_millis(1500)));
    }

    #[test]
    fn small_transfers_stay_in_memory_and_large_ones_never_allocate_their_length() {
        let dir = temp("slots");
        let small = Slot::create(&transfer(1, PART_MIN - 1, part_path(&dir, "a", PART_MIN - 1))).unwrap();
        assert!(matches!(small.body, Body::Ram(ref bytes) if bytes.len() as u64 == PART_MIN - 1));
        let big = transfer(2, 40 << 20, part_path(&dir, "b", 40 << 20));
        assert!(Slot::resume(&big).unwrap().is_none() && !big.part.as_ref().unwrap().exists(), "nothing on disk before its first chunk");
        let slot = Slot::create(&big).unwrap();
        assert!(matches!(slot.body, Body::Disk { frontier: 0, .. }));
        assert_eq!(fs::metadata(big.part.as_ref().unwrap()).unwrap().len(), PART_HEADER + (40 << 20), "preallocated");
        assert!(Slot::resume(&big).unwrap().is_none(), "no data below a zero frontier");
        drop(slot);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Chunks land out of order; the frontier follows the contiguous run, is recorded only at a
    /// checkpoint after the data was flushed, and a process killed mid-write (no final
    /// checkpoint) resumes from the last recorded frontier, never past it.
    #[test]
    fn the_part_frontier_is_durable_contiguous_and_survives_a_crash_mid_write() {
        let dir = temp("frontier");
        let chunk = polp::CHUNK_LEN as u64;
        let len = CHECKPOINT + 4 * chunk + 100;
        let t = transfer(1, len, part_path(&dir, "f", len));
        let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
        let piece = |offset: u64| &data[offset as usize..(offset + chunk).min(len) as usize];
        let mut slot = Slot::create(&t).unwrap();
        // Chunk 1 before chunk 0: no frontier yet.
        slot.write(chunk, piece(chunk)).unwrap();
        assert!(matches!(slot.body, Body::Disk { frontier: 0, .. }));
        slot.write(0, piece(0)).unwrap();
        assert!(matches!(slot.body, Body::Disk { frontier, synced: 0, .. } if frontier == 2 * chunk));
        // Fill up to the checkpoint distance, skipping one chunk past it.
        let mut offset = 2 * chunk;
        while offset < CHECKPOINT + chunk { slot.write(offset, piece(offset)).unwrap(); offset += chunk; }
        slot.write(CHECKPOINT + 2 * chunk, piece(CHECKPOINT + 2 * chunk)).unwrap();
        let recorded = match slot.body { Body::Disk { synced, .. } => synced, Body::Ram(_) => unreachable!() };
        assert_eq!(recorded, CHECKPOINT, "checkpointed once 16 MiB were contiguous, not again a chunk later");
        // Killed here: the handle drops without a final checkpoint.
        drop(slot);
        let (resumed, frontier) = Slot::resume(&t).unwrap().unwrap();
        assert_eq!(frontier, recorded);
        assert_eq!(resumed.left as u64, len.div_ceil(chunk) - recorded / chunk);
        let Body::Disk { file, .. } = &resumed.body else { unreachable!() };
        let mut below = vec![0u8; frontier as usize];
        read_at(file, &mut below, PART_HEADER).unwrap();
        assert!(below == data[..frontier as usize], "the bytes below the frontier are the ones written");
        drop(resumed);
        // Anything malformed is not resumed (the store then starts the part again from 0).
        let path = t.part.clone().unwrap();
        let header = |bytes: &[u8], at: u64| write_at(&fs::OpenOptions::new().write(true).open(&path).unwrap(), bytes, at).unwrap();
        header(b"NOTPART\0", 0);
        assert!(Slot::resume(&t).unwrap().is_none(), "magic");
        header(&PART_MAGIC, 0);
        header(&(chunk + 1).to_le_bytes(), 8);
        assert!(Slot::resume(&t).unwrap().is_none(), "frontier off a chunk boundary");
        header(&(len + chunk).to_le_bytes(), 8);
        assert!(Slot::resume(&t).unwrap().is_none(), "frontier past the end");
        header(&chunk.to_le_bytes(), 8);
        assert_eq!(Slot::resume(&t).unwrap().unwrap().1, chunk);
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(10).unwrap();
        assert!(Slot::resume(&t).unwrap().is_none(), "wrong size");
        fs::remove_dir_all(dir).unwrap();
    }

    /// The store writes every chunk, hands a completed transfer on, and checkpoints every part
    /// still open when its senders go away (a cancel).
    #[test]
    fn the_store_completes_transfers_and_checkpoints_open_parts_when_stopped() {
        let dir = temp("store");
        let chunk = polp::CHUNK_LEN as u64;
        let (small, big) = (transfer(1, 10, None), transfer(2, 6 * chunk, part_path(&dir, "big", 6 * chunk)));
        let (writes, rx) = tokio::sync::mpsc::channel(STORE_FRAMES);
        let (events_tx, mut events) = tokio::sync::mpsc::channel(COMPLETED_QUEUE);
        let thread = std::thread::spawn(move || store(rx, events_tx, HashMap::new()));
        let frame = |n: usize| vec![7u8; n];
        writes.blocking_send(Write { transfer: small.clone(), offset: 0, frame: frame(14), data: 4..14 }).unwrap();
        for offset in [0, chunk, 3 * chunk] {
            writes.blocking_send(Write { transfer: big.clone(), offset, frame: frame(chunk as usize), data: 0..chunk as usize }).unwrap();
        }
        match events.blocking_recv().unwrap() {
            Event::Complete { transfer: 1, body: Done::Ram(bytes) } => assert_eq!(bytes, vec![7u8; 10]),
            other => panic!("{other:?}"),
        }
        drop(writes);
        thread.join().unwrap();
        assert!(events.blocking_recv().is_none());
        assert_eq!(resume_part(big.part.as_ref().unwrap(), big.len).unwrap().1, 2 * chunk, "frontier recorded at the stop");
        fs::remove_dir_all(dir).unwrap();
    }

    /// A zlib payload made from the right file whose deflate body was later damaged (as the
    /// official ROM5/5/113.DAT.slc is on the patch server): it fails with the file's name, and
    /// nothing is staged.
    #[test]
    fn a_damaged_zlib_payload_fails_with_its_name_and_stages_nothing() {
        use std::io::Write as _;
        let file: Vec<u8> = (0..300_000u32).map(|i| ((i / 3).wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let mut encoder = flate2::write::ZlibEncoder::new(vec![3u8], flate2::Compression::default());
        encoder.write_all(&file).unwrap();
        let mut payload = encoder.finish().unwrap();
        let middle = payload.len() / 2;
        payload[middle] ^= 0x10;
        let dir = temp("damaged-zlib");
        let history = FileHistory { path: "ROM5/5/113.DAT".into(), optional: false, versions: vec![manifest::Version {
            stamp: "30210706_0".into(), sig: FileSig::of(&file), direct: "30210706_0/Direct/ROM5/5/113.DAT.slc".into(),
            direct_len: payload.len() as u64, indirect: None }] };
        let cache = dir.join("staged.bin");
        let error = finish(&dir, &history, &Action::Direct, vec![Done::Ram(payload)], &cache, &Operation::new()).unwrap_err();
        assert!(error.starts_with("ROM5/5/113.DAT: "), "{error}");
        assert!(!cache.exists() && !cache.with_extension("xi-part").exists(), "nothing staged");
        fs::remove_dir_all(dir).unwrap();
    }
}