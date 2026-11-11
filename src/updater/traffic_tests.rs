//! Patch-server traffic against a local POLP server, the offline Check, and the
//! Phase 3 version plan against a full scan.
use super::*;
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, PartialEq)]
enum What {
    Connect,
    Request { kind: u32, path: String, offset: u32, count: u32 },
    Replied,
    Close,
    /// More request bytes arrived before the reply went out.
    Overlap,
    /// A request differed from Crystal's own encoder or asked for something unknown.
    Wrong(String),
}

#[derive(Debug, Clone)]
struct Seen { conn: usize, at: Instant, what: What }

struct FakeServer { addr: String, seen: Arc<Mutex<Vec<Seen>>> }
impl FakeServer {
    fn seen(&self) -> Vec<Seen> { self.seen.lock().unwrap().clone() }
}

/// A POLP server on loopback: kind 7 → the version, kind 1 → the stored list, kind 3 →
/// the requested chunk of `files` (keyed by transfer path). Every event is logged.
fn serve(version: &str, list: &[u8], files: HashMap<String, Vec<u8>>) -> FakeServer {
    serve_for(Product::Ffxi, version, list, files)
}

/// `serve` for `product`'s patch service (its platform and title tags).
fn serve_for(product: Product, version: &str, list: &[u8], files: HashMap<String, Vec<u8>>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, version, list, files) = (seen.clone(), version.to_owned(), list.to_vec(), Arc::new(files));
    std::thread::spawn(move || {
        for (conn, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { break };
            log.lock().unwrap().push(Seen { conn, at: Instant::now(), what: What::Connect });
            let (log, version, list, files) = (log.clone(), version.clone(), list.clone(), files.clone());
            std::thread::spawn(move || answer(product, conn, stream, &log, &version, &list, &files));
        }
    });
    FakeServer { addr, seen }
}

fn answer(product: Product, conn: usize, mut stream: TcpStream, log: &Mutex<Vec<Seen>>, version: &str, list: &[u8], files: &HashMap<String, Vec<u8>>) {
    let push = |what| log.lock().unwrap().push(Seen { conn, at: Instant::now(), what });
    loop {
        let mut head = [0u8; 4];
        if stream.read_exact(&mut head).is_err() { push(What::Close); return; }
        let mut packet = vec![0u8; u32::from_le_bytes(head) as usize];
        packet[..4].copy_from_slice(&head);
        if stream.read_exact(&mut packet[4..]).is_err() { push(What::Close); return; }
        let Ok(frame) = polp::parse(&packet) else { push(What::Wrong("unparsable request".into())); return; };
        let tags = product.tags();
        let (what, reply) = match frame.kind {
            polp::kind::VERSION => {
                let mut body = vec![0u8; 72];
                body[8..18].copy_from_slice(b"registered");
                body.extend((version.len() as u32 + 1).to_le_bytes());
                body.extend(version.as_bytes());
                body.push(0);
                // `path`: the installed version the request carries (none for a fresh install).
                let installed: Vec<u8> = frame.body.get(8..).unwrap_or_default().iter().copied().take_while(|byte| *byte != 0).collect();
                if packet != polp::version(&tags, &installed) { push(What::Wrong("version".into())); }
                (What::Request { kind: 7, path: String::from_utf8_lossy(&installed).into_owned(), offset: 0, count: 0 }, polp::seal(polp::kind::VERSION_REPLY, &body))
            }
            polp::kind::HELLO => {
                if packet != polp::hello(&tags) { push(What::Wrong("hello".into())); }
                (What::Request { kind: 1, path: String::new(), offset: 0, count: 0 }, polp::seal(polp::kind::MANIFEST_REPLY, &[&[1u8][..], list].concat()))
            }
            polp::kind::FILE_REQUEST => {
                let body = frame.body;
                let word = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap());
                let (offset, count, name) = (word(0), word(4), word(16) as usize);
                let path = String::from_utf8_lossy(&body[20..20 + name - 1]).into_owned();
                if packet != polp::file_request(&tags, &path, offset, count) { push(What::Wrong(format!("encoding of {path}"))); }
                let Some(data) = files.get(&path).and_then(|data| data.get(offset as usize..(offset + count) as usize)) else {
                    push(What::Wrong(format!("unknown {path} {offset}+{count}")));
                    return;
                };
                let mut reply = Vec::new();
                for value in [offset, count, name as u32] { reply.extend(value.to_le_bytes()); }
                reply.extend(path.as_bytes());
                reply.push(0);
                reply.extend(data);
                (What::Request { kind: 3, path, offset, count }, polp::seal(polp::kind::FILE_REPLY, &reply))
            }
            other => { push(What::Wrong(format!("kind {other}"))); return; }
        };
        push(what);
        // Stop-and-wait: nothing more may be on the wire before this reply leaves.
        std::thread::sleep(Duration::from_millis(5));
        stream.set_nonblocking(true).unwrap();
        if stream.peek(&mut [0u8; 1]).is_ok_and(|n| n > 0) { push(What::Overlap); }
        stream.set_nonblocking(false).unwrap();
        if stream.write_all(&reply).is_err() { push(What::Close); return; }
        push(What::Replied);
    }
}

fn manifest_text(m: &Manifest) -> String {
    let mut text = String::new();
    for history in &m.files {
        text.push_str(&format!("file {} {{\n", history.path));
        for v in &history.versions {
            text.push_str(&format!("{} {} {} {} {} {}", v.stamp, v.sig.len, v.sig.byte_sum, v.sig.md5_word, v.direct, v.direct_len));
            if let Some(delta) = &v.indirect { text.push_str(&format!(" {} {}", delta.path, delta.len)); }
            text.push('\n');
        }
        text.push_str("}\n");
    }
    text.push_str("end\n");
    text
}

/// A delta stream (lz.rs indirect format) copying `copy` bytes of the base, then `tail`.
fn delta_stream(copy: u8, tail: &[u8]) -> Vec<u8> {
    let mut fields: Vec<(u32, u32)> = vec![(3, 2), (0, 16), (copy.into(), 8)];
    for &byte in tail { fields.extend([(0, 1), (byte.into(), 8)]); }
    let bits: usize = fields.iter().map(|field| field.1 as usize).sum();
    let mut out = vec![0; 4 + bits.div_ceil(8)];
    out[..4].copy_from_slice(&(bits as u32).to_le_bytes());
    let mut cursor = 0;
    for (value, width) in fields {
        for shift in 0..width {
            if value & (1 << shift) != 0 { out[4 + cursor / 8] |= 1 << (cursor % 8); }
            cursor += 1;
        }
    }
    out
}

