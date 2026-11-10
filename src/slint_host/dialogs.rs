//! The delete confirmation, Connection & activity and the FFXIAH lookup (ui/dialogs.slint):
//! `wire` turns their buttons into core messages, `sync` decides which one shows (stacking
//! order: close > delete > editor > import > settings > details > FFXIAH, so at
//! most one) and pushes what it shows.
use super::sync::{private, put, sync_model, Private};
use super::{send, AhItem, AhState, DetailsState, DialogStrings, Dialogs, Glyph, Ink, MainWindow};
use crate::i18n::{t, tf, Lang};
use crate::{App, Message, Page};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

/// What the buttons act on, as of the last sync.
struct State {
    lang: Option<Lang>,
    delete_key: Option<String>,
    ah_urls: Vec<String>,
    ah_site: String,
    ah_items: Rc<VecModel<AhItem>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn act(f: impl FnOnce(&State) -> Option<Message>) {
    let message = STATE.with(|cell| cell.borrow().as_ref().and_then(f));
    if let Some(message) = message { send(message); }
}

pub(super) fn wire(ui: &MainWindow) {
    let state = State { lang: None, delete_key: None, ah_urls: Vec::new(), ah_site: String::new(), ah_items: Rc::new(VecModel::default()) };
    let dialogs = ui.global::<Dialogs>();
    dialogs.set_ah_items(ModelRc::from(state.ah_items.clone()));
    STATE.with(|cell| *cell.borrow_mut() = Some(state));
    dialogs.on_delete_cancel(|| send(Message::CancelDeleteAccount));
    dialogs.on_delete_confirm(|| act(|s| s.delete_key.clone().map(Message::DeleteAccount)));
    dialogs.on_details_close(|| send(Message::ToggleDetails));
    dialogs.on_world_status(|| send(Message::OpenUrl(crate::services::WORLD_STATUS_URL.into())));
    dialogs.on_notices(|| send(Message::OpenUrl(crate::services::sources(crate::i18n::current()).maintenance.into())));
    dialogs.on_open_downloads(|| send(Message::Navigate(Page::Maintenance)));
    dialogs.on_ah_close(|| send(Message::CloseAh));
    dialogs.on_ah_open_item(|i| act(|s| s.ah_urls.get(usize::try_from(i).ok()?).cloned().map(Message::OpenUrl)));
    dialogs.on_ah_open_site(|| act(|s| Some(Message::OpenUrl(s.ah_site.clone()))));
}

pub(super) fn forget() {
    STATE.with(|cell| cell.borrow_mut().take());
}

/// Which of these dialogs shows: none while one stacked above it is open.
fn shown(app: &App) -> (Option<&str>, bool, bool) {
    if app.confirm_close { return (None, false, false); }
    if let Some(key) = &app.pending_delete { return (Some(key), false, false); }
    let above = app.editor.is_some() || app.account_import.open || app.show_settings;
    let details = !above && app.show_details;
    (None, details, !above && !details && app.ah_lookup.open)
}

pub(super) fn sync(ui: &MainWindow, app: &App) {
    let dialogs = ui.global::<Dialogs>();
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else { return };
        let lang = crate::i18n::current();
        if state.lang != Some(lang) {
            state.lang = Some(lang);
            put!(dialogs, get_strings, set_strings, strings());
        }
        let (delete, details, ah) = shown(app);
        put!(dialogs, get_delete_open, set_delete_open, delete.is_some());
        state.delete_key = delete.map(str::to_owned);
        if let Some(key) = delete {
            let label = app.accounts.iter().find(|row| row.key == key).map_or_else(|| t("this account").to_owned(), |row| row.account.label.clone());
            put!(dialogs, get_delete_title, set_delete_title, private(app, &tf("Delete {label}?", &[("label", &label)])).into());
        }
        put!(dialogs, get_details_open, set_details_open, details);
        if details { put!(dialogs, get_details, set_details, details_state(app)); }
        put!(dialogs, get_ah_open, set_ah_open, ah);
        if ah {
            lookup(&dialogs, app, state);
        } else if !state.ah_urls.is_empty() {
            // A closed lookup keeps no results in the window.
            state.ah_urls.clear();
            sync_model(&state.ah_items, Vec::new());
        }
    });
}

