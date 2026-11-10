//! The Home page and title bar's view model: `wire` turns the window's callbacks into core
//! messages, `sync` pushes the core's state into its properties after each batch of messages.
//!
//! Everything shown comes from `App`; every action is a core message. `sync` sets a property only when its value differs, updates list models
//! row by row, and builds each news picture once, so a batch that changed nothing redraws
//! nothing.
use super::sync::{private, put, strings_model, sync_model};
use super::{send, AccountLine, Footer, Ink, MainWindow, NewsCard, Sheet, SheetStrings, Strings, View};
use crate::backend::AccountMode;
use crate::i18n::{t, tf, Lang};
use crate::account_row::RowState;
use crate::backend::LaunchWith;
use crate::services::NewsTab;
use crate::{App, EditorFlag, Field, MainAction, Message, Page};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ffxiah::AH_SERVERS;
use crate::play_update::{maintenance_percent, remaining_time};
const NEWS_TABS: [NewsTab; 4] = [NewsTab::Latest, NewsTab::Updates, NewsTab::Events, NewsTab::Maintenance];
const PAGES: [Page; 4] = [Page::Home, Page::Friends, Page::Maintenance, Page::Settings];
const CHARACTER_SELECT: [CharacterSelect; 2] = [CharacterSelect::Automatic, CharacterSelect::AskEachTime];
/// The footer's Character select choice for the accounts Play starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CharacterSelect {
    Automatic,
    AskEachTime,
}
impl std::fmt::Display for CharacterSelect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CharacterSelect::Automatic => t("Automatic"),
            CharacterSelect::AskEachTime => t("Ask me each time"),
        })
    }
}
const EDITOR_LAUNCH: [Option<LaunchWith>; 4] = [None, Some(LaunchWith::Windower), Some(LaunchWith::Ashita), Some(LaunchWith::Vanilla)];
const FIELDS: [Field; 7] = [Field::Label, Field::Server, Field::PolId, Field::PolPassword, Field::SeId, Field::SePassword, Field::PolHandleNames];
/// Below this width the title bar's labelled buttons show glyphs only, and Home stacks Look up
/// under the account list.
const COMPACT_W: f32 = 1180.0;

/// What the callbacks act on, as of the last sync, and the models the window draws from.
struct State {
    lang: Option<Lang>,
    privacy: bool,
    light: bool,
    row_keys: Vec<String>,
    row_actions: Vec<Option<Message>>,
    news_urls: Vec<String>,
    editor_key: Option<String>,
    flags: crate::EditorFlags,
    tool: i32,
    tool_values: Vec<Option<String>>,
    accounts: Rc<VecModel<AccountLine>>,
    news: Rc<VecModel<NewsCard>>,
    news_tabs: Rc<VecModel<SharedString>>,
    launch_items: Rc<VecModel<SharedString>>,
    select_items: Rc<VecModel<SharedString>>,
    launch_choices: Rc<VecModel<SharedString>>,
    tool_items: Rc<VecModel<SharedString>>,
    slot_items: Rc<VecModel<SharedString>>,
    /// News pictures by `NewsImage::id`, pruned to the ones shown.
    images: HashMap<u64, Image>,
}

