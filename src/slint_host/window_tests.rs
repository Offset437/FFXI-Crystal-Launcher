//! The whole window, headless: what each page says for a given core state, and privacy mode
//! (with it on, no account name or ID, friend, character, mail subject or message is readable
//! in any text the window holds, on any page; with it off the same texts read normally).
use super::sync::headless;
use super::MainWindow;
use crate::i18n::{t, tf, Lang};
use crate::{friends::Pane, operation, services, App, MaintenanceKind, Message, Page};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::items::{ComplexText, SimpleText, TextInput};
use i_slint_core::window::WindowInner;
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::ComponentHandle;
use std::rc::Rc;
use std::time::{Duration, Instant};

const SECRETS: [&str; 15] = [
    "Main account", "Second account", "Crafting account", "ABC12340", "ABC12341",
    "Alyria", "Balthier", "Celestine", "Haruko", "Nocturne", "Mallory", "Rook", "Lyrielle", "Ardent",
    "Rhapsodies",
];

/// Every text the window holds now (labels and fields), in tree order.
fn texts(window: &slint::Window) -> Vec<String> {
    let inner = WindowInner::from_pub(window);
    inner.ensure_tree_instantiated();
    let start = ItemRc::new_root(inner.component());
    let mut item = start.clone();
    let mut found = Vec::new();
    for _ in 0..100_000 {
        if item.is_visible() {
            let text = if let Some(t) = item.downcast::<SimpleText>() {
                Some(t.as_pin_ref().text())
            } else if let Some(t) = item.downcast::<ComplexText>() {
                Some(t.as_pin_ref().text())
            } else {
                item.downcast::<TextInput>().map(|t| t.as_pin_ref().text())
            };
            found.extend(text.map(|t| t.to_string()).filter(|t| !t.is_empty()));
        }
        item = item.next_focus_item();
        if item == start { break; }
    }
    found
}

/// The real window, wired, on this thread's headless 1440 x 900 platform.
fn main_window() -> (MainWindow, Rc<MinimalSoftwareWindow>) {
    let window = headless::window(1440, 900);
    let ui = MainWindow::new().unwrap();
    super::view::wire(&ui);
    super::friends::wire(&ui);
    super::downloads::wire(&ui);
    super::settings::wire(&ui);
    super::features::wire(&ui);
    super::dialogs::wire(&ui);
    ui.show().unwrap();
    (ui, window)
}

/// Syncs every page's view model to `app` and draws a frame (which builds the shown items).
fn render(ui: &MainWindow, window: &MinimalSoftwareWindow, app: &App) -> Vec<String> {
    super::view::sync(ui, app);
    super::friends::sync(ui, app);
    super::downloads::sync(ui, app);
    super::settings::sync(ui, app);
    super::features::sync(ui, app);
    super::dialogs::sync(ui, app);
    headless::draw(window);
    texts(ui.window())
}

fn app(page: Page, pane: Option<Pane>, mailbox: bool, in_game: bool) -> App {
    let mut app = crate::tests::app();
    app.load_friends_preview(mailbox, in_game, true, false);
    app.page = page;
    if let Some(pane) = pane { app.friends.select(Some(pane)); }
    app.status = "Main account: Alyria sent a message".into();
    app
}

#[test]
fn privacy_mode_masks_every_identity_on_every_page() {
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let pages = [
        (Page::Home, None, false, false),
        (Page::Friends, None, false, false),
        (Page::Friends, None, false, true),
        (Page::Friends, Some(Pane::Mailbox), true, false),
        (Page::Friends, Some(Pane::Blocked), false, false),
        (Page::Maintenance, None, false, false),
        (Page::Settings, None, false, false),
    ];
    crate::privacy::set(true);
    let mask = crate::privacy::mask("x");
    for (case, (page, pane, mailbox, in_game)) in pages.into_iter().enumerate() {
        let names_shown = page == Page::Home || matches!(pane, Some(Pane::Blocked));
        let shown = render(&ui, &window, &app(page, pane, mailbox, in_game));
        assert!(shown.len() > 10, "case {case} rendered: {shown:?}");
        for secret in SECRETS {
            assert!(!shown.iter().any(|text| text.contains(secret)), "case {case} shows {secret:?}: {shown:?}");
        }
        if names_shown {
            assert!(shown.iter().any(|text| *text == mask), "case {case} shows the names masked: {shown:?}");
        }
    }
    crate::privacy::set(false);
    let home = render(&ui, &window, &app(Page::Home, None, false, false));
    assert!(home.iter().any(|text| text == "Main account"), "{home:?}");
    let friends = render(&ui, &window, &app(Page::Friends, None, false, false));
    assert!(friends.iter().any(|text| text == "Alyria") && friends.iter().any(|text| text.contains("Rhapsodies")), "{friends:?}");
}

