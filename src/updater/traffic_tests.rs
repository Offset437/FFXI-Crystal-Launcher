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

/// Wait (up to 5 s) until the server saw at least `least` connections and every one closed.
async fn wait_all_closed(server: &FakeServer, least: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = server.seen();
        let count = |what: &What| seen.iter().filter(|s| &s.what == what).count();
        if (count(&What::Connect) >= least && count(&What::Close) == count(&What::Connect)) || Instant::now() > deadline { return; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The file requests (path, offset, count) the server saw on connections `conns`, sorted.
fn file_requests(seen: &[Seen], conns: impl Fn(usize) -> bool) -> Vec<(String, u32, u32)> {
    let mut out: Vec<_> = seen.iter().filter_map(|s| match &s.what {
        What::Request { kind: 3, path, offset, count } if conns(s.conn) => Some((path.clone(), *offset, *count)),
        _ => None }).collect();
    out.sort();
    out
}

/// What `update_fixture`'s update asks for, sorted.
fn fixture_requests() -> Vec<(String, u32, u32)> {
    let big = format!("{NEW}/Direct/ROM/1/big.DAT.slc");
    let mut out = vec![
        (format!("{NEW}/Indirect/delta.DAT.olc"), 0, delta_stream(200, b"!!").len() as u32),
        (big.clone(), 0, 65536), (big.clone(), 65536, 65536), (big, 131072, 150_001 - 131072),
        (format!("{NEW}/Direct/small.DAT.slc"), 0, 20),
    ];
    out.sort();
    out
}

/// One routine update against a local server: prepare (version, list), then stage every
/// download. Returns what the server saw (after every connection closed) and the
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
        groups.clone(), &op, &downloads, 0).await.unwrap();
    // Each download connection closes ~0.1 s after its last reply.
    wait_all_closed(&server, 3).await;
    let connections = server.seen().iter().filter(|s| s.what == What::Connect).count();
    // Resuming over the same verified cache stages nothing new and opens no connection.
    let again = Downloads::new(op.clone(), groups.iter()
        .map(|g| download_size(&prepared.manifest.files[g.source.index], &g.source.action)).collect());
    let (resumed, bytes) = stage_plan_shared(Product::Ffxi, &server.addr, &prepared.root, &staged, &prepared.manifest,
        groups, &op, &again, 0).await.unwrap();
    assert_eq!((bytes, &resumed), (0, &ready));
    assert_eq!(server.seen().iter().filter(|s| s.what == What::Connect).count(), connections);
    drop(prepared); // the updater lock
    (server.seen(), ready)
}

/// When the server saw `what` last on connection `conn`.
fn seen_at(seen: &[Seen], conn: usize, what: &What) -> Instant {
    seen.iter().filter(|s| s.conn == conn && &s.what == what).map(|s| s.at).next_back().unwrap()
}

