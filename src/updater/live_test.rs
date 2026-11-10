//! Opt-in end-to-end exercise on an explicitly marked, full scratch install.
//! Uses the production updater and real official files; never signs in.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "read-only official file check; requires CRYSTAL_READONLY_CHECK_INSTALL and network"]
async fn installed_game_readonly_check() {
    let root = PathBuf::from(std::env::var_os("CRYSTAL_READONLY_CHECK_INSTALL").expect("Set the installation to inspect read-only"));
    let report = run(root, Mode::Check, Operation::new()).await.unwrap();
    eprintln!("Read-only FFXI: {} Version: {}", report.summary, report.version);
    for issue in report.issues { eprintln!("{issue}"); }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires official network and a marked failing-install copy beneath TEMP"]
async fn official_installer_bootstrap_updates_stock_remnants_and_creates_applied_record() {
    let supplied = PathBuf::from(std::env::var_os("CRYSTAL_PATCH_TEST_INSTALL").expect("Set the marked full copy"));
    ensure_plain(&supplied).unwrap();
    let root = fs::canonicalize(supplied).unwrap();
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(root != temp && root.starts_with(&temp), "Only a copy beneath TEMP may be modified");
    assert_eq!(fs::read(root.join(".crystal-patch-test")).unwrap(), b"disposable full install copy\n");
    assert!(!root.join(STATE_DIR).exists(), "Preserve earlier transactions; use a new copy");
    assert!(!root.join("patch.ver").exists(), "This exercises the actual missing-version failure");
    let before = run(root.clone(), Mode::Check, Operation::new()).await.unwrap();
    eprintln!("Stock-remnant baseline: {}", before.summary);
    assert!(before.outdated > 0 && before.modified == 0 && before.missing == 0);
    let original = fs::read(root.join("FFXiMain.dll")).unwrap();
    assert!(installer_baseline::load().unwrap().matches("FFXiMain.dll", original.len() as u64, md5::compute(&original).0));
    let started = Instant::now();
    let op = Operation::new();
    let watched = op.clone();
    let monitor = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let p = watched.snapshot();
            eprintln!("Bootstrap progress: {} {}/{} {}", p.stage, p.done, p.total, p.detail);
        }
    });
    let result = run(root.clone(), Mode::Update, op).await;
    monitor.abort();
    let updated = result.unwrap();
    eprintln!("Bootstrap result: {}; staged {} decoded bytes; elapsed {:.2}s", updated.summary, updated.bytes, started.elapsed().as_secs_f64());
    assert!(updated.verified && updated.modified == 0);
    assert_eq!(updated.repaired, before.outdated);
    assert_eq!(crate::patch::installed_for_request(&root), updated.version);
    let after = run(root.clone(), Mode::Check, Operation::new()).await.unwrap();
    assert!(after.verified && after.current == after.total && after.modified == 0);
    eprintln!("Independent final check: {}", after.summary);
    let backup = fs::read_dir(root.join(STATE_DIR).join("backups")).unwrap().next().unwrap().unwrap().path();
    let journal: Journal = serde_json::from_slice(&fs::read(backup.join("records.json")).unwrap()).unwrap();
    let original_backup = journal.entries.iter().find(|e| e.path == "FFXiMain.dll").unwrap().backup.as_ref().unwrap();
    assert_eq!(fs::read(root.join(original_backup)).unwrap(), original);
    assert!(!root.join(STATE_DIR).join("transaction.json").exists());
    eprintln!("Original stock DLL retained byte-for-byte in backups; applied metadata decodes; no pending transaction.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an official-server connection and CRYSTAL_PATCH_TEST_INSTALL pointing to a marked full copy beneath TEMP"]