fn root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("crystal-traffic-{label}-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("FFXi.dll"), b"installed marker").unwrap();
    root
}

fn version(stamp: &str, data: &[u8], path: &str, direct_len: u64, delta: Option<u64>) -> manifest::Version {
    manifest::Version { stamp: stamp.into(), sig: FileSig::of(data), direct: format!("{stamp}/Direct/{path}.slc"), direct_len,
        indirect: delta.map(|len| manifest::Delta { path: format!("{stamp}/Indirect/{path}.olc"), len }) }
}

const OLD: &str = "30260904_1";
const NEW: &str = "30261006_0";

/// An install at OLD and the NEW list: one delta, one 3-chunk new file, one current
/// file and one small new file, with the payloads the server holds.
fn update_fixture(root: &Path) -> (Manifest, HashMap<String, Vec<u8>>) {
    let base: Vec<u8> = (0..200u32).map(|i| (i * 7) as u8).collect();
    let patched = [&base[..], b"!!"].concat();
    let delta = delta_stream(200, b"!!");
    let big: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    let small = b"small official file".to_vec();
    let same = b"already current".to_vec();
    fs::write(root.join("delta.DAT"), &base).unwrap();
    fs::write(root.join("same.DAT"), &same).unwrap();
    let stored = |data: &[u8]| [&[1u8][..], data].concat();
    let m = Manifest { files: vec![
        FileHistory { path: "delta.DAT".into(), optional: false, versions: vec![
            version(OLD, &base, "delta.DAT", base.len() as u64 + 1, None),
            version(NEW, &patched, "delta.DAT", patched.len() as u64 + 1, Some(delta.len() as u64))] },
        FileHistory { path: "ROM/1/big.DAT".into(), optional: false, versions: vec![
            version(NEW, &big, "ROM/1/big.DAT", big.len() as u64 + 1, None)] },
        FileHistory { path: "same.DAT".into(), optional: false, versions: vec![
            version(OLD, &same, "same.DAT", same.len() as u64 + 1, None)] },
        FileHistory { path: "small.DAT".into(), optional: false, versions: vec![
            version(NEW, &small, "small.DAT", small.len() as u64 + 1, None)] },
    ] };
    let mut files = HashMap::new();
    files.insert(format!("{NEW}/Indirect/delta.DAT.olc"), delta);
    files.insert(format!("{NEW}/Direct/ROM/1/big.DAT.slc"), stored(&big));
    files.insert(format!("{NEW}/Direct/small.DAT.slc"), stored(&small));
    (m, files)
}

/// One routine update against a local server: prepare (version, list), then stage every
/// download. Returns what the server saw (after all three connections closed) and the
/// staged files. The caller removes `root`.
async fn fake_update(root: &Path) -> (Vec<Seen>, Vec<Ready>) {
    let (m, files) = update_fixture(root);
    let server = serve(NEW, manifest_text(&m).as_bytes(), files);
    let op = Operation::new();
    let prepared = prepare_at(Product::Ffxi, root.to_path_buf(), Mode::Update, &op, &server.addr, |_| Ok(())).await.unwrap();
    assert_eq!(prepared.report.version, NEW);
    let planned: Vec<_> = prepared.planned.iter().map(|p| (prepared.manifest.files[p.index].path.as_str(), p.action.clone())).collect();
    assert_eq!(planned, [("delta.DAT", Action::Deltas(1)), ("ROM/1/big.DAT", Action::Direct), ("small.DAT", Action::Direct)]);
    let staged = root.join(".xi-updater/staged");
    fs::create_dir_all(&staged).unwrap();
    let groups = stage_groups(Product::Ffxi, &prepared.manifest, &prepared.planned);
    let downloads = Downloads::new(op.clone(), groups.iter()
        .map(|g| download_size(&prepared.manifest.files[g.source.index], &g.source.action)).collect());
    let (ready, _) = stage_plan_shared(Product::Ffxi, &server.addr, &prepared.root, &staged, &prepared.manifest,
        &groups, &op, &downloads, 0).await.unwrap();
    // The download connection's close is ~0.1 s after its last reply.
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.seen().iter().filter(|s| s.what == What::Close).count() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Resuming over the same verified cache stages nothing new and opens no connection.
    let again = Downloads::new(op.clone(), groups.iter()
        .map(|g| download_size(&prepared.manifest.files[g.source.index], &g.source.action)).collect());
    let (resumed, bytes) = stage_plan_shared(Product::Ffxi, &server.addr, &prepared.root, &staged, &prepared.manifest,
        &groups, &op, &again, 0).await.unwrap();
    assert_eq!((bytes, resumed.len()), (0, ready.len()));
    drop(prepared); // the updater lock
    (server.seen(), ready)
}

/// When the server saw `what` last on connection `conn`.
fn seen_at(seen: &[Seen], conn: usize, what: &What) -> Instant {
    seen.iter().filter(|s| s.conn == conn && &s.what == what).map(|s| s.at).next_back().unwrap()
}

