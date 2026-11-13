# Downloader design: pipelined chunk pool, streamed parts, partial resume

Status: design only, nothing implemented. Branch `claude/eu-jp-v2`, HEAD `a7ebcf1`.
Scope: the POLP download stage of `src/updater.rs` (`stage_plan_shared` and what it calls).
The installer's HTTP downloader (`src/installer.rs`) and the version/list connections are not changed.

## 1. Summary

Three findings drive the design. All three were measured, not assumed.

1. **The server pipelines.** Several file requests written back to back on one connection are answered in request order, across different files and arbitrary offsets (live probe, 2026-11-12, section 2.2).
2. **In a normal period one request costs one round trip (about 150 ms) and almost no server time (about 5 ms).** So today's throughput is `connections x 64 KB / round trip`, and 418 of the 459 files in a routine patch are a single small request that still pays a full round trip each (stock capture, section 2.1).
3. **In the slow period measured on 2026-11-12 the first byte still arrives in about 155 ms; the body then trickles at 15-20 KB/s per connection.** The limit is per-flow bandwidth (loss and recovery on a saturated path), not request latency. Pipelining cannot help there; only the number of flows does. That is why the benchmark scaled linearly with connections.

The design, in one paragraph: replace "one file per connection, held in memory" with **one shared queue of 64 KB chunks, largest file first, served by up to 16 connections that each keep a self-sizing pipeline of requests in flight**. Chunks go straight to a preallocated part file through one store thread; a file is decoded, verified and staged as soon as its last chunk lands, while the rest keeps downloading. Part files record a durable frontier, so a cancelled or crashed download resumes mid-file. The install transaction (journal, backups, rollback) is not touched.

| Area | Decision |
|---|---|
| Chunk size | Stays 64 KB (`polp::CHUNK_LEN`). Larger counts work but buy nothing once requests are pipelined. |
| Pipelining | Yes. At most 16 requests in flight per connection, sized from that connection's measured rate (1 second of work). Falls back to 1 by itself in a slow period and on any protocol surprise. |
| Scheduling | One chunk queue, largest download first. A big file is spread over every connection automatically. |
| Connections | Cap stays 16. Opened 100 ms apart while unassigned work remains, so a small patch uses a few. Backoff on refusals and breaks. |
| Memory | Network side holds about 20 MiB at most. The 1 GiB budget now bounds decoding only and no longer limits connections. |
| Resume | Per chunk frontier for transfers of 256 KB and up; at most about 16 MiB per in-flight file is downloaded again. |
| Install overlap | Decode, verify and stage overlap the download. Applying to the install stays one transaction at the end (section 6). |

## 2. Protocol evidence

### 2.1 Captured stock traffic

Source: `stock-update-20261013-004932.pcapng` and `stock-20261013-002122.pcapng`, read with the capture folder's `patchsummary.py` and a per-request timing script. These are the captures behind `src/updater/fixtures/stock-20261013-full-update.requests.tsv` (`traffic_tests.rs:696-705`).

| Fact | Value | Meaning |
|---|---|---|
| Stock is strict stop-and-wait | 422 + 119 requests, 0 sent before the previous reply ended | Stock never pipelines. Pipelining is a Crystal-only wire shape. |
| Handshake round trip | 144.5 ms (146.2 ms in the other capture) | Server is far away; every round trip is expensive. |
| Request to first reply byte | median 149.5 ms, p10 145.0, p90 153.2 | Round trip plus about 5 ms. |
| Server time per request | median 5.1 ms, p90 8.8 ms | The server is not the limit in a normal period. |
| Stock 64 KB reply body | 144.0 ms (one more round trip) | Stock's 64 KB socket advertises a 32 KB window (`stock_socket.rs:9-11`), so a chunk needs two flights. Crystal's plain download socket (`updater.rs:1731-1732`) avoids this. |
| Stock throughput | 151 KiB/s overall, 0.22 MB/s while a full chunk is in flight | Baseline. |
| Server SYN-ACK | window 33320, MSS 1360, window scale 1, SACK | The server's receive window is small. Irrelevant for requests (about 75 bytes each). Its send buffer is unknown (section 9, risk 2). |
| Stock cancel | client RST on the download connection (`stock-20261013-002122`, last packet) | A reset on a cancelled download is stock behaviour. No long drain is needed on cancel. |
| Small replies | 67 replies under 8 KB took a median 190 ms from first to last byte | Most likely the server holding the reply's tail (Nagle) until the client's delayed ACK. With a pipeline the tail should ride with the next reply; the small-set benchmark will show it. |

Routine patch shape (the same fixture): 459 files, 540 requests, 18,451,504 bytes. 418 files are one request. The largest is `FFXiMain.dll.olc`, 1,178,827 bytes, 18 chunks.