/// Reply → FIN of the version and list connections, and the version FIN → list SYN.
fn close_and_reopen(seen: &[Seen]) -> (Vec<Duration>, Duration) {
    let quiet = (0..2).map(|conn| seen_at(seen, conn, &What::Close) - seen_at(seen, conn, &What::Replied)).collect();
    (quiet, seen_at(seen, 1, &What::Connect) - seen_at(seen, 0, &What::Close))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_asks_version_then_list_then_downloads_in_parallel() {
    let root = root("order");
    let (seen, ready) = fake_update(&root).await;
    assert_eq!(ready.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["delta.DAT", "ROM/1/big.DAT", "small.DAT"]);
    // The version and list connections keep one request in flight; downloads may pipeline.
    assert!(!seen.iter().any(|s| matches!(s.what, What::Wrong(_)) || (s.what == What::Overlap && s.conn < 2)), "{seen:#?}");
    let connections = seen.iter().filter(|s| s.what == What::Connect).count();
    assert!((3..=5).contains(&connections), "version, list, one download connection per worker: {seen:#?}");
    let requests = |conn: usize| seen.iter().filter_map(|s| match &s.what {
        What::Request { kind, path, offset, count } if s.conn == conn => Some((*kind, path.clone(), *offset, *count)),
        _ => None }).collect::<Vec<_>>();
    assert_eq!(requests(0), [(7, String::new(), 0, 0)]);
    assert_eq!(requests(1), [(1, String::new(), 0, 0)]);
    assert_eq!(file_requests(&seen, |conn| conn >= 2), fixture_requests());
    // The version and list connections close at least 100 ms after their reply, and the list
    // opens at least 100 ms after the version's FIN.
    let (quiet, gap) = close_and_reopen(&seen);
    assert!(quiet.iter().all(|quiet| *quiet >= crate::stock_socket::CLOSE_AFTER_REPLY.0), "reply → FIN {quiet:?}");
    assert!(gap >= Duration::from_millis(100), "FIN → SYN {gap:?}");
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
        let (quiet, gap) = close_and_reopen(&seen);
        let inside = quiet.iter().all(|quiet| *quiet >= low && *quiet <= high + Duration::from_millis(3))
            && gap <= NEXT_CONNECTION_GAP.1 + Duration::from_millis(8);
        if inside { return; }
        runs.push((quiet, gap));
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
    wait_all_closed(&game_server, 3).await;
    let (viewer_seen, game_seen) = (viewer_server.seen(), game_server.seen());
    assert_eq!(viewer_seen.iter().filter(|s| s.what == What::Connect).count(), 3, "{viewer_seen:#?}");
    assert!((3..=5).contains(&game_seen.iter().filter(|s| s.what == What::Connect).count()), "{game_seen:#?}");
    assert_eq!(file_requests(&game_seen, |conn| conn >= 2), fixture_requests());
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
/// a gap after the W2U's FIN, the file list, parallel download connections. The
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
    wait_all_closed(&server, 4).await;
    // The sign-in that follows: its own W2U, with the version just installed.
    let (address, folder) = (server.addr.clone(), pol.clone());
    let verdict = tokio::task::spawn_blocking(move || crate::patch::viewer_connect_at(&folder, &address)).await.unwrap();
    assert_eq!(verdict, ConnectVerdict::Current { version: VIEWER_NEW.into() });
    let seen = server.seen();
    let last = seen.iter().filter(|s| s.what == What::Connect).count() - 1;
    // Only download connections may pipeline.
    assert!(!seen.iter().any(|s| matches!(s.what, What::Wrong(_)) || (s.what == What::Overlap && !(3..last).contains(&s.conn))), "{seen:#?}");
    assert!((4..=5).contains(&last), "W2U, prompt, list, one or two download connections, W2U: {seen:#?}");
    let requests = |conn: usize| seen.iter().filter_map(|s| match &s.what {
        What::Request { kind, path, offset, count } if s.conn == conn => Some((*kind, path.clone(), *offset, *count)),
        _ => None }).collect::<Vec<_>>();
    assert_eq!(requests(0), [(7, VIEWER_OLD.to_owned(), 0, 0)], "the sign-in's W2U");
    assert!(requests(1).is_empty(), "the prompt connection carries nothing");
    assert_eq!(requests(2), [(1, String::new(), 0, 0)]);
    assert_eq!(file_requests(&seen, |conn| (3..last).contains(&conn)), ["app", "polcore"].map(|name|
        (format!("{VIEWER_NEW}/Direct/viewer/com/{name}.dll.slc"), 0, format!("new official {name} build").len() as u32 + 1)));
    assert_eq!(requests(last), [(7, VIEWER_NEW.to_owned(), 0, 0)], "the next sign-in's W2U");
    // W2U FIN → prompt SYN, the prompt held open, its FIN → list SYN: never within 100 ms.
    let prompt_syn = seen_at(&seen, 1, &What::Connect);
    let prompt_fin = seen_at(&seen, 1, &What::Close);
    assert!(prompt_syn - w2u_fin >= NEXT_CONNECTION_GAP.0, "{:?}", prompt_syn - w2u_fin);
    assert!(prompt_fin - prompt_syn >= hold, "{:?}", prompt_fin - prompt_syn);
    assert!(seen_at(&seen, 2, &What::Connect) - prompt_fin >= Duration::from_millis(100));
    let quiet = seen_at(&seen, 2, &What::Close) - seen_at(&seen, 2, &What::Replied);
    assert!(quiet >= crate::stock_socket::CLOSE_AFTER_REPLY.0, "reply → FIN {quiet:?}");
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
fn stock_connections_keep_their_gap() {
    assert!(NEXT_CONNECTION_GAP.0 > Duration::from_millis(100) && NEXT_CONNECTION_GAP.1 <= Duration::from_millis(115));
}

/// `count` direct files of `size` bytes each (`f00.DAT`…), none installed yet, with the payloads
/// a server holds and the plan that downloads them all.
fn many_files(count: usize, size: usize) -> (Manifest, HashMap<String, Vec<u8>>, Vec<Planned>) {
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    for i in 0..count {
        let path = format!("f{i:02}.DAT");
        let data: Vec<u8> = (0..size).map(|b| (b * 31 + i) as u8).collect();
        let v = version(NEW, &data, &path, data.len() as u64 + 1, None);
        files.insert(v.direct.clone(), [&[1u8][..], &data].concat());
        planned.push(Planned { index: i, action: Action::Direct, original: None, keep: false });
        m.files.push(FileHistory { path, optional: false, versions: vec![v] });
    }
    (m, files, planned)
}

/// A file server on loopback answering `polp::file_request`s for `files` in order, `delay`
/// before each reply. It counts the most connections busy at once, the connections accepted and
/// every request it saw (path, offset). A request for `fail` ends the connection without a
/// reply (a FIN after the earlier replies).
struct ChunkServer { addr: String, peak: Arc<AtomicUsize>, accepted: Arc<AtomicUsize>, asked: Arc<Mutex<Vec<(String, u32)>>> }
fn chunk_server(files: HashMap<String, Vec<u8>>, delay: Duration, fail: Option<String>) -> ChunkServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (peak, accepted, asked) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(Vec::new())));
    let (open, files) = (Arc::new(AtomicUsize::new(0)), Arc::new(files));
    let (server_peak, server_accepted, server_asked) = (peak.clone(), accepted.clone(), asked.clone());
    std::thread::spawn(move || for stream in listener.incoming() {
        let Ok(mut stream) = stream else { break };
        server_accepted.fetch_add(1, Ordering::SeqCst);
        let (peak, asked, open, files, fail) = (server_peak.clone(), server_asked.clone(), open.clone(), files.clone(), fail.clone());
        std::thread::spawn(move || loop {
            let mut head = [0u8; 4];
            if stream.read_exact(&mut head).is_err() { return; }
            let mut packet = vec![0u8; u32::from_le_bytes(head) as usize];
            packet[..4].copy_from_slice(&head);
            if stream.read_exact(&mut packet[4..]).is_err() { return; }
            let body = polp::parse(&packet).unwrap().body.to_vec();
            let word = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap());
            let (offset, count, name) = (word(0), word(4), word(16) as usize);
            let path = String::from_utf8_lossy(&body[20..20 + name - 1]).into_owned();
            asked.lock().unwrap().push((path.clone(), offset));
            if fail.as_ref() == Some(&path) {
                // A FIN, not a reset for requests left unread: the client reads every earlier reply.
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let _ = std::io::copy(&mut stream, &mut std::io::sink());
                return;
            }
            let now = open.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(delay);
            open.fetch_sub(1, Ordering::SeqCst);
            if stream.write_all(&chunk_reply(&path, &files[&path], offset, count)).is_err() { return; }
        });
    });
    ChunkServer { addr, peak, accepted, asked }
}

/// The pool's tuning with `connections`, `depth` and a 10 ms connection spacing.
fn tuning(connections: usize, depth: usize) -> download::Tuning {
    download::Tuning { connections, depth, spacing: Duration::from_millis(10), ..Default::default() }
}

/// Stage `planned` of `m` through the pool with `tuning`; also returns what it did on the wire.
async fn pool(server: &str, root: &Path, m: &Manifest, planned: &[Planned], op: &Operation, tuning: download::Tuning)
    -> (Result<(Vec<Ready>, u64), String>, Arc<download::Stats>) {
    let staged = root.join(".xi-updater/staged");
    fs::create_dir_all(&staged).unwrap();
    let groups = stage_groups(Product::Ffxi, m, planned);
    let downloads = Downloads::new(op.clone(), groups.iter().map(|g| download_size(&m.files[g.source.index], &g.source.action)).collect());
    let stats = Arc::new(download::Stats::default());
    (download::stage(Product::Ffxi, server, root, &staged, m, groups, op, &downloads, 0, tuning, stats.clone()).await, stats)
}

