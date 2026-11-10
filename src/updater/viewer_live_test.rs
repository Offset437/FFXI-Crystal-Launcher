//! Opt-in production POL update on a marked disposable copy; no sign-in,
//! Viewer launch, DLL registration, or writes to the registered installation.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "official network required; CRYSTAL_POL_TEST_INSTALL must be a marked disposable copy beneath TEMP"]
async fn official_viewer_copy_updates_and_verifies_both_deployment_targets() {
    let supplied = PathBuf::from(std::env::var_os("CRYSTAL_POL_TEST_INSTALL").expect("Set the marked disposable POL copy"));
    ensure_plain(&supplied).unwrap();
    let root = fs::canonicalize(supplied).unwrap();
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(root != temp && root.starts_with(&temp));
    assert_eq!(fs::read(root.join(".crystal-pol-test")).unwrap(), b"disposable PlayOnline copy\n");
    assert!(!root.join(STATE_DIR).exists());
    let user_file = root.join("keep-user-file"); fs::write(&user_file, b"keep this").unwrap();
    // The PlayOnline update production runs (main.rs: run_combined(Some(viewer), None, ..)).
    let viewer = |root: PathBuf, mode, op| async move {
        run_combined(Some(root), None, mode, op).await.map(|c| c.viewer.unwrap()).map_err(|f| f.message)
    };
    let before = viewer(root.clone(), Mode::Check, Operation::new()).await.unwrap();
    eprintln!("POL baseline: {}", before.summary);
    assert!(!before.verified, "Use a fresh/outdated POL copy for the live update exercise");
    let op = Operation::new(); let watched = op.clone();
    let watcher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let progress = watched.snapshot();
            eprintln!("POL {}: {}/{} {}", progress.stage, progress.done, progress.total, progress.detail);
        }
    });
    let updated = viewer(root.clone(), Mode::Update, op.clone()).await;
    watcher.abort();
    let updated = updated.unwrap();
    eprintln!("POL updated: {}", updated.summary);
    assert!(updated.verified); assert!(updated.repaired > 0);
    assert_eq!(crate::patch::installed_for_request_for(&root, crate::updater::Product::Viewer), updated.version);
    assert!(op.snapshot().overall.unwrap().complete);
    let after = viewer(root.clone(), Mode::Check, Operation::new()).await.unwrap();
    assert!(after.verified); assert_eq!(after.current, after.total); assert_eq!(after.version, updated.version);
    for name in ["app.dll", "polcore.dll"] {
        assert_eq!(fs::read(root.join(format!("viewer/com/{name}"))).unwrap(),
            fs::read(root.join(format!("patchfiles/PlayOnlineViewer/viewer/com/{name}"))).unwrap());
    }
    assert_eq!(fs::read(&user_file).unwrap(), b"keep this");
    assert!(root.join(format!("{STATE_DIR}/backups")).is_dir());
    assert!(!root.join(format!("{STATE_DIR}/transaction.json")).exists());
    for path in ["file.txt", "version.dat"] { assert!(root.join(path).is_file()); }
    eprintln!("Live POL copy proof: {} files independently rechecked; staged/live DLLs identical; originals retained.", after.current);
}