fn strings() -> DialogStrings {
    DialogStrings {
        cancel: t("Cancel").into(),
        delete_account: t("Delete account").into(),
        delete_body: t("This removes its saved sign-in details and launcher choices. Game files and account data folders are kept.").into(),
        details_title: t("Connection & activity").into(),
        details_sub: t("Service status, your game version and the launcher's latest message.").into(),
        service: t("Service").into(),
        game_version: t("Game version").into(),
        installed: t("Installed").into(),
        latest: t("Latest").into(),
        world_status: t("Official world status").into(),
        notices: t("Maintenance notices").into(),
        latest_activity: t("Latest activity").into(),
        open_downloads: t("Open Downloads").into(),
        done: t("Done").into(),
        close: t("Close").into(),
        search: t("Search").into(),
        find_item: t("Find an item").into(),
        ah_note: t("Website links use the world saved in your browser. These results use the world selected in Crystal.").into(),
        open_ffxiah: t("Open FFXIAH").into(),
        truncated: t("Showing the first 30 matches.").into(),
    }
}

/// Connection & activity's blocks, as data.
fn details_state(app: &App) -> DetailsState {
    use crate::services::Availability;
    let (service_glyph, service_ink, service) = match app.service_availability() {
        Availability::Operational => (Glyph::CheckCircle, Ink::Ok, t("Worlds online")),
        Availability::Maintenance => (Glyph::AlertCircle, Ink::Accent, t("Maintenance")),
        Availability::PartialOutage => (Glyph::AlertCircle, Ink::Accent, t("World disruption")),
        Availability::Outage => (Glyph::AlertCircle, Ink::Danger, t("Worlds offline")),
        Availability::Unknown => (Glyph::AlertCircle, Ink::Sub, if app.news_loading { t("Checking services") } else { t("Status unconfirmed") }),
    };
    use crate::patch::PatchState;
    let (patch_glyph, patch_ink, patch) = match app.patch.as_ref().map(|p| p.state) {
        Some(PatchState::Current) => (Glyph::CheckCircle, Ink::Ok, t("Version current")),
        Some(PatchState::Required) => (Glyph::Download, Ink::Accent, t("Update available")),
        Some(PatchState::Interrupted) => (Glyph::AlertCircle, Ink::Accent, t("Interrupted update")),
        _ if app.patch_loading => (Glyph::Refresh, Ink::Sub, t("Checking version")),
        _ => (Glyph::AlertCircle, Ink::Sub, t("Version unchecked")),
    };
    let private = Private::new(app);
    let patch_detail = app.patch.as_ref().map(|p| {
        if matches!(p.state, PatchState::Required) { t("A newer version is available.").to_owned() } else { private.text(&p.detail) }
    });
    let versions = app.patch.as_ref().filter(|p| !p.local_version.is_empty() || !p.remote_version.is_empty());
    let installed = versions.map(|p| p.local_version.clone()).unwrap_or_default();
    let latest = if versions.is_some() { crate::official_versions::latest_game_text(app) } else { String::new() };
    let mono = |value: &str| value.chars().any(|ch| ch.is_ascii_digit());
    DetailsState {
        service: service.into(),
        service_glyph,
        service_ink,
        has_services: app.services.is_some(),
        service_note: app.services.as_ref().map(|_| app.service_note()).unwrap_or_default().into(),
        checked: app.services.as_ref().map(|s| tf("Last checked: {time}", &[("time", &s.checked_at)])).unwrap_or_default().into(),
        patch: patch.into(),
        patch_glyph,
        patch_ink,
        patch_detail: patch_detail.unwrap_or_default().into(),
        versions: versions.is_some(),
        installed_mono: mono(&installed),
        installed: installed.into(),
        latest_mono: mono(&latest),
        latest: latest.into(),
        activity: private.text(&app.status).into(),
    }
}