/// One new file `path` holding `content`, stored (flag 1) on the server: its manifest, plan and payloads.
fn one_file(path: &str, content: &[u8]) -> (Manifest, Vec<Planned>, HashMap<String, Vec<u8>>) {
    let v = version(NEW, content, path, content.len() as u64 + 1, None);
    let files = HashMap::from([(v.direct.clone(), [&[1u8][..], content].concat())]);
    (Manifest { files: vec![FileHistory { path: path.into(), optional: false, versions: vec![v] }] },
        vec![Planned { index: 0, action: Action::Direct, original: None, keep: false }], files)
}

/// Stage `planned` of `m` from `server` into `root`'s cache, as an update does.
async fn stage_from(server: &str, root: &Path, m: &Manifest, planned: &[Planned], op: &Operation) -> Result<(Vec<Ready>, u64), String> {
    let staged = root.join(".xi-updater/staged");
    fs::create_dir_all(&staged).unwrap();
    let groups = stage_groups(Product::Ffxi, m, planned);
    let downloads = Downloads::new(op.clone(), groups.iter().map(|g| download_size(&m.files[g.source.index], &g.source.action)).collect());
    stage_plan_shared(Product::Ffxi, server, root, &staged, m, groups, op, &downloads, 0).await
}

/// Two hundred files (400 chunks, 40 ms of server time each) reach exactly
/// `MAX_DOWNLOAD_WORKERS` connections, never more, and every file arrives verified, in plan
/// order, with the byte progress covering all of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_run_on_at_most_max_workers_connections_and_all_files_verify() {
    let root = root("parallel");
    let (m, files, planned) = many_files(200, 70_000);
    let server = chunk_server(files, Duration::from_millis(40), None);
    let op = Operation::new();
    let (ready, bytes) = pool(&server.addr, &root, &m, &planned, &op, tuning(MAX_DOWNLOAD_WORKERS, download::MAX_DEPTH)).await.0.unwrap();
    assert_eq!(server.peak.load(Ordering::SeqCst), MAX_DOWNLOAD_WORKERS, "connections busy at once");
    assert_eq!(server.accepted.load(Ordering::SeqCst), MAX_DOWNLOAD_WORKERS, "connections opened");
    assert_eq!(ready.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), m.files.iter().map(|h| h.path.clone()).collect::<Vec<_>>());
    for (path, cache, sig, _) in &ready { assert_eq!(FileSig::of_path(cache).unwrap(), *sig, "{path}"); }
    assert_eq!(bytes, 200 * 70_000, "decoded bytes");
    let progress = op.snapshot();
    assert_eq!((progress.done, progress.total), (200 * 70_001, 200 * 70_001), "every chunk of every file counted");
    fs::remove_dir_all(root).unwrap();
}

/// A file whose request always breaks its connection: it is asked again on other connections
/// (each time at the head of a broken one), then the update fails with its error after three
/// attempts and every connection stops: more work was left than was done, and nothing is sent
/// after the failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_failing_file_retries_then_stops_every_worker() {
    let root = root("parallel-fail");
    let (m, files, planned) = many_files(3000, 1000);
    let bad = m.files[3].current().direct.clone();
    let server = chunk_server(files, Duration::from_millis(30), Some(bad.clone()));
    let (result, stats) = pool(&server.addr, &root, &m, &planned, &Operation::new(), download::Tuning::default()).await;
    let error = result.unwrap_err();
    assert!(error.contains(&bad), "{error}");
    let sent = stats.requests.load(Ordering::SeqCst);
    // The server reads pipelined requests behind its 30 ms replies: let it catch up first.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(stats.requests.load(Ordering::SeqCst), sent, "nothing is sent after the failure");
    let asked = server.asked.lock().unwrap().clone();
    assert_eq!(asked.iter().filter(|(path, _)| *path == bad).count(), 3, "three attempts");
    assert!(asked.len() < 3000, "the other connections stopped: {} requests", asked.len());
    fs::remove_dir_all(root).unwrap();
}

/// A restarted update reuses every verified cached file and downloads only the rest: a file
/// whose cached copy is corrupt or cut short, and one an interrupted save left as `.xi-part`,
/// are downloaded again in full.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_update_reuses_verified_files_and_redownloads_corrupt_or_partial_ones() {
    let root = root("parallel-resume");
    let (m, files, planned) = many_files(12, 70_000);
    let server = chunk_server(files, Duration::ZERO, None);
    let (first, _) = stage_from(&server.addr, &root, &m, &planned, &Operation::new()).await.unwrap();
    // Damage: file 2's copy flipped, file 5's cut short, file 7's lost mid-save (only its .xi-part left).
    let mut flipped = fs::read(&first[2].1).unwrap(); flipped[10] ^= 0xff; fs::write(&first[2].1, flipped).unwrap();
    let short = fs::read(&first[5].1).unwrap(); fs::write(&first[5].1, &short[..65536]).unwrap();
    fs::rename(&first[7].1, first[7].1.with_extension("xi-part")).unwrap();
    server.asked.lock().unwrap().clear();
    let op = Operation::new();
    let (again, bytes) = stage_from(&server.addr, &root, &m, &planned, &op).await.unwrap();
    assert_eq!(again, first);
    for (path, cache, sig, _) in &again { assert_eq!(FileSig::of_path(cache).unwrap(), *sig, "{path}"); }
    let mut asked = server.asked.lock().unwrap().clone();
    asked.sort();
    let expected: Vec<(String, u32)> = [2, 5, 7].iter()
        .flat_map(|&i| [0, 65536].map(|offset| (m.files[i].current().direct.clone(), offset))).collect();
    assert_eq!(asked, expected, "only the damaged files, each from its start");
    assert_eq!(bytes, 3 * 70_000, "decoded bytes");
    let progress = op.snapshot();
    assert_eq!((progress.done, progress.total), (12 * 70_001, 12 * 70_001));
    fs::remove_dir_all(root).unwrap();
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
/// list opens a gap after that FIN, then parallel download connections; nothing
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
    wait_all_closed(&server, 3).await;
    // Nothing else comes: no version check after the update.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = server.seen();
    // The prompt and list connections keep one request in flight; downloads may pipeline.
    assert!(!seen.iter().any(|s| matches!(s.what, What::Wrong(_)) || (s.what == What::Overlap && s.conn < 2)), "{seen:#?}");
    assert!((3..=5).contains(&seen.iter().filter(|s| s.what == What::Connect).count()), "prompt, list, downloads: {seen:#?}");
    assert!(!seen.iter().any(|s| matches!(s.what, What::Request { kind: 7, .. })), "the Play's W20 is reused");
    let requests = |conn: usize| seen.iter().filter_map(|s| match &s.what {
        What::Request { kind, path, offset, count } if s.conn == conn => Some((*kind, path.clone(), *offset, *count)),
        _ => None }).collect::<Vec<_>>();
    assert!(requests(0).is_empty(), "the prompt connection carries nothing");
    assert_eq!(requests(1), [(1, String::new(), 0, 0)]);
    assert_eq!(file_requests(&seen, |conn| conn >= 2), fixture_requests());
    // W20 FIN → prompt SYN, the prompt held open, its FIN → list SYN: never within 100 ms.
    let prompt_syn = seen_at(&seen, 0, &What::Connect);
    let prompt_fin = seen_at(&seen, 0, &What::Close);
    let list_syn = seen_at(&seen, 1, &What::Connect);
    eprintln!("W20 FIN → prompt SYN {:?}, prompt held {:?}, prompt FIN → list SYN {:?}",
        prompt_syn - w20_fin, prompt_fin - prompt_syn, list_syn - prompt_fin);
    assert!(prompt_syn - w20_fin >= NEXT_CONNECTION_GAP.0, "{:?}", prompt_syn - w20_fin);
    assert!(prompt_fin - prompt_syn >= hold, "{:?}", prompt_fin - prompt_syn);
    assert!(list_syn - prompt_fin >= Duration::from_millis(100), "{:?}", list_syn - prompt_fin);
    // The list connection closes like stock after its reply.
    let quiet = seen_at(&seen, 1, &What::Close) - seen_at(&seen, 1, &What::Replied);
    assert!(quiet >= crate::stock_socket::CLOSE_AFTER_REPLY.0, "reply → FIN {quiet:?}");
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

