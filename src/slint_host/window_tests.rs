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
    for (language, remaining, estimate, estimating) in [
        (Lang::En, "65% remaining in this step", "About 2 min remaining", "Estimating time remaining…"),
        (Lang::Ja, "このステップの残り：65%", "残り約2分", "残り時間を計算しています…"),
    ] {
        crate::i18n::set(language);
        let mut app = crate::tests::app();
        updating(&mut app, operation::Progress {
            stage: operation::Stage::Download, done: 42_000_000, total: 120_000_000,
            bytes: true, remaining: Some(Duration::from_secs(115)),
            ..operation::Progress::default()
        });
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text.contains(remaining) && text.contains(estimate)), "{shown:?}");
        assert!(shown.iter().any(|text| text.contains("42 MB")), "byte progress keeps its units: {shown:?}");
        app.maintenance_progress.remaining = None;
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text.contains(estimating)), "{shown:?}");
        app.maintenance_cancelled = true;
        let shown = render(&ui, &window, &app);
        assert!(shown.iter().any(|text| text.contains(remaining)), "{shown:?}");
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
    for (language, title, percent, estimate, calculating, estimating, finalizing) in [
        (Lang::En, "Overall update", "72% remaining overall", "About 5 min remaining for the whole update",
         "Calculating total update size…", "Estimating total time remaining…", "Finalizing update…"),
        (Lang::Ja, "アップデート全体", "全体の残り：72%", "アップデート全体の残り時間：約5分",
         "アップデート全体のサイズを計算しています…", "アップデート全体の残り時間を計算しています…", "アップデートの最終処理を行っています…"),
    ] {
        crate::i18n::set(language);
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
        assert!(shown.iter().any(|text| text == percent), "{shown:?}");
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
        assert!(shown.iter().any(|text| *text == tf("{percent}% remaining overall", &[("percent", &1)])), "{shown:?}");
    }
    crate::i18n::set(Lang::En);
}
