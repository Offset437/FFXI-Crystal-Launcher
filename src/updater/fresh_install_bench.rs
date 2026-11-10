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