### 2.2 Live probe, 2026-11-12 about 22:00 (-06:00)

One plain connection at a time to `124.150.156.107:53001` (`pc001W2U.pol.com`), 7 connections and about 2.1 MB in total, files from the 30261006_0 patch, nothing written to disk. No update was running on this PC. The probe was a throwaway Python script; section 8.3 turns it into an ignored live test.

| Question | Test | Result |
|---|---|---|
| Pipelining across files | Three requests in one write: `patch.sin.slc` (57 B), `ROM/387/70.DAT.slc` (24 B), `ROM/387/29.DAT.slc` (24 B) | Three replies, in request order, each echoing its own path, all complete 356.8 ms after the write. |
| Pipelining depth 6, mixed | Chunks 0-3 of `FFXiMain.dll.olc`, then `patch.sin.slc`, then the file's last chunk (offset 1114112, 64715 B) | Six replies in request order. The MD5 of chunks 0-2 equals the stop-and-wait result (4f825f23, 9bc08752, ad4fb1c4). |
| Unaligned offsets | offset 1000 count 5000; offset 65436 count 200 | Both equal the same bytes cut from aligned chunks. Offsets and counts are arbitrary. |
| Larger counts | 65537, 131072, 135453 (a whole file), 1048576 | All answered in one frame with the full data and a valid frame digest. The limit, if any, is above 1 MiB. |
| Speed during the slow period | Stop-and-wait chunks | First byte at 151-155 ms (once 745 ms); the 64 KB body took 4.2 s, 8.8 s and 5.3 s. One 64 KB reply arrived as 26 reads over 2.5 s with gaps up to 642 ms. |
| Pipelining during the slow period | The depth 6 run | 327 KB in 15.6 s, 21 KB/s. Same rate as stop-and-wait. |

Not probed, and treated as unknown: error replies (missing file, range past the end), the Viewer's patch host `pt008.pol.com:54000`, per-address connection limits above 16, and pipelined throughput in a normal period.

### 2.3 What the code already establishes

- Request and reply layout: `polp.rs:116-130` (offset, count, tags, path) and `polp.rs:170-177` (offset, count, echoed path, data). The reply repeats the request, so a pipelined client can check each reply against the head of its own queue.
- Every frame carries a truncated MD5 over its body (`polp.rs:49-51`, checked at `polp.rs:88`). Each chunk is integrity-checked on arrival.
- A chunk can be asked again at the same offset on a new connection (`traffic_tests.rs:855-898`; stock itself resumed mid-file after a cancel, `traffic_tests.rs:699-703`).
- The manifest signature (`manifest.rs:14-18`) covers the decoded file, not the transfer. Direct payloads are stored, zlib or LZ coded (`updater.rs:1789-1813`), deltas need the old file (`lz.rs:84-137`). So a transfer cannot be hash-verified against the manifest while it streams; end-to-end verification happens after decoding.

## 3. Why the current design is slow

References are to `src/updater.rs` at `a7ebcf1`.

1. **One round trip per request** (`fetch_tracked`, 1666-1712; `exchange`, 1621-1660). In a normal period each connection idles for a round trip between chunks, and each tiny file costs a full round trip.
2. **One file per connection** (`stage_plan_shared`, 982-1016). A large file is bound to one connection at about 0.42 MB/s (64 KB per 155 ms) while the other 15 run out of work. The last big files set the finish time.
3. **Whole files in memory under a 1 GiB budget** (`download_memory_mib`, 935-948; semaphore at 998-1003). Large files wait for budget, so the budget throttles the network.
4. **Whole-file resume only** (995-996). In the slow period a 16 MB file is about 15 minutes of one connection; a cancel at 99% loses it.
5. **Disk and hash work runs on the async workers** (`save` and `signature` at 1005-1006), and each staged file is hashed twice (1062 and 1006).
6. **A reply slower than 30 s per frame is treated as dead** (`cancel_io`, 1771-1776, wraps each `read_exact` at 1633 and 1648). Below about 2 KB/s a healthy 64 KB reply exceeds it, is retried three times and fails the update. The slowest reply seen in the probe ran at 4.7 KB/s.

## 4. The design

### 4.1 Constants

New constants live in the new module `src/updater/download.rs`.