/// A chunk reply that breaks off (one request in flight): the old connection closes like stock
/// (FIN well after its last reply byte, not at once), nothing more is sent on it, and a new
/// connection, opened only after the break backoff, asks for the broken chunk again at the same
/// offset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_chunk_reply_closes_like_stock_and_resumes_the_chunk_on_a_new_connection() {
    let content: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
    let (m, planned, files) = one_file("ROM/1/big.DAT", &content);
    let (path, data) = files.into_iter().next().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    // Per connection: accepted, the requests (offset, count), when it broke the reply, and when
    // the client's FIN arrived.
    let server = std::thread::spawn(move || (0..2).map(|conn| {
        let (mut stream, _) = listener.accept().unwrap();
        let (accepted, mut broke) = (Instant::now(), None);
        let mut requests = Vec::new();
        while let Some((offset, count)) = read_file_request(&mut stream) {
            requests.push((offset, count));
            let reply = chunk_reply(&path, &data, offset, count);
            if conn == 0 && offset > 0 {
                // The second chunk's reply breaks off partway.
                stream.write_all(&reply[..reply.len() / 3]).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                broke = Some(Instant::now());
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
                assert!(rest.is_empty(), "nothing is sent on a broken connection");
                break;
            }
            stream.write_all(&reply).unwrap();
        }
        (accepted, requests, broke, Instant::now())
    }).collect::<Vec<_>>());
    let root = root("broken-chunk");
    let (ready, _) = pool(&addr, &root, &m, &planned, &Operation::new(), tuning(1, 1)).await.0.unwrap();
    assert_eq!(fs::read(&ready[0].1).unwrap(), content);
    let seen = tokio::task::spawn_blocking(move || server.join().unwrap()).await.unwrap();
    let (first, second) = (&seen[0], &seen[1]);
    assert_eq!(first.1, [(0, 0x10000), (0x10000, 0x10000)]);
    assert_eq!(second.1, [(0x10000, 0x10000), (0x20000, 150_001 - 0x20000)], "the broken chunk again, at its offset");
    // The replacement waits out `BREAK_BACKOFF` from the break; the broken connection's drained
    // stock close takes ~0.3 s of that, so its SYN also lands well past `NEXT_CONNECTION_GAP`
    // from the FIN.
    let backoff = second.0.saturating_duration_since(first.2.unwrap());
    assert!(backoff >= download::BREAK_BACKOFF.0 - Duration::from_millis(20), "SYN {backoff:?} after the break");
    let gap = second.0.saturating_duration_since(first.3);
    assert!(gap >= NEXT_CONNECTION_GAP.0, "SYN {gap:?} after the broken connection's FIN");
    fs::remove_dir_all(root).unwrap();
}

/// A download connection out of work closes like stock: a FIN (not a reset), timed from its
/// last reply byte, and recorded for the next connection's gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_session_still_closes_like_stock() {
    let (m, planned, files) = one_file("small.DAT", b"small official file");
    let (path, data) = files.into_iter().next().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (offset, count) = read_file_request(&mut stream).unwrap();
        stream.write_all(&chunk_reply(&path, &data, offset, count)).unwrap();
        let replied = Instant::now();
        let mut rest = Vec::new();
        // A reset is an error here; a FIN is a clean EOF.
        stream.read_to_end(&mut rest).map(|_| replied.elapsed())
    });
    let root = root("download-close");
    let (ready, _) = pool(&addr, &root, &m, &planned, &Operation::new(), tuning(1, 1)).await.0.unwrap();
    assert_eq!(fs::read(&ready[0].1).unwrap(), b"small official file");
    fs::remove_dir_all(root).unwrap();
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

/// How `pipe_server` answers.
#[derive(Clone, Default)]
struct Script {
    delay: Duration,
    /// Answer only the first request of each read and drop the rest.
    first_only: bool,
    /// Swap the first two replies of the first read holding two or more requests.
    swap_once: bool,
    /// Connection 0 ends (a FIN) after this many replies.
    break_after: Option<usize>,
    /// The first this many connections end at their first request, without a reply.
    break_first: usize,
    /// Each reply in pieces of this many bytes, this far apart.
    trickle: Option<(usize, Duration)>,
    /// This much longer before the reply to (path, offset).
    hold: Option<(String, u32, Duration)>,
    /// All replies to one read in a single write (several replies coalesced in one segment).
    coalesce: bool,
    /// The first reply written behind another, sent with another offset's data of the same
    /// length under a valid header and digest: a server corrupting a pipelined reply.
    corrupt_once: bool,
}
#[derive(Clone, Debug)]
struct Asked { conn: usize, path: String, offset: u32, batch: usize }
#[derive(Clone, Default)]
#[allow(clippy::type_complexity)]
struct PipeLog {
    asked: Arc<Mutex<Vec<Asked>>>,
    /// Each connection's (accepted, ended).
    conns: Arc<Mutex<Vec<(Instant, Option<Instant>)>>>,
    replied: Arc<Mutex<Vec<(String, u32, Instant)>>>,
}
impl PipeLog {
    fn asked(&self) -> Vec<Asked> { self.asked.lock().unwrap().clone() }
    fn times(&self, path: &str, offset: u32) -> usize { self.asked().iter().filter(|a| a.path == path && a.offset == offset).count() }
}

