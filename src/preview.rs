//! Screenshot previews (`--preview-*`): sample accounts, news and Friends data instead of the
//! vault, sessions or the network, and the file each capture is saved as. argv is read once.
use super::*;

/// The `--preview*` arguments of this start.
pub(crate) struct Flags(Vec<String>);

impl Flags {
    pub(crate) fn has(&self, flag: &str) -> bool { self.0.iter().any(|arg| arg == flag) }
}

/// This start's preview flags, read once; `None` for a normal start.
pub(crate) fn flags() -> Option<&'static Flags> {
    static FLAGS: std::sync::OnceLock<Option<Flags>> = std::sync::OnceLock::new();
    FLAGS.get_or_init(|| {
        let args: Vec<String> = std::env::args().filter(|arg| arg.starts_with("--preview")).collect();
        (!args.is_empty()).then_some(Flags(args))
    }).as_ref()
}

/// Whether this start has `flag`.
pub(crate) fn has(flag: &str) -> bool { flags().is_some_and(|flags| flags.has(flag)) }

/// The capture's file for each scene flag, in precedence order: (flag, file stem, has a light
/// variant). `--preview-maintenance` and no scene flag are handled in `capture_name`.
const SCENES: &[(&str, &str, bool)] = &[
    ("--preview-features-layouts", "features-layouts", true),
    ("--preview-features-hotkeys", "features-hotkeys", true),
    ("--preview-features-recovery", "features-recovery", true),
    ("--preview-features-backups", "features-backups", true),
    ("--preview-features-clients", "features-clients", true),
    ("--preview-features-activity", "features-activity", true),
    ("--preview-delete", "delete", false),
    ("--preview-server-maintenance", "server-maintenance", false),
    ("--preview-servers-offline", "servers-offline", false),
    ("--preview-update-ready", "update-ready", false),
    ("--preview-news-events", "news-events", false),
    ("--preview-news-maintenance", "news-maintenance", false),
    ("--preview-news-empty", "news-empty", false),
    ("--preview-news-long", "news-long", true),
    ("--preview-inbox", "inbox", true),
    ("--preview-compose", "compose", true),
    ("--preview-friends-ingame", "friends-ingame", true),
    ("--preview-friends-signin", "signin", true),
    ("--preview-friends-blocked", "blocked", true),
    ("--preview-friends-confirm", "confirm", true),
    ("--preview-friends-options", "status-options", true),
    ("--preview-friends", "friends", true),
    ("--preview-close", "close", true),
    // The import dialog: sources found, accounts read (with notes and a warning), a read error.
    ("--preview-import-read", "import-read", true),
    ("--preview-import-error", "import-error", true),
    ("--preview-import", "import", true),
    // With --preview-modal: the account sheet scrolled to its Launch settings.
    ("--preview-modal-launch", "account-launch", true),
    ("--preview-modal", "account", true),
    ("--preview-add", "add", true),
    ("--preview-states", "states", true),
    ("--preview-empty", "accounts-empty", true),
    ("--preview-accounts", "accounts", true),
    ("--preview-launcher-settings", "launcher-settings", true),
    ("--preview-update", "update", true),
    ("--preview-maintenance", "maintenance", true),
    ("--preview-playing", "playing", true),
    ("--preview-starting", "starting", true),
    ("--preview-details", "details", false),
    ("--preview-live-news", "live-news", false),
    ("--preview-vanilla", "vanilla", false),
    ("--preview-small", "small", false),
    ("--preview-wide", "wide", false),
];

/// The capture's file name (before the Settings page, -private and -ja variants): the first
/// scene flag's, with -light when it has a light variant; the busy Downloads page has its own.
pub(super) fn capture_name(light: bool, busy: bool) -> String {
    let theme = |themed: bool| if themed && light { "-light" } else { "" };
    match SCENES.iter().find(|(flag, _, _)| has(flag)) {
        Some((_, stem, themed)) => {
            let busy = if *stem == "maintenance" && busy { "-busy" } else { "" };
            format!("preview-{stem}{busy}{}.png", theme(*themed))
        }
        None => format!("preview-portal-{}.png", if light { "light" } else { "dark" }),
    }
}