/// The account sheet's authenticator key: a saved key is masked and read-only until Change,
/// then Show reveals it, except in privacy mode, where it stays masked. Its caption and note
/// show for retail accounts, in English and Japanese.
#[test]
fn the_authenticator_key_is_masked_unless_shown_and_always_in_privacy_mode() {
    crate::privacy::set(false);
    let (ui, window) = main_window();
    for lang in Lang::ALL {
        crate::i18n::set(lang);
        let mut app = crate::tests::app();
        app.accounts[0].account.totp_key = "JBSWY3DPEHPK3PXP".into();
        let key = app.accounts[0].key.clone();
        let _ = app.update(Message::EditAccount(key));
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text == t("Authenticator key")), "{shown:?}");
        assert!(shown.iter().any(|text| text == t("From Square Enix's authenticator setup. Codes are filled in for you.")), "{shown:?}");
        let sheet = || ui.global::<super::View>().get_sheet();
        assert!(sheet().totp_key_masked && !sheet().totp_key_enabled && sheet().totp_key_button == t("Change"));
        let _ = app.update(Message::EditorFlag(crate::EditorFlag::ChangeTotpKey, true));
        render(&ui, &window, &app);
        assert!(sheet().totp_key_masked && sheet().totp_key_enabled && sheet().totp_key_button == t("Show"));
        let _ = app.update(Message::EditorFlag(crate::EditorFlag::ShowTotpKey, true));
        render(&ui, &window, &app);
        assert!(!sheet().totp_key_masked && sheet().totp_key_button == t("Hide"));
        crate::privacy::set(true);
        render(&ui, &window, &app);
        assert!(sheet().totp_key_masked, "privacy mode masks a shown key");
        crate::privacy::set(false);
        // An account without a key takes one at once.
        let mut app = crate::tests::app();
        let key = app.accounts[1].key.clone();
        let _ = app.update(Message::EditAccount(key));
        render(&ui, &window, &app);
        assert!(sheet().totp_key_masked && sheet().totp_key_enabled && sheet().totp_key_button == t("Show"));
        let _ = app.update(Message::CloseEditor);
        assert!(render(&ui, &window, &app).iter().all(|text| !text.contains("JBSWY3DPEHPK3PXP")), "a closed sheet keeps no key");
    }
    crate::i18n::set(Lang::En);
}

/// The layout zooms only below the 1440 px design width, and no further than 0.8.
#[test]
fn scale_reflows_above_design_width_and_shrinks_below() {
    let mut app = crate::tests::app();
    for (width, expected) in [(2560.0, 1.0), (1920.0, 1.0), (1440.0, 1.0), (1296.0, 0.9), (1080.0, 0.8), (900.0, 0.8)] {
        app.viewport = crate::effect::Size::new(width, 900.0);
        assert!((super::scale(&app) - expected).abs() < 1e-6, "{width}: {}", super::scale(&app));
    }
}

#[test]
fn main_button_has_update_available_and_maintenance_labels_in_both_languages() {
    crate::privacy::set(false);
    let (ui, window) = main_window();
    for lang in Lang::ALL {
        crate::i18n::set(lang);
        let mut app = crate::tests::app();
        for width in [900.0, 1440.0] {
            app.viewport = crate::effect::Size::new(width, 900.0);
            app.services.as_mut().unwrap().availability = services::Availability::Unknown;
            app.patch.as_mut().unwrap().state = crate::patch::PatchState::Required;
            let shown = render(&ui, &window, &app);
            assert!(shown.iter().any(|line| line == t("Update available")), "{shown:?}");
            app.patch.as_mut().unwrap().state = crate::patch::PatchState::Current;
            app.services.as_mut().unwrap().availability = services::Availability::Maintenance;
            let shown = render(&ui, &window, &app);
            assert!(shown.iter().any(|line| line == t("Maintenance")), "{shown:?}");
            let _ = app.update(Message::MainAction);
            let shown = render(&ui, &window, &app);
            assert!(shown.iter().any(|line| line == t("Maintenance notices")), "{shown:?}");
            app.show_details = false;
        }
    }
    crate::i18n::set(Lang::En);
}

/// The button, the row's headline and Cancel, and the update's progress, in both languages.
#[test]
fn wait_and_play_is_shown_in_both_languages() {
    crate::privacy::set(false);
    let (ui, window) = main_window();
    for lang in Lang::ALL {
        crate::i18n::set(lang);
        let mut app = crate::tests::app();
        app.services.as_mut().unwrap().availability = services::Availability::Maintenance;
        for row in &mut app.accounts { row.selected = false; }
        app.accounts[0].selected = true;
        let key = app.accounts[0].key.clone();
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|line| line == t("Wait and play")), "{shown:?}");
        app.waiting_plays.push(crate::WaitingPlay { keys: vec![key.clone()], otp: Vec::new(), armed: Instant::now(), codes: Vec::new() });
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|line| line == t("Waiting for FFXI…")), "{shown:?}");
        assert!(shown.iter().any(|line| line == t("FFXI is down for maintenance. Starting as soon as it's back…")), "{shown:?}");
        assert!(shown.iter().any(|line| line == t("Cancel")), "{shown:?}");
        // The update a Play runs in its session: its progress and Cancel on the row.
        app.waiting_plays.clear();
        app.play_updates.begin(std::path::PathBuf::from(r"C:\fixture\FINAL FANTASY XI"), &key, "30261006_0".into(), Instant::now(), Vec::new(), false);
        let line = crate::play_update::line(&app, &key).unwrap();
        assert!(line.headline.starts_with(t("Updating FINAL FANTASY XI")), "{}", line.headline);
        let shown = render(&ui, &window, &app);
        assert!(shown.contains(&line.headline), "{shown:?}");
        assert!(shown.iter().any(|text| text == t("Cancel")), "{shown:?}");
        app.play_updates.jobs[0].blocked = Some(Some("Alice".into()));
        let shown = render(&ui, &window, &app);
        let ready = tf("Update ready · waiting for FINAL FANTASY XI to close on {name}", &[("name", &"Alice")]);
        assert!(shown.contains(&ready), "{shown:?}");
    }
    crate::i18n::set(Lang::En);
}

