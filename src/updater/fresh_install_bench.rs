//! Opt-in timing of the production combined updater on fresh MSI images.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads and updates fresh MSI copies; requires a marked CRYSTAL_FRESH_UPDATE_BENCH_ROOT beneath TEMP"]
async fn fresh_official_install_total_update_timing() {
    let supplied = PathBuf::from(std::env::var_os("CRYSTAL_FRESH_UPDATE_BENCH_ROOT")
        .expect("Set the marked fresh MSI extraction directory"));
    ensure_plain(&supplied).unwrap();
    let root = fs::canonicalize(supplied).unwrap();
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(root != temp && root.starts_with(&temp));
    assert_eq!(fs::read(root.join(".crystal-fresh-update-benchmark")).unwrap(),
        b"fresh official MSI images; disposable update benchmark\n");
    let game = root.join("ffxi-image/PFiles/PlayOnline/SquareEnix/FINAL FANTASY XI");
    let viewer = root.join("pol-image/PFiles/PlayOnline/SquareEnix/PlayOnlineViewer");
    for install in [&game, &viewer] {
        ensure_plain(install).unwrap();
        assert!(install.is_dir() && !install.join(STATE_DIR).exists(),
            "Use new MSI images with no existing patch cache or transactions");
    }
    let original = fs::read(game.join("FFXiMain.dll")).unwrap();
    assert!(installer_baseline::load().unwrap().matches("FFXiMain.dll", original.len() as u64,
        md5::compute(&original).0), "The game must originate from the official full installer");
    let baseline_game_version = crate::patch::installed_for_request_for(&game, crate::updater::Product::Ffxi);
    let baseline_viewer_version = crate::patch::installed_for_request_for(&viewer, crate::updater::Product::Viewer);
    let output = fs::OpenOptions::new().write(true).create_new(true)
        .open(root.join("update-progress.jsonl")).unwrap();
    let op = Operation::new();
    let watched = op.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_watch = stopped.clone();
    let started_utc = chrono::Utc::now().to_rfc3339();
    let started = Instant::now();
    let monitor = tokio::spawn(async move {
        let mut output = output;
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut previous_stage = crate::operation::Stage::None;
        let mut last_saved = Duration::ZERO;
        let mut last_console = Duration::ZERO;
        loop {
            interval.tick().await;
            let p = watched.snapshot();
            let elapsed = started.elapsed();
            let stage_changed = p.stage != previous_stage;
            let ending = stop_watch.load(Ordering::Acquire);
            if stage_changed || elapsed.saturating_sub(last_saved) >= Duration::from_secs(1) || ending {
                let row = serde_json::json!({"elapsed_seconds": elapsed.as_secs_f64(),
                    "stage": p.stage.as_str(), "done": p.done, "total": p.total, "bytes": p.bytes,
                    "detail": p.detail, "remaining_seconds": p.remaining.map(|d| d.as_secs_f64()),
                    "overall_remaining_seconds": p.overall.and_then(|o| o.remaining).map(|d| d.as_secs_f64()),
                    "overall_fraction": p.overall.and_then(|o| o.fraction)});
                writeln!(output, "{row}").unwrap();
                output.flush().unwrap();
                last_saved = elapsed;
            }
            if stage_changed || elapsed.saturating_sub(last_console) >= Duration::from_secs(30) || ending {
                eprintln!("Fresh update +{:.2}s: {} {}/{} {}", elapsed.as_secs_f64(),
                    p.stage, p.done, p.total, p.detail);
                last_console = elapsed;
            }
            previous_stage = p.stage;
            if ending { break; }
        }
    });
    let result = run_combined(Some(viewer.clone()), Some(game.clone()), Mode::Update, op.clone()).await;
    let elapsed = started.elapsed();
    let completed_utc = chrono::Utc::now().to_rfc3339();
    stopped.store(true, Ordering::Release);
    monitor.await.unwrap();
    let final_progress = op.snapshot();
    let mut summary = serde_json::json!({"started_utc": started_utc, "completed_utc": completed_utc,
        "total_update_seconds": elapsed.as_secs_f64(), "production_download_workers": 1,
        "fresh_patch_cache": true, "baseline_game_version": baseline_game_version,
        "baseline_viewer_version": baseline_viewer_version,
        "timing_scope": "Production run_combined: manifests, file scans, downloads, staging, application, final verification and applied records. MSI extraction and later independent checks excluded.",
        "overall_complete": final_progress.overall.is_some_and(|o| o.complete)});
    struct Both<'a> { viewer: &'a Report, game: &'a Report }
    fn both(combined: &CombinedReport) -> Both<'_> {
        Both { viewer: combined.viewer.as_ref().unwrap(), game: combined.game.as_ref().unwrap() }
    }
    match &result {
        Ok(combined) => {
            let report = both(combined);
            summary["success"] = serde_json::json!(report.viewer.verified && report.game.verified);
            summary["viewer"] = serde_json::json!({"version": report.viewer.version, "files": report.viewer.total,
                "updated": report.viewer.repaired, "decoded_bytes_staged": report.viewer.bytes,
                "modified": report.viewer.modified, "verified": report.viewer.verified});
            summary["game"] = serde_json::json!({"version": report.game.version, "files": report.game.total,
                "updated": report.game.repaired, "decoded_bytes_staged": report.game.bytes,
                "modified": report.game.modified, "verified": report.game.verified});
        }
        Err(error) => {
            summary["success"] = serde_json::json!(false);
            summary["error"] = serde_json::json!(error.message);
        }
    }
    let mut summary_file = fs::OpenOptions::new().write(true).create_new(true)
        .open(root.join("update-result.json")).unwrap();
    writeln!(summary_file, "{}", serde_json::to_string_pretty(&summary).unwrap()).unwrap();
    let combined = result.unwrap_or_else(|error| panic!("Fresh update failed: {}", error.message));
    eprintln!("Fresh update completed in {:.2}s: {}", elapsed.as_secs_f64(), combined.summary());
    let report = both(&combined);
    assert!(report.viewer.verified && report.game.verified);
    assert_eq!(report.game.modified, 0);
    assert!(final_progress.overall.is_some_and(|o| o.complete));
    let independent = run_combined(Some(viewer), Some(game), Mode::Check, Operation::new()).await
        .unwrap_or_else(|error| panic!("Independent check failed: {}", error.message));
    let checked = both(&independent);
    assert!(checked.viewer.verified && checked.game.verified);
    fs::write(root.join("independent-check.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "viewer_current": checked.viewer.current, "viewer_total": checked.viewer.total,
        "game_current": checked.game.current, "game_total": checked.game.total,
        "both_verified": checked.viewer.verified && checked.game.verified})).unwrap()).unwrap();
    eprintln!("Independent post-update check: {}", independent.summary());
}