/// A preview start's state, from the sample data and its flags (no vault, session or network,
/// except `--preview-live-news`).
pub(super) fn boot(mut app: App) -> (App, Effect<Message>) {
    app.settings.light_theme = has("--preview-light");
    app.settings.privacy = privacy::on();
    app.load_preview();
    if has("--preview-vanilla") {
        app.settings.launch_with = crate::backend::LaunchWith::Vanilla;
        for row in &mut app.accounts {
            if row.account.mode == AccountMode::Retail { row.account.launch_with = Some(crate::backend::LaunchWith::Vanilla); }
        }
    }
    if has("--preview-ffxiah") { app.load_ah_preview(); }
    // --preview-features-<section>: the features sheet on that section, with sample data.
    if let Some(section) = features::ui::Section::ALL.into_iter().find(|s| has(&format!("--preview-features-{}", s.flag()))) {
        app.load_features_preview(section);
    }
    if has("--preview-news-events") { app.news_filter = crate::services::NewsTab::Events; }
    if has("--preview-news-maintenance") { app.news_filter = crate::services::NewsTab::Maintenance; }
    if has("--preview-news-empty") { app.news_filter = crate::services::NewsTab::Updates; }
    // --preview-news-long: headlines too long for two lines, to check the ellipsis.
    if has("--preview-news-long") {
        if let Some(snapshot) = &mut app.services {
            for (item, title) in snapshot.news.iter_mut().zip([
                "Adventurer Appreciation Campaign: earn bonus experience, capacity and exemplar points across Vana'diel this month",
                "The Ambuscade and Sortie content update for October brings new battle content, rewards and adjustments",
            ]) {
                item.title = title.into();
            }
        }
    }
    if ["--preview-friends", "--preview-friends-ingame", "--preview-inbox", "--preview-compose"].into_iter().any(has) {
        let flag = has;
        app.load_friends_preview(flag("--preview-inbox"), flag("--preview-friends-ingame"), flag("--preview-compose"), flag("--preview-new-mail"));
    }
    // The other Friends states: the block list, a Remove question, the status options open,
    // and signed out (several retail accounts, the last list's error).
    if ["--preview-friends-blocked", "--preview-friends-confirm", "--preview-friends-options"].into_iter().any(has) {
        app.load_friends_preview(false, false, false, false);
        if has("--preview-friends-blocked") {
            app.friends.select(Some(friends::Pane::Blocked));
        }
        if has("--preview-friends-confirm") {
            app.friends.remove = app.friends.friend().cloned();
        }
        app.friends.status_options = has("--preview-friends-options");
    }
    if has("--preview-friends-signin") {
        app.load_friends_preview(false, false, false, false);
        app.friends.connected = false;
        if let Some(snapshot) = &mut app.friends.snapshot {
            snapshot.fetched_at = 0;
            snapshot.error = Some("PlayOnline closed the connection.".into());
        }
    }
    // --preview-settings-<tab> opens a Settings tab (graphics, sound, controller, system).
    if let Some(tab) = flags().into_iter().flat_map(|flags| &flags.0).find_map(|a| {
        a.strip_prefix("--preview-settings-").and_then(settings_page::Tab::from_flag)
    }) {
        app.page = Page::Settings;
        app.settings_tab = tab;
    }
    // The account list on Home with a crashed client, a sign-in and a private server.
    if has("--preview-accounts") {
        // a client that crashed, with its log actions.
        let crashed = launch_log::classify_loader_end(&launch_log::LoaderEnd::Exit(-1073741819), Duration::from_secs(8));
        if let Some(row) = app.accounts.get_mut(0) {
            row.set_exit(crashed);
            row.log = Some(PathBuf::from("preview.stderr.log"));
        }
        // show every row state in the accounts preview.
        if let Some(row) = app.accounts.get_mut(1) {
            row.busy = true;
            row.status = t("Signing in to PlayOnline…").into();
        }
        if let Some(row) = app.accounts.get_mut(2) {
            row.account.mode = backend::AccountMode::Private;
            row.account.server = "play.example-server.net".into();
            row.selected = false;
        }
        if has("--preview-empty") {
            app.accounts.clear();
        }
    }
    // Home's account states as the approved renders show them: two signed in, one in
    // game, one signing in, one waiting for its one-time password.
    if has("--preview-states") {
        app.load_states_preview();
    }
    // The Play button with the launch profile's game running / starting.
    if has("--preview-playing") {
        for row in app.accounts.iter_mut().skip(1) {
            row.selected = false;
        }
        if let Some(row) = app.accounts.get_mut(0) {
            row.selected = true;
            row.pid = Some(4242);
        }
    }
    if has("--preview-starting") {
        for row in app.accounts.iter_mut().skip(1) {
            row.selected = false;
        }
        if let Some(row) = app.accounts.get_mut(0) {
            row.selected = true;
            row.busy = true;
            row.status = t("Signing in to PlayOnline…").into();
        }
    }
    // close-confirmation preview.
    if has("--preview-close") {
        app.confirm_close = true;
    }
    if has("--preview-delete") {
        app.pending_delete = Some(app.accounts[0].key.clone());
    }
    if has("--preview-maintenance") {
        app.page = Page::Maintenance;
        app.viewer = Some(viewer::Status {
            folder: Some(PathBuf::from(r"C:\Games\PlayOnlineViewer")),
            version: patch::PatchReport {
                state: patch::PatchState::Required,
                local_version: t("Not initialized").into(),
                remote_version: "20110829_E".into(),
                detail: t("PlayOnline has not completed its initial update. Press Update to finish it.").into(),
                files_verified: None,
            },
            core_error: None,
        });
        if has("--preview-fresh") {
            app.patch = Some(patch::PatchReport {
                state: patch::PatchState::Required,
                local_version: t("Not initialized").into(), remote_version: "30260904_1".into(),
                detail: t("FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.").into(),
                files_verified: None,
            });
        }
    }
    // a synthetic mid-download state for the Downloads preview.
    if has("--preview-busy") {
        app.settings.install_selected = true;
        app.settings.installer_selected = true;
        app.maintenance = Some(MaintenanceKind::Download);
        let part = operation::Part {
            number: 3,
            name: "FFXIFullSetup_US.part3.rar",
            state: operation::PartState::Downloading { done: 678_760_000, expected: 1_610_612_736 },
        };
        app.maintenance_progress = operation::Progress {
            stage: crate::operation::Stage::InstallerDownload,
            done: 3_900_000_000,
            total: 7_734_210_712,
            detail: part.english(),
            part: Some(part),
            bytes: true,
            remaining: Some(Duration::from_secs(115)),
            overall: None,
        };
    }
    if has("--preview-update") {
        app.page = Page::Maintenance;
        // Sample folders and versions (never this machine's own paths), a PlayOnline install, and
        // the result of a file check that came before the update.
        app.settings.install_path = r"C:\Games\SquareEnix\FINAL FANTASY XI".into();
        app.settings.installer_path = r"C:\Games\Installers".into();
        app.settings.install_selected = true;
        app.viewer = Some(viewer::Status {
            folder: Some(PathBuf::from(r"C:\Games\PlayOnlineViewer")),
            version: patch::PatchReport {
                state: patch::PatchState::Current,
                local_version: "20110829_E".into(), remote_version: "20110829_E".into(),
                detail: String::new(), files_verified: Some(true),
            },
            core_error: None,
        });
        app.patch = Some(patch::PatchReport {
            state: patch::PatchState::Required,
            local_version: "30260826_2".into(), remote_version: "30260916_1".into(),
            detail: String::new(), files_verified: Some(true),
        });
        app.status.clear();
        app.maintenance_result = t("File check finished: every file matches the official version. One update is waiting.").into();
        app.maintenance_tone = ResultTone::Done;
        app.maintenance = Some(MaintenanceKind::Update);
        app.maintenance_progress = operation::Progress {
            stage: crate::operation::Stage::Download, done: 42_000_000, total: 120_000_000,
            detail: "ROM/285/40.DAT".into(), bytes: true,
            remaining: Some(Duration::from_secs(115)),
            overall: Some(operation::OverallProgress {
                fraction: Some(0.28), remaining: Some(Duration::from_secs(265)), complete: false,
            }),
            ..operation::Progress::default()
        };
    }
    // The Downloads page's Troubleshooting open, the launcher settings dialog, and the
    // launcher-update strip (a download in progress).
    if has("--preview-troubleshooting") {
        app.troubleshooting_open = true;
    }
    if has("--preview-launcher-settings") {
        app.page = Page::Maintenance;
        app.show_settings = true;
    }
    if has("--preview-launcher-update") {
        app.self_update = self_update::State::Downloading;
    }
    if has("--preview-controller-nav") {
        app.controller_nav.preview_focus();
    }
    if has("--preview-details") {
        app.show_details = true;
    }
    if has("--preview-server-maintenance") {
        if let Some(snapshot) = &mut app.services {
            snapshot.availability = services::Availability::Maintenance;
            snapshot.availability_note = tf("Official scheduled all-world maintenance window is active: {title}. Completion may change.",
                &[("title", &"All Worlds Maintenance (Preview)")]);
        }
    }
    if has("--preview-servers-offline") {
        if let Some(snapshot) = &mut app.services {
            snapshot.availability = services::Availability::Outage;
            snapshot.availability_note = t("Official status page reports all 16 production worlds down.").into();
        }
    }
    if has("--preview-update-ready") {
        if let Some(report) = &mut app.patch { report.state = patch::PatchState::Required; }
    }
    if has("--preview-modal") {
        let mut account = app.accounts[0].account.clone();
        account.launch_with = Some(if has("--preview-vanilla") {
            crate::backend::LaunchWith::Vanilla
        } else { crate::backend::LaunchWith::Windower });
        app.editor = Some(Editor {
            key: Some(app.accounts[0].key.clone()),
            account,
            game_install: String::new(),
            dat_overlays: Vec::new(),
            auto_login: pol_less::AutoLogin { enabled: true, character_slot: 1 },
            flags: EditorFlags::default(),
        });
    }
    // The import dialog (--preview-import, -import-read, -import-error).
    if ["--preview-import", "--preview-import-read", "--preview-import-error"].into_iter().any(has) {
        use crate::account_import::{ImportBatch, ImportDraft, ImportSource, SourceKind};
        let import = &mut app.account_import;
        import.open = true;
        import.sources = vec![
            ImportSource { kind: SourceKind::PlayOnline, path: PathBuf::from(r"C:\Program Files (x86)\PlayOnline\SquareEnix\PlayOnlineViewer\usr\all\login_w.bin"), region: Some(0) },
            ImportSource { kind: SourceKind::Forest, path: PathBuf::from(r"C:\Users\Player\AppData\Roaming\Forest\accounts.json"), region: None },
        ];
        import.path = import.sources[0].path.to_string_lossy().into_owned();
        import.companion = import.sources[0].path.to_string_lossy().into_owned();
        if has("--preview-import-read") {
            let draft = |label: &str, notes: &[&str], complete: bool| {
                let mut draft = ImportDraft::default();
                draft.label = label.into();
                if complete {
                    draft.pol_id = "ABC12345".into();
                    draft.pol_password = "synthetic".into();
                    draft.se_id = "preview".into();
                    draft.se_password = "synthetic".into();
                }
                draft.notes = notes.iter().map(|n| n.to_string()).collect();
                draft
            };
            import.batch = Some(ImportBatch {
                drafts: vec![
                    draft("Main account", &[], true),
                    draft("Crafting account", &["Launch profile found: Windower."], true),
                    draft("Old alt", &["No password stored."], false),
                ],
                warnings: vec!["2 entries use an unsupported format and were skipped.".into()],
            });
        }
        if has("--preview-import-error") {
            import.error = "The file couldn't be read. Check the path and the region.".into();
        }
    }
    // The Add account sheet.
    if has("--preview-add") {
        app.editor = Some(Editor::blank());
    }
    if has("--preview-live-news") {
        app.news_loading = true;
        app.news_generation += 1;
        let generation = app.news_generation;
        let lang = i18n::current();
        return (
            app,
            Effect::perform(services::refresh(), move |result| Message::ServicesLoaded(generation, lang, result)),
        );
    }
    (app, Effect::none())
}