| Constant | Value | Reason |
|---|---|---|
| `polp::CHUNK_LEN` | `0x10000` (unchanged) | Requests stay byte-identical to stock's; only their timing differs. Fine-grained scheduling and cheap retries. Keeps the 128 KB frame cap (`updater.rs:1635-1642`). |
| `MAX_DOWNLOAD_WORKERS` | 16 (unchanged) | The owner's measured value. No refusal seen at 16. |
| `MAX_DEPTH` | 16 requests in flight per connection | 1 MiB in flight per connection at most. On tiny files the server answers about one request per 5 ms, so 16 in flight keep it busy for about 80 ms of each 145 ms round trip; the other connections cover the rest. `MAX_DEPTH = 1` restores one request per connection: the kill switch. |
| `LOOKAHEAD` | 1 s | How much work a connection may have requested ahead, measured in its own delivery time. Must exceed the round trip (145 ms here, up to about 300 ms from Europe); bounds the tail, the cancel waste and the reorder window. |
| `CONNECT_SPACING` | 100 ms | 16 connections in 1.5 s. A small patch finishes on the first few. |
| `BREAK_BACKOFF` | 1 s, doubling to 30 s | After a broken connection or failed connect. Resets when any chunk completes. |
| `PIPELINE_PROBATION` | 5 s of silence | Until the run has seen one pipelined reply complete, a connection with more than one request in flight is given up after 5 s without a byte (section 4.4). |
| Frame idle limit | 30 s without a byte | Replaces 30 s per frame. Same number as `cancel_io`, applied to silence. |
| `CHUNK_ATTEMPTS` | 3 (unchanged) | As `fetch_tracked` today. |
| `PART_MIN` | 4 chunks, 256 KiB | Smaller transfers stay in memory: 418 of 459 files in a routine patch are one chunk, and a part file each would only add file churn. |
| `CHECKPOINT` | 16 MiB of new contiguous data | One `sync_data` per 16 MiB per file. |
| Store channel | 64 frames, about 4 MiB | Backpressure from disk to network; bounds memory. |
| `FINISH_WORKERS` | `(available_parallelism / 2).clamp(1, 4)` | Decode, hash and save run far faster than the network; 4 keep hundreds of small files moving. |
| `DOWNLOAD_BUDGET_MIB` | 1024 (unchanged) | Now acquired only around decoding. |

### 4.2 Units of work

```rust
/// One server file: a Direct payload, or one delta of a chain.
struct Transfer { job: usize, step: usize, path: Arc<str>, len: u64, progress: usize /* Downloads index */ }

/// One request. `attempts` counts the times it was at the head of a broken connection.
struct Chunk { transfer: u32, offset: u32, count: u32, attempts: u8 }

/// One StageGroup: its transfers, and how to turn them into the staged file.
struct Job { group: usize, action: Action, transfers: Vec<u32>, open: usize, resumed: bool }

/// Shared by every connection. Mutex + Notify.
struct Queue {
    retry: VecDeque<Chunk>,            // served first
    pending: VecDeque<(u32, u64)>,     // (transfer, next offset), largest job first
}
```

`pop()` takes from `retry`, else cuts the next chunk off the front transfer. Chunks are not materialised up front.

### 4.3 Scheduling

- Jobs are sorted by download size (`download_size`, `updater.rs:910-916`), largest first, ties in plan order. `Ready` is still returned in group order (`updater.rs:1034`).
- Every connection pops from the same queue. A large file's chunks are consecutive in the queue, so they are spread over all connections. This is the range splitting; there is no second mechanism.
- Largest first puts the heavy decoding early, where it overlaps the remaining download, and leaves tiny files for the end, where pipelining clears them at a high request rate. No connection can be left alone with a big file.
- Files fill roughly front to back. The out-of-order window is at most `connections x MAX_DEPTH` chunks (16 MiB), which keeps NTFS from zero-filling large gaps in a preallocated file and keeps the resume frontier close to the data.

### 4.4 The connection loop and its pipeline

One task per connection. No separate reader and writer tasks: requests are tiny and written in one `write_all`.

```rust
loop {
    // Top up.
    while flight.len() < depth_cap()                      // MAX_DEPTH, or 1 after a fallback
        && (flight.is_empty() || flight_bytes < pace.window(now)) {
        let Some(chunk) = queue.pop() else { break };
        batch.extend(polp::file_request(&tags, path, chunk.offset, chunk.count));
        flight_bytes += chunk.count; flight.push_back(chunk);
    }
    if flight.is_empty() { break }                        // out of work: close like stock
    if !batch.is_empty() { session.send(&batch).await? }
    // One reply.
    let frame = session.read_frame(idle_limit).await?;
    let chunk = flight.pop_front();
    // The reply must echo the head request: path, offset, count (as fetch_tracked checks today).
    pace.credit(now, chunk.count); flight_bytes -= chunk.count;
    progress.advance(chunk.count); store.send(Write { .. }).await;
}
```

**Sizing the pipeline.** A connection may request ahead what it delivered in the last second:

```rust
struct Pace { since: Instant, bytes: u64 }               // reply payload completed since `since`
fn credit(&mut self, now, n) {
    if now - self.since > 2 * LOOKAHEAD {                 // forget the older half
        self.bytes /= 2; self.since += (now - self.since) / 2;
    }
    self.bytes += n;
}
fn window(&self, now) -> u64 {                            // bytes that may be in flight
    (self.bytes as f64 * LOOKAHEAD.as_secs_f64() / (now - self.since).as_secs_f64().max(0.001)) as u64
}
```

