use super::*;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

// --- A recording fake core: every used slot records (slot, args) in order --

#[derive(Default)]
struct Fake {
    calls: Vec<(usize, Vec<usize>)>,
    scripted: HashMap<usize, VecDeque<usize>>,
    open0: Vec<i32>,
    open3: Vec<i32>,
    states: VecDeque<i32>,
    mode: usize,
    linked: i32,
    header: Vec<u8>,
    records: Vec<u8>,
    connection: [i32; 8],
    checksum: u32,
    /// The launcher's pipe closes during this slot's call (this thread only).
    close_on: Option<usize>,
    /// (slot, calls left, action): the action runs during that call of the slot,
    /// as a reply-reader line would arrive between two owner frames.
    fire: Option<(usize, usize, fn())>,
}
thread_local! { static FAKE: RefCell<Fake> = RefCell::new(Fake::default()); }
thread_local! { static PAUSES: RefCell<Option<Vec<(Duration, usize)>>> = const { RefCell::new(None) }; }

// Capture only this test thread's requested sleeps; other tests retain their
// real timing. The call index locates each pause in the actual native flow.
pub(super) fn capture_pause(interval: Duration) -> bool {
    PAUSES.with(|pauses| {
        let mut pauses = pauses.borrow_mut();
        let Some(pauses) = pauses.as_mut() else { return false; };
        pauses.push((interval, raw_calls().len()));
        true
    })
}

// This thread's launcher answers to `native_viewer_check`: each owner poll
// takes one entry (None = not answered yet; an empty queue never answers).
// The call index of the first poll (the request was just sent) and of the
// answer locate the W2U wait in the native flow.
// `close_after`: the launcher's pipe closes once that many polls went unanswered.
#[derive(Default)]
struct W2u { polls: VecDeque<Option<proto::ViewerCheckResult>>, asked: Option<usize>, answered: Option<usize>, close_after: Option<usize> }
thread_local! { static W2U: RefCell<Option<W2u>> = const { RefCell::new(None) }; }
fn answer_viewer_check(polls: Vec<Option<proto::ViewerCheckResult>>) {
    W2U.with(|w| *w.borrow_mut() = Some(W2u { polls: polls.into(), ..W2u::default() }));
}
fn viewer_check_current() { answer_viewer_check(vec![Some(proto::ViewerCheckResult::Current)]); }
fn w2u_marks() -> (Option<usize>, Option<usize>) { W2U.with(|w| w.borrow().as_ref().map_or((None, None), |w| (w.asked, w.answered))) }
pub(super) fn viewer_check_answer() -> Option<Option<(proto::ViewerCheckResult, Instant)>> {
    W2U.with(|w| {
        let mut w = w.borrow_mut();
        let w = w.as_mut()?;
        let at = raw_calls().len();
        w.asked.get_or_insert(at);
        let answer = w.polls.pop_front().flatten();
        if answer.is_some() { w.answered = Some(at); }
        if let Some(left) = w.close_after.as_mut() {
            *left = left.saturating_sub(1);
            if *left == 0 { events::set_closed_for_test(true); }
        }
        Some(answer.map(|a| (a, Instant::now())))
    })
}

struct CapturePauses;
impl CapturePauses {
    fn start() -> Self { PAUSES.with(|p| *p.borrow_mut() = Some(Vec::new())); Self }
    fn take(&self) -> Vec<(Duration, usize)> { PAUSES.with(|p| std::mem::take(p.borrow_mut().as_mut().unwrap())) }
}
impl Drop for CapturePauses {
    fn drop(&mut self) { PAUSES.with(|p| *p.borrow_mut() = None); }
}

/// How many arguments each fake records (traces show these). `Api` itself
/// checks every real call against `pol_core::SERVICES`.
fn arity(slot: usize) -> usize {
    match slot {
        376 | 195 => 6,
        1502 => 4,
        824 => 3,
        825 | 826 | 854 | 856 | 852 | 858 | 868 | 892 | 1149 | 814 | 1072 | 1302 | 167 | 169 | 173 => 2,
        870 | 851 | 886 | 853 | 965 | 1527 | 1309 | 871 | 846 | 849 | 1032 | 1140 | 1175 | 969 | 288 | 331 | 375
        | 164 | 172 | 176 | 384 | 377 | 405 | 404 | 819 | 125 | 367 | 1075 | 1107 | 337 | 199 | 816 | 1305 | 330 | 1528 => 1,
        329 => 2,
        _ => 0,
    }
}

#[test]
fn every_recorded_arity_agrees_with_the_core_check_registry() {
    for slot in 0..crate::pol_core::TABLE_LEN {
        let recorded = arity(slot);
        if recorded == 0 { continue; }
        if let Some(service) = crate::pol_core::service(slot) {
            assert_eq!(service.args.map(usize::from), Some(recorded), "slot {slot}");
        }
    }
}

fn dispatch(slot: usize, a: [usize; 6]) -> usize {
    FAKE.with(|f| {
        let mut f = f.borrow_mut();
        f.calls.push((slot, a[..arity(slot)].to_vec()));
        if f.close_on == Some(slot) { events::set_closed_for_test(true); }
        if let Some((on, left, action)) = f.fire {
            if on == slot {
                if left <= 1 { f.fire = None; action(); } else { f.fire = Some((on, left - 1, action)); }
            }
        }
        if let Some(value) = f.scripted.get_mut(&slot).and_then(|q| q.pop_front()) {
            return value;
        }
        match slot {
            173 => unsafe {
                std::ptr::write_bytes(a[1] as *mut u8, 0, 0x28);
                if a[0] == 0 { *(a[1] as *mut u8) = 1; }
                0
            },
            814 => {
                f.mode = a[0];
                f.states = if a[0] == 0 { f.open0.clone() } else { f.open3.clone() }.into();
                0
            }
            815 => {
                let state = if f.states.len() > 1 { f.states.pop_front().unwrap() } else { f.states.front().copied().unwrap_or(0) };
                if state == 30 {
                    f.linked = if f.mode == 0 { 2 } else { 3 };
                }
                state as usize
            }
            838 => f.linked as usize,
            825 => unsafe {
                *(a[0] as *mut usize) = f.header.as_mut_ptr() as usize;
                *(a[1] as *mut u32) = f.header.len() as u32;
                0
            },
            826 => unsafe {
                let n = f.records.len();
                *(a[0] as *mut usize) = if n > 0 { f.records.as_mut_ptr() as usize } else { 0 };
                *(a[1] as *mut u32) = n as u32;
                usize::from(n > 0)
            },
            884 => f.checksum as usize,
            809 => 1,
            849 => f.records.as_ptr() as usize,
            835 => f.connection.as_ptr() as usize,
            891 | 817 => if slot == 891 { 1 } else { 7 },
            190 => -1i32 as usize,
            207 => 1000,
            163 => 1,
            171 => 2,
            175 => 3,
            383 => 4,
            376 => 5,
            404 => 6,
            329 => 7, // the social entry's mail list: an empty inbox
            164 | 172 | 176 | 384 | 375 | 377 | 405 | 330 => 1,
            _ => 0,
        }
    })
}