async fn official_copy_update_repair_and_cancel_resume() {
    let supplied = PathBuf::from(std::env::var_os("CRYSTAL_PATCH_TEST_INSTALL")
        .expect("Set CRYSTAL_PATCH_TEST_INSTALL to the disposable full install copy"));
    ensure_plain(&supplied).unwrap();
    let root = fs::canonicalize(&supplied).unwrap();
    let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
    assert!(root != temp && root.starts_with(&temp), "Only a copy beneath TEMP may be modified");
    assert_eq!(fs::read(root.join(".crystal-patch-test")).unwrap(), b"disposable full install copy\n");
    assert!(!root.join(STATE_DIR).exists(), "Start with a clean full copy, without earlier updater transactions");

    let baseline = run(root.clone(), Mode::Check, Operation::new()).await.unwrap();
    eprintln!("Baseline: {}", baseline.summary);
    assert!(baseline.verified, "A clean baseline is required before introducing controlled damage");

    // Stock's sequence: the version request and the file list each on their own connection,
    // then one download connection, closed like stock.
    let operation = Operation::new();
    let server = Product::Ffxi.server();
    let installed = crate::patch::installed_for_request(&root);
    assert_eq!(official_version(Product::Ffxi, server, &installed, &operation).await.unwrap(), baseline.version);
    let text = official_list(Product::Ffxi, server, &operation).await.unwrap();
    let m = parse_list(Product::Ffxi, &text).unwrap();
    assert_eq!(Product::Ffxi.latest_stamp(&m), Some(baseline.version.as_str()));
    let mut session = Session::connect_to(Product::Ffxi, server, &operation).await.unwrap();
    let history = |path: &str| m.files.iter().find(|h| h.path == path).unwrap();
    let old_file = history("config.sys");
    let missing_file = history("patch.sin");
    let modified_file = history("ROM/285/40.DAT");
    let old_index = (0..old_file.versions.len() - 1).find(|&i| {
        let delta_bytes = old_file.versions[i + 1..].iter().try_fold(0u64, |sum, v| {
            v.indirect.as_ref().and_then(|d| sum.checked_add(d.len))
        });
        old_file.versions[i].sig != old_file.current().sig
            && delta_bytes.is_some_and(|n| n < old_file.current().direct_len)
    }).expect("config.sys must offer a recognized old base with a cheaper delta chain");
    let old = &old_file.versions[old_index];
    let raw = session.fetch(&old.direct, old.direct_len, &operation).await.unwrap();
    let base = decode_payload(&raw, old.sig.len as usize).unwrap();
    assert_eq!(FileSig::of(&base), old.sig);
    let old_path = confined(&root, &old_file.path).unwrap();
    let missing_path = confined(&root, &missing_file.path).unwrap();
    let modified_path = confined(&root, &modified_file.path).unwrap();
    let applied_before = fs::read(root.join("patch.ver")).unwrap();
    let manifest_before = fs::read(root.join("patch.cfg")).unwrap();
    let modified_before = fs::read(&modified_path).unwrap();
    let custom = b"Crystal scratch test: deliberately modified file";
    fs::write(&old_path, &base).unwrap();
    fs::remove_file(&missing_path).unwrap();
    fs::write(&modified_path, custom).unwrap();
    eprintln!("Introduced old {} ({}), missing {}, and modified {} in the copy only", old_file.path, old.stamp, missing_file.path, modified_file.path);

    // This is the same delta producer used by stage_plan, with an independently
    // downloaded full target as the byte-for-byte oracle.
    let delta_result = produce(&mut session, &root, old_file, &Action::Deltas(old_index + 1), &operation, None).await.unwrap();
    let direct = session.fetch(&old_file.current().direct, old_file.current().direct_len, &operation).await.unwrap();
    let direct = decode_payload(&direct, old_file.current().sig.len as usize).unwrap();
    assert_eq!(delta_result, direct);
    assert_eq!(FileSig::of(&delta_result), old_file.current().sig);
    session.close().await;
    eprintln!("Production delta output matches the independently downloaded current full file");

    // Cancel after Apply, while the complete-install verification is in progress.
    // The production Operation is also the GUI's cancellation mechanism.
    let cancel = Operation::new();
    let watched = cancel.clone();
    let monitor = tokio::spawn(async move {
        loop {
            if watched.snapshot().stage == crate::operation::Stage::Verify {
                watched.cancel();
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let interrupted = run(root.clone(), Mode::Update, cancel.clone()).await;
    monitor.abort();
    assert!(cancel.cancelled(), "The cancellation must reach the post-Apply verification phase");
    let interruption = interrupted.unwrap_err();
    assert!(interruption.contains("Transaction recovered"), "{interruption}");
    assert_eq!(fs::read(&old_path).unwrap(), base);
    assert!(!missing_path.exists());
    assert_eq!(fs::read(&modified_path).unwrap(), custom);
    assert_eq!(fs::read(root.join("patch.ver")).unwrap(), applied_before);
    assert_eq!(fs::read(root.join("patch.cfg")).unwrap(), manifest_before);
    assert!(!root.join(STATE_DIR).join("transaction.json").exists());
    eprintln!("Cancellation after Apply restored both file states and original version metadata");

    // The copy is already at the latest version, so this update doesn't change the modified
    // file: it stays, and no longer holds the version back (owner, 2026-10-13).
    let updated = run(root.clone(), Mode::Update, Operation::new()).await.unwrap();
    assert_eq!((updated.outdated, updated.missing, updated.modified, updated.repaired), (1, 1, 1, 2));
    assert_eq!((updated.kept, updated.left), (0, 1));
    assert_eq!(updated.bytes, 0, "Resume must reuse the previously verified staged downloads");
    assert!(updated.verified, "A modification this update doesn't change no longer prevents the commit: {}", updated.summary);
    assert_eq!(updated.version, baseline.version);
    assert_eq!(fs::read(&old_path).unwrap(), direct);
    assert_eq!(FileSig::of_path(&missing_path).unwrap(), missing_file.current().sig);
    assert_eq!(fs::read(&modified_path).unwrap(), custom);
    assert_eq!(crate::patch::installed_for_request(&root), baseline.version, "patch.ver still records the same version");
    let installed_list = manifest::parse(&fs::read_to_string(root.join("patch.cfg")).unwrap()).unwrap();
    assert_eq!(Product::Ffxi.latest_stamp(&installed_list), Some(baseline.version.as_str()), "patch.cfg still lists the same version");
    assert!(!root.join(STATE_DIR).join("kept").join(&modified_file.path).exists(), "a file left in place has no kept copy");
    eprintln!("Update/resume: {}", updated.summary);

    let repaired = run(root.clone(), Mode::Repair, Operation::new()).await.unwrap();
    assert!(repaired.verified);
    assert_eq!((repaired.repaired, repaired.kept), (1, 1));
    assert_eq!(fs::read(&modified_path).unwrap(), modified_before);
    // The lasting copy: older transactions' backups go at the next commit.
    assert_eq!(fs::read(root.join(STATE_DIR).join("kept").join(&modified_file.path)).unwrap(), custom,
        "Repair must keep the modified file's exact original bytes in .xi-updater\\kept");
    assert_eq!(crate::patch::installed_for_request(&root), baseline.version);
    eprintln!("Repair: {}", repaired.summary);

    let final_check = run(root.clone(), Mode::Check, Operation::new()).await.unwrap();
    assert!(final_check.verified);
    assert_eq!(final_check.total, baseline.total);
    assert!(!root.join(STATE_DIR).join("transaction.json").exists());
    eprintln!("Final independent official Check: {}", final_check.summary);
    fs::write(root.parent().unwrap().join("result.txt"), format!(
        "{}\nReal official delta matched an independent direct download.\nCancellation after Apply restored the files and metadata.\n{}\nVerified staged downloads reused without re-downloading; the modification it doesn't change stayed.\n{}\nModified original kept in .xi-updater\\kept.\n{}\n", baseline.summary, updated.summary, repaired.summary, final_check.summary
    )).unwrap();
}