`since` starts at the connection's first request. How the rule behaves:

| Situation | Measured rate | In flight |
|---|---|---|
| First request on a connection | none yet | 1. Nothing is pipelined before the server has answered once on that connection. |
| Normal period, 64 KB chunks, after one reply (64 KB in 0.15 s) | 437 KB/s | 7 chunks. If the rate rises, the window follows, up to 16. |
| Slow period (15-20 KB/s) | 15-20 KB | 1 chunk. Same as today; no chunk is stuck behind another. |
| Tiny files | grows each round | 1, then about 7, then 16 requests per round trip. |

This is not AIMD. TCP already does congestion control for each flow; the client only decides how much to queue behind it, and "one second of this connection's own delivery" has no thresholds to tune and cannot oscillate.

**Fallback to one at a time.** A run-wide flag `no_pipeline` makes `depth_cap()` return 1. It is set when:
- a reply does not echo the head request while more than one was in flight, or
- before any pipelined reply has completed in this run, a connection with more than one request in flight receives nothing for `PIPELINE_PROBATION` (a server that drops the extra requests). A pipelined reply is the reply to a request that was written while an earlier one was still unanswered.

The connection is closed, its chunks are requeued without an attempt charge, and the run continues at depth 1 on every connection. The flag is not persisted.

**Failures.** Any I/O error, bad frame, idle timeout or cancel marks the session broken (as `exchange` does today). The head chunk gets one attempt charged; the chunks behind it are requeued uncharged, in order, to `retry`. A chunk at 3 attempts fails the stage with `"{path}: {error}"`, as today. The broken session closes through the existing `Session::drop` path (`updater.rs:1534-1556`), unchanged.

### 4.5 Connections

The stage function is the only place that opens connections.

- Open one when unassigned work exists, fewer than `limit` are alive, and `CONNECT_SPACING` (or the current backoff) has passed since the last open.
- A connection that finds the queue empty with nothing in flight closes like stock (`Session::close`) and ends. If work reappears (a requeue, a delta falling back to the full file), the same rule opens a new one.
- A failed connect with no connection alive fails the stage, as today. With others alive: `limit = alive` for 30 s, then back to 16.
- A broken connection is replaced under `BREAK_BACKOFF`.

No controller tunes the connection count from throughput. Each flow is already rate-controlled by TCP; a second controller on 2-second samples of a rate that swings between 5 and 20 KB/s per chunk (section 2.2) would freeze the count too low in exactly the period where count is the only lever. The cap, the spacing and the error backoff are the politeness.

### 4.6 The store: memory and streaming

One thread, `patch-store`, owns every transfer's bytes. Connections send it accepted frames over a bounded tokio channel (`send().await` on the async side, `blocking_recv()` on the thread). Nothing blocks a runtime worker, and no lock guards file state.

```rust
struct Write { transfer: u32, offset: u32, frame: Vec<u8>, data: Range<usize> }   // the frame buffer itself, no copy

enum Body {
    Ram(Vec<u8>),                                         // len < PART_MIN
    Disk { file: File, frontier: u64, synced: u64 },      // preallocated part file
}
struct Slot { body: Body, done: Vec<bool> /* per chunk */, left: u32 }
```

- **Part file**: `.xi-updater/staged/{md5(transfer path)}.{len}.xi-part`. Layout: 16-byte header (`b"XIPART\x01\0"`, then the frontier as `u64` little-endian), then the payload. Created with `set_len(16 + len)`: space is reserved, a full disk fails before any download, and NTFS does not zero it. The name cannot collide with `save`'s temporary (`updater.rs:1930`), which is `{hash}-{hash}.xi-part`.
- **Write**: `seek_write(data, 16 + offset)` (`std::os::windows::fs::FileExt`). The header sits at the front on purpose: a record at the end of a preallocated file would make NTFS zero the whole file first.
- **Frontier**: the end of the contiguous run of completed chunks from offset 0.
- **Checkpoint**, whenever the frontier is `CHECKPOINT` past `synced`, and for every open part when the channel closes (cancel, error, normal end): `file.sync_data()`, then write the frontier into the header. The header is written after the data it describes is durable, so whenever a frontier value is on disk, the bytes below it were flushed first.
- **Completion**: when a slot's `left` reaches 0 and all of its job's transfers are complete, the store hands the job to the stage function over a second bounded channel (64 jobs, `blocking_send`). The stage function reads it only while a finish slot is free (4.7). A finish backlog therefore blocks the store, then the connections, then TCP.
- **Memory**: one frame being read per connection (16 x 64 KB), the write channel (4 MiB), and `Ram` bodies waiting to be finished (at most 64 jobs under 256 KiB each, 16 MiB). Decoding is bounded separately by the budget.