impl State {
    fn new() -> Self {
        Self {
            lang: None,
            privacy: false,
            light: false,
            row_keys: Vec::new(),
            row_actions: Vec::new(),
            news_urls: Vec::new(),
            editor_key: None,
            flags: Default::default(),
            tool: 0,
            tool_values: Vec::new(),
            accounts: Rc::new(VecModel::default()),
            news: Rc::new(VecModel::default()),
            news_tabs: Rc::new(VecModel::default()),
            launch_items: Rc::new(VecModel::default()),
            select_items: Rc::new(VecModel::default()),
            launch_choices: Rc::new(VecModel::default()),
            tool_items: Rc::new(VecModel::default()),
            slot_items: Rc::new(VecModel::default()),
            images: HashMap::new(),
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Reads the last sync's state (never while a message is being delivered).
fn read<R>(f: impl FnOnce(&State) -> Option<R>) -> Option<R> {
    STATE.with(|cell| cell.borrow().as_ref().and_then(f))
}

/// Sends the message `f` picks from the last sync's state, after releasing it.
fn act(f: impl FnOnce(&State) -> Option<Message>) {
    if let Some(message) = read(f) {
        send(message);
    }
}

fn index(i: i32) -> Option<usize> {
    usize::try_from(i).ok()
}

/// Connect the page's callbacks (the host wires the window chrome's own).
pub(super) fn wire(ui: &MainWindow) {
    // A new window: fresh models (the old window's went with it) and every value set again.
    let state = State::new();
    let view = ui.global::<View>();
    view.set_accounts(ModelRc::from(state.accounts.clone()));
    view.set_news(ModelRc::from(state.news.clone()));
    view.set_news_tabs(ModelRc::from(state.news_tabs.clone()));
    view.set_launch_items(ModelRc::from(state.launch_items.clone()));
    view.set_select_items(ModelRc::from(state.select_items.clone()));
    view.set_launch_choices(ModelRc::from(state.launch_choices.clone()));
    view.set_tool_items(ModelRc::from(state.tool_items.clone()));
    view.set_slot_items(ModelRc::from(state.slot_items.clone()));
    view.set_ah_servers(ModelRc::new(VecModel::from(AH_SERVERS.iter().map(|(s, _)| SharedString::from(*s)).collect::<Vec<_>>())));
    view.set_regions(ModelRc::new(VecModel::from(vec![SharedString::from("US"), "EU".into(), "JP".into()])));
    STATE.with(|cell| *cell.borrow_mut() = Some(state));

    // ---- title bar ----
    view.on_navigate(|i| { if let Some(page) = index(i).and_then(|i| PAGES.get(i)) { send(Message::Navigate(*page)); } });
    view.on_toggle_privacy(|| act(|s| Some(Message::SetPrivacy(!s.privacy))));
    view.on_toggle_theme(|| act(|s| Some(Message::SetLightTheme(!s.light))));
    view.on_banner_clicked(|| send(Message::OpenSettingsTab(crate::settings_page::Tab::System)));

    // ---- news ----
    view.on_news_picked(|i| { if let Some(tab) = index(i).and_then(|i| NEWS_TABS.get(i)) { send(Message::NewsFilter(*tab)); } });
    view.on_refresh_news(|| send(Message::Refresh));
    view.on_view_all_news(|| send(Message::OpenUrl(crate::services::sources(crate::i18n::current()).home.into())));
    view.on_open_news(|i| act(|s| s.news_urls.get(index(i)?).map(|url| Message::OpenUrl(url.clone()))));

    // ---- accounts ----
    view.on_toggle_account(|i, on| act(|s| s.row_keys.get(index(i)?).map(|key| Message::ToggleAccount(key.clone(), on))));
    view.on_edit_account(|i| act(|s| s.row_keys.get(index(i)?).map(|key| Message::EditAccount(key.clone()))));
    view.on_otp_edited(|i, value| act(|s| s.row_keys.get(index(i)?).map(|key| Message::OtpChanged(key.clone(), value.into()))));
    view.on_row_action(|i| act(|s| s.row_actions.get(index(i)?).cloned().flatten()));
    view.on_shut_down_all(|| send(Message::ShutdownAll));
    view.on_play_all(|| send(Message::LaunchAll));
    view.on_import_accounts(|| send(Message::AccountImport(crate::account_import_ui::Msg::Open)));
    view.on_add_account(|| send(Message::NewAccount));
    view.on_drag_start(|i| act(|s| s.row_keys.get(index(i)?).map(|key| Message::AccountDragStart(key.clone()))));
    view.on_drag_over(|i| act(|s| s.row_keys.get(index(i)?).map(|key| Message::AccountDragOver(key.clone()))));

    // ---- look up ----
    view.on_wiki_edited(|value| send(Message::WikiQueryChanged(value.into())));
    view.on_wiki_search(|| send(Message::SearchWiki));
    view.on_ah_edited(|value| send(Message::AhQueryChanged(value.into())));
    view.on_ah_search(|| send(Message::SearchAh));
    view.on_ah_server_picked(|i| { if let Some(server) = index(i).and_then(|i| AH_SERVERS.get(i)) { send(Message::AhServerChanged(server.0)); } });
    view.on_open_wiki(|| send(Message::OpenUrl(wiki_site().1.into())));
    view.on_open_ffxiah(|| send(Message::OpenUrl("https://www.ffxiah.com/".into())));

    // ---- footer ----
    view.on_toggle_details(|| send(Message::ToggleDetails));
    view.on_install_repair(|| send(Message::Navigate(Page::Maintenance)));
    view.on_help_support(|| send(Message::OpenUrl("https://support.na.square-enix.com/".into())));
    view.on_launch_picked(|i| { if let Some(tool) = index(i).and_then(|i| LaunchWith::ALL.get(i)) { send(Message::SetLaunchWith(*tool)); } });
    view.on_select_picked(|i| { if let Some(choice) = index(i).and_then(|i| CHARACTER_SELECT.get(i)) { send(Message::SetCharacterSelect(*choice == CharacterSelect::Automatic)); } });
    view.on_shut_down_selected(|| send(Message::ShutdownSelected));
    view.on_main_action(|| send(Message::MainAction));

    // ---- account sheet ----
    view.on_close_sheet(|| send(Message::CloseEditor));
    view.on_save_sheet(|| send(Message::SaveAccount));
    view.on_sheet_edited(|i, value| { if let Some(field) = index(i).and_then(|i| FIELDS.get(i)) { send(Message::EditorChanged(*field, value.into())); } });
    view.on_sheet_folder_edited(|value| send(Message::AccountInstallChanged(value.into())));
    view.on_sheet_region(|i| { if (0..3).contains(&i) { send(Message::EditorRegion(i as u8)); } });
    view.on_sheet_launch(|i| { if let Some(value) = index(i).and_then(|i| EDITOR_LAUNCH.get(i)) { send(Message::EditorLaunchWith(*value)); } });
    view.on_sheet_flag(|i, on| act(|s| Some(match i {
        0 => Message::EditorFlag(EditorFlag::UsesOtp, on),
        1 => Message::EditorFlag(EditorFlag::SignInNow, on),
        2 => Message::EditorFlag(EditorFlag::ShowId, !s.flags.show_id),
        3 => Message::EditorFlag(EditorFlag::ShowPassword, !s.flags.show_password),
        4 => Message::EditorFlag(EditorFlag::ChangePassword, true),
        _ => return None,
    })));
    view.on_sheet_auto_login(|on| send(Message::EditorAutoLogin(on)));
    view.on_sheet_slot(|i| { if (0..16).contains(&i) { send(Message::EditorCharacterSlot(i as u8 + 1)); } });
    view.on_sheet_tool(|i| act(|s| {
        let value = s.tool_values.get(index(i)?)?.clone();
        match s.tool {
            1 => Some(Message::EditorBootIni(value)),
            2 => Some(Message::EditorProfile(value)),
            _ => None,
        }
    }));
    view.on_edit_profiles(|| send(Message::EditWindowerProfiles));
    view.on_browse_folder(|| send(Message::Browse(crate::PickTarget::AccountGameFolder)));
    view.on_sign_out(|| act(|s| s.editor_key.clone().map(Message::AccountSignOut)));
    view.on_view_log(|| act(|s| s.editor_key.clone().map(Message::OpenLog)));
    view.on_open_logs(|| act(|s| s.editor_key.clone().map(Message::OpenLogsFolder)));
    view.on_delete_account(|| act(|s| s.editor_key.clone().map(Message::RequestDeleteAccount)));
}

/// The window closed: its models and news pictures go with it (nothing stays in the background).
pub(super) fn forget() {
    STATE.with(|cell| cell.borrow_mut().take());
}


/// Push `app`'s state into the window. Called after every batch of messages: only what
/// changed is set, so an unchanged state redraws nothing.
pub(super) fn sync(ui: &MainWindow, app: &App) {
    let view = ui.global::<View>();
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(State::new);
        state.privacy = app.settings.privacy;
        state.light = app.settings.light_theme;
        let lang = crate::i18n::current();
        if state.lang != Some(lang) {
            state.lang = Some(lang);
            put!(view, get_strings, set_strings, strings());
            put!(view, get_sheet_strings, set_sheet_strings, sheet_strings());
            put!(view, get_mixed, set_mixed, t("Mixed").into());
            strings_model(&state.news_tabs, NEWS_TABS.iter().map(|tab| tab_label(*tab).to_owned()));
            strings_model(&state.launch_items, LaunchWith::ALL.iter().map(ToString::to_string));
            strings_model(&state.select_items, CHARACTER_SELECT.iter().map(ToString::to_string));
            strings_model(&state.launch_choices, [t("Use default"), "Windower", "Ashita", t("Vanilla")].map(str::to_owned));
            strings_model(&state.slot_items, (1u8..=16).map(|number| tf("Slot {number}", &[("number", &number)])));
            put!(view, get_serif_greeting, set_serif_greeting, lang != Lang::Ja);
        }
        shell(&view, app);
        news(&view, app, state);
        accounts(&view, app, state);
        look_up(&view, app);
        footer(&view, app);
        sheet(&view, app, state);
    });
}

fn strings() -> Strings {
    let (wiki_title, _, wiki_placeholder) = wiki_site();
    Strings {
        home: t("Home").into(),
        friends: t("Friends").into(),
        downloads: t("Downloads").into(),
        settings: t("Settings").into(),
        privacy: t("Privacy mode").into(),
        privacy_on_tip: t("Privacy mode is on. Click to turn it off.").into(),
        privacy_off_tip: t("Blur names, IDs and messages").into(),
        light_theme: t("Light theme").into(),
        dark_theme: t("Dark theme").into(),
        greeting: t("Welcome back to Vana’diel.").into(),
        news: t("PlayOnline news").into(),
        refresh_news: t("Refresh news").into(),
        view_all_news: t("View all news").into(),
        more_news: t("More on PlayOnline").into(),
        accounts: t("Accounts").into(),
        shut_down_all: t("Shut down all").into(),
        play_all: t("Play all").into(),
        import: t("Import").into(),
        add_account: t("Add account").into(),
        no_accounts: t("No accounts yet").into(),
        no_accounts_body: t("Add a PlayOnline account.").into(),
        code: t("Code").into(),
        sign_in: t("Sign in").into(),
        look_up: t("Look up").into(),
        open_website: t("Open the website").into(),
        wiki_title: wiki_title.into(),
        wiki_placeholder: wiki_placeholder.into(),
        find_item: t("Find an item").into(),
        install_repair: t("Install & repair").into(),
        help_support: t("Help & support").into(),
        launch_with: t("Launch with").into(),
        character_select: t("Character select").into(),
    }
}

fn sheet_strings() -> SheetStrings {
    SheetStrings {
        account: t("Account").to_uppercase().into(),
        launch: t("Launch").to_uppercase().into(),
        status: t("Status").to_uppercase().into(),
        after_adding: t("After adding").to_uppercase().into(),
        display_name: t("Display name").into(),
        display_placeholder: t("How it appears in Crystal, e.g. Mule").into(),
        private_note: t("Private server · client temporarily unavailable.").into(),
        server: t("Server").into(),
        server_placeholder: t("Server hostname").into(),
        show: t("Show").into(),
        hide: t("Hide").into(),
        change: t("Change").into(),
        se_id: t("Square Enix ID").into(),
        se_id_placeholder: t("Square Enix account ID").into(),
        se_password: t("Square Enix password").into(),
        password_placeholder: t("Password").into(),
        pol_password_placeholder: t("PlayOnline password").into(),
        region: t("Region").into(),
        region_note: t("Retail launch is currently verified with the US configuration.").into(),
        uses_otp: t("Uses a one-time password (security token)").into(),
        handles: t("PlayOnline handle setup (optional)").into(),
        handles_placeholder: t("Public handle name").into(),
        handles_note: t("Registers your characters on first login. Up to eight per handle, separated by commas.").into(),
        saved_note: t("Saved encrypted on this PC. Sent only to PlayOnline.").into(),
        launch_with: t("Launch with").into(),
        auto_login: t("Log in to a character automatically").into(),
        slot: t("Character slot").into(),
        slot_note: t("Unavailable slots stay at character selection.").into(),
        folder: t("Game folder for this account").into(),
        folder_placeholder: t("Use the launcher's default game folder").into(),
        folder_note: t("Leave blank to use the default.").into(),
        active_note: t("Changes apply on the next launch.").into(),
        last_play: t("Last Play:").into(),
        view_log: t("View log").into(),
        open_logs: t("Open logs folder").into(),
        sign_out: t("Sign out").into(),
        sign_in_now: t("Sign in now").into(),
        delete: t("Delete account").into(),
        cancel: t("Cancel").into(),
        edit_profiles: t("Edit profiles in Windower").into(),
        close: t("Close").into(),
    }
}

/// The wiki for the launcher's language: its name, home page and search placeholder.
fn wiki_site() -> (&'static str, &'static str, &'static str) {
    match crate::i18n::current() {
        Lang::En => ("BG Wiki", "https://www.bg-wiki.com/", t("Search BG Wiki")),
        Lang::Ja => ("FF11用語辞典", "https://wiki.ffo.jp/", t("Search FF11 Dictionary")),
    }
}

/// The title bar and the hero band.
fn shell(view: &View, app: &App) {
    let page = PAGES.iter().position(|page| *page == app.page).unwrap_or(0) as i32;
    put!(view, get_page, set_page, page);
    put!(view, get_compact, set_compact, app.viewport.width < COMPACT_W);
    put!(view, get_wide, set_wide, app.viewport.width >= COMPACT_W);
    put!(view, get_privacy, set_privacy, app.settings.privacy);
    let count = app.accounts.iter().filter(|row| row.pid.is_some() && !row.shutting_down && !row.signing_out).count();
    let line = match count {
        0 => t("No accounts signed in").to_owned(),
        1 => t("1 account signed in").to_owned(),
        _ => tf("{count} accounts signed in", &[("count", &count)]),
    };
    put!(view, get_signed_in, set_signed_in, line.into());
    put!(view, get_any_signed_in, set_any_signed_in, count > 0);
    // The Windows components banner.
    let banner = crate::prereqs::banner_text(&app.prereqs);
    put!(view, get_banner, set_banner, banner.is_some());
    if let Some(banner) = banner {
        put!(view, get_banner_lead, set_banner_lead, banner.lead.into());
        put!(view, get_banner_rest, set_banner_rest, banner.rest.into());
        put!(view, get_banner_action, set_banner_action, banner.button.into());
        put!(view, get_banner_restart, set_banner_restart, banner.restart);
    }
}

fn tab_label(tab: NewsTab) -> &'static str {
    match tab {
        NewsTab::Latest => t("Latest"),
        NewsTab::Updates => t("Updates"),
        NewsTab::Events => t("Events"),
        NewsTab::Maintenance => t("Maintenance"),
    }
}

