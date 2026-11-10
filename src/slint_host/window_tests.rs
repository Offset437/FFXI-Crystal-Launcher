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