impl App {
    /// `--preview-ja`: the sample news as the Japanese official site words it
    /// (headlines from its topics feed, dates as it dates them), so a capture
    /// shows Japanese cards, dates and tabs without the network.
    pub(super) fn use_japanese_preview_news(&mut self) {
        let Some(snapshot) = &mut self.services else { return };
        let articles = [
            ("フリートライアルのレベル上限が75に！ プレイ期間が無制限になりました！", "Wed, 30 Sep 2026 14:00:00 JST",
             "「ファイナルファンタジーXI for Windows® フリートライアル」のレベル上限が75に引き上げられ、プレイ期間が無期限になりました！",
             "https://www.playonline.com/pcd2/topics/ff11/detail/40431/detail.html"),
            ("ヴァナ・ディールの異変を再調査せよ", "Thu, 24 Sep 2026 17:00:00 JST",
             "突如、とある巨大なモンスターが、本来は居ないはずのエリアに姿を現しました。冒険者の皆さんで、調査と討伐に向かいましょう！",
             "https://www.playonline.com/pcd2/topics/ff11/detail/40421/detail.html"),
            ("「真夏のノマドモグボナンザ2026」当せん番号発表！", "Fri, 18 Sep 2026 17:00:00 JST",
             "厳正なる抽選の結果、真夏のノマドモグボナンザ2026の当せん番号が決定しました！",
             "https://sqex.to/wuqaC"),
        ];
        for (item, (title, date, summary, url)) in snapshot.news.iter_mut().zip(articles) {
            item.title = title.into();
            item.date = date.into();
            item.summary = summary.into();
            item.url = url.into();
        }
        let notices = [
            ("全ワールド メンテナンス作業のお知らせ(9/29)", "2026.09.27(日) 14:00 From: ファイナルファンタジーXI",
             "下記日時におきまして、ファイナルファンタジーXIのメンテナンス作業を実施いたします。"),
            ("全ワールド メンテナンス作業のお知らせ(9/10)", "2026.09.08(火) 14:00 From: ファイナルファンタジーXI",
             "下記日時におきまして、サーバーメンテナンス作業を実施いたします。"),
            ("アカウント管理システム メンテナンス作業のお知らせ(9/28)", "2026.09.26(土) 01:30 From: プレイオンライン", ""),
        ];
        for (item, (title, date, summary)) in snapshot.notices.iter_mut().zip(notices) {
            item.title = title.into();
            item.date = date.into();
            item.summary = summary.into();
            item.url = "https://www.playonline.com/ff11/polnews/news.xml".into();
        }
    }