/// A file server on loopback that reads requests as they come (a pipeline arrives as one
/// read) and answers them in order, unless `script` says otherwise.
fn pipe_server(files: HashMap<String, Vec<u8>>, script: Script) -> (String, PipeLog) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (log, files, swapped) = (PipeLog::default(), Arc::new(files), Arc::new(AtomicBool::new(false)));
    let server_log = log.clone();
    std::thread::spawn(move || for (conn, stream) in listener.incoming().enumerate() {
        let Ok(stream) = stream else { break };
        server_log.conns.lock().unwrap().push((Instant::now(), None));
        let (log, files, script, swapped) = (server_log.clone(), files.clone(), script.clone(), swapped.clone());
        std::thread::spawn(move || {
            pipe_conn(conn, stream, &files, &script, &log, &swapped);
            log.conns.lock().unwrap()[conn].1 = Some(Instant::now());
        });
    });
    (addr, log)
}

fn pipe_conn(conn: usize, mut stream: TcpStream, files: &HashMap<String, Vec<u8>>, script: &Script, log: &PipeLog, swapped: &AtomicBool) {
    let (mut buffer, mut read, mut sent) = (Vec::new(), vec![0u8; 1 << 16], 0);
    loop {
        let n = match stream.read(&mut read) { Ok(0) | Err(_) => return, Ok(n) => n };
        buffer.extend_from_slice(&read[..n]);
        let mut requests = Vec::new();
        while buffer.len() >= 4 && buffer.len() >= u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize {
            let packet: Vec<u8> = buffer.drain(..u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize).collect();
            let body = polp::parse(&packet).unwrap().body.to_vec();
            let word = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap());
            requests.push((String::from_utf8_lossy(&body[20..20 + word(16) as usize - 1]).into_owned(), word(0), word(4)));
        }
        let batch = requests.len();
        log.asked.lock().unwrap().extend(requests.iter().map(|(path, offset, _)| Asked { conn, path: path.clone(), offset: *offset, batch }));
        if batch == 0 { continue; }
        if conn < script.break_first { return; }
        if script.first_only { requests.truncate(1); }
        if script.swap_once && batch >= 2 && !swapped.swap(true, Ordering::SeqCst) { requests.swap(0, 1); }
        let mut out = Vec::new();
        for (index, (path, offset, count)) in requests.into_iter().enumerate() {
            if conn == 0 && script.break_after == Some(sent) {
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let _ = std::io::copy(&mut stream, &mut std::io::sink());
                return;
            }
            std::thread::sleep(script.delay);
            if let Some((held, at, extra)) = &script.hold { if *held == path && *at == offset { std::thread::sleep(*extra); } }
            let mut reply = chunk_reply(&path, &files[&path], offset, count);
            if script.corrupt_once && index > 0 && !swapped.swap(true, Ordering::SeqCst) {
                let data = &files[&path];
                let other = (offset as usize + count as usize) % (data.len() - count as usize + 1);
                let mut shifted = data.clone();
                shifted[offset as usize..offset as usize + count as usize].copy_from_slice(&data[other..other + count as usize]);
                shifted[offset as usize] ^= 0x5a;
                reply = chunk_reply(&path, &shifted, offset, count);
            }
            if script.coalesce { out.extend(reply); sent += 1; continue; }
            let written = match script.trickle {
                Some((piece, gap)) => reply.chunks(piece).try_for_each(|part| { std::thread::sleep(gap); stream.write_all(part) }),
                None => stream.write_all(&reply),
            };
            if written.is_err() { return; }
            log.replied.lock().unwrap().push((path, offset, Instant::now()));
            sent += 1;
        }
        if !out.is_empty() && stream.write_all(&out).is_err() { return; }
    }
}

/// Add a new stored file `path` holding `content` to a fixture.
fn add_file(m: &mut Manifest, files: &mut HashMap<String, Vec<u8>>, planned: &mut Vec<Planned>, path: &str, content: &[u8]) -> String {
    let (one, _, payload) = one_file(path, content);
    planned.push(Planned { index: m.files.len(), action: Action::Direct, original: None, keep: false });
    m.files.extend(one.files);
    files.extend(payload);
    m.files.last().unwrap().current().direct.clone()
}

fn bytes(n: usize, seed: usize) -> Vec<u8> { (0..n).map(|i| (i * 7 + seed) as u8).collect() }

/// Every staged file is its manifest version, and the progress ends exact.
fn assert_staged(ready: &[Ready], op: &Operation) {
    for (path, cache, sig, _) in ready { assert_eq!(FileSig::of_path(cache).unwrap(), *sig, "{path}"); }
    let progress = op.snapshot();
    assert_eq!(progress.done, progress.total, "progress ends exact");
}

/// Many tiny files and one 2 MiB file: requests are pipelined (several reach the server in one
/// read), every reply matches its request, every file verifies and nothing is asked twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_replies_arrive_in_order_and_every_file_verifies() {
    let root = root("pipeline");
    let (mut m, mut files, mut planned) = many_files(200, 30);
    add_file(&mut m, &mut files, &mut planned, "big.DAT", &bytes(2 << 20, 1));
    let (addr, log) = pipe_server(files, Script { delay: Duration::from_millis(20), ..Default::default() });
    let op = Operation::new();
    let (result, stats) = pool(&addr, &root, &m, &planned, &op, tuning(MAX_DOWNLOAD_WORKERS, download::MAX_DEPTH)).await;
    let (ready, _) = result.unwrap();
    assert_eq!(ready.len(), 201);
    assert_staged(&ready, &op);
    let asked = log.asked();
    assert!(asked.iter().any(|a| a.batch > 1), "requests were pipelined");
    let mut unique: Vec<_> = asked.iter().map(|a| (a.path.clone(), a.offset)).collect();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), asked.len(), "nothing asked twice");
    assert_eq!(stats.fallbacks.load(Ordering::Relaxed), 0);
    fs::remove_dir_all(root).unwrap();
}