/// Each account row's drag grip, as its centre in window coordinates, in row order.
fn grips(window: &slint::Window) -> Vec<slint::LogicalPosition> {
    let inner = WindowInner::from_pub(window);
    inner.ensure_tree_instantiated();
    let start = ItemRc::new_root(inner.component());
    let mut item = start.clone();
    let mut found = Vec::new();
    for _ in 0..100_000 {
        if item.is_visible() {
            if let Some(area) = item.downcast::<i_slint_core::items::TouchArea>() {
                if format!("{:?}", area.as_pin_ref().mouse_cursor()).contains("Grab") {
                    let geometry = item.geometry();
                    let at = item.map_to_window(geometry.origin);
                    found.push(slint::LogicalPosition::new(at.x + geometry.width() / 2.0, at.y + geometry.height() / 2.0));
                }
            }
        }
        item = item.next_focus_item();
        if item == start { break; }
    }
    found.sort_by(|a, b| a.y.total_cmp(&b.y));
    found
}

/// Runs the window's change handlers, hands what the view sent to the core and syncs back,
/// until nothing more is sent (the host's drain, without a host).
fn pump(ui: &MainWindow, window: &MinimalSoftwareWindow, app: &mut App) {
    for _ in 0..8 {
        slint::platform::update_timers_and_animations();
        let sent = super::take_sent();
        if sent.is_empty() { break; }
        for message in sent { let _ = app.update(message); }
        render(ui, window, app);
    }
}

/// A real pointer drag on the window: press `from`'s grip, move in small steps to `to`'s row,
/// release. The host's winit listener ends the drag on a left release; it is run here too.
fn drag_rows(ui: &MainWindow, window: &MinimalSoftwareWindow, app: &mut App, from: usize, to: usize, frame: Option<&mut dyn FnMut(&App)>) {
    use slint::platform::{PointerEventButton, WindowEvent};
    let grips = grips(ui.window());
    assert!(grips.len() > from.max(to), "one grip per row: {grips:?}");
    let (start, end) = (grips[from], grips[to]);
    let dispatch = |event| ui.window().dispatch_event(event);
    dispatch(WindowEvent::PointerMoved { position: start });
    dispatch(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    pump(ui, window, app);
    let steps = 12;
    for step in 1..=steps {
        let y = start.y + (end.y - start.y) * step as f32 / steps as f32;
        dispatch(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(start.x + 3.0, y) });
        pump(ui, window, app);
    }
    if let Some(frame) = frame { frame(app); }
    dispatch(WindowEvent::PointerReleased { position: slint::LogicalPosition::new(start.x + 3.0, end.y), button: PointerEventButton::Left });
    if app.schedule().account_drag { let _ = app.update(Message::AccountDragFinish); }
    pump(ui, window, app);
    render(ui, window, app);
}

/// The window as it is now, into a PNG (CRYSTAL_DRAG_SHOTS: a mid-drag frame).
fn save_png(window: &MinimalSoftwareWindow, width: u32, height: u32, path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| { renderer.render(&mut pixels, width as usize); });
    let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    imagecrate::save_buffer_with_format(path, &bytes, width, height, imagecrate::ExtendedColorType::Rgb8, imagecrate::ImageFormat::Png).unwrap();
}

fn order(app: &App) -> Vec<String> { app.accounts.iter().map(|row| row.key.clone()).collect() }