/// The FFXIAH lookup's contents, as data.
fn lookup(dialogs: &Dialogs, app: &App, state: &mut State) {
    let lookup = &app.ah_lookup;
    let world: [(&str, &dyn std::fmt::Display); 1] = [("world", &lookup.world)];
    let (message, items, truncated, warning) = match &lookup.result {
        None => (tf("Searching {world}…", &world), Vec::new(), false, String::new()),
        Some(Err(error)) => (error.clone(), Vec::new(), false, String::new()),
        Some(Ok(results)) => (
            if results.items.is_empty() { t("No matching items found.").to_owned() } else { String::new() },
            results.items.iter().collect(),
            results.truncated,
            results.warning.clone().unwrap_or_default(),
        ),
    };
    put!(dialogs, get_ah, set_ah, AhState {
        query: lookup.query.as_str().into(),
        heading: tf("Prices on {world}", &world).into(),
        message: message.into(),
        truncated,
        warning: warning.into(),
    });
    state.ah_urls = items.iter().map(|item| item.url.clone()).collect();
    state.ah_site = crate::ffxiah::ah_search_url(&lookup.query, lookup.world);
    let rows = items
        .iter()
        .map(|item| AhItem {
            name: match item.stack { Some(count) => format!("{} ×{count}", item.name), None => item.name.clone() }.into(),
            caption: (if item.last_sale { t("Last sale") } else { t("Price") }).into(),
            price: item.price.map(|v| tf("{price} gil", &[("price", &crate::ffxiah::grouped_number(v))])).unwrap_or_else(|| t("No sales listed").into()).into(),
            stock: item.stock.map(|v| tf("{count} in stock", &[("count", &v)])).unwrap_or_else(|| t("Stock unavailable").into()).into(),
        })
        .collect();
    sync_model(&state.ah_items, rows);
}

#[cfg(test)]
mod tests {
    use super::super::{pad, sync::headless, take_sent};
    use super::*;
    use slint::Model;

    fn window(app: &App) -> (MainWindow, std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>) {
        let window = headless::window(1440, 900);
        let ui = MainWindow::new().unwrap();
        super::super::view::wire(&ui);
        wire(&ui);
        super::super::view::sync(&ui, app);
        sync(&ui, app);
        ui.show().unwrap();
        headless::draw(&window);
        (ui, window)
    }

    fn report(detail: String, version: &str) -> crate::patch::PatchReport {
        crate::patch::PatchReport { state: crate::patch::PatchState::Current, local_version: version.into(), remote_version: version.into(), detail, files_verified: None }
    }

    #[test]
    fn the_delete_confirmation_hides_the_sheet_and_sends_cancel_or_delete() {
        crate::i18n::set(Lang::En);
        let mut app = crate::tests::app();
        let key = app.accounts[0].key.clone();
        let label = app.accounts[0].account.label.clone();
        app.editor = None;
        app.pending_delete = Some(key.clone());
        let (ui, _window) = window(&app);
        let dialogs = ui.global::<Dialogs>();
        assert!(dialogs.get_delete_open());
        assert_eq!(dialogs.get_delete_title(), tf("Delete {label}?", &[("label", &label)]).as_str());
        assert!(!dialogs.get_details_open() && !dialogs.get_ah_open());
        assert_eq!(pad::names(ui.window()), vec![t("Cancel").to_owned(), t("Delete account").to_owned()], "only the dialog's controls");
        take_sent();
        dialogs.invoke_delete_cancel();
        dialogs.invoke_delete_confirm();
        let sent = take_sent();
        assert!(matches!(sent[0], Message::CancelDeleteAccount));
        assert!(matches!(&sent[1], Message::DeleteAccount(k) if *k == key));
        // An unknown key names "this account".
        app.pending_delete = Some("gone".into());
        sync(&ui, &app);
        assert_eq!(dialogs.get_delete_title(), tf("Delete {label}?", &[("label", &t("this account"))]).as_str());
        // The close confirmation goes above it.
        app.confirm_close = true;
        sync(&ui, &app);
        assert!(!dialogs.get_delete_open());
    }

    #[test]
    fn the_delete_title_masks_the_account_name_in_privacy_mode() {
        crate::i18n::set(Lang::En);
        let mut app = crate::tests::app();
        let label = app.accounts[0].account.label.clone();
        app.pending_delete = Some(app.accounts[0].key.clone());
        crate::privacy::set(true);
        let (ui, _window) = window(&app);
        let title = ui.global::<Dialogs>().get_delete_title().to_string();
        crate::privacy::set(false);
        assert!(!title.contains(&label), "{title}");
    }