/// One 8 MiB file is spread over every connection, not held by one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_large_file_spreads_over_every_connection() {
    let root = root("spread");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let direct = add_file(&mut m, &mut files, &mut planned, "large.DAT", &bytes(8 << 20, 2));
    let (addr, log) = pipe_server(files, Script { delay: Duration::from_millis(100), ..Default::default() });
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(8, download::MAX_DEPTH)).await.0.unwrap();
    assert_staged(&ready, &op);
    let mut conns: Vec<usize> = log.asked().iter().filter(|a| a.path == direct).map(|a| a.conn).collect();
    conns.sort();
    conns.dedup();
    assert_eq!(conns.len(), 8, "every connection served part of it: {conns:?}");
    fs::remove_dir_all(root).unwrap();
}

/// The largest download goes first; each file's first request comes in non-increasing size order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downloads_start_with_the_largest_file() {
    let root = root("largest-first");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let sizes = [100, 300_000, 70_000, 2_000_000, 5, 300_000];
    let directs: Vec<String> = sizes.iter().enumerate()
        .map(|(i, &n)| add_file(&mut m, &mut files, &mut planned, &format!("s{i}.DAT"), &bytes(n, i))).collect();
    let (addr, log) = pipe_server(files, Script::default());
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(1, 1)).await.0.unwrap();
    assert_staged(&ready, &op);
    let mut first: Vec<String> = Vec::new();
    for asked in log.asked() { if !first.contains(&asked.path) { first.push(asked.path); } }
    let size = |path: &String| sizes[directs.iter().position(|d| d == path).unwrap()];
    assert_eq!(first.iter().map(size).collect::<Vec<_>>(), [2_000_000, 300_000, 300_000, 70_000, 100, 5]);
    assert_eq!(first[1], directs[1], "ties keep plan order");
    fs::remove_dir_all(root).unwrap();
}

/// A connection that ends partway through a pipeline: the unanswered chunks are asked again
/// once each on a new connection, the answered ones never, and the file verifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_break_mid_pipeline_asks_the_unanswered_chunks_again_once() {
    let root = root("break-pipeline");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let direct = add_file(&mut m, &mut files, &mut planned, "p.DAT", &bytes(12 * 0x10000 - 10, 3));
    let (addr, log) = pipe_server(files, Script { break_after: Some(3), ..Default::default() });
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(1, download::MAX_DEPTH)).await.0.unwrap();
    assert_staged(&ready, &op);
    let first: Vec<u32> = log.asked().iter().filter(|a| a.conn == 0).map(|a| a.offset).collect();
    assert!(first.len() > 3, "a pipeline was out when it broke: {first:?}");
    for (i, offset) in first.iter().enumerate() {
        assert_eq!(log.times(&direct, *offset), if i < 3 { 1 } else { 2 }, "chunk at {offset}");
    }
    fs::remove_dir_all(root).unwrap();
}

/// A server that answers only the first request of each write: once its probation passes, the
/// run falls back to one request per connection, charges no chunk, and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_drops_pipelined_requests_turns_pipelining_off() {
    let root = root("drops-pipeline");
    let (mut m, mut files, mut planned) = many_files(20, 30);
    add_file(&mut m, &mut files, &mut planned, "q.DAT", &bytes(10 * 0x10000, 4));
    let (addr, _log) = pipe_server(files, Script { first_only: true, ..Default::default() });
    let op = Operation::new();
    let config = download::Tuning { probation: Duration::from_millis(300), ..tuning(4, download::MAX_DEPTH) };
    let (result, stats) = pool(&addr, &root, &m, &planned, &op, config).await;
    assert_staged(&result.unwrap().0, &op);
    assert_eq!((stats.fallbacks.load(Ordering::Relaxed), stats.reconnects.load(Ordering::Relaxed)), (1, 0));
    fs::remove_dir_all(root).unwrap();
}

/// Replies out of request order: the run falls back to one request per connection and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_order_reply_turns_pipelining_off() {
    let root = root("swapped");
    let (m, files, planned) = many_files(40, 30);
    let (addr, _log) = pipe_server(files, Script { swap_once: true, ..Default::default() });
    let op = Operation::new();
    let (result, stats) = pool(&addr, &root, &m, &planned, &op, tuning(2, download::MAX_DEPTH)).await;
    assert_staged(&result.unwrap().0, &op);
    assert_eq!(stats.fallbacks.load(Ordering::Relaxed), 1);
    fs::remove_dir_all(root).unwrap();
}

/// A 40 MiB file cancelled near its middle leaves a part file whose frontier is on a chunk
/// boundary at or below what arrived; the next run asks for nothing below it, nothing twice,
/// and the file verifies with the progress exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_large_download_resumes_at_its_part_frontier() {
    let root = root("resume");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let direct = add_file(&mut m, &mut files, &mut planned, "huge.DAT", &bytes(40 << 20, 5));
    let len = m.files[0].current().direct_len;
    let (addr, log) = pipe_server(files, Script { delay: Duration::from_millis(1), ..Default::default() });
    let op = Operation::new();
    let watcher = op.clone();
    tokio::spawn(async move {
        while watcher.snapshot().done < len / 2 { tokio::time::sleep(Duration::from_millis(1)).await; }
        watcher.cancel();
    });
    let (cancelled, stats) = pool(&addr, &root, &m, &planned, &op, tuning(MAX_DOWNLOAD_WORKERS, download::MAX_DEPTH)).await;
    assert!(cancelled.is_err());
    let received = stats.bytes.load(Ordering::Relaxed);
    let part = download::part_path(&root.join(".xi-updater/staged"), &direct, len).unwrap();
    let header = fs::read(&part).unwrap()[..16].to_vec();
    let frontier = u64::from_le_bytes(header[8..].try_into().unwrap());
    assert!(frontier > 0 && frontier % 0x10000 == 0 && frontier <= received, "frontier {frontier}, received {received}");
    log.asked.lock().unwrap().clear();
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(MAX_DOWNLOAD_WORKERS, download::MAX_DEPTH)).await.0.unwrap();
    assert_staged(&ready, &op);
    let asked = log.asked();
    assert!(asked.iter().all(|a| a.offset as u64 >= frontier), "nothing below the frontier");
    assert_eq!(asked.len() as u64, (len - frontier).div_ceil(0x10000), "each remaining chunk once");
    assert!(!part.exists(), "the part is deleted once staged");
    fs::remove_dir_all(root).unwrap();
}