/// Reply → FIN of each connection, and FIN → SYN between consecutive ones.
fn close_and_reopen(seen: &[Seen]) -> (Vec<Duration>, Vec<Duration>) {
    let quiet = (0..3).map(|conn| seen_at(seen, conn, &What::Close) - seen_at(seen, conn, &What::Replied)).collect();
    let gaps = (1..3).map(|conn| seen_at(seen, conn, &What::Connect) - seen_at(seen, conn - 1, &What::Close)).collect();
    (quiet, gaps)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_asks_version_then_list_then_downloads_on_one_stop_and_wait_connection() {
    let root = root("order");
    let (seen, ready) = fake_update(&root).await;
    assert_eq!(ready.len(), 3);
    assert!(!seen.iter().any(|s| matches!(s.what, What::Overlap | What::Wrong(_))), "{seen:#?}");
    let connections = seen.iter().filter(|s| s.what == What::Connect).count();
    assert_eq!(connections, 3, "version, list, downloads: {seen:#?}");
    let requests = |conn: usize| seen.iter().filter_map(|s| match &s.what {
        What::Request { kind, path, offset, count } if s.conn == conn => Some((*kind, path.clone(), *offset, *count)),
        _ => None }).collect::<Vec<_>>();
    assert_eq!(requests(0), [(7, String::new(), 0, 0)]);
    assert_eq!(requests(1), [(1, String::new(), 0, 0)]);
    let big = format!("{NEW}/Direct/ROM/1/big.DAT.slc");
    assert_eq!(requests(2), [
        (3, format!("{NEW}/Indirect/delta.DAT.olc"), 0, delta_stream(200, b"!!").len() as u32),
        (3, big.clone(), 0, 65536), (3, big.clone(), 65536, 65536), (3, big, 131072, 150_001 - 131072),
        (3, format!("{NEW}/Direct/small.DAT.slc"), 0, 20),
    ]);
    // Never early: each connection closes at least 100 ms after its last reply, and the next
    // opens at least 100 ms after that FIN (stock never has a SYN within 100 ms of a FIN).
    // The upper bounds are `patch_connections_close_and_reopen_inside_stocks_windows`'.
    let (quiet, gaps) = close_and_reopen(&seen);
    assert!(quiet.iter().all(|quiet| *quiet >= crate::stock_socket::CLOSE_AFTER_REPLY.0), "reply → FIN {quiet:?}");
    assert!(gaps.iter().all(|gap| *gap >= Duration::from_millis(100)), "FIN → SYN {gaps:?}");
    // A new file waits its pause after the previous file's last reply (a file's next chunk
    // is asked for at once: the pause is only at a file's start).
    let replies: Vec<Instant> = seen.iter().filter(|s| s.conn == 2 && s.what == What::Replied).map(|s| s.at).collect();
    let asked: Vec<Instant> = seen.iter().filter(|s| s.conn == 2 && matches!(s.what, What::Request { .. })).map(|s| s.at).collect();
    assert!(asked[1] - replies[0] >= PAUSE_AFTER_DELTA.0, "after a delta: {:?}", asked[1] - replies[0]);
    assert!(asked[4] - replies[3] >= PAUSE_AFTER_DELTA.0, "after a 150 KB direct file: {:?}", asked[4] - replies[3]);
    eprintln!("next-chunk gaps on loopback: {:?}, {:?}", asked[2] - replies[1], asked[3] - replies[2]);
    for (path, cache, sig, _) in &ready {
        assert_eq!(FileSig::of_path(cache).unwrap(), *sig, "{path}");
    }
    assert!(!root.join("patch.ver").exists());
    fs::remove_dir_all(root).unwrap();
}

/// Not late either: every reply → FIN inside stock's 100-115 ms (plus 3 ms for the loopback
/// server's own wake-up), and version FIN → list SYN inside 103-112 ms (plus 8 ms). Wall-clock
/// upper bounds also measure how busy the machine is (a parallel test run can delay a wake-up
/// by tens of ms), so one of three runs must meet them; a coarse timer misses one of the four
/// bounds in most runs, and `wire_pauses_use_a_high_resolution_timer` pins the timer itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_connections_close_and_reopen_inside_stocks_windows() {
    let (low, high) = crate::stock_socket::CLOSE_AFTER_REPLY;
    let mut runs = Vec::new();
    for attempt in 0..3 {
        let root = root(&format!("windows-{attempt}"));
        let (seen, _) = fake_update(&root).await;
        fs::remove_dir_all(&root).unwrap();
        let (quiet, gaps) = close_and_reopen(&seen);
        let inside = quiet.iter().all(|quiet| *quiet >= low && *quiet <= high + Duration::from_millis(3))
            && gaps[0] <= NEXT_CONNECTION_GAP.1 + Duration::from_millis(8);
        if inside { return; }
        runs.push((quiet, gaps));
    }
    panic!("no run closed and reopened inside stock's windows (reply → FIN, FIN → SYN): {runs:?}");
}

/// An Update covering both: PlayOnline's version, list, downloads and commit all finish
/// before FFXI's patch service hears anything (stock: the Viewer updates at Connect and
/// restarts before any FFXI W20).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_combined_update_finishes_playonline_before_ffxis_patch_service_hears_anything() {
    const VIEWER_OLD: &str = "20260901_0";
    const VIEWER_NEW: &str = "20261001_0";
    let pol = root("combined-pol");
    let (app_old, app_new) = (b"old app build".as_slice(), b"new official app build".as_slice());
    fs::create_dir_all(pol.join("viewer/com")).unwrap();
    fs::write(pol.join("viewer/com/app.dll"), app_old).unwrap();
    let path = "viewer/com/app.dll";
    let viewer_list = Manifest { files: vec![FileHistory { path: path.into(), optional: false, versions: vec![
        version(VIEWER_OLD, app_old, path, app_old.len() as u64 + 1, None),
        version(VIEWER_NEW, app_new, path, app_new.len() as u64 + 1, None)] }] };
    let viewer_files = HashMap::from([(format!("{VIEWER_NEW}/Direct/{path}.slc"), [&[1u8][..], app_new].concat())]);
    let viewer_server = serve_for(Product::Viewer, VIEWER_NEW, manifest_text(&viewer_list).as_bytes(), viewer_files);
    let game = root("combined-ffxi");
    let (m, files) = update_fixture(&game);
    let game_server = serve(NEW, manifest_text(&m).as_bytes(), files);
    let op = Operation::new();
    let combined = match run_combined_at(Some(pol.clone()), Some(game.clone()), Mode::Update, op.clone(),
        (&viewer_server.addr, &game_server.addr), |_| Ok(())).await {
        Ok(combined) => combined,
        Err(failure) => panic!("{}", failure.message),
    };
    assert!(combined.viewer.as_ref().is_some_and(|r| r.verified) && combined.game.as_ref().is_some_and(|r| r.verified));
    assert_eq!(fs::read(pol.join(path)).unwrap(), app_new);
    assert_eq!(crate::patch::installed_for_request_for(&pol, crate::updater::Product::Viewer), VIEWER_NEW);
    assert_eq!(crate::patch::installed_for_request(&game), NEW);
    assert!(op.snapshot().overall.unwrap().complete);
    let deadline = Instant::now() + Duration::from_secs(5);
    while game_server.seen().iter().filter(|s| s.what == What::Close).count() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (viewer_seen, game_seen) = (viewer_server.seen(), game_server.seen());
    assert_eq!(viewer_seen.iter().filter(|s| s.what == What::Connect).count(), 3, "{viewer_seen:#?}");
    assert_eq!(game_seen.iter().filter(|s| s.what == What::Connect).count(), 3, "{game_seen:#?}");
    let viewer_last = viewer_seen.iter().map(|s| s.at).max().unwrap();
    let game_first = game_seen.iter().map(|s| s.at).min().unwrap();
    assert!(viewer_last < game_first, "FFXI's patch service was contacted before PlayOnline's update finished");
    fs::remove_dir_all(pol).unwrap();
    fs::remove_dir_all(game).unwrap();
}