/// The feeds carry no category field, so the tabs go by title words.
fn news_matches(item: &crate::services::NewsItem, tab: NewsTab) -> bool {
    let title = item.title.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|word| title.contains(word));
    match tab {
        NewsTab::Updates => any(&["update", "trial", "バージョンアップ", "アップデート", "更新", "トライアル"]),
        NewsTab::Events => any(&["event", "campaign", "happenings", "bonanza", "イベント", "キャンペーン", "異変", "ボナンザ"]),
        NewsTab::Latest | NewsTab::Maintenance => true,
    }
}

/// A card's kind, and the colour it is written in.
fn category(item: &crate::services::NewsItem) -> (&'static str, Ink) {
    if news_matches(item, NewsTab::Updates) {
        (t("Update"), Ink::Ok)
    } else if item.title.to_ascii_lowercase().contains("bonanza") || item.title.contains("ボナンザ") {
        (t("Community"), Ink::Warn)
    } else {
        (t("Event"), Ink::Info)
    }
}

/// The month and day in any date text the official sources use.
fn news_month_day(raw: &str) -> Option<(u32, u32)> {
    use chrono::Datelike;
    let normalized = raw.replace(" JST", " +0900");
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(&normalized).or_else(|_| chrono::DateTime::parse_from_rfc3339(raw)) {
        return Some((date.month(), date.day()));
    }
    let prefix = raw.split(" [").next().unwrap_or(raw);
    if let Ok(date) = chrono::NaiveDateTime::parse_from_str(prefix, "%b. %d, %Y %H:%M") {
        return Some((date.month(), date.day()));
    }
    let ymd = raw.trim().split(['(', '（', ' ']).next()?;
    let date = chrono::NaiveDate::parse_from_str(ymd, "%Y.%m.%d").ok()?;
    Some((date.month(), date.day()))
}

