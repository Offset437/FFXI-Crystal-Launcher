//! Desktop notifications for new PlayOnline mail and friends coming online.
//!
//! No requests of its own: each signed-in account's session worker already publishes
//! `friends.json` in the account directory, and this module only watches those files. The
//! watcher thread exists only while notifications are enabled and at least one directory is
//! watched, and it sleeps in `WaitForMultipleObjects` (a change handle per directory plus a
//! control event) with a timeout only while a notification is waiting for its throttle window, so
//! an idle launcher costs nothing.
//!
//! Layers, each testable alone: `Differ` (snapshots to causes), `Policy` (throttle and
//! coalesce, driven by an injected clock), text builders, and `Hooks` (foreground test,
//! privacy flag, delivery) so tests never show a real notification.
#![allow(dead_code)]

use crate::i18n::{t, tf};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Master switch; off stops the watcher thread.
    pub enabled: bool,
    pub mail: bool,
    pub friends_online: bool,
}

impl Default for Settings {
    fn default() -> Self { Self { enabled: true, mail: true, friends_online: true } }
}

impl Settings {
    fn active(&self) -> bool { self.enabled && (self.mail || self.friends_online) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind { Mail, FriendsOnline }

/// What the interface can also show. Privacy mode is already applied to the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice { pub kind: Kind, pub title: String, pub body: String, pub account_key: Option<String> }

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Delivery { Balloon, Suppressed(&'static str), Failed(String) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report { pub notice: Notice, pub delivery: Delivery }

pub(crate) type Sink = Arc<dyn Fn(Report) + Send + Sync>;

/// An account directory to watch; `label` names the account in non-private mail notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Watched { pub account_key: String, pub label: String, pub directory: PathBuf }

// ---- diff -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cause { FriendOnline(String), NewMail }

struct Seen { worker: (u32, u64), online: HashMap<String, bool>, unread: Option<u32>, changed: bool }

/// Per-account baseline. The first snapshot after a start, or from a new worker, only sets
/// the baseline: a friend list that is merely loaded is not "coming online".
#[derive(Default)]
pub(crate) struct Differ { seen: HashMap<String, Seen> }

impl Differ {
    pub(crate) fn observe(&mut self, account: &str, worker: (u32, u64), snap: &crate::friends::Snapshot) -> Vec<Cause> {
        let online: HashMap<String, bool> =
            snap.entries.iter().filter(|f| !f.alternate).map(|f| (f.name.clone(), f.online == Some(true))).collect();
        let (unread, changed) = (snap.social.inbox_unread, snap.social.inbox_changed);
        let mut causes = Vec::new();
        if let Some(prev) = self.seen.get(account).filter(|p| p.worker == worker) {
            for f in snap.entries.iter().filter(|f| !f.alternate && f.online == Some(true)) {
                if !prev.online.get(&f.name).copied().unwrap_or(false) { causes.push(Cause::FriendOnline(f.name.clone())); }
            }
            // `unread` rising needs a known earlier count: None to Some is the inbox loading.
            if (changed && !prev.changed) || matches!((prev.unread, unread), (Some(a), Some(b)) if b > a) { causes.push(Cause::NewMail); }
        }
        self.seen.insert(account.to_owned(), Seen { worker, online, unread, changed });
        causes
    }

    pub(crate) fn forget_except(&mut self, keep: &[Watched]) { self.seen.retain(|k, _| keep.iter().any(|w| &w.account_key == k)); }
}

// ---- policy ---------------------------------------------------------------------------

/// At most one notification per window overall.
const WINDOW_MS: u64 = 10_000;
/// A friend's first sign-on waits this long so others arriving together share one notification.
const GATHER_MS: u64 = 1_500;
const FRIEND_REPEAT_MS: u64 = 15 * 60_000;
const MAIL_REPEAT_MS: u64 = 2 * 60_000;
const PENDING_CAP: usize = 64;

/// Throttling and coalescing. Times are caller-supplied milliseconds, so tests inject a clock.
#[derive(Default)]
pub(crate) struct Policy {
    last: Option<u64>,
    friend_sent: HashMap<String, u64>,
    mail_sent: HashMap<String, u64>,
    friends: Vec<(String, String)>,
    first_friend: u64,
    mail: Vec<(String, String)>,
}

impl Policy {
    pub(crate) fn push_friend(&mut self, now: u64, account: &str, name: &str) {
        let id = name.to_lowercase();
        self.friend_sent.retain(|_, at| now.saturating_sub(*at) < FRIEND_REPEAT_MS);
        if self.friend_sent.contains_key(&id) || self.friends.len() >= PENDING_CAP
            || self.friends.iter().any(|(_, n)| n.to_lowercase() == id) { return; }
        if self.friends.is_empty() { self.first_friend = now; }
        self.friends.push((account.to_owned(), name.to_owned()));
    }

    pub(crate) fn push_mail(&mut self, now: u64, account: &str, label: &str) {
        if self.mail_sent.get(account).is_some_and(|at| now.saturating_sub(*at) < MAIL_REPEAT_MS)
            || self.mail.iter().any(|(k, _)| k == account) || self.mail.len() >= PENDING_CAP { return; }
        self.mail.push((account.to_owned(), label.to_owned()));
    }

    /// The next notice if the window is open: mail first, then the gathered friends. When the
    /// game is in the foreground the notice is dropped (not queued, not counted against the
    /// throttles) and returned as suppressed.
    pub(crate) fn flush(&mut self, now: u64, private: bool, foreground: &dyn Fn() -> bool) -> Option<(Notice, bool)> {
        if self.last.is_some_and(|l| now < l + WINDOW_MS) { return None; }
        if !self.mail.is_empty() {
            let (key, label) = self.mail.remove(0);
            let notice = mail_notice(&key, &label, private);
            if foreground() { return Some((notice, true)); }
            self.last = Some(now);
            self.mail_sent.insert(key, now);
            return Some((notice, false));
        }
        if self.friends.is_empty() || now < self.first_friend + GATHER_MS { return None; }
        let taken = std::mem::take(&mut self.friends);
        let names: Vec<String> = taken.iter().map(|(_, n)| n.clone()).collect();
        let account = taken.iter().all(|(k, _)| *k == taken[0].0).then(|| taken[0].0.clone());
        let notice = friends_notice(&names, account, private);
        if foreground() { return Some((notice, true)); }
        self.last = Some(now);
        for n in names { self.friend_sent.insert(n.to_lowercase(), now); }
        Some((notice, false))
    }

    /// When `flush` can next produce something; `None` while nothing is waiting.
    pub(crate) fn deadline(&self) -> Option<u64> {
        let gate = self.last.map_or(0, |l| l + WINDOW_MS);
        let mail = (!self.mail.is_empty()).then_some(gate);
        let friends = (!self.friends.is_empty()).then(|| gate.max(self.first_friend + GATHER_MS));
        mail.into_iter().chain(friends).min()
    }
}

// ---- text -----------------------------------------------------------------------------

fn mail_notice(account: &str, label: &str, private: bool) -> Notice {
    let body = if private { String::new() } else { tf("Mail for {account}", &[("account", &label)]) };
    Notice { kind: Kind::Mail, title: t("New PlayOnline mail").to_owned(), body, account_key: (!private).then(|| account.to_owned()) }
}

/// Privacy mode carries no names and no account at all.
fn friends_notice(names: &[String], account: Option<String>, private: bool) -> Notice {
    let (title, account_key) = if private {
        let title = if names.len() == 1 { t("A friend is online").to_owned() } else { tf("{count} friends are online", &[("count", &names.len())]) };
        (title, None)
    } else {
        let first = &names[0];
        let title = match names.len() {
            1 => tf("{name} is online", &[("name", first)]),
            2 => tf("{name} and {other} are online", &[("name", first), ("other", &names[1])]),
            n => tf("{name} and {count} others are online", &[("name", first), ("count", &(n - 1))]),
        };
        (title, account)
    };
    Notice { kind: Kind::FriendsOnline, title, body: String::new(), account_key }
}

// ---- engine and hooks -----------------------------------------------------------------

#[derive(Default)]
struct Engine { differ: Differ, policy: Policy }

impl Engine {
    fn feed(&mut self, now: u64, watched: &Watched, worker: (u32, u64), snap: &crate::friends::Snapshot, settings: &Settings) {
        for cause in self.differ.observe(&watched.account_key, worker, snap) {
            match cause {
                Cause::FriendOnline(name) if settings.friends_online => self.policy.push_friend(now, &watched.account_key, &name),
                Cause::NewMail if settings.mail => self.policy.push_mail(now, &watched.account_key, &watched.label),
                _ => {}
            }
        }
    }
}

/// Everything environmental, injectable. `foreground` answers "a game window is in the
/// foreground and not minimised"; `privacy` is read per delivery.
#[derive(Clone)]
pub(crate) struct Hooks {
    pub foreground: Arc<dyn Fn() -> bool + Send + Sync>,
    pub privacy: Arc<AtomicBool>,
    pub deliver: Arc<dyn Fn(&Notice) -> Delivery + Send + Sync>,
}

impl Hooks {
    /// Real Windows delivery: a tray balloon.
    pub(crate) fn system(foreground: Arc<dyn Fn() -> bool + Send + Sync>, privacy: Arc<AtomicBool>) -> Self {
        Self { foreground, privacy, deliver: Arc::new(deliver) }
    }
}

// ---- watcher --------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Handle(*mut c_void);
// A kernel handle is usable from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

struct Shared { control: Handle, state: Mutex<State> }
impl Drop for Shared {
    fn drop(&mut self) { unsafe { windows_sys::Win32::Foundation::CloseHandle(self.control.0); } }
}

struct State { desired: Vec<Watched>, settings: Settings, running: bool }
impl State {
    fn wanted(&self) -> Vec<Watched> {
        if self.settings.active() { self.desired.iter().take(MAX_DIRECTORIES).cloned().collect() } else { Vec::new() }
    }
}

/// `WaitForMultipleObjects` takes 64 handles, one of them the control event.
const MAX_DIRECTORIES: usize = 63;

pub(crate) struct Watcher { shared: Arc<Shared>, hooks: Hooks, sink: Sink }

impl Watcher {
    /// Costs nothing yet: the thread starts with the first watched account.
    pub(crate) fn start(sink: Sink, hooks: Hooks, settings: Settings) -> Self {
        // SAFETY: an anonymous auto-reset event.
        let control = unsafe { windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        Self { shared: Arc::new(Shared { control: Handle(control), state: Mutex::new(State { desired: Vec::new(), settings, running: false }) }), hooks, sink }
    }

    /// The accounts to watch (replaces the set). An empty set, or notifications turned off,
    /// ends the thread.
    pub(crate) fn set_accounts(&self, accounts: Vec<Watched>) {
        lock(&self.shared.state).desired = accounts;
        self.kick();
    }

    pub(crate) fn apply(&self, settings: &Settings) {
        lock(&self.shared.state).settings = settings.clone();
        self.kick();
    }

    pub(crate) fn stop(&self) { self.set_accounts(Vec::new()); }

    pub(crate) fn is_running(&self) -> bool { lock(&self.shared.state).running }

    /// Starts the thread if there is something to watch, otherwise wakes it so it can end.
    /// The thread clears `running` under the same lock when it finds nothing to watch, so a
    /// quick stop-then-set cannot leave a set with no thread.
    fn kick(&self) {
        let mut state = lock(&self.shared.state);
        if state.wanted().is_empty() && !state.running { return; }
        if state.running {
            // SAFETY: `control` lives as long as `shared`.
            unsafe { windows_sys::Win32::System::Threading::SetEvent(self.shared.control.0); }
        } else {
            state.running = true;
            let (shared, hooks, sink) = (self.shared.clone(), self.hooks.clone(), self.sink.clone());
            if std::thread::Builder::new().name("notify-watch".into()).spawn(move || run(shared, hooks, sink)).is_err() { state.running = false; }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) { self.stop(); }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(PoisonError::into_inner) }

type Stamp = (u64, Option<SystemTime>);

struct Slot { watched: Watched, change: Option<Handle>, stamp: Option<Stamp>, primed: bool }
impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(h) = self.change { unsafe { windows_sys::Win32::Storage::FileSystem::FindCloseChangeNotification(h.0); } }
    }
}

fn open_change(directory: &std::path::Path) -> Option<Handle> {
    use windows_sys::Win32::Storage::FileSystem::{FindFirstChangeNotificationW, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE};
    // The session worker creates it on first publish; creating it here lets the watch exist first.
    let _ = std::fs::create_dir_all(directory);
    let path = wide(&directory.to_string_lossy());
    // SAFETY: NUL-terminated path.
    let handle = unsafe { FindFirstChangeNotificationW(path.as_ptr(), 0, FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME) };
    (!handle.is_null() && handle as isize != -1).then_some(Handle(handle))
}

fn run(shared: Arc<Shared>, hooks: Hooks, sink: Sink) {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::Storage::FileSystem::FindNextChangeNotification;
    use windows_sys::Win32::System::Threading::{WaitForMultipleObjects, INFINITE};
    let epoch = Instant::now();
    let ms = || epoch.elapsed().as_millis() as u64;
    let (mut engine, mut slots) = (Engine::default(), Vec::<Slot>::new());
    loop {
        let (wanted, settings) = {
            let mut state = lock(&shared.state);
            let wanted = state.wanted();
            if wanted.is_empty() { state.running = false; return; }
            (wanted, state.settings.clone())
        };
        slots.retain(|s| wanted.contains(&s.watched));
        engine.differ.forget_except(&wanted);
        for w in wanted {
            if !slots.iter().any(|s| s.watched == w) {
                slots.push(Slot { change: open_change(&w.directory), watched: w, stamp: None, primed: false });
            }
        }
        for slot in slots.iter_mut().filter(|s| !s.primed) { slot.primed = true; poll(slot, &mut engine, ms(), &settings); }

        if let Some((notice, suppressed)) = engine.policy.flush(ms(), hooks.privacy.load(Ordering::Relaxed), &*hooks.foreground) {
            let delivery = if suppressed { Delivery::Suppressed("game in foreground") } else { (hooks.deliver)(&notice) };
            sink(Report { notice, delivery });
        }

        let watching: Vec<usize> = (0..slots.len()).filter(|&i| slots[i].change.is_some()).collect();
        let mut handles = vec![shared.control.0];
        handles.extend(watching.iter().filter_map(|&i| slots[i].change.map(|h| h.0)));
        let timeout = engine.policy.deadline().map_or(INFINITE, |d| d.saturating_sub(ms()).clamp(1, u32::MAX as u64 - 1) as u32);
        // SAFETY: every handle stays open until after this call returns.
        let woke = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
        match woke {
            WAIT_TIMEOUT => {}
            w if w == WAIT_OBJECT_0 => {}
            w if w > WAIT_OBJECT_0 && ((w - WAIT_OBJECT_0) as usize) < handles.len() => {
                let slot = &mut slots[watching[(w - WAIT_OBJECT_0) as usize - 1]];
                if let Some(h) = slot.change { unsafe { FindNextChangeNotification(h.0); } }
                poll(slot, &mut engine, ms(), &settings);
            }
            // A failed wait must not spin; a later set_accounts starts a fresh thread.
            _ => { lock(&shared.state).running = false; return; }
        }
    }
}

/// Reads the directory's cache only if the file's (length, mtime) changed since the last
/// look, which folds the several notifications one atomic write raises into one read.
fn poll(slot: &mut Slot, engine: &mut Engine, now: u64, settings: &Settings) {
    let stamp = std::fs::metadata(slot.watched.directory.join("friends.json")).ok().map(|m| (m.len(), m.modified().ok()));
    if stamp.is_none() || stamp == slot.stamp { return; }
    slot.stamp = stamp;
    if let Some((worker, snap)) = crate::session_worker::published_friends(&slot.watched.directory) {
        engine.feed(now, &slot.watched, worker, &snap, settings);
    }
}

// ---- delivery -------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

/// A tray balloon (Shell_NotifyIconW), which Windows 10/11 shows as a notification under
/// Crystal's name. No WinRT, no AppUserModelID registration, nothing written to the registry.
fn deliver(notice: &Notice) -> Delivery {
    match show_balloon(notice) {
        Ok(()) => Delivery::Balloon,
        Err(error) => Delivery::Failed(error),
    }
}

/// Balloon on a throwaway tray icon owned by a message-only window. The icon has to outlive
/// the balloon, so a short-lived thread owns it, removes it after a few seconds, and reports
/// whether the shell accepted it.
fn show_balloon(notice: &Notice) -> Result<(), String> {
    let (title, body) = balloon_text(notice);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("notify-balloon".into()).spawn(move || balloon_thread(&title, &body, tx)).map_err(|e| e.to_string())?;
    rx.recv().map_err(|_| t("The notification icon failed.").to_owned())?
}

/// (balloon title, balloon text). The shell shows no balloon without text, so a notice with
/// no detail puts its title in the text and leaves the balloon title empty: one line, not two.
fn balloon_text(notice: &Notice) -> (String, String) {
    if notice.body.is_empty() { (String::new(), notice.title.clone()) } else { (notice.title.clone(), notice.body.clone()) }
}

fn balloon_thread(title: &str, body: &str, done: std::sync::mpsc::Sender<Result<(), String>>) {
    use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIIF_INFO, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyIcon, DestroyWindow, GetSystemMetrics, LoadImageW, HWND_MESSAGE, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON};
    fn copy(dst: &mut [u16], s: &str) {
        let n = dst.len() - 1;
        for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(n)) { *d = c; }
    }
    let class = wide("STATIC");
    // SAFETY: predefined STATIC class, message-only parent; the window and icon live on this thread.
    unsafe {
        let window = CreateWindowExW(0, class.as_ptr(), std::ptr::null(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
        if window.is_null() { let _ = done.send(Err(t("The notification icon failed.").to_owned())); return; }
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = 1;
        // Crystal's icon (crystal.rc: resource 1) for the notification header and its large
        // image; the stock information icon only if the resource cannot be loaded.
        let instance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
        let small = LoadImageW(instance, 1 as _, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR);
        let large = LoadImageW(instance, 1 as _, IMAGE_ICON, GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON), LR_DEFAULTCOLOR);
        data.uFlags = NIF_ICON;
        data.hIcon = small as _;
        let added = Shell_NotifyIconW(NIM_ADD, &data) != 0;
        data.uFlags = NIF_INFO;
        if large.is_null() {
            data.dwInfoFlags = NIIF_INFO;
        } else {
            data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
            data.hBalloonIcon = large as _;
        }
        copy(&mut data.szInfoTitle, title);
        copy(&mut data.szInfo, body);
        let shown = added && Shell_NotifyIconW(NIM_MODIFY, &data) != 0;
        let _ = done.send(if shown { Ok(()) } else { Err(t("The notification icon failed.").to_owned()) });
        if added {
            // Removing the icon dismisses its balloon, so it stays for the display time.
            std::thread::sleep(std::time::Duration::from_secs(9));
            Shell_NotifyIconW(NIM_DELETE, &data);
        }
        for icon in [small, large] { if !icon.is_null() { DestroyIcon(icon as _); } }
        DestroyWindow(window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::friends::{Friend, Snapshot};
    use std::sync::mpsc;
    use std::time::Duration;

    const W1: (u32, u64) = (10, 1);

    fn friend(name: &str, alternate: bool, online: Option<bool>) -> Friend {
        Friend { name: name.into(), alternate, slot: 0, uid: None, online, location: None, character: None }
    }
    fn snap(friends: Vec<Friend>, unread: Option<u32>, changed: bool) -> Snapshot {
        let mut s = Snapshot { entries: friends, ..Snapshot::default() };
        s.social.inbox_unread = unread;
        s.social.inbox_changed = changed;
        s
    }
    fn never() -> bool { false }

    #[test]
    fn first_snapshot_is_baseline_then_transitions_notify() {
        let mut d = Differ::default();
        assert!(d.observe("a", W1, &snap(vec![friend("Alyria", false, Some(true)), friend("Bob", false, Some(false))], Some(1), false)).is_empty());
        let c = d.observe("a", W1, &snap(vec![friend("Alyria", false, Some(true)), friend("Bob", false, Some(true))], Some(1), false));
        assert_eq!(c, vec![Cause::FriendOnline("Bob".into())]);
        // Already online stays quiet; going offline and back notifies again.
        assert!(d.observe("a", W1, &snap(vec![friend("Bob", false, Some(true))], Some(1), false)).is_empty());
        d.observe("a", W1, &snap(vec![friend("Bob", false, None)], Some(1), false));
        assert_eq!(d.observe("a", W1, &snap(vec![friend("Bob", false, Some(true))], Some(1), false)), vec![Cause::FriendOnline("Bob".into())]);
    }

    #[test]
    fn new_worker_resets_baseline_and_alternates_are_ignored() {
        let mut d = Differ::default();
        d.observe("a", W1, &snap(vec![friend("Bob", false, Some(false))], None, false));
        assert!(d.observe("a", (11, 2), &snap(vec![friend("Bob", false, Some(true))], None, false)).is_empty());
        assert!(d.observe("a", (11, 2), &snap(vec![friend("Bob", false, Some(true)), friend("Alt", true, Some(true))], None, false)).is_empty());
        assert!(d.observe("b", W1, &snap(vec![friend("Alt", true, Some(false))], None, false)).is_empty());
        assert!(d.observe("b", W1, &snap(vec![friend("Alt", true, Some(true))], None, false)).is_empty());
    }

    #[test]
    fn mail_edges() {
        let mut d = Differ::default();
        d.observe("a", W1, &snap(vec![], Some(2), false));
        assert_eq!(d.observe("a", W1, &snap(vec![], Some(3), false)), vec![Cause::NewMail]);
        assert!(d.observe("a", W1, &snap(vec![], Some(3), false)).is_empty());
        assert!(d.observe("a", W1, &snap(vec![], Some(1), false)).is_empty(), "reading mail is not new mail");
        assert_eq!(d.observe("a", W1, &snap(vec![], Some(1), true)), vec![Cause::NewMail]);
        assert!(d.observe("a", W1, &snap(vec![], Some(1), true)).is_empty(), "level, not edge");
        // Both signals at once are one notification; an unknown earlier count is not a rise.
        assert_eq!(d.observe("a", W1, &snap(vec![], Some(5), false)).len(), 1);
        let mut e = Differ::default();
        e.observe("a", W1, &snap(vec![], None, false));
        assert!(e.observe("a", W1, &snap(vec![], Some(9), false)).is_empty());
    }

    #[test]
    fn friends_coalesce_into_one_notification_and_throttle() {
        let mut p = Policy::default();
        p.push_friend(0, "a", "Alyria");
        p.push_friend(200, "a", "Bob");
        p.push_friend(900, "b", "Cleo");
        assert_eq!(p.deadline(), Some(1_500));
        assert!(p.flush(1_000, false, &never).is_none(), "still gathering");
        let (n, suppressed) = p.flush(1_500, false, &never).unwrap();
        assert!(!suppressed);
        assert_eq!(n.title, "Alyria and 2 others are online");
        assert_eq!(n.account_key, None);
        // Within 10 s of a notification nothing else goes out; afterwards it does.
        p.push_friend(2_000, "a", "Dan");
        assert!(p.flush(5_000, false, &never).is_none());
        assert_eq!(p.deadline(), Some(11_500));
        assert_eq!(p.flush(11_500, false, &never).unwrap().0.title, "Dan is online");
    }

    #[test]
    fn friend_repeats_wait_fifteen_minutes_and_mail_two() {
        let mut p = Policy::default();
        p.push_friend(0, "a", "Bob");
        assert!(p.flush(2_000, false, &never).is_some());
        p.push_friend(20_000, "a", "BOB");
        assert!(p.friends.is_empty());
        p.push_friend(15 * 60_000 + 2_001, "a", "Bob");
        assert_eq!(p.friends.len(), 1);
        p.flush(15 * 60_000 + 4_000, false, &never);

        let mut m = Policy::default();
        m.push_mail(0, "a", "Main");
        m.push_mail(1, "a", "Main");
        assert_eq!(m.mail.len(), 1, "pending mail dedupes per account");
        assert_eq!(m.flush(5, false, &never).unwrap().0.kind, Kind::Mail);
        m.push_mail(60_000, "a", "Main");
        assert!(m.mail.is_empty());
        m.push_mail(125_000, "a", "Main");
        assert_eq!(m.mail.len(), 1);
        // Mail from a second account still waits out the global window.
        m.push_mail(125_000, "b", "Alt");
        assert!(m.flush(125_000, false, &never).is_some());
        assert!(m.flush(126_000, false, &never).is_none());
        assert_eq!(m.deadline(), Some(135_000));
    }

    #[test]
    fn foreground_drops_without_queueing_or_counting() {
        let mut p = Policy::default();
        p.push_friend(0, "a", "Bob");
        let (n, suppressed) = p.flush(2_000, false, &|| true).unwrap();
        assert!(suppressed);
        assert_eq!(n.title, "Bob is online");
        assert!(p.flush(2_001, false, &never).is_none(), "dropped, not queued");
        assert_eq!(p.deadline(), None);
        p.push_friend(2_002, "a", "Bob");
        assert!(p.flush(4_000, false, &never).is_some(), "a suppressed notification used no throttle budget");
    }

    #[test]
    fn privacy_text_has_no_names() {
        let names = vec!["Alyria".to_string(), "Bob".to_string()];
        let one = friends_notice(&names[..1], Some("acct-key".into()), true);
        let many = friends_notice(&names, Some("acct-key".into()), true);
        let mail = mail_notice("acct-key", "Main Account", true);
        assert_eq!(one.title, "A friend is online");
        assert_eq!(many.title, "2 friends are online");
        assert_eq!(mail.title, "New PlayOnline mail");
        for n in [&one, &many, &mail] {
            let all = format!("{} {} {:?}", n.title, n.body, n.account_key);
            assert!(!all.contains("Alyria") && !all.contains("Bob") && !all.contains("Main") && !all.contains("acct"), "{all}");
        }
        assert_eq!(mail_notice("k", "Main Account", false).body, "Mail for Main Account");
    }

    #[test]
    fn balloon_never_repeats_the_title_as_its_text() {
        let friend = friends_notice(&["Fraude".to_string()], None, false);
        assert_eq!(balloon_text(&friend), (String::new(), "Fraude is online".to_string()));
        let mail = mail_notice("k", "Main Account", false);
        assert_eq!(balloon_text(&mail), ("New PlayOnline mail".to_string(), "Mail for Main Account".to_string()));
        let private = mail_notice("k", "Main Account", true);
        assert_eq!(balloon_text(&private), (String::new(), "New PlayOnline mail".to_string()));
    }



    #[test]
    fn settings_defaults_and_back_compat() {
        assert_eq!(Settings::default(), Settings { enabled: true, mail: true, friends_online: true });
        let old: Settings = serde_json::from_str(r#"{"mail":false}"#).unwrap();
        assert_eq!(old, Settings { enabled: true, mail: false, friends_online: true });
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
        assert!(!Settings { enabled: true, mail: false, friends_online: false }.active());
        assert!(!Settings { enabled: false, ..Settings::default() }.active());
    }

    #[test]
    fn disabled_kinds_are_not_queued() {
        let mut e = Engine::default();
        let w = Watched { account_key: "a".into(), label: "A".into(), directory: PathBuf::new() };
        let off = Settings { enabled: true, mail: false, friends_online: false };
        e.feed(0, &w, W1, &snap(vec![friend("Bob", false, Some(false))], Some(0), false), &off);
        e.feed(1, &w, W1, &snap(vec![friend("Bob", false, Some(true))], Some(1), false), &off);
        assert_eq!(e.policy.deadline(), None);
    }

    fn harness() -> (Watcher, mpsc::Receiver<Report>, PathBuf) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let hooks = Hooks { foreground: Arc::new(never), privacy: Arc::new(AtomicBool::new(false)), deliver: Arc::new(|_| Delivery::Balloon) };
        let watcher = Watcher::start(Arc::new(move |r| { let _ = lock(&tx).send(r); }), hooks, Settings::default());
        let dir = std::env::temp_dir().join(format!("crystal-notify-{}", crate::backend::unique_id().unwrap()));
        (watcher, rx, dir)
    }
    fn watched(dir: &PathBuf) -> Vec<Watched> { vec![Watched { account_key: "acct".into(), label: "Main".into(), directory: dir.clone() }] }
    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !f() { assert!(Instant::now() < end, "{what}"); std::thread::sleep(Duration::from_millis(10)); }
    }

    #[test]
    fn watcher_reports_new_mail_from_a_published_file_and_ends_when_emptied() {
        let (watcher, rx, dir) = harness();
        crate::session_worker::publish_friends(&dir, &snap(vec![], Some(1), false)).unwrap();
        watcher.set_accounts(watched(&dir));
        wait_until("thread starts", || watcher.is_running());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "the existing file is only the baseline");
        crate::session_worker::publish_friends(&dir, &snap(vec![], Some(2), false)).unwrap();
        let report = rx.recv_timeout(Duration::from_secs(2)).expect("a report after the file changed");
        assert_eq!(report.notice.kind, Kind::Mail);
        assert_eq!(report.notice.body, "Mail for Main");
        assert_eq!(report.delivery, Delivery::Balloon);
        watcher.set_accounts(Vec::new());
        wait_until("thread ends", || !watcher.is_running());
        // A new set starts a fresh thread; turning notifications off ends it again.
        watcher.set_accounts(watched(&dir));
        wait_until("thread restarts", || watcher.is_running());
        watcher.apply(&Settings { enabled: false, ..Settings::default() });
        wait_until("thread ends when disabled", || !watcher.is_running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watcher_gathers_a_friend_coming_online() {
        let (watcher, rx, dir) = harness();
        crate::session_worker::publish_friends(&dir, &snap(vec![friend("Bob", false, Some(false))], None, false)).unwrap();
        watcher.set_accounts(watched(&dir));
        wait_until("thread starts", || watcher.is_running());
        // `running` is set before the thread reads its baseline: give it that read first, or a
        // change written meanwhile would itself become the baseline.
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "the existing file is only the baseline");
        crate::session_worker::publish_friends(&dir, &snap(vec![friend("Bob", false, Some(true))], None, false)).unwrap();
        let report = rx.recv_timeout(Duration::from_secs(4)).expect("a friend notice");
        assert_eq!(report.notice.title, "Bob is online");
        assert_eq!(report.notice.account_key.as_deref(), Some("acct"));
        watcher.stop();
        wait_until("thread ends", || !watcher.is_running());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