const VIEWER_OLD: &str = "20110829_E";
const VIEWER_NEW: &str = "20261022_0";

/// A PlayOnline install at VIEWER_OLD (its record included) and a patch service at VIEWER_NEW
/// whose update replaces app.dll and polcore.dll.
fn viewer_fixture(label: &str) -> (PathBuf, FakeServer) {
    let pol = root(label);
    fs::create_dir_all(pol.join("viewer/com")).unwrap();
    let mut files = HashMap::new();
    let mut list = Manifest::default();
    for name in ["app", "polcore"] {
        let path = format!("viewer/com/{name}.dll");
        let (old, new) = (format!("old {name} build").into_bytes(), format!("new official {name} build").into_bytes());
        fs::write(pol.join(&path), &old).unwrap();
        list.files.push(FileHistory { path: path.clone(), optional: false, versions: vec![
            version(VIEWER_OLD, &old, &path, old.len() as u64 + 1, None),
            version(VIEWER_NEW, &new, &path, new.len() as u64 + 1, None)] });
        files.insert(format!("{VIEWER_NEW}/Direct/{path}.slc"), [&[1u8][..], &new].concat());
    }
    let seed = crate::patch::applied_seed_for(&pol, Product::Viewer).unwrap();
    fs::write(pol.join("patch.ver"), crate::patch::encode_applied_record_for(VIEWER_OLD, seed, Product::Viewer).unwrap()).unwrap();
    let server = serve_for(Product::Viewer, VIEWER_NEW, manifest_text(&list).as_bytes(), files);
    (pol, server)
}

/// Play is the PlayOnline update too. A sign-in's W2U (the stock version frame with the
/// installed version) is answered "outdated" and stops that sign-in; PlayOnline's update then
/// sends what stock's patch client sends after a version check that asked for an update
/// (stock-update-20261013-004932): no version request of its own, an idle prompt connection
/// a gap after the W2U's FIN, the file list, one stop-and-wait download connection. The
/// sign-in that follows asks its own W2U, which now says "current".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_w2u_that_asks_for_an_update_is_followed_by_playonlines_update_then_a_current_w2u() {
    use crate::viewer::ConnectVerdict;
    let (pol, server) = viewer_fixture("viewer-after-connect");
    let address = server.addr.clone();
    let folder = pol.clone();
    let verdict = tokio::task::spawn_blocking(move || crate::patch::viewer_connect_at(&folder, &address)).await.unwrap();
    assert_eq!(verdict, ConnectVerdict::Outdated { local: VIEWER_OLD.into(), remote: VIEWER_NEW.into() });
    let ConnectVerdict::Outdated { remote, .. } = verdict else { unreachable!() };
    // The W2U closed before its verdict came back; the window notes that when it hears of it.
    let w2u_fin = Instant::now();
    note_server_closed(&server.addr, w2u_fin);
    let op = Operation::new();
    let hold = Duration::from_millis(400);
    let report = match update_viewer_after_connect_at(pol.clone(), AfterPlay { official: remote, prompt: hold }, op.clone(),
        &server.addr, |_| Ok(()), Duration::ZERO).await {
        Ok(report) => report,
        Err(failure) => panic!("{}", failure.message),
    };
    let viewer = report.viewer.expect("PlayOnline's report");
    assert!(viewer.verified && viewer.version == VIEWER_NEW && report.game.is_none(), "{viewer:?}");
    for name in ["app", "polcore"] {
        assert_eq!(fs::read(pol.join(format!("viewer/com/{name}.dll"))).unwrap(), format!("new official {name} build").into_bytes());
    }
    assert_eq!(crate::patch::installed_for_request_for(&pol, Product::Viewer), VIEWER_NEW);
    assert!(op.snapshot().overall.unwrap().complete);
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.seen().iter().filter(|s| s.what == What::Close).count() < 4 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The sign-in that follows: its own W2U, with the version just installed.
    let (address, folder) = (server.addr.clone(), pol.clone());
    let verdict = tokio::task::spawn_blocking(move || crate::patch::viewer_connect_at(&folder, &address)).await.unwrap();
    assert_eq!(verdict, ConnectVerdict::Current { version: VIEWER_NEW.into() });
    let seen = server.seen();
    assert!(!seen.iter().any(|s| matches!(s.what, What::Overlap | What::Wrong(_))), "{seen:#?}");
    assert_eq!(seen.iter().filter(|s| s.what == What::Connect).count(), 5, "W2U, prompt, list, downloads, W2U: {seen:#?}");
    let requests = |conn: usize| seen.iter().filter_map(|s| match &s.what {
        What::Request { kind, path, offset, count } if s.conn == conn => Some((*kind, path.clone(), *offset, *count)),
        _ => None }).collect::<Vec<_>>();
    assert_eq!(requests(0), [(7, VIEWER_OLD.to_owned(), 0, 0)], "the sign-in's W2U");
    assert!(requests(1).is_empty(), "the prompt connection carries nothing");
    assert_eq!(requests(2), [(1, String::new(), 0, 0)]);
    assert_eq!(requests(3), ["app", "polcore"].map(|name|
        (3, format!("{VIEWER_NEW}/Direct/viewer/com/{name}.dll.slc"), 0, format!("new official {name} build").len() as u32 + 1)));
    assert_eq!(requests(4), [(7, VIEWER_NEW.to_owned(), 0, 0)], "the next sign-in's W2U");
    // W2U FIN → prompt SYN, the prompt held open, its FIN → list SYN: never within 100 ms.
    let prompt_syn = seen_at(&seen, 1, &What::Connect);
    let prompt_fin = seen_at(&seen, 1, &What::Close);
    assert!(prompt_syn - w2u_fin >= NEXT_CONNECTION_GAP.0, "{:?}", prompt_syn - w2u_fin);
    assert!(prompt_fin - prompt_syn >= hold, "{:?}", prompt_fin - prompt_syn);
    assert!(seen_at(&seen, 2, &What::Connect) - prompt_fin >= Duration::from_millis(100));
    assert!(seen_at(&seen, 3, &What::Connect) - seen_at(&seen, 2, &What::Close) >= Duration::from_millis(100));
    for conn in [2, 3] {
        let quiet = seen_at(&seen, conn, &What::Close) - seen_at(&seen, conn, &What::Replied);
        assert!(quiet >= crate::stock_socket::CLOSE_AFTER_REPLY.0, "reply → FIN {quiet:?}");
    }
    fs::remove_dir_all(pol).unwrap();
}