macro_rules! fake_table {
    ($($slot:literal)*) => {
        fn fake_table() -> Vec<usize> {
            let mut table = vec![0usize; 1560];
            $({
                unsafe extern "C" fn stub(a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> usize {
                    dispatch($slot, [a, b, c, d, e, f])
                }
                table[$slot] = stub as *const () as usize;
            })*
            table
        }
    };
}
fake_table!(125 126 163 164 165 166 167 169 171 172 173 175 176 190 195 196 199 206 207 288 322 323 326 327 328
    329 330 331 333 334 337 367 374 375 376 377 383 384 385 401 404 405 434 463 705 807 809 814 815 816
    817 818 819 823 824 825 826 828 835 836 838 845 846 849 851 852 853 854 856 858 868 870 871 872 884
    886 891 892 914 915 920 932 933 965 969 1032 1072 1074 1075 1103 1107 1140 1149 1175 1302 1304 1305 1309 1502 1527 1528);

fn script(slot: usize, values: &[i32]) {
    FAKE.with(|f| f.borrow_mut().scripted.entry(slot).or_default().extend(values.iter().map(|v| *v as usize)));
}
fn set_linked(linked: i32) { FAKE.with(|f| f.borrow_mut().linked = linked); }
fn raw_calls() -> Vec<(usize, Vec<usize>)> { FAKE.with(|f| f.borrow().calls.clone()) }
fn clear_calls() { FAKE.with(|f| f.borrow_mut().calls.clear()); }
fn show(v: usize) -> String {
    let i = v as i32;
    if (-0x10000..0x10000).contains(&i) { i.to_string() } else { "*".into() }
}
/// The recorded calls as `slot(args)`, pointers shown as `*`.
fn trace(skip: &[usize]) -> Vec<String> {
    // The handle inventory is read-only; its record-copy calls are covered by
    // native_handles and do not participate in the authentication request order.
    raw_calls().iter().filter(|(s, _)| *s != 173 && !skip.contains(s))
        .map(|(s, a)| format!("{s}({})", a.iter().map(|v| show(*v)).collect::<Vec<_>>().join(",")))
        .collect()
}
fn until(calls: &[String], last: &str) -> Vec<String> {
    let end = calls.iter().position(|c| c.starts_with(last)).unwrap_or_else(|| panic!("{last} missing from {calls:?}"));
    calls[..=end].to_vec()
}

/// A fake Viewer root with a stock-shaped usr\all\inet_w.bin, and a fresh fake core.
fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!("crystal-pol-parity-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir_all(root.join("usr/all")).unwrap();
    let mut file = vec![0u8; native_profile::HEADER_LEN + native_profile::RECORD_LEN];
    file[..41].copy_from_slice(b"PlayOnline NetWork Setting File Ver 1.00\0");
    for (offset, value) in [(0x40, 0xe4i32), (0x44, 0x9ac), (0x48, 0x1234), (0x4c, !0x1234), (0x50, 1), (0x54, 0), (0x58, 0), (0x5c, -1), (0x60, 1), (0x64, 0)] {
        file[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    let record = native_profile::HEADER_LEN;
    file[record + 8..record + 8 + 14].copy_from_slice(b"encoded-name-1");
    file[record + 0x10c..record + 0x110].copy_from_slice(&1i32.to_le_bytes());
    fs::write(root.join("usr/all/inet_w.bin"), &file).unwrap();
    FAKE.with(|f| {
        *f.borrow_mut() = Fake {
            open0: vec![0, 17, 30],
            open3: vec![18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 40, 41, 29, 30],
            header: vec![0; native_profile::HEADER_LEN],
            records: vec![0; native_profile::RECORD_LEN],
            connection: [1, 0, 0, 0, 0, 0, 0, 0],
            checksum: 0x1234,
            ..Fake::default()
        }
    });
    root
}

fn session(viewer: &Path) -> NativeSession {
    NativeSession::new(Api::test_table(&fake_table()), viewer, 1, 10)
}

fn credentials() -> NativeCredentials {
    NativeCredentials {
        pol_id: *b"ABCD1234\0",
        pol_password: *b"fake password\0\0\0",
        se_id: *b"synthetic\0\0\0\0\0\0\0\0",
        se_secret: [0x51; 20],
        otp: Some(*b"123456"),
        profile_key: "a".repeat(40),
        handle_names: String::new(),
    }
}

/// Leave the session without a teardown so later assertions see only their calls.
fn forget(mut session: NativeSession) {
    session.signed_in = false;
    session.home = false;
    session.registered = false;
    session.files = false;
    drop(session);
}

/// Tests touching the global command state run one at a time.
static CONTROL_TEST: Mutex<()> = Mutex::new(());

// --- Sign-in --------------------------------------------------------------

// Startup polls step at the Viewer frame like everything else.
#[test]
fn default_build_steps_sign_in_mail_presence_return_and_idle_at_the_viewer_frame() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = c"C:\\PlayOnlineViewer\\";
    let key = c"SOFTWARE\\PlayOnlineUS";
    s.bootstrap_core(root, key).unwrap();
    script(891, &[native_profile::PROFILE_NOT_FOUND]);
    viewer_check_current();
    // Exercise a pending poll in every startup request stage, including mail.
    for poll in [375, 164, 330, 377] { script(poll, &[0, 1]); }
    script(190, &[0]); // actually publish presence
    let pauses = CapturePauses::start();
    s.sign_in_core(50, &credentials()).unwrap();
    let calls = raw_calls();
    let first = |slot| calls.iter().position(|(s, _)| *s == slot).unwrap();
    let stages = [first(814), first(374), first(163), first(329), first(376)];
    assert!(stages.windows(2).all(|p| p[0] < p[1]), "open, status, lists, mail, presence stay ordered");
    let startup = pauses.take();
    assert_eq!(STARTUP_FRAME, FRAME);
    for (i, &start) in stages.iter().enumerate() {
        let end = stages.get(i + 1).copied().unwrap_or(calls.len() + 1);
        let sleeps: Vec<_> = startup.iter().filter(|(_, at)| *at > start && *at <= end).map(|(d, _)| *d).collect();
        assert!(!sleeps.is_empty(), "stage {i} must exercise a pending poll");
        assert!(sleeps.iter().all(|d| *d == FRAME), "stage {i}: {sleeps:?}");
    }
    // The same requests after a game, and the idle pump, stay at 20 ms.
    // Mail timers keep the stock cold period at the fixed FRAME (08 §2).
    clear_calls();
    for poll in [164, 330, 377] { script(poll, &[0, 1]); }
    script(190, &[0]);
    s.return_from_game().unwrap();
    assert!(owner_frame(&s.api, &mut s.pushes));
    let ordinary = pauses.take();
    assert!(ordinary.len() >= 4);
    assert!(ordinary.iter().all(|(d, _)| *d == Duration::from_millis(20)));
    assert_eq!(FRAME * native_jobs::mail_frames(), Duration::from_millis(112_500));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_in_drives_the_core_in_the_stock_viewer_order() {
    // The 815 count below counts owner frames: keep the floor frame-bound.
    let _pauses = CapturePauses::start();
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = c"C:\\PlayOnlineViewer\\";
    let key = c"SOFTWARE\\PlayOnlineUS";
    s.bootstrap_core(root, key).unwrap();
    script(891, &[native_profile::PROFILE_NOT_FOUND]);
    viewer_check_current();
    let events = events::capture(|| s.sign_in_core(50, &credentials()).unwrap());
    // W2U between the Opens; the main menu after the (skipped) publish.
    assert_eq!(events.iter().filter(|e| !e.contains("native_auth_stage")).collect::<Vec<_>>(), [
        "{\"type\":\"native_viewer_check\"}\n", "{\"type\":\"authenticated\"}\n",
        "{\"type\":\"native_main_menu\",\"code\":0,\"returned\":false}\n",
    ]);
    let calls = trace(&[818, 815, 838, 1103]);
    assert_eq!(until(&calls, "384("), [
        // app.dll startup (01 §2.1): no 420, no lone 815.
        "1528(1)", "125(*)", "367(*)", "870(-1)", "932()", "914()", "965(1)", "1527(0)", "1309(*)",
        "871(*)", "819(0)", "824(*,*,*)", "825(*,*)", "826(*,*)", "884()", "1149(*,*)",
        // Member select 101a425f; its 891 fails like stock.
        "870(50)", "854(*,*)", "851(*)", "856(*,*)", "852(*,0)", "858(*,*)", "886(*)", "868(*,*)", "853(*)", "891()",
        // Connect screen 10199d02: the selected profile, 891 == 1, LAN kind.
        "809()", "849(0)", "886(*)", "891()", "835()",
        // Open(0) to the network level, SE 1502, then Open(1) member login (cold stock probe).
        "835()", "814(0,*)", "1502(2,*,*,*)", "835()", "814(1,*)",
        // Post sign-in: delivery on, 871 again, the status load alone, then four lists together.
        "1032(1)", "871(*)", "374()", "375(0)", "163()", "171()", "175()", "383()", "164(1)", "172(2)", "176(3)", "384(4)",
    ]);
    let raw = raw_calls();
    // Main menu entry after the lists: the mail list, then the Viewer presence
    // decision (already the Viewer here, so nothing is published).
    let tail = &calls[calls.iter().position(|c| c == "384(4)").unwrap() + 1..];
    assert_eq!(&tail[..5], ["1304()", "329(*,200)", "330(7)", "190()", "207()"]);
    // A6: no status request of our own and no 420 at sign-in.
    assert!(!raw.iter().any(|(s, _)| [195, 196, 376, 377, 420].contains(s)));
    // A4/A9: Open(0) then Open(1); each 815 follows that frame's 818.
    let steps: Vec<_> = raw.iter().enumerate().filter(|(_, (s, _))| *s == 815).map(|(i, _)| i).collect();
    assert_eq!(steps.len(), 18);
    assert!(steps.iter().all(|&i| raw[i - 1].0 == 818));
    let opens: Vec<_> = raw.iter().enumerate().filter(|(_, (s, _))| *s == 814).map(|(i, _)| i).collect();
    assert_eq!(opens.len(), 2);
    assert!(opens[0] < steps[0] && *steps.last().unwrap() < raw.iter().position(|(s, _)| *s == 374).unwrap());
    assert!(raw[..opens[0]].iter().all(|(s, _)| *s != 815), "no 815 before the Open");
    assert!(s.signed_in && s.home && s.registered);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_in_failure_is_a_negative_state_after_transitional_ones() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = c"C:\\v\\";
    s.bootstrap_core(root, root).unwrap();
    FAKE.with(|f| f.borrow_mut().open3 = vec![18, 31, 32, 33, 39, -1036]);
    script(823, &[-1036]);
    viewer_check_current();
    let error = s.sign_in_core(50, &credentials()).unwrap_err();
    assert!(error.contains("state -1036") && error.contains("POL result -1036"), "{error}");
    assert!(!s.signed_in);
    assert!(!raw_calls().iter().any(|(s, _)| *s == 374), "no loads after a failed login");
    assert!(s.network_opened);
    clear_calls();
    drop(s);
    assert_eq!(&trace(&[])[..5], ["816(1)", "817()", "463()", "434()", "1502(0,0,0,0)"]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn network_open_failure_does_not_issue_member_sign_out() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let root = c"C:\\v\\";
    s.bootstrap_core(root, root).unwrap();
    FAKE.with(|f| f.borrow_mut().open0 = vec![17, 31, 32, 39, -1025]);
    assert!(s.sign_in_core(50, &credentials()).is_err());
    assert!(!s.signed_in && !s.network_opened);
    clear_calls();
    drop(s);
    assert!(!raw_calls().iter().any(|(slot, _)| [816, 817, 463, 434].contains(slot)));
    fs::remove_dir_all(viewer).unwrap();
}

fn bootstrapped(viewer: &Path) -> NativeSession {
    let mut s = session(viewer);
    let root = c"C:\\v\\";
    s.bootstrap_core(root, root).unwrap();
    script(891, &[native_profile::PROFILE_NOT_FOUND]);
    s
}

#[test]
fn startup_sign_out_before_login_sends_no_credentials() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    with_startup_state(|state| state.store(SIGNING_IN, Ordering::Release));
    clear_calls();
    let replies = events::capture(request_sign_out);
    assert!(replies.iter().any(|reply| reply.contains("\"action\":\"sign_out\",\"code\":0")));
    assert!(s.sign_in_core(50, &credentials()).unwrap_err().contains("cancelled"));
    assert!(raw_calls().is_empty());
    // Nothing was opened, so nothing closes; the requested sign-out still completes.
    assert_eq!(events::capture(|| drop(s)), ["{\"type\":\"native_sign_out_complete\",\"code\":0}\n"]);
    assert!(!startup_cancelled());
    assert!(!raw_calls().iter().any(|(slot, _)| [814, 1502, 816].contains(slot)));
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn startup_sign_out_cancels_each_open_with_the_native_cancel_operation() {
    for mode in [0, 1] {
        let viewer = fixture();
        let mut s = bootstrapped(&viewer);
        with_startup_state(|state| state.store(SIGNING_IN, Ordering::Release));
        viewer_check_current();
        let _pauses = CapturePauses::start();
        FAKE.with(|f| {
            let mut f = f.borrow_mut();
            f.open0 = if mode == 0 { vec![17, -1] } else { vec![30] };
            f.open3 = vec![17, -1];
            f.fire = Some((815, if mode == 0 { 1 } else { 2 }, request_sign_out));
        });
        clear_calls();
        let error = s.sign_in_core(50, &credentials()).unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert_eq!(raw_calls().iter().filter(|(slot, _)| *slot == 807).count(), 1);
        assert!(!raw_calls().iter().any(|(slot, _)| *slot == 374));
        if mode == 0 { assert!(!raw_calls().iter().any(|(slot, _)| *slot == 1502)); }
        drop(s);
        assert!(!startup_cancelled());
        assert_eq!(raw_calls().iter().filter(|(slot, _)| *slot == 816).count(), mode);
        fs::remove_dir_all(viewer).unwrap();
    }
}

#[test]
fn startup_sign_out_at_member_success_uses_normal_logout_before_any_status_load() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    s.set_viewer_gaps(true);
    with_startup_state(|state| state.store(SIGNING_IN, Ordering::Release));
    viewer_check_current();
    let _pauses = CapturePauses::start();
    FAKE.with(|f| {
        let mut f = f.borrow_mut();
        f.open0 = vec![30];
        f.open3 = vec![30];
        f.fire = Some((815, 2, request_sign_out));
    });
    clear_calls();
    assert!(s.sign_in_core(50, &credentials()).is_err());
    assert!(s.signed_in);
    assert!(!raw_calls().iter().any(|(slot, _)| [374, 807].contains(slot)));
    let replies = events::capture(|| drop(s));
    assert!(replies.iter().any(|reply| reply.contains("\"native_sign_out_complete\",\"code\":0")));
    assert_eq!(raw_calls().iter().filter(|(slot, _)| *slot == 816).count(), 1);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn startup_cancellation_never_claims_a_running_game_or_a_queued_play() {
    let state = AtomicU8::new(SIGNING_IN);
    assert_eq!(cancel_startup(&state, true), Some(-256));
    assert_eq!(state.load(Ordering::Acquire), SIGNING_IN);
    assert_eq!(cancel_startup(&state, false), Some(0));
    assert_eq!(cancel_startup(&state, false), Some(0));
    state.store(STARTUP_FINISHED, Ordering::Release);
    assert_eq!(cancel_startup(&state, false), None);
    let control = AtomicU8::new(GAME);
    assert_eq!(accept_control(&control, &AtomicBool::new(false), SIGN_OUT), -256);
}
fn position(calls: &[(usize, Vec<usize>)], slot: usize, first: Option<usize>) -> Option<usize> {
    calls.iter().position(|(s, a)| *s == slot && first.is_none_or(|v| a.first() == Some(&v)))
}

#[test]
fn viewer_check_runs_once_between_the_opens_and_member_login_waits_for_its_answer_and_floor() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    answer_viewer_check(vec![None, None, None, Some(proto::ViewerCheckResult::Current)]);
    let pauses = CapturePauses::start();
    let events = events::capture(|| s.sign_in_core(50, &credentials()).unwrap());
    assert_eq!(events.iter().filter(|e| e.contains("native_viewer_check")).count(), 1);
    let calls = raw_calls();
    let (asked, answered) = w2u_marks();
    let (asked, answered) = (asked.unwrap(), answered.unwrap());
    let open0 = position(&calls, 814, Some(0)).unwrap();
    let se = position(&calls, 1502, None).unwrap();
    let open1 = position(&calls, 814, Some(1)).unwrap();
    assert!(open0 < asked && asked < answered && answered < se && se < open1);
    assert_eq!(calls[asked - 2].0, 815, "asked right after Open(0)'s last step");
    // While waiting: owner frames only (818, pushes, 1103); no 815 and no request.
    assert!(calls[asked..se].iter().all(|(slot, _)| [818, 838, 1103].contains(slot)), "{:?}", &calls[asked..se]);
    assert_eq!(calls[asked..answered].iter().filter(|(s, _)| *s == 818).count(), 3, "one 818 per unanswered poll");
    // The floor: ≥ 40 ms of frames after the answer, before 1502 and Open(1).
    let floor: Vec<_> = pauses.take().into_iter().filter(|(_, at)| *at > answered && *at <= se).map(|(d, _)| d).collect();
    assert_eq!(floor, [FRAME; 2]);
    assert!(FRAME * 2 >= VIEWER_CHECK_FLOOR);
    // Never twice for one sign-in.
    let again = events::capture(|| assert!(s.connect_viewer_check().is_err()));
    assert!(again.is_empty());
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn an_unanswered_viewer_check_still_signs_in_after_the_floor() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    answer_viewer_check(vec![None, Some(proto::ViewerCheckResult::Unreachable("No reply.".into()))]);
    let pauses = CapturePauses::start();
    let events = events::capture(|| s.sign_in_core(50, &credentials()).unwrap());
    assert_eq!(events.iter().filter(|e| e.contains("native_viewer_check")).count(), 1);
    let calls = raw_calls();
    let answered = w2u_marks().1.unwrap();
    let (se, open1) = (position(&calls, 1502, None).unwrap(), position(&calls, 814, Some(1)).unwrap());
    assert!(answered < se && se < open1, "member login follows the unanswered check");
    let floor: Vec<_> = pauses.take().into_iter().filter(|(_, at)| *at > answered && *at <= se).map(|(d, _)| d).collect();
    assert_eq!(floor, [FRAME; 2], "the same floor as an answered check");
    assert!(s.signed_in);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn every_viewer_check_failure_stops_before_member_login_with_the_existing_teardown() {
    use proto::ViewerCheckResult::*;
    let cases: [(Vec<Option<proto::ViewerCheckResult>>, &[i32], &str, usize); 6] = [
        (vec![Some(Outdated("Update PlayOnline first.".into()))], &[], "Update PlayOnline first.", 0),
        (vec![Some(Outdated(String::new()))], &[], "PlayOnline Viewer needs an update before signing in.", 0),
        (vec![None, Some(Failed("No Viewer install.".into()))], &[], "No Viewer install.", 1),
        // Timeout: VIEWER_CHECK_LIMIT (35 s, 07d §3) of frames, never answered.
        (vec![], &[], VIEWER_CHECK_SILENT, 1750),
        // The launcher's capped retry found no network (07d §2.3): stops before 1502/Open(1) with its text.
        (vec![None, Some(NetworkNotReady("The network is not ready (pc001.pol.com: No such host is known. (11001)).".into()))], &[],
            "pc001.pol.com: No such host is known. (11001)", 1),
        // The owner stops while waiting (latched pump error; a closed pipe takes the same exit).
        (vec![None, None], &[-1025, -1025], "POL result -1025", 1),
    ];
    for (answers, pump_errors, error, frames) in cases {
        let viewer = fixture();
        let mut s = bootstrapped(&viewer);
        answer_viewer_check(answers);
        script(1103, pump_errors);
        let pauses = CapturePauses::start();
        let events = events::capture(|| {
            let failed = s.sign_in_core(50, &credentials()).unwrap_err();
            assert!(failed.contains(error), "{failed} / {error}");
        });
        assert_eq!(events.iter().filter(|e| e.contains("native_viewer_check")).count(), 1);
        assert!(!events.iter().any(|e| e.contains("authenticated")));
        let calls = raw_calls();
        assert!(position(&calls, 1502, None).is_none() && position(&calls, 814, Some(1)).is_none(), "{error}: no member login");
        let asked = w2u_marks().0.unwrap();
        assert_eq!(calls[asked..].iter().filter(|(s, _)| *s == 818).count(), frames, "{error}");
        assert!(pauses.take().iter().filter(|(_, at)| *at >= asked).all(|(d, _)| *d == FRAME));
        assert!(s.network_opened && !s.signed_in);
        clear_calls();
        drop(s);
        // The same close as a failed member login (816 at the network level).
        assert_eq!(&trace(&[])[..5], ["816(1)", "817()", "463()", "434()", "1502(0,0,0,0)"]);
        fs::remove_dir_all(viewer).unwrap();
    }
}

#[test]
fn viewer_gaps_restore_the_two_viewer_pauses_as_owner_frames_only_when_asked() {
    for gaps in [false, true] {
        let viewer = fixture();
        let mut s = bootstrapped(&viewer);
        viewer_check_current();
        s.set_viewer_gaps(gaps);
        script(190, &[0]); // publish presence
        let pauses = CapturePauses::start();
        s.sign_in_core(50, &credentials()).unwrap();
        let calls = raw_calls();
        let frames = |from: usize, to: usize| calls[from..to].iter().filter(|(s, _)| *s == 818).count();
        // Member login complete (Open(1), then 1032(1)) → the status load 374.
        let open1 = position(&calls, 814, Some(1)).unwrap();
        let delivery = open1 + position(&calls[open1..], 1032, Some(1)).unwrap();
        let status = position(&calls, 374, None).unwrap();
        // Mail-list response (its last 330) → the publish decision (190).
        let mail = calls.iter().rposition(|(s, _)| *s == 330).unwrap();
        let publish = position(&calls, 190, None).unwrap();
        assert!(delivery < status && mail < publish);
        if gaps {
            // The Viewer's frame counts (08 §4 item 5), as owner frames.
            assert_eq!((OP7_TO_STATUS_FRAMES, MAIL_TO_PUBLISH_FRAMES), (34, 51));
            assert_eq!(frames(delivery, status), 34);
            assert_eq!(frames(mail, publish), 51);
            let entry = delivery + position(&calls[delivery..], 871, None).unwrap();
            assert_eq!(frames(entry, status), 0, "the pause, then 871 and 374");
        } else {
            assert_eq!(frames(delivery, status), 0);
            assert_eq!(frames(mail, publish), 0);
        }
        let paused = pauses.take();
        let in_gap = |&&(_, at): &&(Duration, usize)| (delivery..status).contains(&at) || (mail..publish).contains(&at);
        assert!(paused.iter().filter(in_gap).all(|(d, _)| *d == FRAME));
        forget(s);
        fs::remove_dir_all(viewer).unwrap();
    }
}

fn queue_play() { note_play_pending(); }
fn request_play() { TEST_PLAY.with(|p| p.set(true)); }

/// 07d §4.3: a Play queued (`native_play_pending`) or requested during a
/// pre-sign-in ends the not-yet-elapsed gaps at the next frame; a cold Play
/// has none. Nothing else changes: the same requests in the same order.
#[test]
fn a_queued_or_requested_play_ends_the_viewer_gaps_at_the_next_frame() {
    // The owner frames before member login are the same in every run. Frames
    // take no real sleep here, so a loaded machine cannot let the wall clock
    // end the Viewer-check floor a frame early in one run but not another.
    let _pauses = CapturePauses::start();
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    viewer_check_current();
    s.sign_in_core(50, &credentials()).unwrap();
    let calls = raw_calls();
    let before_gap = calls[..position(&calls, 1032, Some(1)).unwrap()].iter().filter(|(s, _)| *s == 818).count();
    let reference = trace(&[818, 838, 1103]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
    // (action, its owner frame inside the first gap, 0 = before the gap starts)
    for (action, at) in [(queue_play as fn(), 10usize), (request_play as fn(), 10), (queue_play as fn(), 0)] {
        let viewer = fixture();
        let mut s = bootstrapped(&viewer);
        viewer_check_current();
        s.set_viewer_gaps(true);
        if at == 0 {
            FAKE.with(|f| f.borrow_mut().fire = Some((1032, 1, action)));
        } else {
            FAKE.with(|f| f.borrow_mut().fire = Some((818, before_gap + at, action)));
        }
        s.sign_in_core(50, &credentials()).unwrap();
        let calls = raw_calls();
        let frames = |from: usize, to: usize| calls[from..to].iter().filter(|(s, _)| *s == 818).count();
        let delivery = position(&calls, 1032, Some(1)).unwrap();
        let status = position(&calls, 374, None).unwrap();
        let mail = calls.iter().rposition(|(s, _)| *s == 330).unwrap();
        let publish = position(&calls, 190, None).unwrap();
        assert_eq!(frames(delivery, status), at, "the first gap stops at the frame the Play arrived on");
        assert_eq!(frames(mail, publish), 0, "the second gap is skipped");
        assert_eq!(trace(&[818, 838, 1103]), reference, "the same requests as without gaps");
        TEST_PLAY_PENDING.with(|p| p.set(false));
        drop(TestPlay);
        forget(s);
        fs::remove_dir_all(viewer).unwrap();
    }
}

/// 07d §3: the loader waits 35 s, longer than the launcher's 32.115 s worst case (a late
/// fresh lookup, then a whole exchange).
#[test]
fn a_viewer_check_answered_after_34_s_of_frames_still_signs_in() {
    assert!(VIEWER_CHECK_LIMIT > Duration::from_millis(32_115));
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    let mut answers = vec![None; 1700];
    answers.push(Some(proto::ViewerCheckResult::Current));
    answer_viewer_check(answers);
    let _pauses = CapturePauses::start();
    s.sign_in_core(50, &credentials()).unwrap();
    let calls = raw_calls();
    let (asked, answered) = w2u_marks();
    assert_eq!(calls[asked.unwrap()..answered.unwrap()].iter().filter(|(s, _)| *s == 818).count(), 1700);
    assert!(position(&calls, 814, Some(1)).is_some() && s.signed_in);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

/// 07d §4.7: start and end lines with poll and attempt counts for every
/// sign-in request; no loss note on a clean run.
#[test]
fn every_sign_in_request_logs_its_start_end_polls_and_attempts() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    viewer_check_current();
    for poll in [375, 164, 330, 377] { script(poll, &[0, 1]); }
    script(171, &[SLOTS_FULL]); // handles: one full-table reissue
    script(190, &[0]); // publish presence
    let _pauses = CapturePauses::start();
    let lines = crate::log::capture(|| s.sign_in_core(50, &credentials()).unwrap());
    for (name, result, polls, attempts) in [("open-network", 30, 3, 1), ("open-member", 30, 15, 1), ("status", 1, 2, 1),
        ("friends", 1, 2, 1), ("handles", 1, 1, 2), ("characters", 1, 1, 1), ("groups", 1, 1, 1), ("mail-list", 1, 2, 1), ("presence", 1, 2, 1)] {
        assert_eq!(lines.iter().filter(|l| l.starts_with(&format!("native POL request {name} start at +"))).count(), 1, "{name}: {lines:#?}");
        let end: Vec<_> = lines.iter().filter(|l| l.starts_with(&format!("native POL request {name} end "))).collect();
        assert_eq!(end.len(), 1, "{name}");
        assert!(end[0].starts_with(&format!("native POL request {name} end {result} after ")), "{}", end[0]);
        assert!(end[0].ends_with(&format!("s: {polls} polls, {attempts} attempt(s)")), "{}", end[0]);
    }
    assert!(!lines.iter().any(|l| l.starts_with("possible packet loss")), "{lines:#?}");
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- The parked host (07d §1.3) ----------------------------------------------

/// Parked: no polcore entry call at all (no 870/871/814/815/818/853/868) and no
/// event, however long the wait; then credentials give exactly the cold
/// sequence (bootstrap, then the sign-in `sign_in_drives_the_core_in_the_stock_viewer_order` pins).
#[test]
fn a_parked_host_calls_nothing_until_its_credentials_then_signs_in_exactly_like_a_cold_host() {
    // The cold and parked traces include every owner frame: no real sleep, so
    // the wall clock cannot end the Viewer-check floor a frame later in one run.
    let _pauses = CapturePauses::start();
    let root = c"C:\\PlayOnlineViewer\\";
    let key = c"SOFTWARE\\PlayOnlineUS";
    let sign_in = |viewer: &Path| {
        let mut s = session(viewer);
        s.bootstrap_core(root, key).unwrap();
        script(891, &[native_profile::PROFILE_NOT_FOUND]);
        viewer_check_current();
        let events = events::capture(|| s.sign_in_core(50, &credentials()).unwrap());
        forget(s);
        (trace(&[]), events)
    };
    let viewer = fixture();
    let cold = sign_in(&viewer);
    fs::remove_dir_all(viewer).unwrap();

    for delay in [1usize, 25] {
        let viewer = fixture();
        let (tx, rx) = mpsc::channel();
        let mut tx = Some(tx);
        let mut pumps = 0;
        let events = events::capture(|| {
            let play = crate::park::wait_for_play(&rx, Duration::from_millis(1), &mut || {
                assert!(raw_calls().is_empty(), "nothing reaches polcore while parked");
                pumps += 1;
                if pumps == delay { tx.take().unwrap().send(credentials()).unwrap(); }
            });
            assert!(play.is_some());
        });
        assert!(events.is_empty() && raw_calls().is_empty(), "no event and no call while parked");
        assert_eq!(pumps, delay);
        assert_eq!(sign_in(&viewer), cold, "after {delay} pump(s): the cold calls and events");
        fs::remove_dir_all(viewer).unwrap();
    }

    // The launcher closes the pipe while parked: the game thread ends with no call.
    let viewer = fixture();
    let (tx, rx) = mpsc::channel::<NativeCredentials>();
    drop(tx);
    let events = events::capture(|| assert!(crate::park::wait_for_play(&rx, Duration::from_millis(1), &mut || {}).is_none()));
    assert!(events.is_empty() && raw_calls().is_empty());
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn a_launcher_gone_during_the_viewer_check_stops_before_member_login() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    answer_viewer_check(vec![None, None, Some(proto::ViewerCheckResult::Current)]);
    W2U.with(|w| w.borrow_mut().as_mut().unwrap().close_after = Some(2));
    let events = events::capture(|| assert_eq!(s.sign_in_core(50, &credentials()).unwrap_err(), VIEWER_CHECK_SILENT));
    events::set_closed_for_test(false);
    assert_eq!(events.iter().filter(|e| e.contains("native_viewer_check")).count(), 1);
    assert!(!events.iter().any(|e| e.contains("authenticated")));
    let calls = raw_calls();
    assert!(position(&calls, 1502, None).is_none() && position(&calls, 814, Some(1)).is_none(), "no member login");
    assert_eq!(w2u_marks().1, None, "the answer never arrived");
    assert!(s.network_opened && !s.signed_in);
    clear_calls();
    drop(s);
    assert_eq!(&trace(&[])[..5], ["816(1)", "817()", "463()", "434()", "1502(0,0,0,0)"]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn a_launcher_gone_during_a_viewer_gap_starts_neither_the_next_request_nor_a_cancel() {
    // (the pipe closes during this call, the request that gap precedes)
    for (close_on, next) in [(1032, 374), (330, 376)] {
        let viewer = fixture();
        let mut s = bootstrapped(&viewer);
        viewer_check_current();
        s.set_viewer_gaps(true);
        script(190, &[0]); // presence would publish
        FAKE.with(|f| f.borrow_mut().close_on = Some(close_on));
        let error = s.sign_in_core(50, &credentials()).unwrap_err();
        events::set_closed_for_test(false);
        assert!(error.contains("closed the session"), "{close_on}: {error}");
        let calls = raw_calls();
        let closed = calls.iter().rposition(|(slot, _)| *slot == close_on).unwrap();
        assert!(position(&calls, next, None).is_none(), "{next} never starts");
        assert!(!calls.iter().any(|(slot, _)| [337, 328, 199].contains(slot)), "nothing to cancel: {:?}", trace(&[]));
        assert!(calls[closed + 1..].iter().all(|(slot, _)| [818, 838, 1103].contains(slot)), "owner frames only: {:?}", &calls[closed + 1..]);
        assert!(s.signed_in, "the signed-in teardown follows");
        forget(s);
        fs::remove_dir_all(viewer).unwrap();
    }
}

/// Without Viewer gaps (a cold or batch-prepared sign-in) a stopping owner
/// still starts no status load (374) or presence (376) only to cancel it.
#[test]
fn a_launcher_gone_without_viewer_gaps_starts_neither_the_next_request_nor_a_cancel() {
    for (close_on, next) in [(1032, 374), (330, 376)] {
        let viewer = fixture();
        let mut s = bootstrapped(&viewer);
        viewer_check_current();
        script(190, &[0]); // presence would publish
        FAKE.with(|f| f.borrow_mut().close_on = Some(close_on));
        let error = s.sign_in_core(50, &credentials()).unwrap_err();
        events::set_closed_for_test(false);
        assert!(error.contains("closed the session"), "{close_on}: {error}");
        let calls = raw_calls();
        assert!(position(&calls, next, None).is_none(), "{next} never starts");
        assert!(!calls.iter().any(|(slot, _)| [337, 328, 199].contains(slot)), "nothing to cancel: {:?}", trace(&[]));
        assert!(s.signed_in, "the signed-in teardown follows");
        forget(s);
        fs::remove_dir_all(viewer).unwrap();
    }
}

#[test]
fn main_menu_follows_the_publish_with_its_code_on_sign_in_and_on_return() {
    let viewer = fixture();
    let mut s = bootstrapped(&viewer);
    viewer_check_current();
    script(190, &[0]); // publish: 376 ticket 5, 377 completes with 1
    let events = events::capture(|| s.sign_in_core(50, &credentials()).unwrap());
    let events: Vec<_> = events.into_iter().filter(|e| !e.contains("native_auth_stage")).collect();
    assert_eq!(events[1..], ["{\"type\":\"authenticated\"}\n", "{\"type\":\"native_main_menu\",\"code\":1,\"returned\":false}\n"]);
    // Return: published again (a character was selected) → the 377 code.
    script(190, &[0]);
    script(377, &[-5000]);
    assert_eq!(events::capture(|| assert!(s.return_from_game().unwrap())), ["{\"type\":\"native_main_menu\",\"code\":-5000,\"returned\":true}\n"]);
    // Already the Viewer: nothing published, still the main menu.
    assert_eq!(events::capture(|| assert!(s.return_from_game().unwrap())), ["{\"type\":\"native_main_menu\",\"code\":0,\"returned\":true}\n"]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn invalid_or_missing_profile_files_get_the_viewer_default_in_memory_only() {
    let viewer = fixture();
    let s = session(&viewer);
    s.api.call(824, &[1, 2, 3]);
    let path = viewer.join("usr/all/inet_w.bin");
    let stock = fs::read(&path).unwrap();
    clear_calls();
    FAKE.with(|f| f.borrow_mut().checksum = 0x9999); // 884 mismatch
    let (_, from_file) = native_profile::load(&s.api, Some(&stock)).unwrap();
    assert!(!from_file);
    assert_eq!(trace(&[]), ["825(*,*)", "826(*,*)", "884()", "845()", "858(*,*)", "892(*,*)", "892(*,*)", "846(*)"]);
    clear_calls();
    let (_, from_file) = native_profile::load(&s.api, None).unwrap();
    assert!(!from_file);
    assert_eq!(trace(&[]), ["825(*,*)", "845()", "858(*,*)", "892(*,*)", "892(*,*)", "846(*)"]);
    clear_calls();
    let (_, from_file) = native_profile::load(&s.api, Some(&stock[..stock.len() - 1])).unwrap();
    assert!(!from_file, "a truncated file is never partly used");
    assert_eq!(fs::read(&path).unwrap(), stock, "the Viewer's file is never written");
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn malformed_profile_lists_never_reach_native_allocation_or_record_walks() {
    let viewer = fixture();
    let s = session(&viewer);
    let stock = fs::read(viewer.join("usr/all/inet_w.bin")).unwrap();
    for (offset, value) in [(0x40, u32::MAX), (0x44, u32::MAX), (0x50, 2), (0x54, 1)] {
        let mut invalid = stock.clone();
        invalid[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        clear_calls();
        assert!(!native_profile::load(&s.api, Some(&invalid)).unwrap().1);
        assert!(!raw_calls().iter().any(|(slot, _)| [826, 884, 849].contains(slot)), "invalid field {offset:#x} must stay outside the native core");
    }
    let mut invalid = stock.clone();
    invalid.resize(native_profile::HEADER_LEN + 2 * native_profile::RECORD_LEN, 0);
    for offset in [0x50, 0x60] { invalid[offset..offset + 4].copy_from_slice(&2u32.to_le_bytes()); }
    for next in [u32::MAX, 2, 0] {
        invalid[native_profile::HEADER_LEN + 4..native_profile::HEADER_LEN + 8].copy_from_slice(&next.to_le_bytes());
        clear_calls();
        assert!(!native_profile::load(&s.api, Some(&invalid)).unwrap().1);
        assert!(!raw_calls().iter().any(|(slot, _)| [826, 884, 849].contains(slot)), "an invalid link must not reach native pointer traversal");
    }
    invalid[native_profile::HEADER_LEN + 4..native_profile::HEADER_LEN + 8].copy_from_slice(&1u32.to_le_bytes());
    FAKE.with(|f| f.borrow_mut().records.resize(2 * native_profile::RECORD_LEN, 0));
    assert!(native_profile::load(&s.api, Some(&invalid)).unwrap().1, "a bounded two-record list remains supported");
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- Game hand-off and return ---------------------------------------------

#[test]
fn game_start_mirrors_the_viewer_exit() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    s.registered = true;
    s.attach_game();
    assert_eq!(trace(&[]), ["1305(0)", "1032(1)", "1140(0)", "1175(1)", "969(0)", "463()", "828()", "824(0,0,0)", "819(0)", "872()", "1149(0,0)"]);
    assert_eq!(CONTROL.load(Ordering::Acquire), GAME);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn handoff_does_not_run_an_overdue_mail_retry_inside_callback_teardown() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    s.registered = true;
    set_linked(3);
    s.register_callbacks();
    s.social.retry_mail_now();
    clear_calls();
    s.attach_game();
    assert!(raw_calls().iter().all(|(slot, _)| ![329, 330, 326, 327, 322, 323, 818, 705].contains(slot)),
        "handoff must remove the callback without starting a mail cycle or pumping");
    assert!(trace(&[]).contains(&"872()".to_string()));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn return_from_game_restarts_the_viewer_on_the_live_session() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    set_linked(3);
    s.detach_game();
    assert_eq!(trace(&[]), ["288(0)"]);
    clear_calls();
    script(190, &[0]); // a character is selected: publish clears it
    s.return_from_game().unwrap();
    let calls = trace(&[838, 818, 1103, 1107]);
    assert_eq!(until(&calls, "377("), [
        "965(1)", "1527(0)", "1309(*)",
        "819(0)", "824(*,*,*)", "825(*,*)", "826(*,*)", "884()", "1149(*,*)",
        "1032(1)", "871(*)",
        "163()", "171()", "175()", "383()", "164(1)", "172(2)", "176(3)", "384(4)",
        // The mail list on every entry, then the Viewer presence.
        "1304()", "329(*,200)", "330(7)",
        "190()", "376(-2,-1,1,-2,0,1000)", "377(5)",
    ]);
    assert!(!raw_calls().iter().any(|(s, _)| *s == 374), "no status load on return");

    // No character and already the Viewer: nothing is published.
    clear_calls();
    s.return_from_game().unwrap();
    let calls = trace(&[838, 818, 1103, 1107]);
    let tail = &calls[calls.iter().position(|c| c == "330(7)").unwrap() + 1..];
    assert_eq!(&tail[..2], ["190()", "207()"]);
    assert!(!calls.iter().any(|c| c.starts_with("376(")));

    // No character, another application: publish the Viewer only.
    clear_calls();
    script(207, &[1]);
    s.return_from_game().unwrap();
    assert!(trace(&[]).contains(&"376(-2,-2,1,-2,0,1000)".to_string()));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn return_without_a_member_session_is_a_sign_out_not_a_reload() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    set_linked(0);
    assert!(s.return_from_game().is_err());
    assert!(!s.signed_in);
    assert!(!raw_calls().iter().any(|(s, _)| [163, 824, 871, 376].contains(s)));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- Idle Viewer ----------------------------------------------------------

#[test]
fn status_changes_push_to_groups_only_when_asked_and_in_a_group() {
    let viewer = fixture();
    let mut s = session(&viewer);
    let api = s.api.clone();
    let mut frame = |status: &mut ViewerStatus| { status.step(&api); true };
    set_linked(3);
    script(385, &[1]);
    NativeSession::user_status(&api, 0, true, &mut s.viewer_status, &mut frame);
    assert_eq!(trace(&[838, 1103]), ["385()", "376(-2,-2,1,0,0,-2)", "404(0)", "377(5)", "405(6)"]);
    clear_calls();
    NativeSession::user_status(&api, 4, false, &mut s.viewer_status, &mut frame);
    assert_eq!(trace(&[838, 1103]), ["376(-2,-2,1,4,0,-2)", "377(5)"]);
    clear_calls();
    NativeSession::user_status(&api, 1, true, &mut s.viewer_status, &mut frame); // 385() == 0: no group active
    assert_eq!(trace(&[838, 1103]), ["385()", "376(-2,-2,1,1,0,-2)", "377(5)"]);
    clear_calls();
    // 10040f39 starts both jobs on apply: a failed status change is still pushed.
    script(377, &[-5000]);
    script(385, &[1]);
    NativeSession::user_status(&api, 0, true, &mut s.viewer_status, &mut frame);
    assert_eq!(trace(&[838, 1103, 1107]), ["385()", "376(-2,-2,1,0,0,-2)", "404(0)", "377(5)", "405(6)"]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn stopped_status_owner_cancels_both_tickets_without_another_frame() {
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    script(385, &[1]);
    script(377, &[0]);
    let api = s.api.clone();
    let mut frames = 0;
    NativeSession::user_status(&api, 0, true, &mut s.viewer_status, &mut |status| {
        frames += 1;
        status.step(&api);
        false
    });
    assert_eq!(frames, 1);
    assert!(s.viewer_status.pending.is_none());
    assert_eq!(trace(&[838, 1103]), ["385()", "376(-2,-2,1,0,0,-2)", "404(0)", "377(5)", "337(6)", "337(5)"]);
    clear_calls();
    script(385, &[1]);
    script(405, &[native_jobs::ABORTED]);
    script(377, &[0, 1]);
    frames = 0;
    NativeSession::user_status(&api, 0, true, &mut s.viewer_status, &mut |status| {
        frames += 1;
        status.step(&api);
        true
    });
    assert_eq!(frames, 2, "a group failure must not stop the independent presence job");
    assert!(s.viewer_status.pending.is_none());
    assert!(!raw_calls().iter().any(|(slot, _)| *slot == 337));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn auto_away_counts_gated_frames_and_checks_idle_at_the_stock_cold_period() {
    // The Viewer checks after 3600 cold frames, 112.5 s: 5625 frames of 20 ms, not 3600 (72 s).
    assert_eq!(auto_away_frames(), 5625);
    assert_eq!(FRAME * auto_away_frames(), Duration::from_millis(112_500));
    let n = auto_away_frames();
    let idle = Duration::from_secs(10 * 60 + 1);
    let mut away = AutoAway::new(10);
    for _ in 0..3600 { assert!(!away.frame(true, idle)); }
    assert!(!away.frame(true, idle), "no check at the 3601st frame of 20 ms");
    for _ in 3601..n { assert!(!away.frame(true, idle)); }
    assert!(away.frame(true, idle), "the 5626th Visible frame checks");
    for _ in 0..n { assert!(!away.frame(true, idle)); }
    assert!(!away.frame(true, Duration::from_secs(600)), "idle must exceed the setting");
    for _ in 0..n { assert!(!away.frame(true, idle)); }
    assert!(away.frame(true, idle), "the check resets the counter either way");
    let mut away = AutoAway::new(10);
    for _ in 0..10_000 { assert!(!away.frame(false, idle)); }
    for _ in 0..n { assert!(!away.frame(true, idle)); }
    assert!(away.frame(true, idle), "frames while not Visible do not count");
    let mut off = AutoAway::new(0);
    assert!((0..10_000).all(|_| !off.frame(true, Duration::from_secs(86_400))));
    let mut hour = AutoAway::new(60);
    for _ in 0..n { hour.frame(true, idle); }
    assert!(!hour.frame(true, Duration::from_secs(3599)));
}

#[test]
fn auto_away_sends_the_viewer_away_status_only_while_visible() {
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.auto_away.frames = auto_away_frames();
    script(206, &[1]); // Away already: not gated, counter unchanged
    NativeSession::auto_away_tick(&s.api, &mut s.auto_away, &mut s.viewer_status, Duration::from_secs(3600));
    assert!(s.viewer_status.pending.is_none());
    assert_eq!(s.auto_away.frames, auto_away_frames());
    NativeSession::auto_away_tick(&s.api, &mut s.auto_away, &mut s.viewer_status, Duration::from_secs(3600));
    s.viewer_status.step(&s.api);
    s.viewer_status.step(&s.api);
    assert_eq!(trace(&[838, 1103]), ["206()", "206()", "376(-2,-2,1,1,0,-2)", "377(5)"]);
    assert_eq!(s.auto_away.frames, 0);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn auto_away_progresses_during_another_native_request_wait() {
    let pauses = CapturePauses::start();
    let viewer = fixture();
    let mut s = session(&viewer);
    let api = s.api.clone();
    set_linked(3);
    s.auto_away.frames = auto_away_frames();
    script(172, &[0, 0, 1]);
    script(377, &[0, 1]);
    let result = native_jobs::run(&api, Request { name: "handles", start: &mut || api.zero(171), poll: 172, policy: Policy::Normal },
        &mut || idle_owner_frame(&api, &mut s.pushes, &mut s.auto_away, &mut s.viewer_status, Duration::from_secs(601)));
    assert_eq!(result, 1);
    assert!(s.viewer_status.pending.is_none());
    assert_eq!(s.auto_away.frames, 2);
    let calls = trace(&[838, 1103]);
    assert_eq!(calls, ["171()", "818()", "206()", "376(-2,-2,1,1,0,-2)", "172(2)",
        "818()", "206()", "377(5)", "172(2)", "818()", "206()", "377(5)", "172(2)"]);
    assert!(pauses.take().iter().all(|(interval, _)| *interval == FRAME));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn newer_status_replaces_only_its_pending_ticket_and_handoff_waits_for_it_without_a_cancel() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.viewer_status.queue(&s.api, 1, true);
    s.viewer_status.step(&s.api);
    clear_calls();
    s.viewer_status.queue(&s.api, 0, false); // the Viewer's own replacement cancel
    s.viewer_status.queue(&s.api, 4, false); // queued replacement has no ticket
    s.viewer_status.step(&s.api);
    assert_eq!(trace(&[838, 1103]), ["337(5)", "376(-2,-2,1,4,0,-2)"]);
    clear_calls();
    // Play: launch states 3/6/7 poll the in-flight ticket to its end, one
    // idle frame (818) each, then hand off with no 337/328/199.
    let _play = TestPlay::requested();
    script(377, &[0, 0, 1]);
    let pauses = CapturePauses::start();
    let events = events::capture(|| assert_eq!(s.settle_for_play(), Ok(true)));
    assert_eq!(trace(&[838, 1103]), ["818()", "377(5)", "818()", "377(5)", "818()", "377(5)"]);
    assert_eq!(pauses.take().iter().map(|(d, _)| *d).collect::<Vec<_>>(), [FRAME; 3]);
    assert_eq!(events, ["{\"type\":\"native_control_result\",\"action\":\"invisible\",\"code\":1}\n"]);
    assert!(s.viewer_status.pending.is_none());
    clear_calls();
    s.attach_game();
    assert!(!raw_calls().iter().any(|(slot, _)| [337, 328, 199, 376, 377, 818].contains(slot)), "{:?}", trace(&[]));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn status_waiting_to_retry_at_play_is_dropped_unsent_and_never_cancelled() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.viewer_status.queue(&s.api, 1, true);
    s.viewer_status.step(&s.api);
    script(377, &[-515]);
    script(1107, &[1]);
    s.viewer_status.step(&s.api); // retryable failure: the core released ticket 5
    clear_calls();
    let _play = TestPlay::requested();
    assert_eq!(s.settle_for_play(), Ok(true));
    assert!(raw_calls().is_empty(), "nothing is in flight, so Play does not wait");
    let events = events::capture(|| assert!(idle_owner_frame(&s.api, &mut s.pushes, &mut s.auto_away, &mut s.viewer_status, Duration::ZERO)));
    // Logged only: an error result would stand as the worker's session error
    // until attach_game's state line, and a batch release reads that as a dead session.
    assert!(events.is_empty(), "{events:?}");
    assert!(s.viewer_status.pending.is_none());
    s.attach_game();
    assert!(!raw_calls().iter().any(|(slot, _)| [337, 328, 199, 376, 377].contains(slot)), "{:?}", trace(&[]));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn an_in_flight_status_that_fails_after_play_is_logged_not_reported() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.viewer_status.queue(&s.api, 1, true);
    s.viewer_status.step(&s.api); // auto-Away 376 out, ticket 5 pending
    clear_calls();
    let _play = TestPlay::requested();
    script(377, &[0, -515]);
    script(1107, &[1]); // retryable, but no new attempt after Play
    // Polled to its failure; the retry it now waits for is dropped on the next idle frame.
    let events = events::capture(|| {
        assert_eq!(s.settle_for_play(), Ok(true));
        assert!(idle_owner_frame(&s.api, &mut s.pushes, &mut s.auto_away, &mut s.viewer_status, Duration::ZERO));
    });
    assert!(events.is_empty(), "{events:?}");
    assert!(s.viewer_status.pending.is_none());
    assert_eq!(raw_calls().iter().filter(|(slot, _)| *slot == 377).count(), 2);
    assert!(!raw_calls().iter().any(|(slot, _)| [337, 376].contains(slot)), "{:?}", trace(&[]));
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn the_shipped_play_predicate_follows_start_game_and_the_latch() {
    let control = AtomicU8::new(SOCIAL);
    let latch = AtomicBool::new(false);
    assert!(!play_requested_in(&control, &latch));
    // A user command (status with its groups flag, a social request) is not Play...
    for command in [VISIBLE | GROUPS, AWAY, INVISIBLE | GROUPS, SOCIAL_REQUEST, SIGN_OUT, GAME] {
        control.store(command, Ordering::Release);
        assert!(!play_requested_in(&control, &latch), "{command:#x}");
    }
    // ...until a Play latches behind it, then becomes START_GAME on release.
    control.store(VISIBLE | GROUPS, Ordering::Release);
    assert_eq!(accept_play(&control, &latch), 0);
    assert!(play_requested_in(&control, &latch), "latched");
    release_command(&control, &latch);
    assert!(play_requested_in(&control, &latch), "started");
    control.store(SOCIAL, Ordering::Release);
    assert_eq!(accept_play(&control, &latch), 0);
    assert!(play_requested_in(&control, &latch));
}

#[test]
fn after_play_no_auto_away_and_no_mail_retry_start() {
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    let _play = TestPlay::requested();
    s.auto_away.frames = auto_away_frames();
    assert!(idle_owner_frame(&s.api, &mut s.pushes, &mut s.auto_away, &mut s.viewer_status, Duration::from_secs(86_400)));
    assert_eq!(trace(&[838, 1103]), ["818()"], "no Visible check and no Away request");
    assert!(s.viewer_status.pending.is_none());
    assert_eq!(s.auto_away.frames, auto_away_frames());
    clear_calls();
    s.social.retry_mail_now();
    s.social.idle_frame(&s.api, &mut || true);
    assert!(raw_calls().is_empty(), "a due mail retry waits for the next main-menu entry");
    drop(_play);
    s.social.idle_frame(&s.api, &mut || true);
    assert!(raw_calls().iter().any(|(slot, _)| *slot == 329), "the same retry runs while idle");
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn a_real_start_game_or_latched_play_starts_no_auto_away_and_no_mail_retry() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let _real = TestPlay::real_control();
    for latched in [false, true] {
        let viewer = fixture();
        let mut s = session(&viewer);
        set_linked(3);
        if latched {
            CONTROL.store(SOCIAL_REQUEST, Ordering::Release);
            assert_eq!(accept_play(&CONTROL, &PLAY_LATCH), 0);
        } else {
            CONTROL.store(START_GAME, Ordering::Release);
        }
        s.auto_away.frames = auto_away_frames();
        assert!(idle_owner_frame(&s.api, &mut s.pushes, &mut s.auto_away, &mut s.viewer_status, Duration::from_secs(86_400)));
        assert_eq!(trace(&[838, 1103]), ["818()"], "latched={latched}: no Away request");
        assert!(s.viewer_status.pending.is_none());
        clear_calls();
        s.social.retry_mail_now();
        s.social.idle_frame(&s.api, &mut || true);
        assert!(raw_calls().is_empty(), "latched={latched}: no mail retry");
        CONTROL.store(GAME, Ordering::Release);
        PLAY_LATCH.store(false, Ordering::Release);
        forget(s);
        fs::remove_dir_all(viewer).unwrap();
    }
}

/// Drive the real idle loop; `command` runs on another thread (as the reply
/// reader does) once the loop is accepting commands.
fn drive_idle(s: &mut NativeSession, command: fn()) -> (Result<bool, String>, Vec<String>) {
    let helper = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while CONTROL.load(Ordering::Acquire) != SOCIAL {
            assert!(Instant::now() < deadline, "the idle loop never accepted commands");
            std::thread::sleep(Duration::from_millis(1));
        }
        command();
    });
    let mut result = None;
    let events = events::capture(|| result = Some(s.wait_for_game()));
    helper.join().unwrap();
    (result.unwrap(), events)
}

#[test]
fn play_with_an_in_flight_auto_away_lets_it_finish_and_hands_off_without_a_cancel() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.viewer_status.queue(&s.api, 1, true);
    s.viewer_status.step(&s.api); // auto-Away 376 out, ticket 5 pending
    let mut polls = vec![0; 25];
    polls.push(1);
    script(377, &polls);
    clear_calls();
    // The shipped predicate (CONTROL == START_GAME || PLAY_LATCH), not TestPlay.
    let _real = TestPlay::real_control();
    let (result, events) = drive_idle(&mut s, request_start_game);
    assert_eq!(result, Ok(true));
    assert!(events.iter().any(|e| e == "{\"type\":\"native_control_result\",\"action\":\"away\",\"code\":1}\n"), "{events:?}");
    assert!(s.viewer_status.pending.is_none());
    s.attach_game();
    let calls = raw_calls();
    assert_eq!(calls.iter().filter(|(slot, _)| *slot == 377).count(), 26, "polled to completion");
    assert!(!calls.iter().any(|(slot, _)| [337, 328, 199].contains(slot)), "no cancel between START_GAME and the handoff");
    assert_eq!(CONTROL.load(Ordering::Acquire), GAME);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn return_reports_the_main_menu_before_the_idle_viewer() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    set_linked(3);
    s.detach_game();
    script(190, &[0]); // a character is selected: the publish clears it
    let (result, events) = drive_idle(&mut s, request_sign_out);
    assert_eq!(result, Ok(false));
    assert_eq!(&events[..3], [
        "{\"type\":\"native_main_menu\",\"code\":1,\"returned\":true}\n",
        "{\"type\":\"native_session_state\",\"game\":false}\n",
        "{\"type\":\"native_social_ready\"}\n",
    ]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn replacing_retrying_or_completed_status_never_cancels_a_released_ticket() {
    let viewer = fixture();
    let mut s = session(&viewer);
    set_linked(3);
    s.viewer_status.queue(&s.api, 1, true);
    s.viewer_status.step(&s.api);
    script(377, &[-256]);
    script(1107, &[1]);
    s.viewer_status.step(&s.api); // the failed native poll has released ticket 5
    clear_calls();
    s.viewer_status.queue(&s.api, 4, false);
    s.viewer_status.step(&s.api);
    s.viewer_status.step(&s.api);
    s.viewer_status.cancel(&s.api);
    assert_eq!(trace(&[838, 1103]), ["376(-2,-2,1,4,0,-2)", "377(5)"]);
    assert!(s.viewer_status.pending.is_none());
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn launcher_activity_resets_the_idle_clock() {
    note_activity();
    assert!(idle_time() < Duration::from_secs(5));
}

#[test]
fn connection_loss_is_a_local_sign_out_without_close_or_relogin() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    s.home = true;
    set_linked(3);
    // The owner frame peeks the latched pump error once per frame (101984da).
    script(1103, &[-1025]);
    assert!(!owner_frame(&s.api, &mut s.pushes));
    assert_eq!(trace(&[]), ["818()", "838()", "1103()"]);
    clear_calls();
    let error = s.connection_lost(-1025);
    assert!(error.contains("-1025"), "{error}");
    assert_eq!(trace(&[]), ["463()", "434()"]);
    assert!(!s.signed_in);
    clear_calls();
    drop(s);
    let calls = trace(&[]);
    assert!(!calls.iter().any(|c| c.starts_with("816(") || c.starts_with("817(") || c.starts_with("815(")));
    assert_eq!(calls, ["1502(0,0,0,0)"]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_out_closes_like_the_viewer_without_pumping() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    s.home = true;
    s.registered = true;
    s.files = true;
    script(817, &[1, 1, 7]);
    let started = Instant::now();
    let events = events::capture(|| drop(s));
    assert_eq!(events, ["{\"type\":\"native_sign_out_complete\",\"code\":0}\n"]);
    assert!(started.elapsed() >= Duration::from_millis(200), "817 polls are >= 100 ms apart");
    // 838 only decides whether final pushes can still be answered.
    assert_eq!(trace(&[838, 1103]), [
        "816(1)", "817()", "817()", "817()", "463()", "434()", "1502(0,0,0,0)",
        "828()", "824(0,0,0)", "819(0)", "872()", "1149(0,0)", "920()", "915()",
    ]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn sign_out_failure_is_reported_after_teardown_and_never_as_success() {
    let viewer = fixture();
    let mut s = session(&viewer);
    s.signed_in = true;
    s.home = true;
    s.files = true;
    script(817, &[-1038]);
    let events = events::capture(|| drop(s));
    assert_eq!(events, ["{\"type\":\"native_sign_out_complete\",\"code\":-1038}\n"]);
    assert_eq!(trace(&[838, 1103]), ["816(1)", "817()", "463()", "434()", "1502(0,0,0,0)", "920()", "915()"]);
    fs::remove_dir_all(viewer).unwrap();
}

#[test]
fn concurrent_loads_retry_a_full_table_and_cancel_only_their_own_tickets() {
    let viewer = fixture();
    let s = session(&viewer);
    set_linked(3);
    script(171, &[SLOTS_FULL]);
    script(164, &[0, 0]);
    let mut frames = 0;
    let results = run_concurrent(&s.api, &LISTS[..2], &mut || { frames += 1; true });
    assert_eq!(results, [1, 1]);
    assert_eq!(trace(&[838, 1103]), ["163()", "171()", "164(1)", "171()", "164(1)", "172(2)", "164(1)"]);
    assert_eq!(frames, 3);
    clear_calls();
    script(164, &[SLOTS_FULL]);
    script(1107, &[1]);
    assert_eq!(run_concurrent(&s.api, &LISTS[..1], &mut || true), [SLOTS_FULL]);
    assert_eq!(trace(&[838, 1103]), ["163()", "164(1)", "1107(-256)"],
        "a failed poll is not a new allocation attempt under the Login policy");
    clear_calls();
    let results = run_concurrent(&s.api, &LISTS[..2], &mut || false);
    assert_eq!(results, [native_jobs::ABORTED, native_jobs::ABORTED]);
    assert_eq!(trace(&[838, 1103]), ["163()", "171()", "337(1)", "337(2)"]);
    forget(s);
    fs::remove_dir_all(viewer).unwrap();
}

// --- No requests during play; no manual refresh (B1, C6) --------------------

#[test]
fn friend_observer_only_reads_locked_getters() {
    let (stop, stopping) = mpsc::channel();
    let (done, calls) = mpsc::channel();
    let table = fake_table();
    let thread = std::thread::spawn(move || {
        set_linked(3);
        run_friend_observer(Api::test_table(&table), stopping, |_| {});
        let _ = done.send(raw_calls());
    });
    std::thread::sleep(Duration::from_millis(700));
    stop.send(()).unwrap();
    thread.join().unwrap();
    let calls = calls.recv().unwrap();
    assert!(calls.iter().any(|(s, _)| *s == 167));
    assert!(calls.iter().all(|(s, _)| [838, 206, 207, 167, 169].contains(s)), "{:?}", calls.iter().map(|c| c.0).collect::<Vec<_>>());
}

#[test]
fn native_controls_cannot_sign_out_under_a_game_or_queue_duplicate_starts() {
    let control = AtomicU8::new(GAME);
    let latch = AtomicBool::new(false);
    assert_eq!(accept_control(&control, &latch, SIGN_OUT), -256);
    assert_eq!(accept_play(&control, &latch), -256);
    assert_eq!(accept_control(&control, &latch, VISIBLE | GROUPS), -256, "no status request during play");
    assert_eq!(control.load(Ordering::Acquire), GAME);
    control.store(SOCIAL, Ordering::Release);
    assert_eq!(accept_play(&control, &latch), 0);
    assert_eq!(accept_play(&control, &latch), -256);
    assert_eq!(accept_control(&control, &latch, SIGN_OUT), -256);
    control.store(SOCIAL, Ordering::Release);
    assert_eq!(accept_control(&control, &latch, SIGN_OUT), 0);
    assert_eq!(accept_play(&control, &latch), -256);
    assert!(!latch.load(Ordering::Acquire), "never latched behind a game, a sign-out or a Play");
}

#[test]
fn an_unconfirmed_portrait_is_refused_without_queuing_a_command() {
    let _serial = CONTROL_TEST.lock().unwrap_or_else(|e| e.into_inner());
    assert!(!crate::wire::PORTRAIT_FIELD_CONFIRMED);
    let before = CONTROL.swap(SOCIAL, Ordering::AcqRel);
    let events = events::capture(|| request_portrait(12));
    assert_eq!(events, [format!("{{\"type\":\"native_control_result\",\"action\":\"portrait\",\"code\":{}}}\n", crate::native_portrait::NOT_CONFIRMED)]);
    assert_eq!(CONTROL.load(Ordering::Acquire), SOCIAL, "the owner never sees a portrait command");
    assert_eq!(PORTRAIT_REQUEST.load(Ordering::Acquire), NO_PORTRAIT);
    CONTROL.store(before, Ordering::Release);
}

#[test]
fn play_latches_behind_a_running_user_command_and_starts_when_it_returns() {
    let control = AtomicU8::new(SOCIAL_REQUEST);
    let latch = AtomicBool::new(false);
    assert_eq!(accept_play(&control, &latch), 0, "answered, not refused");
    assert_eq!(accept_play(&control, &latch), -256, "one Play");
    assert_eq!(accept_control(&control, &latch, SIGN_OUT), -256);
    assert_eq!(control.load(Ordering::Acquire), SOCIAL_REQUEST, "the running command is untouched");
    release_command(&control, &latch);
    assert_eq!((control.load(Ordering::Acquire), latch.load(Ordering::Acquire)), (START_GAME, false));
    // A status change (with its groups flag) latches the same way.
    control.store(VISIBLE | GROUPS, Ordering::Release);
    assert_eq!(accept_play(&control, &latch), 0);
    release_command(&control, &latch);
    assert_eq!(control.load(Ordering::Acquire), START_GAME);
    // Without a latch, a released command returns to the idle Viewer.
    control.store(AWAY, Ordering::Release);
    release_command(&control, &latch);
    assert_eq!(control.load(Ordering::Acquire), SOCIAL);
    // Latched just after the release: the idle loop takes it...
    latch.store(true, Ordering::Release);
    assert_eq!(accept_control(&control, &latch, INVISIBLE), -256, "no new command while Play waits");
    take_latched_play(&control, &latch);
    assert_eq!((control.load(Ordering::Acquire), latch.load(Ordering::Acquire)), (START_GAME, false));
    // ...unless another command already holds the Viewer; then its release does.
    control.store(AWAY, Ordering::Release);
    latch.store(true, Ordering::Release);
    take_latched_play(&control, &latch);
    assert_eq!((control.load(Ordering::Acquire), latch.load(Ordering::Acquire)), (AWAY, true));
    release_command(&control, &latch);
    assert_eq!(control.load(Ordering::Acquire), START_GAME);
}

static OBSERVER_CONNECTED: AtomicBool = AtomicBool::new(true);
unsafe extern "C" fn observer_connection() -> i32 {
    if OBSERVER_CONNECTED.load(Ordering::Acquire) { 3 } else { 0 }
}
unsafe extern "C" fn observer_friend(index: usize, output: usize) {
    if index == 0 {
        let record = std::slice::from_raw_parts_mut(output as *mut u8, proto::FRIEND_RECORD_LEN);
        record[0x98] = 1;
        record[8..16].copy_from_slice(&((1u64 << 11) | (1u64 << 33)).to_le_bytes());
        record[0x10..0x18].copy_from_slice(&123u64.to_le_bytes());
    }
}
unsafe extern "C" fn observer_empty(_: usize, _: usize) {}
unsafe extern "C" fn observer_presence() -> i32 { 1 }

#[test]
fn observer_republishes_unchanged_native_friends_after_connection_returns() {
    OBSERVER_CONNECTED.store(true, Ordering::Release);
    let mut table = vec![0; 1560];
    table[838] = observer_connection as *const () as usize;
    table[167] = observer_friend as *const () as usize;
    table[169] = observer_empty as *const () as usize;
    table[206] = observer_presence as *const () as usize;
    table[207] = observer_presence as *const () as usize;
    // The test leaves all request, pump and notification slots unavailable.
    let (stop, stopping) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let observer = FriendObserver {
        stop,
        thread: Some(std::thread::spawn(move || {
            run_friend_observer(Api(table.into()), stopping, |line| { let _ = tx.send(line); });
        })),
    };
    let receive = || rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(receive(), "{\"type\":\"native_pol_status\",\"code\":0}\n");
    assert_eq!(receive(), "{\"type\":\"native_own_presence\",\"mode\":1}\n");
    let initial = receive();
    assert!(initial.starts_with("{\"type\":\"native_friends\",\"payload\":\""));

    OBSERVER_CONNECTED.store(false, Ordering::Release);
    assert_eq!(receive(), "{\"type\":\"native_pol_status\",\"code\":-1025}\n");
    OBSERVER_CONNECTED.store(true, Ordering::Release);
    assert_eq!(receive(), "{\"type\":\"native_pol_status\",\"code\":0}\n");
    assert_eq!(receive(), "{\"type\":\"native_own_presence\",\"mode\":1}\n");
    // Without the fix this second snapshot is never emitted: the bytes match.
    assert_eq!(receive(), initial);
    drop(observer); // Same stop-and-join path as the live host.
}

// --- Unchanged local behaviour ---------------------------------------------

#[test]
fn authentic_success_requires_the_expected_link() {
    assert_eq!(connection_result(27, || 0, 3), None);
    assert_eq!(connection_result(31, || 0, 3), None, "31..38 are transitional");
    assert_eq!(connection_result(39, || 0, 3), None);
    assert_eq!(connection_result(30, || 3, 3), Some(Ok(())));
    assert_eq!(connection_result(30, || 2, 2), Some(Ok(())));
    assert_eq!(connection_result(30, || 2, 3), Some(Err(())));
    assert_eq!(connection_result(-1025, || 3, 3), Some(Err(())));
}

/// Review finding (2026-10-13): the launcher marks an account as one that needs a one-time
/// code only from a refused member login, so a failed Open names its stage. The network-level
/// Open runs before 1502 sets any code and a cancelled login never reached a verdict.
#[test]
fn a_failed_open_names_its_stage() {
    assert_eq!(open_failure(0, -1025, -1036, false), "Native PlayOnline network connection failed (state -1025, POL result -1036).");
    assert_eq!(open_failure(1, 39, -1036, false), "Native PlayOnline member login failed (state 39, POL result -1036).");
    for mode in [0, 1] {
        assert_eq!(open_failure(mode, -1, -2, true), "Native PlayOnline login was cancelled at state -1 (POL result -2).");
        // -19 stays readable as the launcher's network failure at either stage.
        assert!(open_failure(mode, 39, -19, false).contains("POL result -19)"));
    }
}

#[test]
fn notification_texts_lose_carriage_returns_like_the_viewer() {
    assert_eq!(viewer_text(1302, b"a\r\nb\r".to_vec()), b"a\nb");
    assert_eq!(viewer_text(1072, b"a\r\nb".to_vec()), b"a\r\nb");
}

#[test]
fn account_stores_never_alias_or_adopt_foreign_folders() {
    let root = std::env::temp_dir().join(format!(
        "crystal-pol-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("pub/home50/msg")).unwrap();
    fs::create_dir_all(root.join("usr/home51")).unwrap();
    fs::write(root.join("pub/home50/msg/official"), b"preserve").unwrap();
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    assert_eq!(claim_home(&root, &a).unwrap(), 52);
    assert_eq!(claim_home(&root, &b).unwrap(), 53);
    assert_eq!(claim_home(&root, &a).unwrap(), 52);
    assert!(!root.join("usr/home51/.crystal-profile").exists());
    assert_eq!(
        fs::read(root.join("pub/home50/msg/official")).unwrap(),
        b"preserve"
    );
    assert!(claim_home(&root, "../home50").is_err());
    // Test-owned absolute temporary root only.
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn localization_refuses_truncated_and_oversized_data() {
    assert!(decode_pex(b"PEX\0").is_none());
    let mut raw = vec![0; 16];
    raw[..4].copy_from_slice(b"PEX\0");
    raw[4..8].copy_from_slice(&2_000_001u32.to_le_bytes());
    assert!(decode_pex(&raw).is_none());
    let mut decoded = vec![0; 16];
    decoded.extend_from_slice(&10u16.to_le_bytes());
    decoded.extend_from_slice(&0x46c9u16.to_le_bytes());
    decoded.extend_from_slice(&[b'H', 0, b'i', 0, 0, 0]);
    assert_eq!(resource_text(&decoded, 0x46c9).unwrap(), b"Hi");
    assert!(resource_text(&decoded[..decoded.len() - 1], 0x46c9).is_none());
}

#[test]
fn a_held_portrait_slot_refuses_a_second_request_and_keeps_the_first_value() {
    let slot = AtomicU16::new(NO_PORTRAIT);
    assert_eq!(claim_portrait(&slot, 12, || 0), 0);
    // Accepted, not yet read by the owner: the next request is refused and changes nothing.
    assert_eq!(claim_portrait(&slot, 34, || panic!("no command is asked for")), BUSY);
    assert_eq!(slot.swap(NO_PORTRAIT, Ordering::AcqRel), 12);
    // A refused command gives the slot back.
    assert_eq!(claim_portrait(&slot, 56, || BUSY), BUSY);
    assert_eq!(slot.load(Ordering::Acquire), NO_PORTRAIT);
}