/// A part whose bytes below the frontier are wrong: the decoded file fails verification once,
/// the file is downloaded again from 0, verifies, and the progress ends exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lying_part_is_downloaded_again_from_the_start() {
    let root = root("lying-part");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let direct = add_file(&mut m, &mut files, &mut planned, "lie.DAT", &bytes(10 * 0x10000 - 3, 6));
    let len = m.files[0].current().direct_len;
    let staged = root.join(".xi-updater/staged");
    fs::create_dir_all(&staged).unwrap();
    let mut part = [&b"XIPART\x01\0"[..], &(4u64 * 0x10000).to_le_bytes()[..], &files[&direct][..]].concat();
    part[16 + 100] ^= 0xff;
    fs::write(download::part_path(&staged, &direct, len).unwrap(), part).unwrap();
    let (addr, log) = pipe_server(files, Script::default());
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(2, download::MAX_DEPTH)).await.0.unwrap();
    assert_staged(&ready, &op);
    assert_eq!(log.asked().len(), 6 + 10, "the rest, then everything again");
    assert_eq!(op.snapshot().total, len + len, "the second download is added to the total");
    fs::remove_dir_all(root).unwrap();
}

/// A reply that keeps trickling for longer than the idle limit is not dead: only silence is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_reply_that_keeps_arriving_is_not_cut_off() {
    let root = root("slow-reply");
    let (m, planned, files) = one_file("slow.DAT", &bytes(40_000, 7));
    let direct = m.files[0].current().direct.clone();
    let (addr, log) = pipe_server(files, Script { trickle: Some((4096, Duration::from_millis(150))), ..Default::default() });
    let op = Operation::new();
    let config = download::Tuning { idle: Duration::from_millis(500), ..tuning(1, 1) };
    let started = Instant::now();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, config).await.0.unwrap();
    assert!(started.elapsed() > Duration::from_millis(1000), "slower than the idle limit");
    assert_staged(&ready, &op);
    assert_eq!(log.times(&direct, 0), 1, "no retry");
    fs::remove_dir_all(root).unwrap();
}

/// Finishing overlaps the download: a completed file is staged while another still downloads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_file_is_staged_while_others_still_download() {
    let root = root("overlap");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    add_file(&mut m, &mut files, &mut planned, "a.DAT", &bytes(3 << 20, 8));
    let b = add_file(&mut m, &mut files, &mut planned, "b.DAT", &bytes(1 << 20, 9));
    let last = (m.files[1].current().direct_len - 1) / 0x10000 * 0x10000;
    let script = Script { hold: Some((b.clone(), last as u32, Duration::from_millis(800))), ..Default::default() };
    let (addr, log) = pipe_server(files, script);
    let staged = root.join(".xi-updater/staged");
    fs::create_dir_all(&staged).unwrap();
    let watch = staged.clone();
    let first_bin = tokio::spawn(async move {
        loop {
            let found = fs::read_dir(&watch).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "bin"));
            if found { return Instant::now(); }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(2, download::MAX_DEPTH)).await.0.unwrap();
    assert_staged(&ready, &op);
    let staged_at = first_bin.await.unwrap();
    let b_done = log.replied.lock().unwrap().iter().find(|(path, offset, _)| *path == b && *offset as u64 == last).unwrap().2;
    assert!(staged_at < b_done, "a.DAT was staged before b.DAT's last chunk arrived");
    fs::remove_dir_all(root).unwrap();
}

/// Broken connections are replaced after a backoff that doubles while nothing completes; a
/// chunk at the head of two broken connections still completes on its third.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_connections_are_replaced_after_a_doubling_backoff() {
    let root = root("backoff");
    let (m, planned, files) = one_file("b.DAT", b"backoff");
    let (addr, log) = pipe_server(files, Script { break_first: 2, ..Default::default() });
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(1, 1)).await.0.unwrap();
    assert_staged(&ready, &op);
    let conns = log.conns.lock().unwrap().clone();
    assert_eq!(conns.len(), 3);
    let slack = Duration::from_millis(30);
    let wait = |i: usize| conns[i + 1].0 - conns[i].1.unwrap();
    assert!(wait(0) + slack >= download::BREAK_BACKOFF.0 && wait(0) < download::BREAK_BACKOFF.0 * 2, "{:?}", wait(0));
    assert!(wait(1) + slack >= download::BREAK_BACKOFF.0 * 2, "{:?}", wait(1));
    fs::remove_dir_all(root).unwrap();
}

/// A delta that does not produce the manifest's file falls back to the full file, and the
/// progress total grows by the full file instead of counting the delta twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_delta_falls_back_to_the_full_file() {
    let root = root("delta-fallback");
    let (m, mut files) = update_fixture(&root);
    let history = &m.files[0];
    let patched = [&(0..200u32).map(|i| (i * 7) as u8).collect::<Vec<u8>>()[..], b"!!"].concat();
    let delta = history.current().indirect.clone().unwrap();
    files.insert(delta.path.clone(), delta_stream(200, b"??"));
    files.insert(history.current().direct.clone(), [&[1u8][..], &patched].concat());
    let planned = [Planned { index: 0, action: Action::Deltas(1), original: Some(history.versions[0].sig), keep: false }];
    let (addr, log) = pipe_server(files, Script::default());
    let op = Operation::new();
    let (ready, _) = pool(&addr, &root, &m, &planned, &op, tuning(1, 1)).await.0.unwrap();
    assert_staged(&ready, &op);
    assert_eq!(fs::read(&ready[0].1).unwrap(), patched);
    assert_eq!(log.asked().iter().map(|a| a.path.clone()).collect::<Vec<_>>(), [delta.path, history.current().direct.clone()]);
    assert_eq!(op.snapshot().total, delta.len + history.current().direct_len);
    fs::remove_dir_all(root).unwrap();
}
/// A zlib payload (flag 3) of `content`, as the server stores many files.
fn zlib(content: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(vec![3u8], flate2::Compression::fast());
    encoder.write_all(content).unwrap();
    encoder.finish().unwrap()
}