    #[test]
    fn connection_and_activity_shows_and_sends_its_messages() {
        crate::i18n::set(Lang::En);
        let mut app = crate::tests::app();
        app.show_details = true;
        app.status = "Signed in as Sample".into();
        let (ui, _window) = window(&app);
        let dialogs = ui.global::<Dialogs>();
        assert!(dialogs.get_details_open());
        assert_eq!(dialogs.get_details().activity, "Signed in as Sample");
        let names = pad::names(ui.window());
        for name in [t("Close"), t("Official world status"), t("Maintenance notices"), t("Open Downloads"), t("Done")] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        take_sent();
        dialogs.invoke_details_close();
        dialogs.invoke_world_status();
        dialogs.invoke_notices();
        dialogs.invoke_open_downloads();
        let sent = take_sent();
        assert!(matches!(sent[0], Message::ToggleDetails));
        assert!(matches!(&sent[1], Message::OpenUrl(url) if url == crate::services::WORLD_STATUS_URL));
        assert!(matches!(&sent[2], Message::OpenUrl(url) if url == crate::services::sources(Lang::En).maintenance));
        assert!(matches!(sent[3], Message::Navigate(Page::Maintenance)));
        // Settings is stacked above it.
        app.show_settings = true;
        sync(&ui, &app);
        assert!(!dialogs.get_details_open());
    }

    #[test]
    fn connection_and_activity_masks_the_status_and_patch_detail_in_privacy_mode() {
        crate::i18n::set(Lang::En);
        let mut app = crate::tests::app();
        let label = app.accounts[0].account.label.clone();
        app.show_details = true;
        app.status = format!("{label} signed in");
        app.patch = Some(report(format!("Checked for {label}"), ""));
        crate::privacy::set(true);
        let (ui, _window) = window(&app);
        let details = ui.global::<Dialogs>().get_details();
        crate::privacy::set(false);
        assert!(!details.activity.contains(&label) && !details.patch_detail.contains(&label));
        assert_eq!(details.patch, t("Version current"));
    }

    #[test]
    fn the_ffxiah_lookup_shows_results_and_sends_its_messages() {
        crate::i18n::set(Lang::En);
        let mut app = crate::tests::app();
        app.ah_lookup.open = true;
        app.ah_lookup.query = "copper".into();
        app.ah_lookup.world = "Asura";
        let (ui, window) = window(&app);
        let dialogs = ui.global::<Dialogs>();
        assert!(dialogs.get_ah_open());
        assert_eq!(dialogs.get_ah().message, tf("Searching {world}…", &[("world", &"Asura")]).as_str());
        app.ah_lookup.result = Some(Ok(crate::ffxiah::Results {
            items: vec![
                crate::ffxiah::Quote { name: "Copper Ore".into(), url: "https://www.ffxiah.com/item/640".into(), stack: Some(12), price: Some(12500), stock: Some(3), last_sale: false },
                crate::ffxiah::Quote { name: "Copper Ingot".into(), url: "https://www.ffxiah.com/item/648".into(), stack: None, price: None, stock: None, last_sale: true },
            ],
            truncated: true,
            warning: None,
        }));
        sync(&ui, &app);
        headless::draw(&window);
        let items = dialogs.get_ah_items();
        assert_eq!(items.row_count(), 2);
        let first = items.row_data(0).unwrap();
        assert_eq!(first.name, "Copper Ore ×12");
        assert_eq!(first.price, tf("{price} gil", &[("price", &"12,500")]).as_str());
        assert_eq!(first.stock, tf("{count} in stock", &[("count", &3)]).as_str());
        let second = items.row_data(1).unwrap();
        assert_eq!((second.caption.as_str(), second.price.as_str(), second.stock.as_str()), (t("Last sale"), t("No sales listed"), t("Stock unavailable")));
        assert!(dialogs.get_ah().truncated && dialogs.get_ah().message.is_empty());
        assert!(pad::names(ui.window()).iter().any(|n| n == "Copper Ingot"), "the rows are controller stops");
        take_sent();
        dialogs.invoke_ah_open_item(1);
        dialogs.invoke_ah_open_site();
        dialogs.invoke_ah_close();
        ui.global::<super::super::View>().invoke_ah_search();
        let sent = take_sent();
        assert!(matches!(&sent[0], Message::OpenUrl(url) if url == "https://www.ffxiah.com/item/648"));
        assert!(matches!(&sent[1], Message::OpenUrl(url) if *url == crate::ffxiah::ah_search_url("copper", "Asura")));
        assert!(matches!(sent[2], Message::CloseAh));
        assert!(matches!(sent[3], Message::SearchAh));
        // Closed: no results stay in the window.
        app.ah_lookup.open = false;
        sync(&ui, &app);
        assert!(!dialogs.get_ah_open() && dialogs.get_ah_items().row_count() == 0);
    }