/// PlayOnline's files in use (a game, the Viewer, a host still exiting): the update waits a
/// moment for them, and sends nothing at all when they stay in use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn playonlines_update_waits_for_its_files_and_sends_nothing_while_they_stay_in_use() {
    static SCANS: AtomicUsize = AtomicUsize::new(0);
    let (pol, server) = viewer_fixture("viewer-in-use");
    let play = || AfterPlay { official: VIEWER_NEW.into(), prompt: Duration::from_millis(50) };
    let busy: Guard = |_| Err("Close the game using this install before updating files".into());
    let started = Instant::now();
    let failure = update_viewer_after_connect_at(pol.clone(), play(), Operation::new(), &server.addr, busy, Duration::from_millis(600)).await.err().unwrap();
    assert_eq!(failure.message, viewer_in_use());
    assert!(started.elapsed() >= Duration::from_millis(600), "it looked again before giving up");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.seen().is_empty(), "nothing is sent for an update that cannot install: {:?}", server.seen());
    assert_eq!(crate::patch::installed_for_request_for(&pol, Product::Viewer), VIEWER_OLD);
    // A host that was still exiting: gone at the third look, the update goes ahead.
    let exiting: Guard = |_| if SCANS.fetch_add(1, Ordering::SeqCst) < 2 { Err("still exiting".into()) } else { Ok(()) };
    let report = update_viewer_after_connect_at(pol.clone(), play(), Operation::new(), &server.addr, exiting, Duration::from_secs(10)).await;
    assert!(report.is_ok_and(|report| report.viewer.is_some_and(|viewer| viewer.verified)));
    assert_eq!(crate::patch::installed_for_request_for(&pol, Product::Viewer), VIEWER_NEW);
    fs::remove_dir_all(pol).unwrap();
}

#[test]
fn the_pause_before_a_file_follows_what_came_before_it() {
    assert_eq!(next_file_pause(Some(Transfer::Delta)), PAUSE_AFTER_DELTA);
    assert_eq!(next_file_pause(Some(Transfer::Direct(24))), PAUSE_AFTER_SMALL_DIRECT);
    assert_eq!(next_file_pause(Some(Transfer::Direct(SMALL_DIRECT))), PAUSE_AFTER_SMALL_DIRECT);
    assert_eq!(next_file_pause(Some(Transfer::Direct(SMALL_DIRECT + 1))), PAUSE_AFTER_DELTA);
    // Stock: after a delta never under 11.5 ms; after a 24 B file median 8.4, min 3.3.
    assert!(PAUSE_AFTER_DELTA.0 >= Duration::from_micros(11_500) && PAUSE_AFTER_SMALL_DIRECT.1 < PAUSE_AFTER_DELTA.0);
    assert!(NEXT_CONNECTION_GAP.0 > Duration::from_millis(100) && NEXT_CONNECTION_GAP.1 <= Duration::from_millis(115));
}

/// tokio's timer on Windows rounds a 7 ms wait up to the 15.6 ms tick; the pauses Square
/// Enix can see use a high-resolution one. The shortest of nine waits: a busy machine only
/// lengthens waits, so it is the timer's own resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_pauses_use_a_high_resolution_timer() {
    let mut took = Vec::new();
    for _ in 0..9 {
        let started = Instant::now();
        precise_sleep(Duration::from_millis(7)).await;
        took.push(started.elapsed());
    }
    took.sort();
    assert!(took[0] >= Duration::from_millis(7) && took[0] < Duration::from_millis(10), "{took:?}");
}

#[tokio::test]
async fn check_reads_the_installed_list_and_never_contacts_the_server() {
    let root = root("offline-check");
    let (m, files) = update_fixture(&root);
    let server = serve(NEW, manifest_text(&m).as_bytes(), files);
    let op = Operation::new();
    // No installed list yet: nothing to compare, nothing sent.
    let prepared = prepare_at(Product::Ffxi, root.clone(), Mode::Check, &op, &server.addr, |_| Err("Check never needs the guard".into())).await.unwrap();
    assert!(prepared.report.summary.contains("patch.cfg") && !prepared.report.verified);
    // The installed list is the OLD one the last update wrote.
    let installed = Manifest { files: m.files.iter().map(|h| FileHistory { versions: h.versions.iter()
        .filter(|v| v.stamp.as_str() <= OLD).cloned().collect(), ..h.clone() }).filter(|h| !h.versions.is_empty()).collect() };
    fs::write(root.join("patch.cfg"), manifest_text(&installed)).unwrap();
    let report = execute(vec![prepare_at(Product::Ffxi, root.clone(), Mode::Check, &op, &server.addr, |_| Err("unused".into())).await.unwrap()], &op, true, |_| Err("unused".into()))
        .await.unwrap().remove(0);
    assert_eq!((report.total, report.current, report.verified, report.version.as_str()), (2, 2, true, OLD), "{}", report.summary);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(server.seen().is_empty(), "Check opened a connection: {:?}", server.seen());
    assert!(!root.join(".xi-updater/lock").exists(), "Check creates no updater state");
    fs::remove_dir_all(root).unwrap();
}