/// Thirty zlib files of uneven sizes (each ending in a short chunk) on two pipelined
/// connections, so replies for different files interleave on each: with replies coalesced
/// into one write per read, and with every reply split into 1000-byte pieces, every file is
/// assembled exactly, decodes to its size and verifies, and nothing is asked twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_zlib_files_assemble_exactly_from_split_and_coalesced_replies() {
    for (label, script) in [("coalesced", Script { coalesce: true, ..Default::default() }),
        ("split", Script { trickle: Some((1000, Duration::ZERO)), ..Default::default() })] {
        let root = root(&format!("assembly-{label}"));
        let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
        for i in 0..30usize {
            // Barely compressible (byte pairs of a hash), so most transfers span several chunks.
            let content: Vec<u8> = (0..70_000 + i * 31_337).map(|b| ((((b / 2) ^ i) as u32).wrapping_mul(2_654_435_761) >> 24) as u8).collect();
            let path = format!("z{i:02}.DAT");
            let v = version(NEW, &content, &path, 0, None);
            let payload = zlib(&content);
            let v = manifest::Version { direct_len: payload.len() as u64, ..v };
            files.insert(v.direct.clone(), payload);
            planned.push(Planned { index: i, action: Action::Direct, original: None, keep: false });
            m.files.push(FileHistory { path, optional: false, versions: vec![v] });
        }
        assert!(m.files.iter().filter(|h| h.current().direct_len > 0x10000).count() > 10, "multi-chunk transfers");
        let (addr, log) = pipe_server(files, script);
        let op = Operation::new();
        let (result, stats) = pool(&addr, &root, &m, &planned, &op, tuning(2, download::MAX_DEPTH)).await;
        let (ready, bytes) = result.unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_staged(&ready, &op);
        assert_eq!(bytes, m.files.iter().map(|h| h.current().sig.len).sum::<u64>(), "{label}");
        let asked = log.asked();
        assert!(asked.iter().any(|a| a.batch > 1), "{label}: pipelined");
        let mut unique: Vec<_> = asked.iter().map(|a| (a.path.clone(), a.offset)).collect();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), asked.len(), "{label}: nothing asked twice");
        assert_eq!((stats.fallbacks.load(Ordering::Relaxed), stats.redownloads.lock().unwrap().len()), (0, 0), "{label}");
        fs::remove_dir_all(root).unwrap();
    }
}

/// A server that once sends wrong bytes under a valid header and digest for a pipelined reply:
/// the file fails verification and is never staged from those bytes; it is downloaded again
/// from 0 one request at a time, and the stage completes with every file verified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_pipelined_reply_is_never_staged_and_the_file_is_downloaded_again() {
    let root = root("corrupt-reply");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let direct = add_file(&mut m, &mut files, &mut planned, "c.DAT", &bytes(6 * 0x10000 + 123, 10));
    let len = m.files[0].current().direct_len;
    let (addr, log) = pipe_server(files, Script { corrupt_once: true, ..Default::default() });
    let op = Operation::new();
    let (result, stats) = pool(&addr, &root, &m, &planned, &op, tuning(1, download::MAX_DEPTH)).await;
    let (ready, _) = result.unwrap();
    assert_staged(&ready, &op);
    assert_eq!((stats.fallbacks.load(Ordering::Relaxed), stats.redownloads.lock().unwrap().len()), (0, 1));
    assert_eq!(log.asked().iter().filter(|a| a.path == direct).count() as u64, 2 * len.div_ceil(0x10000), "the whole file twice");
    assert!(log.asked().iter().rev().take(len.div_ceil(0x10000) as usize).all(|a| a.batch == 1), "the second download is not pipelined");
    assert_eq!(op.snapshot().total, 2 * len);
    fs::remove_dir_all(root).unwrap();
}
/// Offline stress at live scale: 16 pipelined connections, ~300 MB of zlib files of 256 KB to
/// 16 MB, replies trickled in uneven pieces. Every file must assemble, decode and verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "offline stress, ~300 MB through loopback; run explicitly"]
async fn pipelined_pool_stress_at_live_scale() {
    let root = root("stress");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    let mut total = 0usize;
    for i in 0..80usize {
        let size = if i % 16 == 0 { 16 << 20 } else { (256 << 10) + (i * 977_123) % (4 << 20) };
        let content: Vec<u8> = (0..size).map(|b| ((((b / 2) ^ (i * 131)) as u32).wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let path = format!("stress{i:03}.DAT");
        let v = version(NEW, &content, &path, 0, None);
        let payload = zlib(&content);
        total += payload.len();
        let v = manifest::Version { direct_len: payload.len() as u64, ..v };
        files.insert(v.direct.clone(), payload);
        planned.push(Planned { index: i, action: Action::Direct, original: None, keep: false });
        m.files.push(FileHistory { path, optional: false, versions: vec![v] });
    }
    eprintln!("{} MB", total >> 20);
    let (addr, _log) = pipe_server(files, Script { trickle: Some((1460, Duration::ZERO)), ..Default::default() });
    let op = Operation::new();
    let started = Instant::now();
    let (result, stats) = pool(&addr, &root, &m, &planned, &op, download::Tuning::default()).await;
    eprintln!("{:?} in {:?}; redownloads {:?}", result.as_ref().map(|r| r.1), started.elapsed(), stats.redownloads.lock().unwrap());
    assert_staged(&result.unwrap().0, &op);
    assert!(stats.redownloads.lock().unwrap().is_empty());
    fs::remove_dir_all(root).unwrap();
}
/// A file whose fresh copy fails verification is asked again one request at a time, while the
/// other files keep their pipelines: no run-wide fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_fails_verification_is_asked_again_alone_while_others_stay_pipelined() {
    let root = root("solo-redownload");
    let (mut m, mut files, mut planned) = (Manifest::default(), HashMap::new(), Vec::new());
    // The largest goes first, so the first pipelined reply (the one corrupted) is its.
    let bad = add_file(&mut m, &mut files, &mut planned, "big.DAT", &bytes(40 * 0x10000 + 9, 11));
    for i in 0..20 { add_file(&mut m, &mut files, &mut planned, &format!("s{i}.DAT"), &bytes(3 * 0x10000 - 7, 20 + i)); }
    let (addr, log) = pipe_server(files, Script { corrupt_once: true, delay: Duration::from_millis(3), ..Default::default() });
    let op = Operation::new();
    let (result, stats) = pool(&addr, &root, &m, &planned, &op, tuning(1, download::MAX_DEPTH)).await;
    assert_staged(&result.unwrap().0, &op);
    let redownloaded = stats.redownloads.lock().unwrap().clone();
    assert_eq!((stats.fallbacks.load(Ordering::Relaxed), redownloaded.len()), (0, 1), "{redownloaded:?}");
    assert!(redownloaded[0].starts_with("big.DAT"), "{redownloaded:?}");
    let asked = log.asked();
    // Where big.DAT's second download starts: its 42nd request.
    let again = asked.iter().enumerate().filter(|(_, a)| a.path == bad).nth(41).unwrap().0;
    assert!(asked[again..].iter().filter(|a| a.path == bad).all(|a| a.batch == 1), "asked alone the second time");
    assert!(asked[again..].iter().any(|a| a.path != bad && a.batch > 1), "the other files stay pipelined after it");
    fs::remove_dir_all(root).unwrap();
}