/// The owner's 1.1.0 bug: dragging a row's grip moved nothing. A real press, move and release
/// on the window reorders the rows, with the few rows that fit and with a list that scrolls.
#[test]
fn dragging_a_grip_onto_another_row_reorders_the_accounts() {
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    // 3 rows in a tall wide window: nothing scrolls, the only case 1.1.0 handled. 14 rows wide:
    // the account list scrolls. A narrow, short window: the page scrolls.
    for (count, width, height) in [(3usize, 1440, 900), (14, 1440, 900), (14, 1440, 650), (3, 900, 650), (14, 900, 650)] {
        window.set_size(slint::PhysicalSize::new(width, height));
        let mut app = app(Page::Home, None, false, false);
        if count > app.accounts.len() {
            let mut values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
            for i in values.len()..count {
                let mut extra = values[0].clone();
                extra.label = format!("Extra {i}");
                extra.pol_id = format!("XYZ{i:05}");
                values.push(extra);
            }
            app.replace_accounts(values);
        }
        app.viewport = crate::effect::Size::new(width as f32, height as f32);
        ui.global::<super::Theme>().set_scale(super::scale(&app));
        render(&ui, &window, &app);
        // Wheel the page down at its right edge (clear of the list and above the footer) until
        // the first three rows show, as a user would.
        for _ in 0..60 {
            let shown = grips(ui.window());
            // Clear of the footer.
            if shown.len() >= 3 && shown[2].y < height as f32 - 110.0 { break; }
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(width as f32 - 20.0, height as f32 - 120.0), delta_x: 0.0, delta_y: -40.0,
            });
            render(&ui, &window, &app);
        }
        eprintln!("case: {count} rows at {width}x{height}: grips {:?}", grips(ui.window()));
        let original = order(&app);
        assert_eq!(original.len(), count);
        // Down: the first row onto the third.
        let mut seen = None;
        let shot = std::env::var_os("CRYSTAL_DRAG_SHOTS").map(|dir| std::path::PathBuf::from(dir).join(format!("drag-{count}-rows-{width}x{height}.png")));
        drag_rows(&ui, &window, &mut app, 0, 2, Some(&mut |app: &App| {
            seen = Some((app.account_drag.source.clone(), order(app)));
            if let Some(path) = &shot { save_png(&window, width, height, path); }
        }));
        let (source, during) = seen.unwrap();
        assert_eq!(source.as_ref(), Some(&original[0]), "{count} rows at {height}: the press started the drag");
        let mut expected = original.clone();
        let moved = expected.remove(0);
        expected.insert(2, moved);
        assert_eq!(during, expected, "{count} rows at {height}: the rows move while dragging");
        assert_eq!(order(&app), expected, "{count} rows at {height}: the drop keeps the order");
        assert!(app.account_drag.source.is_none());
        assert_eq!(app.settings.account_order, expected, "{count} rows at {height}: the drop saves the order");
        // Up: the third row onto the first (the rows a scrolled-to-the-top list always shows).
        drag_rows(&ui, &window, &mut app, 2, 0, None);
        let moved = expected.remove(2);
        expected.insert(0, moved);
        assert_eq!(order(&app), expected, "{count} rows at {height}: dragging up");
        assert_eq!(app.settings.account_order, expected);
    }
}

fn updating(app: &mut App, progress: operation::Progress) {
    app.page = Page::Maintenance;
    app.maintenance = Some(MaintenanceKind::Update);
    app.maintenance_progress = progress;
}

#[test]
fn update_activity_shows_remaining_time_and_clears_it_when_cancelled() {
    crate::privacy::set(false);
    let (ui, window) = main_window();
    for (language, estimating) in [(Lang::En, "Estimating time remaining…"), (Lang::Ja, "残り時間を計算しています…")] {
        crate::i18n::set(language);
        // Time left only: the share is the panel's header figure.
        let estimate = tf("{time} left", &[("time", &crate::play_update::remaining_time(Duration::from_secs(115)))]);
        let estimate = estimate.as_str();
        let mut app = crate::tests::app();
        updating(&mut app, operation::Progress {
            stage: operation::Stage::Download, done: 42_000_000, total: 120_000_000,
            bytes: true, remaining: Some(Duration::from_secs(115)),
            ..operation::Progress::default()
        });
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text == estimate), "{shown:?}");
        assert!(!shown.iter().any(|text| text.contains("65%")), "no share repeated under the header: {shown:?}");
        assert!(shown.iter().any(|text| text.contains("42 MB")), "byte progress keeps its units: {shown:?}");
        app.maintenance_progress.remaining = None;
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text.contains(estimating)), "{shown:?}");
        app.maintenance_cancelled = true;
        let shown = render(&ui, &window, &app);
        assert!(!shown.iter().any(|text| text.contains(estimating) || text.contains(estimate)), "{shown:?}");
        app.maintenance_cancelled = false;
        app.maintenance = Some(MaintenanceKind::Setup);
        app.maintenance_progress = operation::Progress::default();
        let shown = render(&ui, &window, &app);
        assert!(!shown.iter().any(|text| text.contains(estimating)), "{shown:?}");
    }
    crate::i18n::set(Lang::En);
}