    /// `--preview-ffxiah`: a finished auction-house lookup.
    pub(super) fn load_ah_preview(&mut self) {
        self.ah_query = "copper".into();
        self.ah_server = "Bismarck";
        let generation = self.ah_lookup.begin("copper", "Bismarck");
        self.ah_lookup.finish(generation, Ok(ffxiah::Results {
            items: vec![ffxiah::Quote { name: "Copper Ore".into(), url: "https://www.ffxiah.com/item/640".into(),
                stack: Some(12), price: Some(2000), stock: Some(2), last_sale: false },
                ffxiah::Quote { name: "Copper Ingot".into(), url: "https://www.ffxiah.com/item/648".into(),
                stack: None, price: Some(2000), stock: Some(0), last_sale: false }], ..Default::default()
        }));
    }

    /// The Friends previews: synthetic friends, mail and a conversation with
    /// messages both ways and one unread. Nothing is read from a session.
    pub(super) fn load_friends_preview(&mut self, mailbox: bool, in_game: bool, compose: bool, new_mail: bool) {
        const ME: u64 = 900;
        self.page = Page::Friends;
        self.friends.account = self.accounts.first().map(|a| a.key.clone());
        self.friends.connected = true;
        let friend = |slot: u16, name: &str, online: Option<bool>, location: Option<&str>, character: Option<&str>, alternate: bool| friends::Friend {
            name: name.into(), alternate, slot, uid: Some(slot as u64 + 1 + if alternate { 500 } else { 0 }), online,
            location: location.map(Into::into), character: character.map(Into::into),
        };
        let entries = vec![
            friend(0, "Alyria", Some(true), Some("Lyrielle · Port Jeuno"), Some("Lyrielle"), false),
            friend(1, "Balthier", Some(true), Some("PlayOnline"), None, false),
            friend(2, "Celestine", Some(true), Some("FINAL FANTASY XI"), None, false),
            friend(3, "Haruko", Some(false), None, None, false),
            friend(4, "Nocturne", None, None, None, false),
            friend(0, "Mallory", None, None, None, true),
        ];
        // A day ago and today, at fixed clock times, so the capture reads naturally.
        let at = |days_ago: i64, hour: u32, minute: u32| -> u32 {
            let day = chrono::Local::now().date_naive() - chrono::Duration::days(days_ago);
            day.and_hms_opt(hour, minute, 0).and_then(|t| t.and_local_timezone(chrono::Local).single()).map_or(0, |t| t.timestamp() as u32)
        };
        let header = |id: u32, from: u64, to: u64, sender: &str, subject: &str, unread: bool, stamp: u32| {
            let mut bytes = [0u8; 72];
            bytes[..8].copy_from_slice(&from.to_le_bytes());
            bytes[8..16].copy_from_slice(&to.to_le_bytes());
            bytes[16..16 + sender.len().min(15)].copy_from_slice(&sender.as_bytes()[..sender.len().min(15)]);
            bytes[32..32 + subject.len().min(15)].copy_from_slice(&subject.as_bytes()[..subject.len().min(15)]);
            bytes[0x30..0x34].copy_from_slice(&id.to_le_bytes());
            bytes[0x34..0x38].copy_from_slice(&stamp.to_le_bytes());
            bytes[0x3e..0x40].copy_from_slice(&(if unread { 0x8000u16 } else { 0 }).to_le_bytes());
            let mut raw = Vec::new();
            pol_less::pipe::base64_into(&mut raw, &bytes);
            social::Header { raw: String::from_utf8(raw).unwrap(), sender: sender.into(), subject: subject.into(), unread, kind: 0, recipient: Some(to) }
        };
        // (id, from Alyria?, subject, text, unread, stamp)
        let alyria: [(u32, bool, &str, &str, bool, u32); 6] = [
            (1, true, "Sortie tonight?", "Are you free for Sortie tonight? We have five so far and could use your Red Mage.", false, at(1, 20, 41)),
            (2, false, "", "I'm in. Give me twenty minutes to finish crafting.", false, at(1, 20, 47)),
            (3, false, "", "Should I bring the Ambuscade set or the usual one?", false, at(1, 20, 48)),
            (4, true, "", "Usual one is fine. We'll meet at the Rhapsodies home point in Jeuno.", false, at(1, 20, 52)),
            (5, false, "Thanks!", "That was a great run. Thanks for carrying the last boss!", false, at(0, 9, 14)),
            (6, true, "Re: Thanks!", "Any time! Same time on Saturday? Celestine might join us too.", true, at(0, 18, 6)),
        ];
        let mut inbox = Vec::new();
        let mut sent = Vec::new();
        let mut bodies = Vec::new();
        for (id, theirs, subject, message, unread, stamp) in alyria {
            let h = if theirs { header(id, 1, ME, "Alyria", subject, unread, stamp) } else { header(id, ME, 1, "Ardent", subject, false, stamp) };
            bodies.push((social::message_key(&h.raw), subject, message));
            if theirs { inbox.push(h) } else { sent.push(h) }
        }
        inbox.push(header(7, 2, ME, "Balthier", "Linkshell event", true, at(0, 12, 30)));
        inbox.push(header(8, 77, ME, "Rook", "Bazaar price", false, at(2, 15, 2)));
        inbox.sort_by_key(|h| std::cmp::Reverse(h.stamp()));
        sent.sort_by_key(|h| std::cmp::Reverse(h.stamp()));
        let mut snapshot = friends::Snapshot {
            entries, fetched_at: friends::now(), game_running: Some(in_game),
            own_mode: Some(0), social: social::State { ready: true, ..Default::default() }, ..friends::Snapshot::default()
        };
        snapshot.social.inbox_unread = Some(if mailbox { 2 } else { 1 });
        if mailbox {
            snapshot.social.headers = inbox.clone();
            snapshot.social.total = inbox.len() as u32;
            snapshot.social.inbox_changed = new_mail;
            snapshot.social.opened = Some(social::Opened { sender: "Alyria".into(), subject: "Sortie tonight?".into(), message: "Are you free for Sortie tonight? We have five so far and could use your Red Mage.".into(), recipient: None });
        }
        self.friends.snapshot = Some(snapshot.clone());
        let mut listed = snapshot.clone();
        listed.social.action = Some(crate::social::Action::Inbox);
        listed.social.headers = inbox;
        self.friends.completed(&listed);
        listed.social.folder = 2;
        listed.social.headers = sent;
        self.friends.completed(&listed);
        for (key, subject, message) in bodies {
            self.friends.mail.bodies.insert(key, friends::Body { subject: subject.into(), message: message.into() });
        }
        if mailbox {
            self.friends.select(Some(friends::Pane::Mailbox));
            return;
        }
        self.friends.select(Some(friends::Pane::Friend { slot: 0, name: "Alyria".into() }));
        // Opening the conversation read the unread one; the divider stays.
        for entry in self.friends.mail.listed[0].iter_mut().flatten().filter(|e| e.sender == "Alyria") {
            entry.unread = false;
        }
        if let Some(draft) = &mut self.friends.composer {
            if compose {
                draft.show_subject = true;
                draft.subject = "Saturday".into();
                draft.text = String::from("Saturday works for me.\nI'll bring the usual set.");
            }
        }
    }