    #[test]
    fn an_account_row_drag_sends_the_drag_messages_and_marks_the_dragged_row() {
        let mut app = crate::tests::app();
        let (ui, _window) = window(&app);
        let view = ui.global::<super::super::View>();
        let (first, second) = (app.accounts[0].key.clone(), app.accounts[1].key.clone());
        take_sent();
        view.invoke_drag_start(0);
        view.invoke_drag_over(1);
        let sent = take_sent();
        assert!(matches!(&sent[0], Message::AccountDragStart(k) if *k == first));
        assert!(matches!(&sent[1], Message::AccountDragOver(k) if *k == second));
        app.account_drag.source = Some(first);
        super::super::view::sync(&ui, &app);
        assert!(view.get_dragging());
        assert!(view.get_accounts().row_data(0).unwrap().dragging && !view.get_accounts().row_data(1).unwrap().dragging);
        assert!(app.schedule().account_drag, "the host's release and unfocus listener runs while this is set");
    }

    #[test]
    fn escape_that_no_control_takes_goes_back() {
        let app = crate::tests::app();
        let (ui, window) = window(&app);
        ui.on_escape(|| send(Message::ControllerBack));
        take_sent();
        // Focus a page control that ignores Escape; the window's scope takes it.
        pad::step(ui.window(), pad::Way::Down);
        let text: slint::SharedString = slint::platform::Key::Escape.into();
        ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
        drop(window);
        assert!(matches!(take_sent().as_slice(), [Message::ControllerBack]));
    }

    /// Renders Home, Friends, Downloads, the account sheet, the close confirmation, the
    /// controller's focus and Home at the small and wide sizes (software renderer, every page's
    /// sync as the host runs it) into CRYSTAL_SLINT_SHOTS:
    /// `cargo test render_the_screens -- --ignored`. Run alone.
    #[test]
    #[ignore]
    fn render_the_screens() {
        use super::super::{downloads, features, friends, settings, view, Theme};
        let Some(dir) = std::env::var_os("CRYSTAL_SLINT_SHOTS").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let window = headless::window(1440, 900);
        let ui = MainWindow::new().unwrap();
        view::wire(&ui);
        friends::wire(&ui);
        downloads::wire(&ui);
        settings::wire(&ui);
        features::wire(&ui);
        wire(&ui);
        ui.show().unwrap();
        type Setup = fn(&mut App);
        let screens: [(&str, (u32, u32), Setup); 9] = [
            ("home", (1440, 900), |_| {}),
            ("home-small", (900, 650), |_| {}),
            ("home-wide", (1920, 1080), |_| {}),
            ("friends", (1440, 900), |app| app.page = Page::Friends),
            ("downloads", (1440, 900), |app| app.page = Page::Maintenance),
            ("sheet-edit", (1440, 900), |app| { let key = app.accounts[0].key.clone(); let _ = app.update(Message::EditAccount(key)); }),
            ("sheet-add", (1440, 900), |app| { let _ = app.update(Message::NewAccount); }),
            ("close", (1440, 900), |app| app.confirm_close = true),
            ("controller-nav", (1440, 900), |app| app.controller_nav.preview_focus()),
        ];
        let variants = [(false, false, false), (true, false, false), (false, true, false), (false, false, true)];
        for (name, (width, height), setup) in screens {
            for (light, ja, private) in variants {
                crate::i18n::set(if ja { Lang::Ja } else { Lang::En });
                crate::privacy::set(private);
                let mut app = crate::tests::app();
                app.settings.light_theme = light;
                app.viewport = crate::effect::Size::new(width as f32, height as f32);
                setup(&mut app);
                window.set_size(slint::PhysicalSize::new(width, height));
                let theme = ui.global::<Theme>();
                theme.set_light(light);
                theme.set_scale(super::super::scale(&app));
                ui.global::<super::super::Nav>().set_keyboard(false);
                view::sync(&ui, &app);
                friends::sync(&ui, &app);
                downloads::sync(&ui, &app);
                settings::sync(&ui, &app);
                features::sync(&ui, &app);
                sync(&ui, &app);
                headless::draw(&window);
                // The host's controller step: the queued focus move lands on the first control.
                for command in app.controller_nav.take_pending() { pad::apply(ui.window(), command); }
                let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                window.request_redraw();
                window.draw_if_needed(|renderer| { renderer.render(&mut pixels, width as usize); });
                let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                let file = format!("{name}-{}{}{}.png", if light { "light" } else { "dark" }, if ja { "-ja" } else { "" }, if private { "-private" } else { "" });
                imagecrate::save_buffer_with_format(dir.join(file), &bytes, width, height, imagecrate::ExtendedColorType::Rgb8, imagecrate::ImageFormat::Png).unwrap();
            }
        }
        crate::i18n::set(Lang::En);
        crate::privacy::set(false);
    }