#[test]
fn update_activity_uses_the_whole_update_estimate_above_current_step() {
    crate::privacy::set(false);
    let (ui, window) = main_window();
    for (language, title, calculating, estimating, finalizing) in [
        (Lang::En, "Overall update", "Calculating total update size…", "Estimating total time remaining…", "Finalizing update…"),
        (Lang::Ja, "アップデート全体", "アップデート全体のサイズを計算しています…", "アップデート全体の残り時間を計算しています…", "アップデートの最終処理を行っています…"),
    ] {
        crate::i18n::set(language);
        // The header carries the share (28%); under it, the whole update's time left.
        let estimate = tf("{time} left", &[("time", &crate::play_update::remaining_time(Duration::from_secs(265)))]);
        let estimate = estimate.as_str();
        let mut app = crate::tests::app();
        updating(&mut app, operation::Progress {
            stage: operation::Stage::Download, done: 120_000_000, total: 120_000_000, bytes: true,
            remaining: Some(Duration::ZERO),
            overall: Some(operation::OverallProgress { fraction: Some(0.28), remaining: Some(Duration::from_secs(265)), complete: false }),
            ..operation::Progress::default()
        });
        let shown = render(&ui, &window, &app);
        let title_index = shown.iter().position(|text| text == title).unwrap_or_else(|| panic!("{shown:?}"));
        let step = tf("Current step: {step}", &[("step", &t("Downloading official files"))]);
        let step_index = shown.iter().position(|text| *text == step).unwrap_or_else(|| panic!("{shown:?}"));
        assert!(title_index < step_index, "overall progress leads the current step");
        assert!(shown.iter().any(|text| text == "28%"), "{shown:?}");
        assert!(!shown.iter().any(|text| text.contains("72%")), "no second share: {shown:?}");
        assert!(shown.iter().any(|text| text == estimate), "{shown:?}");
        let step_remaining = tf("{percent}% remaining in this step", &[("percent", &0)]);
        assert!(!shown.iter().any(|text| text.contains(&step_remaining)), "{shown:?}");
        app.maintenance_progress.overall.as_mut().unwrap().remaining = None;
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text == estimating), "{shown:?}");
        app.maintenance_cancelled = true;
        let shown = render(&ui, &window, &app);
        assert!(!shown.iter().any(|text| text == estimating || text == estimate), "{shown:?}");
        app.maintenance_cancelled = false;
        app.maintenance_progress.overall = Some(operation::OverallProgress::default());
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text == calculating), "{shown:?}");
        app.maintenance_progress.overall = Some(operation::OverallProgress { fraction: Some(0.99), remaining: Some(Duration::ZERO), complete: false });
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text == finalizing), "{shown:?}");
        assert!(shown.iter().any(|text| text == "99%"), "{shown:?}");
    }
    crate::i18n::set(Lang::En);
}

/// The rail keeps every message Home's rows and footer sent, plus the v1.2 shortcuts: number keys
/// pick accounts on Home (not under a dialog, not on other pages), Return presses Play, a code row
/// takes digits only and its sixth digit signs in, and the More menu sends what its entries name.
#[test]
fn the_rail_sends_the_row_footer_and_shortcut_messages() {
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let mut app = app(Page::Home, None, false, false);
    let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
    let picked = app.accounts[1].selected;
    render(&ui, &window, &app);
    let _ = super::take_sent();
    // Number keys toggle that row's pick on Home only.
    assert!(ui.invoke_number_key(2));
    assert!(matches!(super::take_sent().as_slice(), [Message::ToggleAccount(key, on)] if *key == keys[1] && *on == !picked));
    assert!(!ui.invoke_number_key(9), "no ninth account");
    app.page = Page::Settings;
    render(&ui, &window, &app);
    assert!(!ui.invoke_number_key(1), "number keys are Home's");
    app.page = Page::Home;
    render(&ui, &window, &app);
    let _ = super::take_sent();
    // Return presses Play when Play can.
    let view = ui.global::<super::View>();
    assert_eq!(ui.invoke_enter_key(), view.get_footer().play_enabled);
    if view.get_footer().play_enabled {
        assert!(matches!(super::take_sent().as_slice(), [Message::MainAction]));
    }
    // A code row: digits only, six at most, and the sixth signs in.
    app.settings.account_otp.insert(keys[1].clone());
    app.accounts[1].otp = "12345".into();
    render(&ui, &window, &app);
    let _ = super::take_sent();
    view.invoke_otp_edited(1, "12a3456789".into());
    let sent = super::take_sent();
    assert!(matches!(sent.first(), Some(Message::OtpChanged(key, code)) if *key == keys[1] && code == "123456"), "{sent:?}");
    assert!(matches!(sent.get(1), Some(Message::SignIn(key)) if *key == keys[1]), "the sixth digit signs in: {sent:?}");
    view.invoke_otp_edited(1, "12".into());
    assert_eq!(super::take_sent().len(), 1, "a partial code only updates the row");
    // The More menu: Import, then Install & repair and Help & support at its end.
    let menu: Vec<String> = slint::Model::iter(&view.get_rail_menu()).map(|s| s.to_string()).collect();
    assert_eq!(menu.first().map(String::as_str), Some(t("Import")), "{menu:?}");
    assert_eq!(menu[menu.len() - 2..], [t("Install & repair").to_owned(), t("Help & support").to_owned()]);
    view.invoke_rail_menu_picked(0);
    assert!(matches!(super::take_sent().as_slice(), [Message::AccountImport(_)]));
    view.invoke_rail_menu_picked(menu.len() as i32 - 2);
    assert!(matches!(super::take_sent().as_slice(), [Message::Navigate(Page::Maintenance)]));
    // Character select names its choice and switches it.
    let at = menu.iter().position(|entry| entry.starts_with(t("Character select"))).expect("character select in the menu");
    view.invoke_rail_menu_picked(at as i32);
    assert!(matches!(super::take_sent().as_slice(), [Message::SetCharacterSelect(_)]));
}