/// The fixture for the version plan: the NEW list and the installed OLD list, with every
/// kind of file a routine update meets.
fn plan_fixture(root: &Path) -> (Manifest, Manifest) {
    let mut new = Manifest::default();
    let add = |new: &mut Manifest, path: &str, versions: Vec<manifest::Version>| new.files.push(FileHistory { path: path.into(), optional: false, versions });
    let bytes = |label: &str, n: usize| -> Vec<u8> { label.bytes().cycle().take(n).collect() };
    for (index, case) in ["current", "changed-old", "changed-missing", "unchanged-missing", "unchanged-resized",
        "new-file", "changed-modified", "unchanged-modified-resized", "changed-current"].into_iter().enumerate() {
        let path = format!("ROM/{index}/{case}.DAT");
        let old = bytes(&format!("old {case}"), 300);
        let current = bytes(&format!("new {case}"), 320);
        let changed = case.starts_with("changed") || case == "new-file";
        let mut versions = Vec::new();
        if case != "new-file" { versions.push(version(OLD, &old, &path, 301, None)); }
        if changed { versions.push(version(NEW, &current, &path, 321, Some(40))); }
        let target = if changed { &current } else { &old };
        let local: Option<Vec<u8>> = match case {
            "current" | "changed-current" => Some(target.clone()),
            "changed-old" => Some(old.clone()),
            "unchanged-resized" => Some(bytes("damaged", 123)),
            "changed-modified" => Some(bytes("a texture mod", 500)),
            "unchanged-modified-resized" => Some(bytes("another mod", 77)),
            _ => None,
        };
        if let Some(local) = local {
            let file = root.join(&path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, local).unwrap();
        }
        add(&mut new, &path, versions);
    }
    let installed = Manifest { files: new.files.iter().map(|h| FileHistory { versions: h.versions.iter()
        .filter(|v| v.stamp == OLD).cloned().collect(), ..h.clone() }).filter(|h| !h.versions.is_empty()).collect() };
    (new, installed)
}

#[test]
fn the_version_plan_equals_a_full_scan() {
    let root = root("version-plan");
    let (new, installed) = plan_fixture(&root);
    let op = Operation::new();
    let full = scan_for(Product::Ffxi, &root, &new, Mode::Update, &op, Some(OLD)).unwrap();
    for workers in [1, 4] {
        let version = plan_from_applied(Product::Ffxi, &root, &installed, &new, Mode::Update, &op, workers).unwrap();
        assert_eq!(version, full, "workers={workers}");
    }
    let (report, planned) = &full;
    let names: Vec<&str> = planned.iter().map(|p| new.files[p.index].path.rsplit('/').next().unwrap()).collect();
    // A damaged file of the wrong size reads as a modification: one this update doesn't
    // change stays, like any other (Repair restores it).
    assert_eq!(names, ["changed-old.DAT", "changed-missing.DAT", "unchanged-missing.DAT", "new-file.DAT", "changed-modified.DAT"]);
    assert!(planned.iter().any(|p| p.action == Action::Deltas(1)), "the changed old file is delta-patched from its read base");
    assert_eq!((report.current, report.outdated, report.missing, report.modified, report.kept), (2, 1, 3, 3, 1));
    // An unchanged file that kept its size is taken as the listed version without a read:
    // the accepted cost of checking only what changed (owner, 2026-10-13).
    let same_size = new.files.iter().position(|h| h.path.ends_with("/current.DAT")).unwrap();
    fs::write(root.join(&new.files[same_size].path), vec![b'x'; 300]).unwrap();
    let version = plan_from_applied(Product::Ffxi, &root, &installed, &new, Mode::Update, &op, 2).unwrap();
    assert_eq!(version.1, full.1);
    assert_eq!(scan_for(Product::Ffxi, &root, &new, Mode::Update, &op, Some(OLD)).unwrap().0.modified, 4);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_version_plan_is_used_only_for_a_routine_update_with_a_matching_record() {
    let root = root("plan-choice");
    let (new, installed) = plan_fixture(&root);
    fs::create_dir_all(root.join(STATE_DIR)).unwrap();
    assert_eq!(applied_list(&root, Product::Ffxi, Some(OLD)), None, "no installed list");
    fs::write(root.join("patch.cfg"), manifest_text(&installed)).unwrap();
    assert_eq!(applied_list(&root, Product::Ffxi, Some(OLD)).map(|m| m.files.len()), Some(installed.files.len()));
    assert_eq!(applied_list(&root, Product::Ffxi, Some(NEW)), None, "the list is not the recorded version's");
    assert_eq!(applied_list(&root, Product::Ffxi, None), None, "fresh install or unreadable record");
    fs::write(root.join("patch.cfg"), b"not a list").unwrap();
    assert_eq!(applied_list(&root, Product::Ffxi, Some(OLD)), None);
    assert!(!full_check_due(&root));
    mark_full_check(&root);
    assert!(full_check_due(&root));
    let _ = new;
    fs::remove_dir_all(root).unwrap();
}

/// Crystal's request sequence for stock's own update, from the list stock installed
/// (the updated install's patch.cfg) and the requests decoded from a stock capture.
/// Assumes every file the update changed was at its `CRYSTAL_STOCK_BASE` version.
/// Default: stock's whole 30260904_1 → 30261006_0 update, 540 command-3 requests decoded in
/// full from two captures (snaplen 160 holds each request): stock-20261013-002122 made the
/// first 119 before it was cancelled mid-request, and stock-update-20261013-004932 resumed
/// with that request and made the other 421. Matched Crystal exactly on 2026-10-13. This
/// compares the command-3 requests only (path, offset, size, order), not the connections or
/// their timing: `an_update_asks_version_then_list_…` checks those on a local server, and
/// the version → list switch still lacks stock's idle prompt connection (Phase 2).
#[test]
#[ignore = "needs CRYSTAL_STOCK_MANIFEST: the patch.cfg stock installed (30261006_0 for the default capture)"]
fn crystal_requests_match_a_stock_capture() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let m = manifest::parse(&fs::read_to_string(var("CRYSTAL_STOCK_MANIFEST")).unwrap()).unwrap();
    validate_manifest(&m).unwrap();
    let base = std::env::var("CRYSTAL_STOCK_BASE").unwrap_or_else(|_| OLD.into());
    let order = |stamp: &str| Product::Ffxi.stamp_order(stamp).unwrap();
    let mut signatures = Vec::new();
    for history in &m.files {
        let at_base = history.versions.iter().rposition(|v| order(&v.stamp) <= order(&base));
        signatures.push(at_base.map(|i| LocalFile { sig: history.versions[i].sig, md5: [0; 16] }));
    }
    let (_, planned) = plan_from_signatures(Product::Ffxi, &m, Mode::Update, &Operation::new(), signatures,
        &|index| Product::Ffxi.changed_since(Some(&base), &m.files[index]), true).unwrap();
    let mut crystal = Vec::new();
    for item in &planned {
        let history = &m.files[item.index];
        let transfers: Vec<(String, u64)> = match item.action {
            Action::Direct => vec![(history.current().direct.clone(), history.current().direct_len)],
            Action::Deltas(from) => history.versions[from..].iter().map(|v| { let d = v.indirect.as_ref().unwrap(); (d.path.clone(), d.len) }).collect(),
        };
        for (path, len) in transfers {
            let mut offset = 0u64;
            while offset < len {
                let count = (len - offset).min(polp::CHUNK_LEN as u64);
                crystal.push(format!("{path}\t{offset}\t{count}"));
                offset += count;
            }
        }
    }
    let stock: Vec<String> = match std::env::var("CRYSTAL_STOCK_REQUESTS") {
        Ok(path) => fs::read_to_string(path).unwrap(),
        Err(_) => include_str!("fixtures/stock-20261013-full-update.requests.tsv").to_owned(),
    }.lines().map(|line| line.trim_end_matches('\r').to_owned()).collect();
    eprintln!("stock {} requests, Crystal {} requests", stock.len(), crystal.len());
    let stock_files: Vec<&str> = stock.iter().map(|line| line.split('\t').next().unwrap()).collect();
    for (index, (a, b)) in stock.iter().zip(&crystal).enumerate() {
        if a != b {
            let file = b.split('\t').next().unwrap();
            panic!("first difference at request {index}: stock {a:?}, Crystal {b:?} (Crystal's file is at stock request {:?})",
                stock_files.iter().position(|path| *path == file));
        }
    }
    assert_eq!(stock.len(), crystal.len());
}