    pub(super) fn load_preview(&mut self) {
        let values = ["Main account", "Second account", "Crafting account"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| {
                let mut a = blank_account();
                a.label = label.into();
                a.pol_id = format!("ABC1234{i}");
                a.pol_password = "synthetic".into();
                a.se_id = "preview".into();
                a.se_password = "synthetic".into();
                a
            })
            .collect();
        self.replace_accounts(values);
        self.vault_ready = true;
        self.status = t("Design preview — synthetic accounts. No account vault or game connection was accessed.").into();
        self.services = Some(services::ServiceSnapshot {
            news: vec![
                services::NewsItem {
                    title: "Free trial expands to level 75".into(),
                    url: "https://www.playonline.com/pcd2/topics/ff11us/detail/40432/detail.html"
                        .into(),
                    date: "Sep 30".into(),
                    summary: "No time limit. More to explore.".into(),
                    image_url: String::new(),
                    image: None,
                },
                services::NewsItem {
                    title: "Strange Happenings returns to Vana’diel".into(),
                    url: "https://www.playonline.com/pcd2/topics/ff11us/detail/40422/detail.html"
                        .into(),
                    date: "Sep 24".into(),
                    summary: "Unusual visitors await.".into(),
                    image_url: String::new(),
                    image: None,
                },
                services::NewsItem {
                    title: "Nomad Mog Bonanza · The results are in".into(),
                    url: "https://sqex.to/w97dY".into(),
                    date: "Sep 18".into(),
                    summary: "Find your prize collection details.".into(),
                    image_url: String::new(),
                    image: None,
                },
            ],
            notices: [
                ("All Worlds Maintenance (Sep. 28)", "Sep. 26, 2026 22:00 [PDT] From: FINAL FANTASY XI", "At the following time, we will be performing maintenance on all worlds."),
                ("All Worlds Maintenance (Sep. 9)", "Sep. 7, 2026 22:00 [PDT] From: FINAL FANTASY XI", "At the following time, we will be performing server maintenance."),
                ("Forum Technical Difficulties (Sep. 2)", "Sep. 3, 2026 01:24 [PDT] From: FINAL FANTASY XI", ""),
            ].into_iter().map(|(title, date, summary)| services::NewsItem {
                title: title.into(), date: date.into(), summary: summary.into(),
                url: "https://www.playonline.com/ff11us/polnews/news.xml".into(),
                image_url: String::new(), image: None,
            }).collect(),
            news_note: t("Preview content. Live feeds load when the launcher starts normally.").into(),
            availability: services::Availability::Unknown,
            availability_note: t("Not checked in design preview").into(),
            windows: Vec::new(),
            windows_complete: false,
            ended_early: Vec::new(),
            checked_at: t("Preview").into(),
        });
        if i18n::current() == i18n::Lang::Ja {
            self.use_japanese_preview_news();
        }
        self.patch = Some(patch::PatchReport {
            state: patch::PatchState::Current, // mockup shows "Ready to play"
            local_version: t("Not checked").into(),
            remote_version: t("Not checked").into(),
            detail: t("The launcher compares your installation with the official patch service.")
                .into(),
            files_verified: None,
        });
    }