/// Home's news: three cards and four older lines (Latest takes the maintenance notices in, newest
/// first), two and two in the small window, the lines opening after the cards; first run shows
/// the cards alone and its three checks, whose actions are the import, Downloads and Settings →
/// System (never a second Add account), with the game folder masked in privacy mode.
#[test]
fn home_news_and_first_run_checks() {
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let view = ui.global::<super::View>();
    let size = |app: &mut App, width: u32, height: u32| {
        window.set_size(slint::PhysicalSize::new(width, height));
        app.viewport = crate::effect::Size::new(width as f32, height as f32);
        ui.global::<super::Theme>().set_small(super::small(app));
    };
    let mut app = app(Page::Home, None, false, false);
    size(&mut app, 1440, 900);
    render(&ui, &window, &app);
    let older = |view: &super::View| slint::Model::iter(&view.get_news_older()).map(|line| line.date.to_string()).collect::<Vec<_>>();
    assert_eq!(slint::Model::row_count(&view.get_news()), 3);
    assert_eq!(older(&view), ["Sep 26", "Sep 7", "Sep 3"], "the notices, newest first");
    assert!(!view.get_first_run());
    let _ = super::take_sent();
    view.invoke_open_news(3);
    let notice = app.services.as_ref().unwrap().notices[0].url.clone();
    assert!(matches!(super::take_sent().as_slice(), [Message::OpenUrl(url)] if *url == notice), "a line opens its post");

    size(&mut app, 1000, 700);
    // The feeds' own date form (the sample's "Sep 18" has no year and would sort last).
    app.services.as_mut().unwrap().news[2].date = "Fri, 18 Sep 2026 10:00:00 +0900".into();
    render(&ui, &window, &app);
    assert_eq!(slint::Model::row_count(&view.get_news()), 2);
    assert_eq!(older(&view), ["Sep 26", "Sep 18"], "the third post joins the lines by date");

    // The look-up bar shows the chosen site's field.
    view.set_look_scope(0);
    let shown = render(&ui, &window, &app);
    assert!(shown.iter().any(|text| text == t("Search BG Wiki")), "{shown:?}");
    view.set_look_scope(1);

    size(&mut app, 1440, 900);
    app.replace_accounts(Vec::new());
    app.settings.install_path = r"C:\Users\Jane\Games\FFXI".into();
    crate::privacy::set(true);
    let shown = render(&ui, &window, &app);
    crate::privacy::set(false);
    assert!(view.get_first_run());
    assert!(!shown.iter().any(|text| text.contains("Jane")), "{shown:?}");
    assert!(older(&view).is_empty() && slint::Model::iter(&view.get_news()).all(|card| card.teaser.is_empty()));
    let steps: Vec<_> = slint::Model::iter(&view.get_setup()).collect();
    assert_eq!(steps.len(), 3);
    assert!(steps.iter().all(|step| step.action != t("Add account")), "Add account is the rail's alone");
    let _ = super::take_sent();
    view.invoke_setup_action(0);
    assert!(matches!(super::take_sent().as_slice(), [Message::AccountImport(_)]));
    view.invoke_setup_action(1);
    assert!(matches!(super::take_sent().first(), Some(Message::Navigate(Page::Maintenance))));
    view.invoke_setup_action(2);
    assert!(matches!(super::take_sent().as_slice(), [Message::OpenSettingsTab(crate::settings_page::Tab::System)]));
}

/// The account lamps follow the row state.
#[test]
fn rail_lamps_follow_the_row_state() {
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let mut app = app(Page::Home, None, false, false);
    app.accounts[0].pid = Some(7);
    app.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let key = app.accounts[1].key.clone();
    app.settings.account_otp.insert(key);
    render(&ui, &window, &app);
    let rows = ui.global::<super::View>().get_accounts();
    let lamp = |i: usize| slint::Model::row_data(&rows, i).unwrap().lamp;
    assert_eq!(lamp(0), super::LampState::Ready, "signed in, not in game");
    assert_eq!(lamp(1), super::LampState::Code, "needs a one-time code");
    assert_eq!(lamp(2), super::LampState::Off, "signed out");
    app.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
    render(&ui, &window, &app);
    let rows = ui.global::<super::View>().get_accounts();
    assert_eq!(slint::Model::row_data(&rows, 0).unwrap().lamp, super::LampState::Game);
}