/// "Sep 26" / "9月26日", or the text as it came.
fn news_date(raw: &str) -> String {
    let Some((month, day)) = news_month_day(raw) else { return raw.to_owned() };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    match crate::i18n::current() {
        Lang::Ja => format!("{month}月{day}日"),
        Lang::En => format!("{} {day}", MONTHS[month as usize - 1]),
    }
}

// ---- headline wrapping ----
// The card's two-line blocks are wrapped here rather than by the text engine:
// Slint's wrap-and-elide misplaces lines of Japanese set in a fallback face.

/// Approximate advance of `ch` in ems, slightly generous so a line judged to fit does.
fn glyph_em(ch: char) -> f32 {
    match ch {
        ' ' => 0.28,
        'i' | 'j' | 'l' | 'I' | '.' | ',' | ':' | ';' | '\'' | '’' | '!' | '|' => 0.26,
        'f' | 't' | 'r' | '(' | ')' | '[' | ']' | '"' | '“' | '”' | '/' => 0.36,
        '-' => 0.40,
        's' | 'c' | 'z' | 'J' => 0.47,
        'm' | 'M' | 'W' | '—' => 0.9,
        'w' | '&' | '%' | '@' => 0.76,
        'a'..='z' => 0.56,
        'A'..='Z' => 0.66,
        '0'..='9' => 0.56,
        '\u{1100}'..='\u{115f}'
        | '\u{2e80}'..='\u{a4cf}'
        | '\u{ac00}'..='\u{d7a3}'
        | '\u{f900}'..='\u{faff}'
        | '\u{fe30}'..='\u{fe6f}'
        | '\u{ff00}'..='\u{ff60}'
        | '\u{ffe0}'..='\u{ffe6}' => 1.0,
        _ => 0.62,
    }
}

fn text_em(line: &str) -> f32 {
    line.chars().map(glyph_em).sum()
}