/// Play is the update (proposal 09 phase 2), against stock-update-20261013-004932: the update a
/// Play's W20 asked for sends no version request of its own. Its first connection opens
/// `NEXT_CONNECTION_GAP` after that W20's FIN and stays idle for the prompt's hold, the file
/// list opens a gap after that FIN, then one stop-and-wait download connection; nothing
/// follows the install (no version re-check before the game starts).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_play_update_reuses_the_w20_holds_the_prompt_then_lists_and_downloads() {
    let root = root("after-play");
    let (m, files) = update_fixture(&root);
    let server = serve(NEW, manifest_text(&m).as_bytes(), files);
    let op = Operation::new();
    // The Play's W20 closed just now (patch.rs notes its FIN for the update).
    let w20_fin = Instant::now();
    note_server_closed(&server.addr, w20_fin);
    let hold = Duration::from_millis(400);
    let staged = stage_after_play_at(root.clone(), AfterPlay { official: NEW.into(), prompt: hold }, &op, &server.addr, |_| Ok(())).await.unwrap();
    assert_eq!(staged.root(), fs::canonicalize(&root).unwrap());
    // Downloaded while "a game" could still run: only the install needs the folder free.
    let report = install_staged_with(staged, &op, |_| Ok(())).await.unwrap();
    assert!(report.verified && report.version == NEW, "{report:?}");
    assert_eq!(crate::patch::installed_for_request(&root), NEW);
    assert!(op.snapshot().overall.unwrap().complete);
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.seen().iter().filter(|s| s.what == What::Close).count() < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Nothing else comes: no version check after the update.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = server.seen();
    assert!(!seen.iter().any(|s| matches!(s.what, What::Overlap | What::Wrong(_))), "{seen:#?}");
    assert_eq!(seen.iter().filter(|s| s.what == What::Connect).count(), 3, "prompt, list, downloads: {seen:#?}");
    assert!(!seen.iter().any(|s| matches!(s.what, What::Request { kind: 7, .. })), "the Play's W20 is reused");
    let requests = |conn: usize| seen.iter().filter_map(|s| match &s.what {
        What::Request { kind, path, offset, count } if s.conn == conn => Some((*kind, path.clone(), *offset, *count)),
        _ => None }).collect::<Vec<_>>();
    assert!(requests(0).is_empty(), "the prompt connection carries nothing");
    assert_eq!(requests(1), [(1, String::new(), 0, 0)]);
    let big = format!("{NEW}/Direct/ROM/1/big.DAT.slc");
    assert_eq!(requests(2), [
        (3, format!("{NEW}/Indirect/delta.DAT.olc"), 0, delta_stream(200, b"!!").len() as u32),
        (3, big.clone(), 0, 65536), (3, big.clone(), 65536, 65536), (3, big, 131072, 150_001 - 131072),
        (3, format!("{NEW}/Direct/small.DAT.slc"), 0, 20),
    ]);
    // W20 FIN → prompt SYN, the prompt held open, its FIN → list SYN: never within 100 ms.
    let prompt_syn = seen_at(&seen, 0, &What::Connect);
    let prompt_fin = seen_at(&seen, 0, &What::Close);
    let list_syn = seen_at(&seen, 1, &What::Connect);
    eprintln!("W20 FIN → prompt SYN {:?}, prompt held {:?}, prompt FIN → list SYN {:?}",
        prompt_syn - w20_fin, prompt_fin - prompt_syn, list_syn - prompt_fin);
    assert!(prompt_syn - w20_fin >= NEXT_CONNECTION_GAP.0, "{:?}", prompt_syn - w20_fin);
    assert!(prompt_fin - prompt_syn >= hold, "{:?}", prompt_fin - prompt_syn);
    assert!(list_syn - prompt_fin >= Duration::from_millis(100), "{:?}", list_syn - prompt_fin);
    // The list and download connections close like stock after their last reply.
    for conn in [1, 2] {
        let quiet = seen_at(&seen, conn, &What::Close) - seen_at(&seen, conn, &What::Replied);
        assert!(quiet >= crate::stock_socket::CLOSE_AFTER_REPLY.0, "reply → FIN {quiet:?}");
    }
    assert!(seen_at(&seen, 2, &What::Connect) - seen_at(&seen, 1, &What::Close) >= Duration::from_millis(100));
    fs::remove_dir_all(root).unwrap();
}

/// The prompt's hold is a plausible quick human confirmation (stock: 1.0 s and 5.5 s), never
/// one constant; the W20 Play's check closes and the updater's first connection are keyed to
/// the same server.
#[test]
fn the_prompt_hold_is_a_jittered_human_confirmation() {
    assert_eq!(PROMPT_HOLD, (Duration::from_secs(1), Duration::from_secs(3)));
    let holds: Vec<Duration> = (0..50).map(|_| crate::stock_socket::uniform(PROMPT_HOLD)).collect();
    assert!(holds.iter().all(|hold| *hold >= PROMPT_HOLD.0 && *hold <= PROMPT_HOLD.1));
    assert_eq!(format!("{}:53001", crate::dns::GAME_PATCH_HOST), Product::Ffxi.server());
}