/// CRYSTAL_SHOTS=<dir>: the Skyline shell as PNGs (Home dark and light, first run, the small
/// window, an inner page, the four skies) for review against crystal-design-v12b.
#[test]
fn skyline_screenshots() {
    let Some(dir) = std::env::var_os("CRYSTAL_SHOTS").map(std::path::PathBuf::from) else { return };
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    super::sky::update(&ui);
    let shot = |name: &str, app: &mut App, width: u32, height: u32, light: bool| {
        window.set_size(slint::PhysicalSize::new(width, height));
        app.viewport = crate::effect::Size::new(width as f32, height as f32);
        let theme = ui.global::<super::Theme>();
        theme.set_light(light);
        theme.set_scale(super::scale(app));
        theme.set_small(super::small(app));
        render(&ui, &window, app);
        save_png(&window, width, height, &dir.join(format!("{name}.png")));
    };
    let mut home = app(Page::Home, None, false, false);
    home.accounts[0].pid = Some(7);
    home.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
    let key = home.accounts[1].key.clone();
    home.settings.account_otp.insert(key);
    home.accounts[1].otp = "41".into();
    shot("home-dark", &mut home, 1440, 900, false);
    shot("home-light", &mut home, 1440, 900, true);
    shot("home-small-dark", &mut home, 1000, 700, false);
    for (phase, name) in [(0, "night"), (1, "dawn"), (2, "day"), (3, "dusk")] {
        ui.global::<super::Sky>().set_phase(phase);
        shot(&format!("sky-{name}"), &mut home, 1440, 900, false);
    }
    let mut first = app(Page::Home, None, false, false);
    first.replace_accounts(Vec::new());
    shot("first-run-dark", &mut first, 1440, 900, false);
    let mut settings = app(Page::Settings, None, false, false);
    shot("settings-dark", &mut settings, 1440, 900, false);
    let mut downloads = app(Page::Maintenance, None, false, false);
    shot("downloads-light", &mut downloads, 1440, 900, true);
    // The account sheet of an account with a saved authenticator key (masked, Change).
    let mut sheet = app(Page::Home, None, false, false);
    sheet.accounts[0].account.totp_key = "JBSWY3DPEHPK3PXP".into();
    let key = sheet.accounts[0].key.clone();
    let _ = sheet.update(Message::EditAccount(key));
    shot("sheet-authenticator-dark", &mut sheet, 1440, 900, false);
    shot("sheet-authenticator-light", &mut sheet, 1440, 900, true);
    crate::i18n::set(Lang::Ja);
    shot("sheet-authenticator-ja", &mut sheet, 1440, 900, false);
    crate::i18n::set(Lang::En);
}

/// CRYSTAL_GAUGE_SHOTS=<dir>: the crystal wherever it shows, dark and light, at 1x and 150 %:
/// the rail's lamps in every state (unlit, rising, ready, in game, code, error), the first-run
/// rail, the brand mark, and the Downloads gauge idle, half lit and full.
#[test]
fn crystal_screenshots() {
    let Some(dir) = std::env::var_os("CRYSTAL_GAUGE_SHOTS").map(std::path::PathBuf::from) else { return };
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    let (ui, window) = main_window();
    let shot = |name: &str, app: &mut App, scale: f32, light: bool, setup: &dyn Fn(&MainWindow)| {
        let (width, height) = (1440u32, 900u32);
        window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: scale });
        let (pw, ph) = ((width as f32 * scale) as u32, (height as f32 * scale) as u32);
        window.set_size(slint::PhysicalSize::new(pw, ph));
        app.viewport = crate::effect::Size::new(width as f32, height as f32);
        let theme = ui.global::<super::Theme>();
        theme.set_light(light);
        theme.set_scale(super::scale(app));
        theme.set_small(super::small(app));
        render(&ui, &window, app);
        setup(&ui);
        save_png(&window, pw, ph, &dir.join(format!("{name}.png")));
    };
    // Six rows, one per lamp state.
    let mut home = app(Page::Home, None, false, false);
    let mut values: Vec<_> = home.accounts.iter().map(|row| row.account.clone()).collect();
    for i in values.len()..6 {
        let mut extra = values[0].clone();
        extra.label = format!("Extra {i}");
        extra.pol_id = format!("XYZ{i:05}");
        values.push(extra);
    }
    home.replace_accounts(values);
    let lamps = |ui: &MainWindow| {
        use slint::Model;
        let rows = ui.global::<super::View>().get_accounts();
        let states = [super::LampState::Off, super::LampState::Rising, super::LampState::Ready, super::LampState::Game, super::LampState::Code, super::LampState::Error];
        for (i, lamp) in states.into_iter().enumerate() {
            if let Some(mut line) = rows.row_data(i) {
                line.lamp = lamp;
                line.fill = 0.55;
                rows.set_row_data(i, line);
            }
        }
    };
    let gauge = |fraction: f32| {
        move |ui: &MainWindow| {
            let dl = ui.global::<super::Downloads>();
            let mut activity = dl.get_activity();
            activity.gauge = fraction;
            activity.gauge_label = if fraction < 0.0 { "".into() } else { format!("{}%", (fraction * 100.0).round() as u32).into() };
            dl.set_activity(activity);
        }
    };
    let mut first = app(Page::Home, None, false, false);
    first.replace_accounts(Vec::new());
    let mut downloads = app(Page::Maintenance, None, false, false);
    for (suffix, scale) in [("", 1.0), ("@1.5x", 1.5)] {
        for (theme, light) in [("dark", false), ("light", true)] {
            shot(&format!("rail-{theme}{suffix}"), &mut home, scale, light, &lamps);
            shot(&format!("first-run-{theme}{suffix}"), &mut first, scale, light, &|_| {});
            for (name, fraction) in [("idle", -1.0), ("50", 0.5), ("100", 1.0)] {
                shot(&format!("downloads-{name}-{theme}{suffix}"), &mut downloads, scale, light, &gauge(fraction));
            }
        }
    }
}