/// Word-wraps `text` into at most `max_lines` lines of `width` px at `size` px; text beyond
/// the last line ends in an ellipsis. Words too long for a line break between characters.
fn wrap_lines(text: &str, width: f32, size: f32, max_lines: usize) -> Vec<String> {
    let max_em = (width / size.max(1.0)).max(1.0);
    let mut words: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if text_em(word) <= max_em {
            words.push(word.to_owned());
            continue;
        }
        let mut piece = String::new();
        for ch in word.chars() {
            if !piece.is_empty() && text_em(&piece) + glyph_em(ch) > max_em {
                words.push(std::mem::take(&mut piece));
            }
            piece.push(ch);
        }
        if !piece.is_empty() {
            words.push(piece);
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in &words {
        if current.is_empty() {
            current.push_str(word);
        } else if text_em(&current) + glyph_em(' ') + text_em(word) <= max_em {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::replace(&mut current, word.clone()));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if max_lines > 0 && lines.len() > max_lines {
        lines.truncate(max_lines);
        let last = lines.last_mut().expect("max_lines > 0");
        while !last.is_empty() && text_em(last) + glyph_em('…') > max_em {
            match last.rfind(' ') {
                Some(cut) => last.truncate(cut),
                None => {
                    last.pop();
                }
            }
        }
        let trimmed = last.trim_end_matches(|ch: char| ch.is_whitespace() || ",;:-–—".contains(ch)).len();
        last.truncate(trimmed);
        last.push('…');
    }
    lines
}

/// `wrap_lines` for headlines: a title that fits two lines breaks where they come out most
/// even, never just before a separator.
fn balanced_lines(text: &str, width: f32, size: f32, max_lines: usize) -> Vec<String> {
    let greedy = wrap_lines(text, width, size, max_lines);
    if max_lines < 2 || greedy.len() != 2 || greedy[1].ends_with('…') {
        return greedy;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let max_em = (width / size.max(1.0)).max(1.0);
    let mut best: Option<(f32, usize)> = None;
    for split in 1..words.len() {
        let (first, second) = (words[..split].join(" "), words[split..].join(" "));
        let (a, b) = (text_em(&first), text_em(&second));
        if a > max_em || b > max_em {
            continue;
        }
        let opens_on_separator = second.starts_with(['·', '|', '-', ':', '–', '—']);
        let score = (a - b).abs() + if b > a { 0.5 } else { 0.0 } + if opens_on_separator { 100.0 } else { 0.0 };
        if best.is_none_or(|(s, _)| score < s) {
            best = Some((score, split));
        }
    }
    match best {
        Some((_, split)) => vec![words[..split].join(" "), words[split..].join(" ")],
        None => greedy,
    }
}

/// A news picture, built once per image id.
fn picture(images: &mut HashMap<u64, Image>, source: &crate::services::NewsImage) -> Option<Image> {
    let expected = source.width as usize * source.height as usize * 4;
    if source.width == 0 || source.height == 0 || source.pixels.len() < expected {
        return None;
    }
    Some(
        images
            .entry(source.id)
            .or_insert_with(|| {
                let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&source.pixels[..expected], source.width, source.height);
                Image::from_rgba8(buffer)
            })
            .clone(),
    )
}

fn news(view: &View, app: &App, state: &mut State) {
    let s = super::scale(app);
    let width = (app.viewport.width - 80.0 * s).max(160.0);
    let gap = 16.0 * s;
    let cols = (1..=4usize).rev().find(|&n| (width - gap * (n - 1) as f32) / n as f32 >= 230.0 * s).unwrap_or(1);
    put!(view, get_news_cols, set_news_cols, cols as i32);
    let card_w = ((width - gap * (cols - 1) as f32) / cols as f32).floor();
    put!(view, get_news_card_w, set_news_card_w, card_w);
    let text_w = card_w - 2.0 - 28.0 * s;
    put!(view, get_news_thumbs, set_news_thumbs, app.viewport.height >= 760.0);
    put!(view, get_news_tab, set_news_tab, NEWS_TABS.iter().position(|tab| *tab == app.news_filter).unwrap_or(0) as i32);

    let month = {
        use chrono::Datelike;
        chrono::Local::now().month()
    };
    let this_month = app
        .services
        .iter()
        .flat_map(|snapshot| snapshot.news.iter().chain(&snapshot.notices))
        .filter(|item| news_month_day(&item.date).is_some_and(|(m, _)| m == month))
        .count();
    let note = match this_month {
        0 => String::new(),
        1 => t("1 notice this month").to_owned(),
        _ => tf("{count} notices this month", &[("count", &this_month)]),
    };
    put!(view, get_news_note, set_news_note, note.into());

    let mut picked: Vec<(&crate::services::NewsItem, &'static str, Ink, bool)> = Vec::new();
    if let Some(snapshot) = &app.services {
        if app.news_filter == NewsTab::Maintenance {
            picked.extend(snapshot.notices.iter().take(cols).map(|item| (item, t("Maintenance"), Ink::Sub, true)));
        } else {
            picked.extend(
                snapshot.news.iter().filter(|item| news_matches(item, app.news_filter)).take(cols).map(|item| {
                    let (label, ink) = category(item);
                    (item, label, ink, false)
                }),
            );
        }
    }
    let shown: Vec<u64> = picked.iter().filter_map(|(item, ..)| item.image.as_ref().map(|image| image.id)).collect();
    state.images.retain(|id, _| shown.contains(id));
    let mut cards = Vec::with_capacity(picked.len());
    state.news_urls.clear();
    for (item, label, ink, notice) in &picked {
        let image = item.image.as_ref().and_then(|source| picture(&mut state.images, source));
        cards.push(NewsCard {
            title: balanced_lines(&item.title, text_w, 16.0 * s, 2).join("\n").into(),
            teaser: wrap_lines(&item.summary, text_w, 13.0 * s, 2).join("\n").into(),
            kind: label.to_uppercase().into(),
            kind_ink: *ink,
            date: news_date(&item.date).into(),
            has_image: image.is_some(),
            image: image.unwrap_or_default(),
            notice: *notice,
        });
        state.news_urls.push(item.url.clone());
    }
    put!(view, get_news_more, set_news_more, !picked.is_empty() && cols > picked.len());
    let (news_state, title, body) = if !picked.is_empty() {
        (0, "", "")
    } else if app.news_loading {
        (1, t("Loading PlayOnline news…"), "")
    } else if !app.news_error.is_empty() {
        (2, t("PlayOnline news is unavailable"), app.news_error.as_str())
    } else {
        (3, t("No articles in this category."), t("Try another tab."))
    };
    put!(view, get_news_state, set_news_state, news_state);
    put!(view, get_news_empty_title, set_news_empty_title, title.into());
    put!(view, get_news_empty_body, set_news_empty_body, body.into());
    sync_model(&state.news, cards);
}

fn row_ink(state: RowState) -> Ink {
    match state {
        RowState::Closing | RowState::Waiting | RowState::Updating | RowState::ViewerConfirm | RowState::Starting | RowState::SigningIn => Ink::Warn,
        RowState::Playing => Ink::Info,
        RowState::SignedIn => Ink::Ok,
        RowState::Private | RowState::Idle => Ink::Sub,
        RowState::NeedsCode | RowState::Problem => Ink::Danger,
    }
}

fn accounts(view: &View, app: &App, state: &mut State) {
    let total = app.accounts.len();
    let ticked = app.accounts.iter().filter(|row| row.selected).count();
    put!(view, get_accounts_note, set_accounts_note, tf("{ticked} of {total} selected", &[("ticked", &ticked), ("total", &total)]).into());
    put!(view, get_show_shut_down_all, set_show_shut_down_all, app.shutdown_count(false) > 0);
    put!(view, get_show_play_all, set_show_play_all, app.launchable_count() > 1 && app.vault_ready);
    put!(view, get_can_add, set_can_add, app.vault_ready && !app.vault_saving);
    let editing = app.editor.as_ref().and_then(|editor| editor.key.as_deref());
    state.row_keys.clear();
    state.row_actions.clear();
    let mut lines = Vec::with_capacity(total);
    for item in &app.accounts {
        let row = crate::account_row::row_view(app, item);
        let working = item.busy || item.shutting_down || item.signing_out || item.game_start_pending();
        let (action, action_soft, action_enabled, message) = match row.action {
            Some((label, message, soft)) => (label, soft, true, Some(message)),
            None if row.otp => (t("Sign in").to_owned(), true, false, None),
            None => (String::new(), false, false, None),
        };
        lines.push(AccountLine {
            name: crate::privacy::mask(&item.account.label).into(),
            checked: item.selected,
            editing: editing == Some(item.key.as_str()),
            name_enabled: !app.vault_saving,
            ink: row_ink(crate::account_row::row_state(app, item)),
            label: private(app, &row.label).into(),
            detail: private(app, &row.detail).into(),
            progress: row.progress.unwrap_or(-1.0),
            percent: row.progress.map(|fraction| format!("{:.0}%", fraction * 100.0)).unwrap_or_default().into(),
            working: working && row.progress.is_none(),
            otp: row.otp,
            otp_text: item.otp.as_str().into(),
            action: action.into(),
            action_soft,
            action_enabled,
            dragging: app.account_drag.source.as_deref() == Some(item.key.as_str()),
        });
        state.row_keys.push(item.key.clone());
        state.row_actions.push(message);
    }
    sync_model(&state.accounts, lines);
    put!(view, get_dragging, set_dragging, app.account_drag.source.is_some());
}

fn look_up(view: &View, app: &App) {
    put!(view, get_wiki_query, set_wiki_query, app.wiki_query.as_str().into());
    put!(view, get_ah_query, set_ah_query, app.ah_query.as_str().into());
    put!(view, get_ah_server, set_ah_server, AH_SERVERS.iter().position(|(server, _)| *server == app.ah_server).map_or(-1, |i| i as i32));
}

// ---- the footer play bar ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlayState {
    Play,
    Starting,
    Playing,
}

fn play_state(starting: bool, running: usize, launchable: usize) -> PlayState {
    if launchable > 0 {
        PlayState::Play
    } else if starting {
        PlayState::Starting
    } else if running > 0 {
        PlayState::Playing
    } else {
        PlayState::Play
    }
}

fn shared<T: PartialEq + Copy>(values: impl IntoIterator<Item = T>) -> Option<T> {
    let mut values = values.into_iter();
    let first = values.next()?;
    values.all(|value| value == first).then_some(first)
}

fn maintenance_status(app: &App) -> String {
    let progress = &app.maintenance_progress;
    let mut parts = vec![app.maintenance_label().to_owned()];
    if let Some(percent) = maintenance_percent(progress) {
        parts.push(format!("{percent}%"));
    }
    if let Some(left) = progress.overall.map_or(progress.remaining, |overall| overall.remaining).filter(|left| !left.is_zero()) {
        parts.push(tf("{time} left", &[("time", &remaining_time(left))]));
    }
    parts.join(" · ")
}

fn footer(view: &View, app: &App) {
    let current = app.accounts.iter().find(|a| a.selected).or_else(|| app.accounts.first());
    let main_action = app.main_action();
    let targets: Vec<&crate::AccountRow> = {
        let selected: Vec<_> = app.accounts.iter().filter(|a| a.selected).collect();
        if selected.is_empty() { current.into_iter().collect() } else { selected }
    };
    let running = targets.iter().filter(|a| a.is_playing()).count();
    let play_state = play_state(targets.iter().any(|a| a.game_start_pending()), running, app.selected_count());
    let playing_row = targets.iter().find(|a| a.is_playing());
    let starting_row = targets.iter().find(|a| a.game_start_pending());
    let play_update = targets.iter().find_map(|a| crate::play_update::line(app, &a.key));
    let (status, detail, tone): (String, String, i32) = if let Some(kind) = app.maintenance {
        (maintenance_status(app), super::downloads::dl_stage(kind, app.maintenance_progress.stage), 2)
    } else if let Some(update) = &play_update {
        (update.headline.clone(), String::new(), 2)
    } else if let Some(update) = (main_action == MainAction::Maintenance)
        .then(|| targets.iter().find_map(|a| crate::play_update::watch_line(app, &a.key))).flatten() {
        (t("Maintenance").into(), update.headline, 2)
    } else if matches!(main_action, MainAction::Maintenance | MainAction::Offline) && targets.iter().any(|a| app.is_waiting(&a.key)) {
        ((if main_action == MainAction::Maintenance { t("Maintenance") } else { t("Worlds offline") }).into(), app.wait_headline().into(), 2)
    } else if main_action == MainAction::Maintenance {
        (t("Maintenance").into(), t("View official maintenance notices").into(), 1)
    } else if main_action == MainAction::Offline {
        (t("Worlds offline").into(), t("View official world status").into(), 1)
    } else if let (PlayState::Playing, Some(row)) = (play_state, playing_row) {
        let who = if running > 1 { tf("{running} accounts", &[("running", &running)]) } else { row.account.label.clone() };
        (tf("Playing · {who}", &[("who", &who)]), String::new(), 0)
    } else if let (PlayState::Starting, Some(row)) = (play_state, starting_row) {
        let step = if row.status.is_empty() { t("Signing in").into() } else { row.status_text().into_owned() };
        (tf("Starting · {name}", &[("name", &row.account.label)]), step, 2)
    } else {
        let verified = app.patch.as_ref().is_some_and(|p| p.files_verified == Some(true));
        let (s, d, tone) = match app.patch.as_ref().map(|p| p.state) {
            Some(crate::patch::PatchState::Current) => (t("Ready to play"), if verified { t("Game files verified") } else { "" }, 0),
            Some(crate::patch::PatchState::Required) => (t("Update available"), "", 1),
            _ if app.viewer.as_ref().is_some_and(crate::viewer::Status::requires_update) => (t("PlayOnline update needed"), "", 1),
            Some(crate::patch::PatchState::Interrupted) => (t("Interrupted update"), "", 1),
            _ if app.patch_loading => (t("Checking version…"), "", 2),
            _ if verified => (t("Files verified"), "", 0),
            _ => (t("Ready to launch"), "", 3),
        };
        (s.into(), d.into(), tone)
    };
    let badge_ink = if tone == 1 { Ink::Warn } else if tone == 0 { Ink::Ok } else { Ink::Sub };
    let badge_fill = match tone { 0 => 3, 1 => 1, _ => 2 };
    let wait_label = (app.vault_ready && !app.wait_keys().is_empty())
        .then(|| t("Wait and play"))
        .or_else(|| targets.iter().any(|a| app.is_waiting(&a.key)).then(|| t("Waiting for FFXI…")));
    let count = app.selected_count();
    let (label, ink): (String, Ink) = match main_action {
        MainAction::Play if play_update.is_some() => (play_update.as_ref().map(|update| update.label.clone()).unwrap_or_default(), Ink::Sub),
        MainAction::Maintenance | MainAction::Offline if wait_label.is_some() => (wait_label.unwrap_or_default().to_owned(), badge_ink),
        MainAction::Maintenance => (t("Maintenance").to_owned(), badge_ink),
        MainAction::Offline => (t("Worlds offline").to_owned(), badge_ink),
        MainAction::Working if app.reshade.busy => (t("ReShade setup").to_owned(), Ink::Sub),
        MainAction::Working => (
            match maintenance_percent(&app.maintenance_progress) {
                Some(percent) if app.maintenance.is_some() => tf("{label} · {percent}%", &[("label", &app.maintenance_label()), ("percent", &percent)]),
                _ => app.maintenance_label().to_owned(),
            },
            Ink::Sub,
        ),
        MainAction::Play => match play_state {
            PlayState::Playing => (t("Playing").to_owned(), Ink::Ok),
            PlayState::Starting => (t("Starting…").to_owned(), Ink::Sub),
            PlayState::Play if count > 0 => (tf("Play {count}", &[("count", &count)]), Ink::PlayText),
            PlayState::Play => (t("Play").to_owned(), Ink::Sub),
        },
    };
    let glyph = match main_action {
        MainAction::Play if play_update.is_some() => 3,
        MainAction::Maintenance => 4,
        MainAction::Offline => 5,
        MainAction::Working => 2,
        MainAction::Play => match play_state {
            PlayState::Play => 0,
            PlayState::Playing => 1,
            PlayState::Starting => 2,
        },
    };
    let enabled = main_action != MainAction::Play || app.vault_ready && count > 0;
    let fill = match main_action {
        MainAction::Maintenance | MainAction::Offline => badge_fill,
        MainAction::Working => 2,
        MainAction::Play if play_state == PlayState::Playing => 3,
        _ if !enabled => 2,
        _ => 0,
    };
    let ready = main_action == MainAction::Play && play_state == PlayState::Play && count > 0 && play_update.is_none();
    let names = if ready && app.viewport.width >= 1280.0 {
        app.accounts
            .iter()
            .filter(|row| row.selected && row.account.mode == AccountMode::Retail && row.can_play())
            .map(|row| crate::privacy::mask(&row.account.label))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    };
    let can_shutdown = app.shutdown_count(true) > 0;
    let shutdown_pending = app.accounts.iter().any(|a| a.selected && a.shutting_down);
    let busy = !can_shutdown && shutdown_pending;
    let retail: Vec<&&crate::AccountRow> = targets.iter().filter(|row| row.account.mode == AccountMode::Retail).collect();
    let picks = !retail.is_empty() && app.viewport.width >= 1100.0;
    let launcher = shared(retail.iter().map(|row| app.launcher_for(&row.account)));
    let select = shared(retail.iter().map(|row| app.settings.account_auto_login.get(&row.key).copied().unwrap_or_default().enabled));
    let footer = Footer {
        status: private(app, &status).into(),
        detail: private(app, &detail).into(),
        badge: tone,
        shutdown: can_shutdown || shutdown_pending,
        shutdown_busy: busy,
        shutdown_enabled: can_shutdown,
        shutdown_label: (if busy { t("Shutting down…") } else { t("Shut down selected") }).into(),
        picks,
        launch_current: launcher.and_then(|tool| LaunchWith::ALL.iter().position(|t| *t == tool)).map_or(-1, |i| i as i32),
        select_current: select.map_or(-1, |automatic| if automatic { 0 } else { 1 }),
        play_label: label.into(),
        play_glyph: glyph,
        play_ink: ink,
        play_fill: fill,
        play_enabled: enabled,
        names: names.into(),
    };
    put!(view, get_footer, set_footer, footer);
}

// ---- the account sheet ----

/// The picker's entries: "Default (…)" first, then the names found on disk, then the account's own value
/// if it is no longer there. Returns the labels, values and the selected index.
fn picks(default_label: String, found: &[String], current: Option<&String>, missing: impl Fn(&str) -> String) -> (Vec<String>, Vec<Option<String>>, i32) {
    let mut labels = vec![default_label];
    let mut values = vec![None];
    for name in found {
        labels.push(name.clone());
        values.push(Some(name.clone()));
    }
    let selected = match current {
        None => 0,
        Some(name) => match values.iter().position(|v| v.as_ref().is_some_and(|v| v.eq_ignore_ascii_case(name))) {
            Some(i) => i,
            None => {
                labels.push(missing(name));
                values.push(Some(name.clone()));
                values.len() - 1
            }
        },
    };
    (labels, values, selected as i32)
}

/// Whether the account sheet shows: open, and no close or delete confirmation over it.
pub(super) fn sheet_shown(app: &App) -> bool {
    app.editor.is_some() && !app.confirm_close && app.pending_delete.is_none()
}

fn sheet(view: &View, app: &App, state: &mut State) {
    // The close and delete confirmations stack above the sheet: it hides behind them.
    put!(view, get_sheet_open, set_sheet_open, sheet_shown(app));
    let Some(editor) = &app.editor else {
        state.editor_key = None;
        // A closed sheet keeps no sign-in details in the window.
        put!(view, get_sheet_label, set_sheet_label, SharedString::new());
        put!(view, get_sheet_server, set_sheet_server, SharedString::new());
        put!(view, get_sheet_pol_id, set_sheet_pol_id, SharedString::new());
        put!(view, get_sheet_pol_password, set_sheet_pol_password, SharedString::new());
        put!(view, get_sheet_se_id, set_sheet_se_id, SharedString::new());
        put!(view, get_sheet_se_password, set_sheet_se_password, SharedString::new());
        put!(view, get_sheet_handles, set_sheet_handles, SharedString::new());
        put!(view, get_sheet_folder, set_sheet_folder, SharedString::new());
        return;
    };
    state.editor_key = editor.key.clone();
    state.flags = editor.flags;
    let account = &editor.account;
    let flags = editor.flags;
    let privacy = crate::privacy::on();
    let retail = account.mode == AccountMode::Retail;
    let editing = editor.key.is_some();
    let row = editor.key.as_ref().and_then(|key| app.accounts.iter().find(|row| &row.key == key));
    let active = row.is_some_and(|row| row.busy || row.pid.is_some());
    let can_type = !editing || flags.change_password;

    // Launch.
    let default = app.settings.launch_with;
    let launch_index = EDITOR_LAUNCH
        .iter()
        .position(|value| *value == account.launch_with.filter(|tool| *tool != LaunchWith::Native))
        .unwrap_or(0) as i32;
    let launch_hint = match account.launch_with {
        None => tf("Default launcher: {default}.", &[("default", &default)]),
        Some(LaunchWith::Native) => tf("The former native client is unavailable. This retail account now uses {default}.", &[("default", &default)]),
        Some(LaunchWith::Vanilla) => t("Starts FFXI directly, without Windower or Ashita.").into(),
        Some(tool) => tf("Signs in without PlayOnline and starts the game through {tool}.", &[("tool", &tool)]),
    };
    let found = &app.launch_tools.detected;
    let (tool, tool_label, tool_note, labels, values, tool_current) = match account.effective_launch_with(default) {
        LaunchWith::Ashita => {
            let base = app.settings.ashita_boot_ini.trim();
            let base = if base.is_empty() { crate::pol_less::tools::DEFAULT_BOOT_INI } else { base };
            let (labels, values, current) = picks(tf("Default ({name})", &[("name", &base)]), &found.boot_inis, account.ashita_boot_ini.as_ref(), |name| tf("{name} (missing)", &[("name", &name)]));
            let note = if found.ashita.is_none() { t("Ashita was not found. Set its folder in Settings → System → Launch tools.") } else { "" };
            (1, t("Ashita boot config"), note, labels, values, current)
        }
        LaunchWith::Windower => {
            let base = app.settings.windower_profile.trim();
            let base = if base.is_empty() { t("Windower's default profile") } else { base };
            let (labels, values, current) = picks(tf("Default ({name})", &[("name", &base)]), &found.profiles, account.windower_profile.as_ref(), |name| tf("{name} (not in settings.xml)", &[("name", &name)]));
            let note = if found.windower.is_none() { t("Windower was not found. Set its folder in Settings → System → Launch tools.") } else { "" };
            (2, t("Windower profile"), note, labels, values, current)
        }
        LaunchWith::Native | LaunchWith::Vanilla => (0, "", "", Vec::new(), Vec::new(), -1),
    };
    state.tool = tool;
    state.tool_values = values;
    strings_model(&state.tool_items, labels);

    // Status (saved accounts) / After adding (new ones).
    let (has_status, status_ink, status_label, status_detail, sign_out_enabled, timing, has_log, log_enabled) = match row {
        Some(row) => {
            let line = crate::account_row::row_view(app, row);
            let idle = !row.busy && row.pid.is_none();
            (
                true,
                row_ink(crate::account_row::row_state(app, row)),
                private(app, &line.label),
                private(app, &line.detail),
                row.can_sign_out() && !app.launch_blocked(),
                row.timing.clone(),
                row.log.is_some(),
                idle,
            )
        }
        None => (false, Ink::Sub, String::new(), String::new(), false, String::new(), false, false),
    };
    let allowed = app.settings.auto_sign_in && !flags.uses_otp;
    let sign_in_note = if !app.settings.auto_sign_in {
        t("Automatic sign-in is off in Settings → System.")
    } else if flags.uses_otp {
        t("One-time password accounts sign in when you type a code on Home.")
    } else {
        ""
    };

    let sheet = Sheet {
        editing,
        caption: (if editing { t("Account settings") } else { t("New account") }).to_uppercase().into(),
        title: (if editing { crate::privacy::mask(&account.label) } else { t("Add account").to_owned() }).into(),
        retail,
        active,
        label_masked: privacy,
        id_label: (if retail { t("PlayOnline ID") } else { t("Username") }).into(),
        id_placeholder: (if retail { t("Your PlayOnline ID") } else { t("Account ID") }).into(),
        id_masked: privacy || editing && !flags.show_id,
        id_button: (if flags.show_id { t("Hide") } else { t("Show") }).into(),
        password_label: (if retail { t("PlayOnline password") } else { t("Password") }).into(),
        password_masked: privacy || !flags.show_password,
        password_enabled: can_type,
        password_button: (if !can_type { t("Change") } else if flags.show_password { t("Hide") } else { t("Show") }).into(),
        se_id_masked: privacy,
        handles_masked: privacy,
        region: i32::from(account.region.min(2)),
        uses_otp: flags.uses_otp,
        launch_index,
        launch_hint: launch_hint.into(),
        tool,
        tool_label: tool_label.into(),
        tool_current,
        tool_note: tool_note.into(),
        edit_profiles_enabled: found.windower.is_some() && !app.windower_editor_open,
        auto_login: editor.auto_login.enabled,
        slot: i32::from(editor.auto_login.character_slot.clamp(1, 16)) - 1,
        folder_masked: privacy,
        folder_enabled: true,
        browse_enabled: !app.vault_saving,
        has_status,
        status_ink,
        status_label: status_label.into(),
        status_detail: status_detail.into(),
        sign_out_enabled,
        timing: timing.into(),
        has_log,
        log_enabled,
        after_adding: row.is_none() && retail,
        sign_in_now: flags.sign_in_now && allowed,
        sign_in_now_enabled: allowed,
        sign_in_note: sign_in_note.into(),
        app_status: private(app, &app.status).into(),
        delete_enabled: !active && !app.vault_saving,
        save_label: (if editing { t("Save") } else { t("Add account") }).into(),
        save_enabled: !app.vault_saving,
    };
    put!(view, get_sheet, set_sheet, sheet);
    put!(view, get_sheet_label, set_sheet_label, account.label.as_str().into());
    put!(view, get_sheet_server, set_sheet_server, account.server.as_str().into());
    put!(view, get_sheet_pol_id, set_sheet_pol_id, account.pol_id.as_str().into());
    put!(view, get_sheet_pol_password, set_sheet_pol_password, account.pol_password.as_str().into());
    put!(view, get_sheet_se_id, set_sheet_se_id, account.se_id.as_str().into());
    put!(view, get_sheet_se_password, set_sheet_se_password, account.se_password.as_str().into());
    put!(view, get_sheet_handles, set_sheet_handles, account.pol_handle_names.as_str().into());
    put!(view, get_sheet_folder, set_sheet_folder, editor.game_install.as_str().into());
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