/// Cancel during the prompt: its connection closes at once, nothing else is sent, and the next
/// Play starts again from the W20 (the verified cache keeps any finished download).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_during_the_prompt_closes_it_and_sends_nothing_more() {
    let root = root("after-play-cancel");
    let (m, files) = update_fixture(&root);
    let server = serve(NEW, manifest_text(&m).as_bytes(), files);
    let op = Operation::new();
    let cancel = op.clone();
    tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(400)).await; cancel.cancel(); });
    let started = Instant::now();
    let failed = stage_after_play_at(root.clone(), AfterPlay { official: NEW.into(), prompt: Duration::from_secs(20) }, &op, &server.addr, |_| Ok(())).await;
    assert!(failed.is_err() && started.elapsed() < Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = server.seen();
    assert_eq!(seen.iter().filter(|s| s.what == What::Connect).count(), 1, "{seen:#?}");
    assert!(seen.iter().any(|s| s.what == What::Close) && !seen.iter().any(|s| matches!(s.what, What::Request { .. })));
    assert!(!root.join("patch.ver").exists());
    fs::remove_dir_all(root).unwrap();
}

/// One file request (`polp::file_request`) read off `stream`: (offset, count), `None` at EOF.
fn read_file_request(stream: &mut TcpStream) -> Option<(u32, u32)> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).ok()?;
    let mut packet = vec![0u8; u32::from_le_bytes(head) as usize];
    packet[..4].copy_from_slice(&head);
    stream.read_exact(&mut packet[4..]).ok()?;
    let body = polp::parse(&packet).ok()?.body.to_vec();
    Some((u32::from_le_bytes(body[0..4].try_into().unwrap()), u32::from_le_bytes(body[4..8].try_into().unwrap())))
}

fn chunk_reply(path: &str, data: &[u8], offset: u32, count: u32) -> Vec<u8> {
    let mut reply = Vec::new();
    for value in [offset, count, path.len() as u32 + 1] { reply.extend(value.to_le_bytes()); }
    reply.extend(path.as_bytes());
    reply.push(0);
    reply.extend(&data[offset as usize..(offset + count) as usize]);
    polp::seal(polp::kind::FILE_REPLY, &reply)
}

/// A chunk reply that breaks off: the old connection closes like stock (FIN well after its last
/// reply byte, not at once), the new one's SYN keeps `NEXT_CONNECTION_GAP` from that FIN, and
/// the new connection asks for the broken chunk again at the same offset. Nothing more is sent
/// on the broken connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_chunk_reply_closes_like_stock_and_resumes_the_chunk_on_a_new_connection() {
    let path = "30261006_0/Direct/ROM/1/big.DAT.slc";
    let data: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let served = data.clone();
    // Per connection: accepted, the requests (offset, count), and when the client's FIN arrived.
    let server = std::thread::spawn(move || (0..2).map(|conn| {
        let (mut stream, _) = listener.accept().unwrap();
        let accepted = Instant::now();
        let mut requests = Vec::new();
        while let Some((offset, count)) = read_file_request(&mut stream) {
            requests.push((offset, count));
            let reply = chunk_reply(path, &served, offset, count);
            if conn == 0 && offset > 0 {
                // The second chunk's reply breaks off partway.
                stream.write_all(&reply[..reply.len() / 3]).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
                assert!(rest.is_empty(), "nothing is sent on a broken connection");
                break;
            }
            stream.write_all(&reply).unwrap();
        }
        (accepted, requests, Instant::now())
    }).collect::<Vec<_>>());
    let op = Operation::new();
    let mut session = Session::connect_to(Product::Ffxi, &addr, &op).await.unwrap();
    let fetched = session.fetch(path, data.len() as u64, &op).await.unwrap();
    assert_eq!(fetched, data);
    session.close().await;
    let seen = tokio::task::spawn_blocking(move || server.join().unwrap()).await.unwrap();
    let (first, second) = (&seen[0], &seen[1]);
    assert_eq!(first.1, [(0, 0x10000), (0x10000, 0x10000)]);
    assert_eq!(second.1, [(0x10000, 0x10000), (0x20000, 150_000 - 0x20000)], "the broken chunk again, at its offset");
    let gap = second.0.saturating_duration_since(first.2);
    assert!(gap >= NEXT_CONNECTION_GAP.0 - Duration::from_millis(10), "SYN {gap:?} after the broken connection's FIN");
}

/// A session dropped on an error or cancel path still closes like stock: a FIN (not a reset),
/// timed from its last reply byte, and recorded for the next connection's gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_session_still_closes_like_stock() {
    let path = "30261006_0/Direct/small.DAT.slc";
    let data = b"small official file".to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let served = data.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (offset, count) = read_file_request(&mut stream).unwrap();
        stream.write_all(&chunk_reply(path, &served, offset, count)).unwrap();
        let replied = Instant::now();
        let mut rest = Vec::new();
        // A reset is an error here; a FIN is a clean EOF.
        stream.read_to_end(&mut rest).map(|_| replied.elapsed())
    });
    let op = Operation::new();
    let mut session = Session::connect_to(Product::Ffxi, &addr, &op).await.unwrap();
    assert_eq!(session.fetch(path, data.len() as u64, &op).await.unwrap(), data);
    drop(session);
    let after = tokio::task::spawn_blocking(move || server.join().unwrap()).await.unwrap().expect("a FIN, not a reset");
    let (low, _) = crate::stock_socket::CLOSE_AFTER_REPLY;
    assert!(after >= low - Duration::from_millis(10), "FIN {after:?} after the reply");
    // A next connection waits for the close thread, then spaces itself from the FIN.
    wait_closing(&addr, Duration::from_secs(2));
    assert!(connect_gap(&addr) > Duration::ZERO, "the close counts for the next connection's gap");
}

/// A file another handle holds open counts as held (so the install waits for it); a closed,
/// missing or read-only one does not.
#[test]
fn an_open_file_is_held_and_a_closed_or_read_only_one_is_not() {
    let root = root("held");
    let path = root.join("FFXiMain.dll");
    fs::write(&path, b"image").unwrap();
    let open = fs::File::open(&path).unwrap();
    assert!(held(&path));
    drop(open);
    assert!(!held(&path) && !held(&root.join("missing.DAT")));
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions.clone()).unwrap();
    assert!(!held(&path));
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    fs::remove_dir_all(root).unwrap();
}
