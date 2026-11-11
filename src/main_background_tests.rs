use super::*;

fn live_app(name: &str) -> App {
    let mut app = tests::app();
    app.preview = false;
    app.config.experiment = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
        .join(format!("crystal-background-{name}-{}", backend::unique_id().unwrap()));
    app.window = Some(WindowId::unique());
    app
}

#[test]
fn the_close_button_hides_to_the_tray_only_when_settings_say_so() {
    let mut app = live_app("hide");
    app.settings.close_to_tray = true;
    let _ = app.update(Message::CloseRequested);
    assert!(app.window.is_none(), "the window closed");
    assert!(!app.closing, "Crystal keeps running: sessions stay signed in");
    // From the icon: back again, with a new window.
    let _ = app.update(Message::Tray(tray::Command::Open));
    assert!(app.window.is_some() && !app.closing);

    let mut app = live_app("quit");
    let _ = app.update(Message::CloseRequested);
    assert!(app.closing, "off (the default): closing quits as before");
    assert!(app.window.is_none());
}

#[test]
fn quitting_from_the_icon_while_something_holds_it_opens_the_window_to_say_why() {
    let mut app = live_app("held-quit");
    app.settings.close_to_tray = true;
    let _ = app.update(Message::CloseRequested);
    assert!(app.window.is_none());
    app.vault_saving = true;
    let _ = app.update(Message::Tray(tray::Command::Quit));
    assert!(!app.closing && app.confirm_close);
    assert!(app.window.is_some(), "the confirmation is visible");
    app.vault_saving = false;
    let _ = app.update(Message::Tray(tray::Command::Quit));
    assert!(app.closing);
}

#[test]
fn an_update_handshake_is_never_hidden_away() {
    let mut app = live_app("applying");
    app.settings.close_to_tray = true;
    app.self_update = self_update::State::Applying;
    let _ = app.update(Message::CloseRequested);
    assert!(app.window.is_some() && !app.closing);
    assert!(app.status.contains("update helper"));
}

/// A signed-in session at its idle main menu for the first retail account, whose worker is
/// this test (`take_social` / `publish_friends` play the session worker), and the window
/// on the Friends page once its opening read asked for the Inbox.
fn friends_session(name: &str) -> (App, PathBuf, friends::Snapshot) {
    let mut app = live_app(name);
    let row = app.accounts.iter().position(|r| r.account.mode == AccountMode::Retail).unwrap();
    let account = app.accounts[row].account.clone();
    let config = app.settings.config_for(&app.config, &account);
    let directory = config.account_directory(&account);
    session_worker::save_live_record_for_test(&config, &account);
    let mut snapshot = friends::Snapshot { game_running: Some(false), fetched_at: 1, ..Default::default() };
    snapshot.social.ready = true;
    session_worker::publish_friends(&directory, &snapshot).unwrap();
    app.reset_friends(Some(app.accounts[row].key.clone()));
    let _ = app.update(Message::Navigate(Page::Friends));
    let started = Instant::now();
    while app.friends.pending.is_none() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
        let _ = app.update(Message::Tick);
    }
    assert!(app.friends.pending.is_some(), "opening the page lists the Inbox: {}", app.friends.error);
    // The read the request started has finished: what follows decides what is seen.
    tick_until(&mut app, |app| !app.friends.loading);
    (app, directory, snapshot)
}

/// The session worker takes the request and PlayOnline answers it at once: busy, then
/// result 0 (accepted) and result 1 (the page) — live 2026-10-22 11:17:46, one second.
fn answer_inbox(directory: &Path, snapshot: &mut friends::Snapshot) {
    let (request, _lock) = session_worker::take_social(directory).expect("the request reached the worker");
    snapshot.social.begin(request.id.clone(), &request.request);
    session_worker::publish_friends(directory, snapshot).unwrap();
    snapshot.social.apply(&request.id, crate::social::Action::Inbox, 0, None).unwrap();
    session_worker::publish_friends(directory, snapshot).unwrap();
    snapshot.social.apply(&request.id, crate::social::Action::Inbox, 1, Some(&[0; 12 + 32 * 72])).unwrap();
    session_worker::publish_friends(directory, snapshot).unwrap();
}

fn tick_until(app: &mut App, done: impl Fn(&App) -> bool) {
    let started = Instant::now();
    while !done(app) && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
        let _ = app.update(Message::Tick);
    }
}

#[test]
fn opening_friends_lists_the_inbox_and_sees_it_complete() {
    let (mut app, directory, mut snapshot) = friends_session("friends-open");
    answer_inbox(&directory, &mut snapshot);
    tick_until(&mut app, |app| app.friends.pending.is_none());
    assert!(app.friends.pending.is_none() && app.friends.error.is_empty(), "{}", app.friends.error);
    assert!(app.friends.mail.listed[0].is_some());
}