### 4.7 Finish: decode, verify, stage, overlapping the download

A completed job runs on the blocking pool, at most `FINISH_WORKERS` at once, holding `download_memory_mib(history, action)` permits of the 1 GiB budget (the same estimate as today, acquired here instead of around the transfer).

```
Direct:  raw = body (Ram, or the part file after its header)
         data = decode_payload(raw, sig.len)
Deltas:  data = read_delta_base(installed file, versions[from - 1].sig)
         for each delta in order: data = apply_indirect_bounded(raw_i, data, v.sig.len); FileSig::of(data) must equal v.sig
then:    FileSig::of(data) must equal current.sig;  save(cache, data);  delete the job's part files
```

This is `produce` plus `download` (`updater.rs:1814-1844`, `1041-1066`) reading from bodies instead of the socket. Differences:

- A delta chain's files are all downloaded before the first is applied. They are small; a chain that fails wasted a few requests.
- **Delta failure**: as today (`1054-1058`), `transfer.fallback(direct_len, ...)` and the job is re-enqueued as one Direct transfer.
- **A job that used a resumed part and fails verification**: delete its parts, reset its progress, enqueue it again from offset 0, once. A fresh download that fails verification is an error, as today.
- The second read of the saved file (`signature(&cache)`, `1006`) is dropped. The bytes were verified in memory before `save` wrote and flushed them, and the commit verifies the staged file again before using it (`replace`, `2143`).

### 4.8 Start of a stage

On the blocking pool, `FINISH_WORKERS` threads, before the first request:

1. Verified cache file present (`{hash}-{hash}.bin`, signature equal): reused, as today (`995-996`).
2. Otherwise, for each transfer of at least `PART_MIN`: a part file is accepted when its size is `16 + len`, the magic matches, and the frontier is at most `len` and a multiple of 64 KB (or equal to `len`). Its chunks below the frontier are marked done, the job is marked `resumed`, and the bytes are credited to progress with `progress_bytes_rebased` (they were not transferred at network speed, `operation.rs:384-391`). Anything else is created new.
3. Jobs whose transfers are all complete go straight to finish.

## 5. Crash safety and resume

| Event | What is on disk | Next run |
|---|---|---|
| Cancel, network error, failed stage | Every staged `.bin` verified. Every open part synced, with its frontier in the header. The stage returns only after the store thread and all finish tasks have ended, so nothing is still writing when the updater lock is released. | Reuses `.bin` files; resumes each part at its frontier. At most the out-of-order window (16 MiB worst case, usually a few chunks) is downloaded again per in-flight file. |
| Process killed or power lost | Parts hold a frontier from the last checkpoint: at most `CHECKPOINT` plus the window behind the data. | Same resume. |
| A part's header claims more than is really there (torn write, disk fault, tampering) | Not detectable per chunk: frame digests are not kept. | The decoded file fails its manifest signature; the job's parts are deleted and it is downloaded again from 0, once. Nothing unverified is ever staged. |
| Crash during `save` of a staged file | A `{hash}-{hash}.xi-part` temporary. | Ignored and overwritten, as today (`traffic_tests.rs:563-590`). |
| A new game version | Old parts and `.bin` files have other names (the transfer path contains the version). | Unused; removed by `clean_up_after_commit` (`2242-2247`) after the next successful update. |
| Anything during the install | Unchanged: journal, backups, `recover` (`2021-2128`, `2312-2388`). | Unchanged. |

Invariants:
1. Only a file whose decoded bytes matched the manifest signature is written to the verified cache.
2. The install is modified only inside the existing transaction, after every file is staged.
3. A resume record never has to be trusted: it only chooses which chunks to skip, and the end-to-end signature decides.

## 6. Install overlap: what overlaps and what does not

**Overlapped**: decoding, signature verification and staging of each file, as it completes (4.7). After the last chunk arrives, only the last files' finish work remains.

**Not overlapped: applying files to the install while the download runs.** Rejected, because it would trade away three things the updater has today for a saving no larger than the commit itself:

1. Downloads may run while a game uses the folder; only the install waits (`stage_after_play` and `install_staged`, `654-681`). A rolling install cannot.
2. A cancel or network failure today leaves the install untouched and costs nothing. With files already applied it would have to roll back and set the full-check marker (`2108-2115`), so the next update reads the whole install.
3. The install would be mixed-version for the whole download instead of the commit's seconds. A crash there is recoverable, but always into a rollback and a full check.

