//! Product-1000 protocol/deployment regressions and the shared transaction flow.
use super::*;

// Captured read-only from the official W2U/1000 server on 2026-10-09.
const VIEWER_MANIFEST: &str = include_str!("fixtures/viewer-us-20110829.patch.cfg");

fn root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("crystal-products-{label}-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir(&root).unwrap(); root
}

#[test]
fn official_viewer_manifest_projects_us_components_and_existing_staging() {
    let root = root("projection");
    let raw = Product::Viewer.parse_manifest(VIEWER_MANIFEST).unwrap();
    validate_manifest_for(Product::Viewer, &raw).unwrap();
    assert_eq!(Product::Viewer.latest_stamp(&raw), Some("20110829_E"));
    assert!(validate_manifest_for(Product::Ffxi, &raw).is_err());
    for name in ["app.dll", "polcore.dll"] {
        let staged = root.join(format!("patchfiles/PlayOnlineViewer/viewer/com/{name}"));
        fs::create_dir_all(staged.parent().unwrap()).unwrap(); fs::write(staged, b"old staged dll").unwrap();
    }
    let projected = Product::Viewer.deployment(&root, raw.clone()).unwrap();
    assert!(!projected.files.iter().any(|h| h.path.starts_with("EU/") || h.path.contains("EU.dll")));
    for name in ["app.dll", "polcore.dll"] {
        let live = projected.files.iter().find(|h| h.path == format!("viewer/com/{name}")).unwrap();
        let staged = projected.files.iter().find(|h| h.path == format!("patchfiles/PlayOnlineViewer/viewer/com/{name}")).unwrap();
        assert_eq!(live.versions, staged.versions);
        assert!(live.current().direct.contains("patchfiles/PlayOnlineViewer/"));
    }
    for path in ["file.txt", "version.dat", "pol.exe", "util/startpol.exe"] {
        assert!(projected.files.iter().any(|h| h.path == path), "Required official payload {path}");
    }
    fs::remove_dir_all(root.join("patchfiles")).unwrap();
    let no_staging = Product::Viewer.deployment(&root, raw).unwrap();
    assert!(!no_staging.files.iter().any(|h| h.path.starts_with("patchfiles/")));
    assert_eq!(projected.files.len(), no_staging.files.len() + 2);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn viewer_sections_must_be_recognized_and_complete() {
    let one = "file pol.exe {\n20110829_E 1 1 1 20110829_E/Direct/pol.exe.slc 3\n}\n";
    let text = format!("Reset2 {{\n{one}}}\nend\n");
    assert!(manifest::parse_viewer(&text).is_ok());
    assert!(manifest::parse(&text).is_err());
    for malformed in [format!("Reset2 {{\n{one}end\n"), format!("Unknown {{\n{one}}}\nend\n"),
        format!("Reset2 {{\nOption {{\n{one}}}\n}}\nend\n")] {
        assert!(manifest::parse_viewer(&malformed).is_err());
    }
}

#[test]
fn optional_viewer_files_follow_stock_absence_rules() {
    let root = root("optional");
    let required = history(Product::Viewer, "pol.exe", b"required");
    let mut optional = history(Product::Viewer, "optional.bin", b"optional"); optional.optional = true;
    let manifest = Manifest { files: vec![required, optional] };
    assert_eq!(Product::Viewer.deployment(&root, manifest.clone()).unwrap().files.len(), 1);
    fs::write(root.join("optional.bin"), b"old").unwrap();
    assert_eq!(Product::Viewer.deployment(&root, manifest).unwrap().files.len(), 2);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn each_product_uses_its_own_stock_packet_tags() {
    let hex = |data: Vec<u8>| data.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(hex(polp::version(&Product::Viewer.tags(), b"20110829_E")),
        "58000000a6d67388504f4c5007000000573255003130303032303131303832395f45000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000");
    assert_eq!(hex(polp::version(&Product::Ffxi.tags(), b"30260904_1")),
        "5800000061d437a3504f4c5007000000573230003030303133303236303930345f31000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000");
}

fn history(product: Product, path: &str, bytes: &[u8]) -> FileHistory {
    let stamp = match product { Product::Viewer => "20110829_E", Product::Ffxi => "30260904_1" };
    FileHistory { path: path.into(), optional: false, versions: vec![manifest::Version { stamp: stamp.into(), sig: FileSig::of(bytes),
        direct: format!("{stamp}/Direct/{path}.slc"), direct_len: bytes.len() as u64, indirect: None }] }
}

fn prepared(product: Product, root: PathBuf, manifest: Manifest, data: &[u8]) -> PreparedUpdate {
    let (mut report, planned) = scan(&root, &manifest, Mode::Repair, &Operation::new()).unwrap();
    report.version = Product::latest_stamp(product, &manifest).unwrap().into();
    let staged = root.join(format!("{STATE_DIR}/staged")); fs::create_dir_all(&staged).unwrap();
    for group in stage_groups(product, &manifest, &planned) {
        let h = &manifest.files[group.source.index];
        let cache = staged.join(format!("{}-{}.bin", hex_md5(if product == Product::Viewer { h.current().direct.as_bytes() } else { h.path.as_bytes() }),
            hex_md5(format!("{:?}", h.current().sig).as_bytes())));
        fs::write(cache, data).unwrap();
    }
    let seed = crate::patch::applied_seed_for(&root, product).unwrap();
    PreparedUpdate { product, root, _lock: None, mode: Mode::Repair, report, manifest,
        text: b"fixture manifest".to_vec(), planned, seed: Some(seed), server: product.server().into(), full_scan: true }
}

#[tokio::test]
async fn one_operation_synchronizes_both_dll_targets_then_updates_the_game() {
    let pol = root("pol"); let ffxi = root("ffxi"); let new = b"new official payload";
    let mut live = history(Product::Viewer, "viewer/com/polcore.dll", new);
    live.versions[0].direct = "20110829_E/Direct/patchfiles/PlayOnlineViewer/viewer/com/polcore.dll.slc".into();
    let mut stage = live.clone(); stage.path = "patchfiles/PlayOnlineViewer/viewer/com/polcore.dll".into();
    for h in [&stage, &live] {
        let dest = pol.join(&h.path); fs::create_dir_all(dest.parent().unwrap()).unwrap(); fs::write(dest, b"old").unwrap();
    }
    let viewer = prepared(Product::Viewer, pol.clone(), Manifest { files: vec![stage.clone(), live.clone()] }, new);
    assert_eq!(stage_groups(Product::Viewer, &viewer.manifest, &viewer.planned).len(), 1);
    let game = prepared(Product::Ffxi, ffxi.clone(), Manifest { files: vec![history(Product::Ffxi, "game.dat", new)] }, new);
    let op = Operation::new(); op.begin_update();
    let reports = execute(vec![viewer, game], &op, true, ensure_idle).await.unwrap();
    assert_eq!(reports.len(), 2); assert!(reports.iter().all(|r| r.verified));
    assert_eq!(fs::read(pol.join(&stage.path)).unwrap(), fs::read(pol.join(&live.path)).unwrap());
    assert_eq!(fs::read(ffxi.join("game.dat")).unwrap(), new);
    assert_eq!(crate::patch::installed_for_request_for(&pol, crate::updater::Product::Viewer), "20110829_E");
    assert_eq!(crate::patch::installed_for_request_for(&ffxi, crate::updater::Product::Ffxi), "30260904_1");
    assert!(op.snapshot().overall.unwrap().complete);
    fs::remove_dir_all(pol).unwrap(); fs::remove_dir_all(ffxi).unwrap();
}

#[test]
fn final_verification_failure_rolls_back_live_and_staged_dlls_together() {
    let pol = root("rollback"); let old = b"old dll"; let new = b"new dll";
    let live = history(Product::Viewer, "viewer/com/polcore.dll", new);
    let mut stage = live.clone(); stage.path = "patchfiles/PlayOnlineViewer/viewer/com/polcore.dll".into();
    let cache = pol.join("new.bin"); fs::write(&cache, new).unwrap();
    let mut ready = Vec::new();
    for h in [&stage, &live] {
        let dest = pol.join(&h.path); fs::create_dir_all(dest.parent().unwrap()).unwrap(); fs::write(dest, old).unwrap();
        ready.push((h.path.clone(), cache.clone(), h.current().sig, Some(FileSig::of(old))));
    }
    let mut update = prepared(Product::Viewer, pol.clone(), Manifest { files: vec![stage.clone(), live.clone()] }, new);
    update.manifest.files.push(history(Product::Viewer, "missing-final-file", new));
    assert!(commit(update, ready, &Operation::new()).is_err());
    for h in [&stage, &live] { assert_eq!(fs::read(pol.join(&h.path)).unwrap(), old); }
    assert!(!pol.join("patch.ver").exists());
    fs::remove_dir_all(pol).unwrap();
}

#[tokio::test]
async fn second_component_failure_retains_verified_pol_and_does_not_finish_the_operation() {
    let pol = root("partial-pol"); let ffxi = root("partial-ffxi"); let new = b"new";
    let viewer = prepared(Product::Viewer, pol.clone(), Manifest { files: vec![history(Product::Viewer, "pol.exe", new)] }, new);
    let mut game = prepared(Product::Ffxi, ffxi.clone(), Manifest { files: vec![history(Product::Ffxi, "game.dat", new)] }, new);
    game.manifest.files.push(history(Product::Ffxi, "missing-final-file", new));
    let op = Operation::new(); op.begin_update();
    assert!(execute(vec![viewer, game], &op, true, ensure_idle).await.is_err());
    assert_eq!(crate::patch::installed_for_request_for(&pol, crate::updater::Product::Viewer), "20110829_E");
    assert_eq!(fs::read(pol.join("pol.exe")).unwrap(), new);
    assert!(!ffxi.join("patch.ver").exists()); assert!(!ffxi.join("game.dat").exists());
    assert!(!op.snapshot().overall.unwrap().complete);
    fs::remove_dir_all(pol).unwrap(); fs::remove_dir_all(ffxi).unwrap();
}

#[test]
fn scoped_progress_preserves_the_shared_total_across_components() {
    let op = Operation::new();
    let downloads = Downloads::new(op.clone(), vec![10, 30]);
    downloads.file_scoped(0, op.component("PlayOnline")).advance(10, "pol.exe".into(), false);
    assert_eq!((op.snapshot().done, op.snapshot().total), (10, 40));
    downloads.file_scoped(1, op.component("FINAL FANTASY XI")).advance(15, "game.dat".into(), false);
    let progress = op.snapshot(); assert_eq!((progress.done, progress.total), (25, 40));
    assert_eq!(progress.detail, "FINAL FANTASY XI: game.dat");
}