/// Download throughput of the production download pool against the official FFXI patch host,
/// over a matrix of (connections, pipeline depth): CRYSTAL_BENCH_MATRIX, default
/// `1x1,1x16,4x1,4x16,16x1,16x16,16x1` (the last repeats for drift). Two sets of real current
/// files: `large` (full files of 256 KB-16 MB, up to 600 MB) and `small` (the 500 smallest);
/// CRYSTAL_BENCH_SET picks one (default both). Each run downloads into a fresh folder beneath
/// TEMP for a fixed window (CRYSTAL_BENCH_SECONDS, default 40), then deletes it. Touches no
/// install. The regime is "normal" when the median depth-1 chunk completes within 500 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "downloads up to ~4 GB of official FFXI files into TEMP over several minutes; run explicitly"]
async fn download_concurrency_bench() {
    let server = Product::Ffxi.server();
    let text = official_list(Product::Ffxi, server, &Operation::new()).await.unwrap();
    let m = parse_list(Product::Ffxi, &text).unwrap();
    let plan = |index: usize| Planned { index, action: Action::Direct, original: None, keep: false };
    let (mut large, mut set) = (Vec::new(), 0u64);
    for (index, history) in m.files.iter().enumerate() {
        let len = history.current().direct_len;
        // ROM5/5/113.DAT's official payload is damaged (`official_rom5_5_113_payload_is_damaged_…`).
        if (256 << 10..=16 << 20).contains(&len) && set < 600 << 20 && history.path != "ROM5/5/113.DAT" { large.push(plan(index)); set += len; }
    }
    let mut by_size: Vec<usize> = (0..m.files.len()).collect();
    by_size.sort_by_key(|&index| m.files[index].current().direct_len);
    let small: Vec<Planned> = by_size.into_iter().take(500).map(plan).collect();
    let env = |name: &str| std::env::var(name).ok();
    let window = Duration::from_secs(env("CRYSTAL_BENCH_SECONDS").and_then(|s| s.parse().ok()).unwrap_or(40));
    let matrix: Vec<(usize, usize)> = env("CRYSTAL_BENCH_MATRIX").unwrap_or_else(|| "1x1,1x16,4x1,4x16,16x1,16x16,16x1".into())
        .split(',').map(|run| { let (c, d) = run.trim().split_once('x').unwrap(); (c.parse().unwrap(), d.parse().unwrap()) }).collect();
    let which = env("CRYSTAL_BENCH_SET").unwrap_or_else(|| "both".into());
    let mut rows = Vec::new();
    for (name, planned) in [("large", &large), ("small", &small)] {
        if which != "both" && which != name { continue; }
        let bytes: u64 = planned.iter().map(|p| m.files[p.index].current().direct_len).sum();
        eprintln!("{name} set: {} files, {:.1} MB; window {window:?}", planned.len(), bytes as f64 / 1e6);
        for (run, &(connections, depth)) in matrix.iter().enumerate() {
            let root = std::env::temp_dir().join(format!("crystal-download-bench-{}-{name}-{run}", std::process::id()));
            let staged = root.join(STATE_DIR).join("staged");
            fs::create_dir_all(&staged).unwrap();
            let op = Operation::new();
            let groups = stage_groups(Product::Ffxi, &m, planned);
            let downloads = Downloads::new(op.clone(), groups.iter().map(|g| download_size(&m.files[g.source.index], &g.source.action)).collect());
            let stats = Arc::new(download::Stats::default());
            let tuning = download::Tuning { connections, depth, ..Default::default() };
            // Samples the working set and when 95% and 100% of the set's bytes arrived.
            let idle = crate::footprint::memory(None).map_or(0, |m| m.working_set);
            let (watched, total) = (op.clone(), downloads.state.lock().unwrap().total);
            let sampler = tokio::spawn(async move {
                let (mut peak, mut p95) = (idle, None);
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    peak = peak.max(crate::footprint::memory(None).map_or(0, |m| m.working_set));
                    let done = watched.snapshot().done;
                    if p95.is_none() && done * 20 >= total * 19 { p95 = Some(Instant::now()); }
                    if done >= total || watched.cancelled() { return (peak, p95); }
                }
            });
            let cancel = op.clone();
            let timer = tokio::spawn(async move { tokio::time::sleep(window).await; cancel.cancel(); });
            let started = Instant::now();
            let result = download::stage(Product::Ffxi, server, &root, &staged, &m, groups, &op, &downloads, 0, tuning, stats.clone()).await;
            let (elapsed, ended) = (started.elapsed(), Instant::now());
            timer.abort();
            op.cancel(); // ends the sampler
            let (peak, p95) = sampler.await.unwrap();
            let outcome = match &result {
                Ok(_) => "whole set".to_owned(),
                Err(_) if elapsed >= window => "window ended".to_owned(),
                Err(error) => format!("ERROR: {error}"),
            };
            let received = stats.bytes.load(Ordering::Relaxed);
            let requests = stats.requests.load(Ordering::Relaxed);
            let mut solo = stats.solo.lock().unwrap().clone();
            solo.sort();
            let median = solo.get(solo.len() / 2).copied();
            let in_flight = stats.in_flight.load(Ordering::Relaxed) as f64 / requests.max(1) as f64;
            let tail = p95.filter(|_| result.is_ok()).map(|at| ended - at);
            let rate = received as f64 / 1e6 / elapsed.as_secs_f64();
            eprintln!("{name} {connections:>2}x{depth:<2}: {rate:6.2} MB/s, {:7.1} req/s, depth-1 median {median:?}, {in_flight:.1} in flight, \
                {} reconnects, {} fallbacks, +{} MiB peak, 95→100% {tail:?} ({:.1} MB in {:.1} s; {outcome})",
                requests as f64 / elapsed.as_secs_f64(), stats.reconnects.load(Ordering::Relaxed), stats.fallbacks.load(Ordering::Relaxed),
                peak.saturating_sub(idle) >> 20, received as f64 / 1e6, elapsed.as_secs_f64());
            for failure in stats.redownloads.lock().unwrap().iter() { eprintln!("  downloaded again: {failure}"); }
            rows.push((name, connections, depth, rate, median, outcome));
            // Connections close on their own threads ~0.1-0.3 s after the cancel.
            tokio::time::sleep(Duration::from_secs(2)).await;
            diagnose(server, &staged, &m).await;
            fs::remove_dir_all(&root).unwrap();
        }
    }
    eprintln!("{rows:#?}");
}
/// Each payload that failed to decode or verify in a run (kept as `.xi-bad` by test builds),
/// compared with the same file fetched again one request at a time: does the clean copy decode,
/// and which 64 KB chunks differ (zeros, another chunk of the file, or other bytes)?
async fn diagnose(server: &str, staged: &Path, m: &Manifest) {
    let op = Operation::new();
    for bad in fs::read_dir(staged).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "xi-bad")) {
        let Some(history) = m.files.iter().find(|h| download::part_path(staged, &h.current().direct, h.current().direct_len)
            .is_some_and(|part| part.with_extension("xi-bad") == bad)) else { continue };
        let (current, chunk) = (history.current(), polp::CHUNK_LEN as usize);
        let received = fs::read(&bad).unwrap()[download::PART_HEADER as usize..].to_vec();
        let mut session = Session::connect_download(server, &op).await.unwrap();
        let mut clean = Vec::new();
        while (clean.len() as u64) < current.direct_len {
            let count = (current.direct_len - clean.len() as u64).min(chunk as u64) as u32;
            session.send(&polp::file_request(&Product::Ffxi.tags(), &current.direct, clean.len() as u32, count), &op).await.unwrap();
            let frame = session.read_frame(128 * 1024, Duration::from_secs(60), &op, false).await.unwrap();
            clean.extend_from_slice(polp::file_chunk(polp::parse(&frame).unwrap().body).unwrap().1);
        }
        session.broken = false;
        session.close().await;
        let decoded = decode_payload(&clean, current.sig.len as usize);
        eprintln!("  {}: clean copy {}", history.path, match &decoded {
            Ok(data) if FileSig::of(data) == current.sig => "decodes and verifies".to_owned(),
            Ok(data) => format!("decodes to {} bytes but does not verify (manifest {})", data.len(), current.sig.len),
            Err(error) => format!("fails too: {error}"),
        });
        let chunks: Vec<&[u8]> = clean.chunks(chunk).collect();
        for (index, (got, want)) in received.chunks(chunk).zip(&chunks).enumerate() {
            if got == *want { continue; }
            let what = if got.iter().all(|&b| b == 0) { "zeros".to_owned() }
                else if let Some(other) = chunks.iter().position(|c| c == &got) { format!("chunk {other}'s bytes") }
                else { format!("other bytes, first difference at +{}", got.iter().zip(*want).position(|(a, b)| a != b).unwrap_or(0)) };
            eprintln!("    chunk {index} (offset {}): {what}", index * chunk);
        }
    }
}
/// The official ROM5/5/113.DAT payload is damaged on the patch server: its zlib trailer's
/// Adler-32 is the installed file's (the stream was made from that file), but its deflate body
/// stops reproducing the file 861,479 bytes in (input ~445.7 KB of 899,009) and never verifies,
/// whatever the request shape. The pool must fail it with its name and stage nothing.
/// CRYSTAL_ROM5_5_113 (optional) names a known-good copy to compare against. About 0.9 MB from
/// the FFXI patch host into memory; touches no install.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads one 0.9 MB official file; run explicitly"]
async fn official_rom5_5_113_payload_is_damaged_and_never_staged() {
    use std::io::Read as _;
    let server = Product::Ffxi.server();
    let m = parse_list(Product::Ffxi, &official_list(Product::Ffxi, server, &Operation::new()).await.unwrap()).unwrap();
    let index = m.files.iter().position(|h| h.path == "ROM5/5/113.DAT").unwrap();
    let current = m.files[index].current().clone();
    let root = std::env::temp_dir().join(format!("crystal-rom5-5-113-{}", std::process::id()));
    let staged = root.join(STATE_DIR).join("staged");
    fs::create_dir_all(&staged).unwrap();
    let planned = [Planned { index, action: Action::Direct, original: None, keep: false }];
    let op = Operation::new();
    let downloads = Downloads::new(op.clone(), vec![current.direct_len]);
    let error = stage_plan_shared(Product::Ffxi, server, &root, &staged, &m, stage_groups(Product::Ffxi, &m, &planned), &op, &downloads, 0)
        .await.unwrap_err();
    assert!(error.contains("ROM5/5/113.DAT"), "{error}");
    assert!(!fs::read_dir(&staged).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "bin")), "nothing staged");
    // The payload once more, as one request.
    let mut session = Session::connect_download(server, &op).await.unwrap();
    session.send(&polp::file_request(&Product::Ffxi.tags(), &current.direct, 0, current.direct_len as u32), &op).await.unwrap();
    let frame = session.read_frame(2 << 20, Duration::from_secs(120), &op, false).await.unwrap();
    let payload = polp::file_chunk(polp::parse(&frame).unwrap().body).unwrap().1.to_vec();
    session.broken = false;
    session.close().await;
    let mut inflated = Vec::new();
    let ended = flate2::read::ZlibDecoder::new(&payload[1..]).read_to_end(&mut inflated);
    let trailer = u32::from_be_bytes(payload[payload.len() - 4..].try_into().unwrap());
    eprintln!("{error}; flag {}, inflates to {} bytes ({ended:?}), trailer Adler-32 {trailer:08x}", payload[0], inflated.len());
    assert!(payload[0] == 3 && ended.is_err(), "the stream does not decode");
    if let Some(good) = std::env::var_os("CRYSTAL_ROM5_5_113").map(|path| fs::read(path).unwrap()) {
        assert_eq!(FileSig::of(&good), current.sig, "a known-good copy");
        assert_eq!(adler32(&good), trailer, "the stream was made from that file");
        let same = inflated.iter().zip(&good).take_while(|(a, b)| a == b).count();
        eprintln!("the inflated stream matches the good file for its first {same} bytes");
        assert!(same < good.len());
    }
    fs::remove_dir_all(root).unwrap();
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data { a = (a + byte as u32) % 65521; b = (b + a) % 65521; }
    (b << 16) | a
}