**Follow-up, outside this change.** Once the download of a routine patch takes 1-3 s, the commit is likely the larger part of an update, and its cost is flushes and repeated passes over the data, not ordering: per replaced file three `sync_all` calls and two write-through renames (`replace`, `2150-2189`; `write_journal`, `1994-1999`), a journal rewritten in full for every file, and by `applying_work`'s own accounting five passes over the old file and four over the new (`839-843`). Measure it per stage with `fresh_official_install_total_update_timing` (`fresh_install_bench.rs:4-116`) before designing anything; it is a separate change to code the rollback guarantees depend on.

## 7. Integration points

Leanest diff: one new module holds the pool; `updater.rs` loses its per-file download path.

**New `src/updater/download.rs`** (about 400 lines): constants, `Transfer`, `Chunk`, `Job`, `Queue`, `Pace`, the connection loop, the store thread, `finish`, and `stage(...)` (the supervisor: opens connections, receives store and finish events, collects `Ready`). Declared beside the other submodules at `updater.rs:5-8`.

**`src/updater.rs`**

| Lines | Item | Change |
|---|---|---|
| 49-65 | Traffic comment, `MAX_DOWNLOAD_WORKERS`, `DOWNLOAD_BUDGET_MIB`, `BENCH_DOWNLOAD_WORKERS` | Update the comments (a connection no longer owns a file or keeps one request in flight; the budget bounds decoding). Add `BENCH_PIPELINE_DEPTH` (test only; 0 keeps `MAX_DEPTH`). |
| 855-909 | `Downloads`, `FileProgress` | Keep. Add `resume(bytes)` (credit resumed bytes, rebased) and `restart()` (take a file's received bytes back out when it is downloaded again from 0). |
| 935-948 | `download_memory_mib` | Keep; called from `finish`. |
| 956-1036 | `stage_plan_shared` | Same signature and return value; the body becomes the call into `download::stage`. It stays the seam for `stage_updates` (758), `stage_plan` (930), the traffic tests and the benchmark. |
| 1041-1066 | `download` | Deleted; its verification and delta fallback move into `finish`. |
| 1454-1473 | `Session` doc | Update: download connections pipeline. |
| 1621-1660 | `Session::exchange` | Split out `send(&[u8])` and `read_frame(cap, idle)`; `exchange` becomes those two plus the kind check, so the version, list and download paths share one framing routine. `read_frame` limits silence (the loop `read_manifest_body` already has, 1749-1769), not the frame. |
| 1663-1712 | `fetch`, `fetch_tracked` | Deleted. The tests that call `fetch` (`traffic_tests.rs:888-889`, `919-920`; `live_test.rs:96`, `114`) use a small test helper that stages one transfer through the pool. |
| 1718-1745 | `patch_connect` | For `plain`: `stream.set_nodelay(true)`. |
| 1814-1844 | `produce` | Deleted; becomes `finish`. `read_delta_base` (1846-1859) and `decode_payload` (1789-1813) are used as they are. |
| 1928-1937 | `save` | Unchanged. |
| 2242-2247 | `clean_up_after_commit` | Unchanged; it already removes every file in `staged`, parts included. |

**Unchanged**: `polp.rs` (no wire change), `lz.rs`, `manifest.rs`, `operation.rs`, `stock_socket.rs`, `patch.rs`, the version, prompt and list connections (`520-551`), and the whole commit path (`767-830`, `1948-2388`).

Constraints for the implementer:
- `cache_only_staging_reuses_verified_files_without_a_server_connection` (`updater.rs:2550-2551`) runs on a current-thread runtime: use `spawn_blocking`, never `block_in_place`.
- No new crates. `tokio`'s `sync`, `time`, `net` and `io-util` features are enabled (`Cargo.toml`); `tokio::fs` is not needed.
- On an error, `tasks.abort_all()` still stops the connections (as at `1027-1028`); then drop the store sender, join the store thread, and await the finish tasks before returning.

## 8. Test plan

### 8.1 Existing tests that encode stop-and-wait

| Test (`src/updater/traffic_tests.rs`) | Change |
|---|---|
| `an_update_asks_version_then_list_then_downloads_in_parallel` (260-285), the W2U test (line 415), the Play test (line 779) | They assert no `What::Overlap` on any connection. Keep that for the version, prompt and list connections; allow it on download connections. |
| `downloads_run_on_at_most_max_workers_connections_and_all_files_verify` (527-541) | Requires `peak == MAX_DOWNLOAD_WORKERS`. With spaced opening, 80 requests can finish before 16 connections exist: use enough work to reach 16 and assert never more than 16. Progress totals unchanged. |
| `one_failing_file_retries_then_stops_every_worker` (545-561) | Must pass unchanged: the failing request is asked exactly 3 times, the rest stop. |
| `a_restarted_update_reuses_verified_files_and_redownloads_corrupt_or_partial_ones` (566-590) | Must pass unchanged (70 KB files stay in memory, so "each from its start" holds). |
| `a_broken_chunk_reply_closes_like_stock_and_resumes_the_chunk_on_a_new_connection` (859-898), `a_dropped_session_still_closes_like_stock` (902-928) | Run with `BENCH_PIPELINE_DEPTH = 1` through the one-transfer helper; they pin the stock close, not the pipeline. |
| `crystal_requests_match_a_stock_capture` (706-751) | Unaffected: it derives requests from the plan, not from the pool. |

### 8.2 New unit tests (loopback servers, same file)

1. **Pipelined replies in order**: 200 single-chunk files and one 2 MiB file, server delay 20 ms. Every file verifies; the server saw more than one request waiting on a connection; no request was asked twice.
2. **A large file is spread out**: one 8 MiB file, enough delay to open 16 connections. Every connection served at least one of its chunks.
3. **Largest first**: the first request is for the largest transfer; the first request of every file comes in non-increasing size order.
4. **Break mid-pipeline**: the server answers 2 of 4 pipelined requests, then closes. The 2 unanswered chunks are asked again once each; the file verifies; only the head chunk was charged an attempt.
5. **A server that ignores pipelined requests**: it answers the first request of each write and discards the rest. With the probation shortened to 300 ms through a test seam: the run completes, every later connection keeps one request in flight, no chunk is charged.
6. **Out-of-order reply**: the server swaps two replies. The connection closes, the run falls back to depth 1 and completes.
7. **Partial resume**: a 40 MiB stored file, cancelled near the middle. The part's header frontier is a multiple of 64 KB at or below the bytes received. The second run requests no offset below the frontier, at most `CHECKPOINT + 16 MiB` in total more than the remainder, and the file verifies.
8. **A lying part**: flip bytes below the frontier of a valid part. The second run fails verification once, downloads the file again from 0, verifies, and the progress total ends exact.
9. **Slow replies are not dead**: a server sending one 64 KB reply at 1 KB per 400 ms (26 s) with the idle limit shortened to 2 s through a seam: the chunk completes without a retry.
10. **`Pace::window`** as a pure function: 64 KB after 150 ms gives 7 chunks; 15 KB/s gives 1; 24-byte replies reach 16 requests by the third round; a 3 s gap halves the history.
11. **Bounded memory, structurally**: the store channel never holds more than 64 frames, and a transfer of at least `PART_MIN` never allocates its length (assert on `Body`).
12. **Delta chain from parts**: the existing delta fixture (`update_fixture`, 157-183) through the new path, including a corrupt delta falling back to the full file with the progress total rebased.

### 8.3 Live tests (ignored)

**`official_server_answers_pipelined_requests_in_order`** (new, in `fresh_install_bench.rs`): the probe of section 2.2 as a test. Fetch the list, pick 3 tiny files and one multi-chunk file, send 6 requests in one write, assert order, echoes and equality with stop-and-wait copies. About 400 KB. Run it for both products before shipping, since the Viewer host was not probed.

**`download_concurrency_bench`** (`fresh_install_bench.rs:118-169`), extended:
- Matrix of (connections, depth): (1,1), (1,16), (4,1), (4,16), (16,1), (16,16), then (16,1) again for drift. Depth through `BENCH_PIPELINE_DEPTH`.
- Two sets: the existing large set (256 KB to 16 MB, 600 MB), and a small set (the 500 smallest current files).
- Print per run: MB/s, requests/s, median time to first byte at depth 1 (the regime marker), mean requests in flight, reconnects, fallbacks, peak working set, and the time from 95% to 100% of bytes.

Success metrics, by regime. The regime is "normal" when the median depth-1 chunk completes within 500 ms, otherwise "slow".

| Metric | Normal period | Slow period |
|---|---|---|
| Small set, requests/s, (16,16) vs (16,1) | at least 5x | at least 0.95x |
| Large set, MB/s, (16,16) vs (16,1) | at least 1.0x; record the ratio | at least 0.95x |
| Large set, (1,16) vs (1,1) | record. Below 1.2x means a connection is capped near 64 KB per round trip by the server or the window; connection count stays the only large-file lever | not applicable |
| Tail: time for the last 5% of bytes | at most 2x the average time per 5% | same |
| Peak working set above idle, 16 connections, large set | at most 300 MiB | same |
| Reconnects, fallbacks, failed chunks | 0 | 0 |
| Resume: cancel at 50% of a 100 MB file, rerun | downloads at most 50 MB + 17 MiB | same |

### 8.4 Once, by hand

`fresh_official_install_total_update_timing` before and after, for the stage split (download, staging, apply, verify) that decides whether the commit follow-up of section 6 is worth doing.

## 9. Expected results and risks

### 9.1 Expected speedups

Estimates from the measurements in section 2, at a 145 ms round trip. Only the stock row is measured end to end.

| Case | Today | After | Why |
|---|---|---|---|
| Routine patch download (459 files, 540 requests, 18.5 MB), normal period | Stock 118 s (measured). Crystal about 5-6 s: 540 round trips over 16 connections. | About 1.5-3 s | Requests stop costing a round trip each. The remaining time is the 18.5 MB itself. |
| Large download, normal period | At most 16 x 64 KB per 155 ms, about 6.7 MB/s, falling toward 0.42 MB/s as the last big files each hold one connection | At least 6.7 MB/s held to the last chunk. More if a connection can carry more than 64 KB per round trip; unknown until the benchmark runs in a normal period | Every connection stays busy; pipelines keep the server sending. |
| A lone 100 MB file | About 4 minutes on one connection | About 15 s at 6.7 MB/s | Chunk queue. |
| Slow period (as on 2026-11-12, 15-20 KB/s per flow) | 0.24 MB/s at 16 | Same throughput | Per-flow limit. Gains are the removed tail, no false 30 s timeouts, and resume. |
| Cancel or crash mid-file | The whole file again (in the slow period, 15 minutes for 16 MB) | At most about 16 MiB again | Frontier resume. |
| Memory | Up to the 1 GiB budget in transfer and decode buffers; large files wait for budget | About 20 MiB on the network side, plus the decode budget in use | Streamed parts. |

### 9.2 Risks, ranked

1. **Wire shape (owner decision).** Download connections already differ from stock (plain sockets, 16 at once). Pipelined, interleaved requests differ further and are visible to Square Enix: several requests arrive before a reply, and no connection carries a whole file. `MAX_DEPTH = 1` removes the pipelining; the interleaving remains.
2. **Per-connection ceiling unknown in a normal period.** If the server keeps only about 64 KB in flight per connection, large-file throughput stays at today's ceiling and only the small-file, tail, memory and resume gains remain. The (1,16) vs (1,1) benchmark row answers this. If it shows the ceiling, the one further experiment is an explicit 1-4 MiB `SO_RCVBUF` set before connect, to tell a client window limit from a server limit; Windows receive auto-tuning is left on by default.
3. **Server or host differences.** Only the FFXI host was probed, on one day. The Viewer host, other regions and future server changes are covered by the run-time fallback (4.4) and the live test (8.3).
4. **Store bugs.** Positional writes and resume are new code. Every staged file is still verified end to end before it is cached and again at commit, so a bug costs a retry or a failed update, never a wrong install.
5. **Connection limits.** Nothing was refused at 16. A refusal now lowers the limit for 30 s instead of failing while other connections work.
6. **Test churn.** Five traffic tests encode stop-and-wait or instant opening (8.1).
7. **Slow disks.** One store thread and a `sync_data` per 16 MiB can pause the network briefly on a hard disk. The channel absorbs 4 MiB; beyond that TCP flow control slows the server, which is the correct outcome.

## 10. Smaller items

- **TCP_NODELAY** on download connections. A pipeline is topped up one small request at a time; with Nagle, each top-up would wait for the ACK of the previous small segment. The earlier A/B (`Crystal-Launcher\src\updater\download_bench.rs`) tested it on stop-and-wait, where it cannot matter. The stock-shaped version and list sockets keep stock's settings (`stock_socket.rs:18`).
- **Receive buffer**: unchanged, auto-tuned. The stock 64 KB socket costs a second round trip per chunk (2.1); the plain socket already avoids that.
- **Delayed ACK**: unchanged. With a continuous stream it no longer holds anything back.
- **Socket reuse**: a connection now lives for the whole stage and closes when the queue is empty, with the stock close (FIN 100-115 ms after the last reply).
- **DNS**: unchanged. Each connect resolves through the Windows cache, which the list connection filled seconds earlier (`dns.rs:121-125`).
- **Hashing**: one MD5 per frame (`polp.rs:88`) and one signature pass per decoded file, both far above network speed. No new crates. The dropped re-read (4.7) removes one full pass per staged file.
- **Staged-file flushes (optional, measure first)**: `save` flushes every staged cache file. A cache file is verified before every use, so an unflushed variant for cache files only would be safe and would remove one flush per file from the finish stage. Journal and record writes must keep the flush.
- **Manifest connection (owner decision, not part of this)**: the 1.9 MB file list takes 5.7 s on the stock-shaped socket (capture) because of its 32 KB window. A plain socket would take about 1.5 s but changes that connection's fingerprint.

## 11. Decisions for the owner

1. Pipelined requests on download connections: yes (this design) or keep one request per connection (`MAX_DEPTH = 1`; the chunk queue, streaming and resume still apply).
2. The connection cap stays 16. In a slow period more flows is the only thing that raises throughput, and it takes a larger share of a congested server from other players. Not recommended.
3. Install stays one transaction at the end (section 6). The commit's own cost is a separate, measured follow-up.