/// Live 2026-10-22: Friends opened after FFXI returned, the Inbox listing finished in the
/// same second (results 0 then 1, the cache idle with its id), yet the page later said
/// PlayOnline had not reported completion. The limit was checked before anything read
/// what had arrived: a window that had not looked for 45 s called a finished request lost.
#[test]
fn a_finished_request_is_not_reported_missing_when_the_window_looks_late() {
    let (mut app, directory, mut snapshot) = friends_session("friends-late");
    answer_inbox(&directory, &mut snapshot);
    // Nothing read the session since: the window looks again only now, past the limit.
    let id = app.friends.pending.as_ref().unwrap().0.clone();
    app.friends.pending = Some((id, Instant::now() - FRIENDS_REQUEST_LIMIT - Duration::from_secs(1)));
    let _ = app.update(Message::Tick);
    assert!(app.friends.error.is_empty(), "{}", app.friends.error);
    assert!(app.friends.pending.is_none() && !app.friends.mail.stalled);
    assert!(app.friends.mail.listed[0].is_some(), "the listing it finished is learned");
}

/// Leaving the page stopped the reads while the limit kept running on every page.
#[test]
fn a_request_is_followed_after_leaving_the_friends_page() {
    let (mut app, directory, mut snapshot) = friends_session("friends-left");
    let _ = app.update(Message::Navigate(Page::Home));
    tick_until(&mut app, |app| !app.friends.loading);
    answer_inbox(&directory, &mut snapshot);
    tick_until(&mut app, |app| app.friends.pending.is_none());
    assert!(app.friends.pending.is_none() && app.friends.error.is_empty(), "{}", app.friends.error);
    assert!(app.friends.mail.listed[0].is_some());
    // Off the page nothing new is asked for.
    app.friends.snapshot.as_mut().unwrap().social.inbox_changed = true;
    app.worker_event(WorkerEvent::Friends(app.friends_generation, app.friends.account.clone().unwrap(), true, app.friends.snapshot.clone()));
    assert!(app.friends.pending.is_none());
}

/// A request PlayOnline really has not finished still says so at the limit.
#[test]
fn an_unanswered_request_still_reports_at_the_limit() {
    let (mut app, directory, mut snapshot) = friends_session("friends-unanswered");
    let (request, lock) = session_worker::take_social(&directory).unwrap();
    snapshot.social.begin(request.id.clone(), &request.request);
    session_worker::publish_friends(&directory, &snapshot).unwrap();
    drop(lock);
    let id = app.friends.pending.as_ref().unwrap().0.clone();
    app.friends.pending = Some((id, Instant::now() - FRIENDS_REQUEST_LIMIT - Duration::from_secs(1)));
    let _ = app.update(Message::Tick);
    assert!(app.friends.pending.is_none() && app.friends.mail.stalled);
    assert!(app.friends.error.contains("has not reported completion"), "{}", app.friends.error);
}

#[test]
fn the_idle_tick_is_quiet_only_with_no_timed_work() {
    let mut app = tests::app();
    app.startup_signin_pending = false;
    app.startup_waiting.clear();
    assert!(app.tick_quiet());
    app.page = Page::Friends;
    assert!(!app.tick_quiet(), "the Friends page polls its list");
    app.page = Page::Home;
    app.startup_signin_pending = true;
    assert!(!app.tick_quiet(), "startup sign-ins wait for the vault");
    app.startup_signin_pending = false;
    app.parks_settled = false;
    assert!(!app.tick_quiet(), "a park to start");
    app.parks_settled = true;
    app.accounts[0].resign = Some(Resign { generation: 0, ended: Instant::now(), free_since: None, network: false, host: None });
    assert!(!app.tick_quiet(), "a sign-in after the game");
    app.accounts[0].resign = None;
    app.accounts[0].queued_play = Some(Instant::now());
    assert!(!app.tick_quiet(), "a queued Play");
    app.accounts[0].queued_play = None;
    assert!(app.tick_quiet());
}

#[test]
fn a_quiet_window_watches_each_session_and_game_flag() {
    let mut app = tests::app();
    let alive = Arc::new(AtomicBool::new(true));
    let game = Arc::new(AtomicBool::new(false));
    app.accounts[0].alive = Some(alive.clone());
    app.accounts[0].game_running = Some(game.clone());
    let flags = app.watched_flags();
    assert_eq!(flags.len(), 2);
    assert!(flags.iter().any(|(flag, seen)| Arc::ptr_eq(flag, &alive) && *seen));
    assert!(flags.iter().any(|(flag, seen)| Arc::ptr_eq(flag, &game) && !*seen));
}

#[test]
fn background_settings_are_off_by_default_and_saved_only_when_on() {
    let settings = Settings::default();
    assert!(!settings.close_to_tray && !settings.start_with_windows);
    let json = serde_json::to_string(&settings).unwrap();
    assert!(!json.contains("close_to_tray") && !json.contains("start_with_windows"));
    let on = Settings { close_to_tray: true, start_with_windows: true, ..Settings::default() };
    let back: Settings = serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
    assert!(back.close_to_tray && back.start_with_windows);
}

#[test]
fn start_with_windows_waits_for_its_task_and_keeps_the_switch_on_failure() {
    let mut app = live_app("startup-task");
    let task = app.update(Message::StartWithWindows(true));
    assert!(task.units() > 0 && app.startup_task_busy());
    assert!(app.update(Message::StartWithWindows(false)).units() == 0, "one change at a time");
    let _ = app.update(Message::StartupTaskSet(true, Err("refused".into())));
    assert!(!app.settings.start_with_windows && !app.startup_task_busy());
    assert_eq!(app.status, "refused");
    let _ = app.update(Message::StartupTaskSet(true, Ok(())));
    assert!(app.settings.start_with_windows);
}