    /// `--preview-states`: five synthetic accounts in every Home row state.
    pub(super) fn load_states_preview(&mut self) {
        let values = ["Main account", "Second account", "Crafting account", "Trading account", "Alt account"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| {
                let mut a = blank_account();
                a.label = label.into();
                a.pol_id = format!("PREVIEW{i}");
                a.pol_password = "synthetic".into();
                a
            })
            .collect();
        self.replace_accounts(values);
        let keys: Vec<String> = self.accounts.iter().map(|row| row.key.clone()).collect();
        for (i, row) in self.accounts.iter_mut().enumerate() {
            row.selected = matches!(i, 0 | 1 | 3);
            match i {
                0 | 1 => {
                    row.pid = Some(4000 + i as u32);
                    row.game_running = Some(Arc::new(AtomicBool::new(false)));
                    row.status = t("Signed in to PlayOnline").into();
                }
                2 => {
                    row.pid = Some(4002);
                    row.game_running = Some(Arc::new(AtomicBool::new(true)));
                    row.play_timing = Some(launch_timing::Timeline::begin_at(Instant::now() - Duration::from_secs(47 * 60)));
                }
                3 => {
                    row.busy = true;
                    row.preparing_session = true;
                    row.status = t("Signing in to PlayOnline…").into();
                }
                _ => {}
            }
        }
        if let Some(key) = keys.get(4) { self.settings.account_otp.insert(key.clone()); }
        self.settings.selected_accounts = Some(keys.iter().enumerate().filter(|(i, _)| matches!(i, 0 | 1 | 3)).map(|(_, k)| k.clone()).collect());
    }
}