/// CRYSTAL_SHOTS=<dir>: the Friends page in Skyline (dark, light, Japanese, privacy, the
/// Mailbox, the block list, the in-game notice and the sign-in card), into <dir>.
#[test]
fn friends_screenshots() {
    let Some(dir) = std::env::var_os("CRYSTAL_SHOTS").map(std::path::PathBuf::from) else { return };
    let (ui, window) = main_window();
    super::sky::update(&ui);
    let shot = |name: &str, app: &mut App, light: bool| {
        let (width, height) = (1440, 900);
        window.set_size(slint::PhysicalSize::new(width, height));
        app.viewport = crate::effect::Size::new(width as f32, height as f32);
        let theme = ui.global::<super::Theme>();
        theme.set_light(light);
        theme.set_scale(super::scale(app));
        theme.set_small(super::small(app));
        render(&ui, &window, app);
        save_png(&window, width, height, &dir.join(format!("{name}.png")));
    };
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    shot("friends-dark", &mut app(Page::Friends, None, false, false), false);
    shot("friends-light", &mut app(Page::Friends, None, false, false), true);
    shot("mailbox-dark", &mut app(Page::Friends, Some(Pane::Mailbox), true, false), false);
    shot("blocked-dark", &mut app(Page::Friends, Some(Pane::Blocked), false, false), false);
    shot("ingame-dark", &mut app(Page::Friends, None, false, true), false);
    let mut signin = app(Page::Friends, None, false, false);
    signin.friends.connected = false;
    if let Some(snapshot) = &mut signin.friends.snapshot { snapshot.entries.clear(); }
    signin.friends.snapshot = None;
    shot("signin-dark", &mut signin, false);
    crate::i18n::set(Lang::Ja);
    shot("friends-ja", &mut app(Page::Friends, None, false, false), false);
    crate::i18n::set(Lang::En);
    crate::privacy::set(true);
    shot("friends-privacy", &mut app(Page::Friends, None, false, false), false);
    crate::privacy::set(false);
}

/// CRYSTAL_SHOTS=<dir>: the Settings and Features pages as PNGs (dark, light, Japanese, privacy)
/// for review against crystal-design-v12b\a-skyline\features-dark.png.
#[test]
fn settings_and_features_screenshots() {
    use crate::features::ui::Section;
    use crate::settings_page::Tab;
    let Some(dir) = std::env::var_os("CRYSTAL_SHOTS").map(std::path::PathBuf::from) else { return };
    let (ui, window) = main_window();
    let shot = |name: &str, app: &mut App, width: u32, height: u32, light: bool| {
        window.set_size(slint::PhysicalSize::new(width, height));
        app.viewport = crate::effect::Size::new(width as f32, height as f32);
        let theme = ui.global::<super::Theme>();
        theme.set_light(light);
        theme.set_scale(super::scale(app));
        theme.set_small(super::small(app));
        render(&ui, &window, app);
        save_png(&window, width, height, &dir.join(format!("{name}.png")));
    };
    let features = |section: Section| {
        let mut app = app(Page::Home, None, false, false);
        app.accounts[0].pid = Some(7);
        app.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
        for row in app.accounts.iter_mut().skip(1) { row.pid = Some(8); }
        app.load_features_sample(section, false);
        app
    };
    crate::privacy::set(false);
    crate::i18n::set(Lang::En);
    for (name, section) in [("windows", Section::Windows), ("hotkeys", Section::Hotkeys), ("recovery", Section::Recovery), ("backups", Section::Backups), ("clients", Section::Clients), ("activity", Section::Activity)] {
        shot(&format!("features-{name}-dark"), &mut features(section), 1440, 900, false);
    }
    shot("features-windows-light", &mut features(Section::Windows), 1440, 900, true);
    shot("features-windows-small-dark", &mut features(Section::Windows), 1000, 700, false);
    shot("features-clients-light", &mut features(Section::Clients), 1440, 900, true);
    for (name, tab) in [("graphics", Tab::Graphics), ("sound", Tab::Sound), ("controller", Tab::Controller), ("system", Tab::System)] {
        let mut settings = app(Page::Settings, None, false, false);
        settings.settings_tab = tab;
        shot(&format!("settings-{name}-dark"), &mut settings, 1440, 900, false);
        if name == "graphics" || name == "system" { shot(&format!("settings-{name}-light"), &mut settings, 1440, 900, true); }
    }
    crate::i18n::set(Lang::Ja);
    shot("features-windows-ja", &mut features(Section::Windows), 1440, 900, false);
    shot("features-activity-ja", &mut features(Section::Activity), 1440, 900, false);
    let mut settings = app(Page::Settings, None, false, false);
    shot("settings-graphics-ja", &mut settings, 1440, 900, false);
    crate::i18n::set(Lang::En);
    crate::privacy::set(true);
    shot("features-windows-privacy", &mut features(Section::Windows), 1440, 900, false);
    shot("features-hotkeys-privacy", &mut features(Section::Hotkeys), 1440, 900, false);
    let mut settings = app(Page::Settings, None, false, false);
    shot("settings-graphics-privacy", &mut settings, 1440, 900, false);
    crate::privacy::set(false);
}