    /// Renders the three dialogs (software renderer) into CRYSTAL_SLINT_SHOTS:
    /// `cargo test render_the_dialogs -- --ignored`. Run alone: it
    /// switches the language and privacy mode.
    #[test]
    #[ignore]
    fn render_the_dialogs() {
        let Some(dir) = std::env::var_os("CRYSTAL_SLINT_SHOTS").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let window = headless::window(1440, 900);
        let ui = MainWindow::new().unwrap();
        super::super::view::wire(&ui);
        wire(&ui);
        ui.show().unwrap();
        let states: [(&str, fn(&mut App)); 4] = [
            ("delete", |app| app.pending_delete = Some(app.accounts[0].key.clone())),
            ("details", |app| {
                app.show_details = true;
                app.status = format!("{} signed in.", app.accounts[0].account.label);
                app.patch = Some(report("Files match the official version.".into(), "30251104_0"));
            }),
            ("ffxiah", |app| {
                app.ah_lookup.open = true;
                app.ah_lookup.query = "copper".into();
                app.ah_lookup.world = "Asura";
                app.ah_lookup.result = Some(Ok(crate::ffxiah::Results {
                    items: vec![
                        crate::ffxiah::Quote { name: "Copper Ore".into(), url: String::new(), stack: Some(12), price: Some(12500), stock: Some(3), last_sale: false },
                        crate::ffxiah::Quote { name: "Copper Ingot".into(), url: String::new(), stack: None, price: Some(800), stock: None, last_sale: true },
                        crate::ffxiah::Quote { name: "Copper Ring".into(), url: String::new(), stack: None, price: None, stock: Some(0), last_sale: false },
                    ],
                    truncated: true,
                    warning: None,
                }));
            }),
            ("ffxiah-searching", |app| {
                app.ah_lookup.open = true;
                app.ah_lookup.query = "copper".into();
                app.ah_lookup.world = "Asura";
            }),
        ];
        let variants = [(false, false, false), (true, false, false), (false, true, false), (false, false, true)];
        for (name, setup) in states {
            for (light, ja, private) in variants {
                crate::i18n::set(if ja { Lang::Ja } else { Lang::En });
                crate::privacy::set(private);
                let mut app = crate::tests::app();
                app.settings.light_theme = light;
                setup(&mut app);
                ui.global::<super::super::Theme>().set_light(light);
                super::super::view::sync(&ui, &app);
                sync(&ui, &app);
                let (width, height) = (1440u32, 900u32);
                let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                window.request_redraw();
                window.draw_if_needed(|renderer| { renderer.render(&mut pixels, width as usize); });
                let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                let file = format!("{name}-{}{}{}.png", if light { "light" } else { "dark" }, if ja { "-ja" } else { "" }, if private { "-private" } else { "" });
                imagecrate::save_buffer_with_format(dir.join(file), &bytes, width, height, imagecrate::ExtendedColorType::Rgb8, imagecrate::ImageFormat::Png).unwrap();
            }
        }
        crate::i18n::set(Lang::En);
        crate::privacy::set(false);
    }
}
