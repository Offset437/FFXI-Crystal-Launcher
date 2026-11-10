//! The features' screens, toolkit-agnostic: the messages their controls send (`Msg`, carried by
//! `Message::Feature`), what the window remembers about them between messages (`State`), and how
//! the core answers each message (`App::feature_update`). The Slint view draws from `State`,
//! `Runtime::settings()` and the runtime's read-outs; nothing here draws.
//!
//! Every setting goes through `Runtime::update`, which saves features.json and applies the change
//! at once (hotkeys, threads, priorities). Backups and restores run off the UI thread.

use super::backups::{self, BackupInfo, RestoreReport, Sources};
use super::game_window::Monitor;
use super::hotkeys::{self, Hotkey};
use super::layouts::Preset;
use super::perf::{self, Priority};
use super::profiles;
use super::recovery::Decision;
use super::ClientKey;
use crate::effect::Effect;
use crate::i18n::{t, tf};
use crate::{App, Message};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The sheet's sections, in tab order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Section {
    #[default]
    Windows,
    Hotkeys,
    Recovery,
    Backups,
    Clients,
    Activity,
}

impl Section {
    /// The `--preview-features-<flag>` name.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Self::Windows => "layouts",
            Self::Hotkeys => "hotkeys",
            Self::Recovery => "recovery",
            Self::Backups => "backups",
            Self::Clients => "clients",
            Self::Activity => "activity",
        }
    }

    pub(crate) const ALL: [Section; 6] = [Self::Windows, Self::Hotkeys, Self::Recovery, Self::Backups, Self::Clients, Self::Activity];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Windows => t("Layouts"),
            Self::Hotkeys => t("Hotkeys"),
            Self::Recovery => t("Recovery"),
            Self::Backups => t("Backups"),
            Self::Clients => t("Clients"),
            Self::Activity => t("Activity"),
        }
    }
}

/// What a hotkey field sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Next,
    Previous,
    Account(String),
}

impl Target {
    fn id(&self) -> String {
        match self {
            Self::Next => "hotkey:next".into(),
            Self::Previous => "hotkey:previous".into(),
            Self::Account(key) => format!("hotkey:{key}"),
        }
    }
}

/// The features' messages.
#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Open(bool),
    Section(Section),
    // Window layouts.
    Remember(bool),
    RestoreLayouts(bool),
    LaunchAtSavedSize(bool),
    OrderWindows(bool),
    /// Arrange every game window; `None` uses every monitor.
    Arrange(Preset, Option<String>),
    /// Put the windows the last Arrange all moved back where they were.
    RestoreWindows,
    /// Forget a remembered window (a `ClientKey` id).
    Forget(String),
    // Hotkeys: the typed chord ("" clears it).
    Hotkey(Target, String),
    // Crash recovery.
    Offer(bool),
    AutoRelaunch(bool),
    CancelRelaunch(String),
    DismissRecovery(String),
    // Backups.
    LoadBackups,
    Backups(Vec<BackupInfo>),
    BackupNow,
    BackupDone(Result<BackupInfo, String>),
    AskRestore(PathBuf),
    CancelRestore,
    RestoreBackup,
    RestoreDone(Result<RestoreReport, String>),
    Keep(u32),
    BeforeUpdate(bool),
    // Notifications.
    Notify(bool),
    NotifyMail(bool),
    NotifyFriends(bool),
    // Per-client performance.
    Priority(String, Option<Priority>),
    /// The processors typed for an account ("" is every processor).
    Affinity(String, String),
    MuteClient(String, bool),
    BackgroundPriority(bool),
    MuteBackground(bool),
    // Play time and presence.
    RecordPlaytime(bool),
    Discord(bool),
    // Launch profiles (read off the UI thread).
    Profiles(ProfileDirs, Arc<profiles::Lists>),
}

/// What the window keeps for the features' screens between messages.
#[derive(Default)]
pub(crate) struct State {
    pub open: bool,
    pub section: Section,
    /// Every configured hotkey and its problem, as the runtime last reported.
    pub hotkeys: Vec<hotkeys::Status>,
    /// Ended games recovery offers a relaunch for, until played or dismissed.
    pub offers: BTreeMap<String, Decision>,
    pub backups: Vec<BackupInfo>,
    pub backups_loaded: bool,
    /// A backup or restore is running.
    pub busy: bool,
    /// The backup being restored, while a restore runs.
    pub restoring: Option<PathBuf>,
    /// The backup a restore waits on confirmation for.
    pub confirm_restore: Option<PathBuf>,
    /// The last outcome to show: (is an error, text).
    pub notice: Option<(bool, String)>,
    /// Typed values that could not be used, by field id, with why.
    pub field_errors: BTreeMap<String, String>,
    pub monitors: Vec<Monitor>,
    /// The launch profiles read for the detected tool folders (Windower, Ashita), off the UI
    /// thread; read again each time the sheet or the account editor opens.
    pub profiles: Option<(ProfileDirs, Arc<profiles::Lists>)>,
    /// What the last profile read was asked for: (folders, sheet open, editor's account).
    profiles_asked: Option<(ProfileDirs, bool, Option<Option<String>>)>,
    /// Moves with every change to this state (`touch`), so a view rebuilds only what changed.
    pub revision: u64,
}

/// The detected Windower and Ashita folders.
pub(crate) type ProfileDirs = (Option<PathBuf>, Option<PathBuf>);

impl State {
    /// Marks the state changed.
    pub(crate) fn touch(&mut self) { self.revision = self.revision.wrapping_add(1); }
}

/// The keep-N choices offered for backups.
pub(crate) const KEEP: [u32; 5] = [3, 5, 10, 20, 50];

/// `"0-3,6"` as a processor mask; `""` is every processor (`None`).
pub(crate) fn parse_affinity(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() { return Ok(None); }
    let mut mask = 0u64;
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (from, to) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<u32>(), b.trim().parse::<u32>()),
            None => (part.parse::<u32>(), part.parse::<u32>()),
        };
        let (Ok(from), Ok(to)) = (from, to) else { return Err(t("Use processor numbers, e.g. 0-3,6.").into()) };
        if from > to || to > 63 { return Err(t("Use processor numbers, e.g. 0-3,6.").into()); }
        for cpu in from..=to { mask |= 1 << cpu; }
    }
    Ok(Some(mask))
}

/// A mask as `"0-3,6"`.
pub(crate) fn affinity_text(mask: u64) -> String {
    let mut parts = Vec::new();
    let mut cpu = 0u32;
    while cpu < 64 {
        if mask & (1 << cpu) == 0 { cpu += 1; continue; }
        let start = cpu;
        while cpu + 1 < 64 && mask & (1 << (cpu + 1)) != 0 { cpu += 1; }
        parts.push(if start == cpu { start.to_string() } else { format!("{start}-{cpu}") });
        cpu += 1;
    }
    parts.join(",")
}

impl App {
    /// Where USER data lives for a backup or restore: the game folder first, then each other
    /// folder an account uses (sorted, so a restore maps folders as the backup did), and the
    /// detected Windower and Ashita folders.
    pub(crate) fn feature_backup_sources(&self) -> Sources {
        let first = self.shown_game_folder();
        let mut others: Vec<PathBuf> = self.accounts.iter()
            .filter_map(|row| self.settings.config_for(&self.config, &row.account).game_install)
            .filter(|path| path != &first)
            .collect();
        others.sort();
        others.dedup();
        let detected = &self.launch_tools.detected;
        Sources {
            ffxi: std::iter::once(first).chain(others).collect(),
            windower: detected.windower.clone(),
            ashita: detected.ashita.clone(),
        }
    }

    /// The account's label, masked while privacy mode is on.
    pub(crate) fn feature_account_label(&self, key: &str) -> String {
        let label = self.accounts.iter().find(|row| row.key == key).map_or_else(|| t("Removed account").to_owned(), |row| row.account.label.clone());
        crate::privacy::mask(&label)
    }

    /// A client's name: its account, and the character slot when it has one.
    pub(crate) fn feature_client_label(&self, key: &ClientKey) -> String {
        let account = self.feature_account_label(&key.account);
        match key.slot {
            Some(slot) => tf("{account} · slot {slot}", &[("account", &account), ("slot", &slot)]),
            None => account,
        }
    }

    /// `--preview-features-<section>`: the sheet open on `section`, over a runtime that keeps its
    /// files in a fresh temporary folder with sample settings, play time and two backups. Nothing
    /// of the user's is read or written; no game is touched.
    pub(crate) fn load_features_preview(&mut self, section: Section) { self.load_features_sample(section, true) }

    /// The preview's sample. `live_hotkeys` false leaves out the chords Windows would register
    /// (tests), keeping only one that is refused before registration.
    pub(crate) fn load_features_sample(&mut self, section: Section, live_hotkeys: bool) {
        use super::{game_window, hotkeys::Hotkey, layouts, store};
        let dir = std::env::temp_dir().join(format!("crystal-features-preview-{}-{}", std::process::id(), crate::backend::unique_id().unwrap_or_default()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = crate::backend::Config { experiment: dir.clone(), ..crate::backend::Config::default() };
        let keys: Vec<String> = self.accounts.iter().map(|row| row.key.clone()).collect();
        let key = |i: usize| keys.get(i).cloned().unwrap_or_else(|| format!("preview-{i}"));
        let mut settings = super::FeatureSettings::default();
        // The preview keeps the default cycle chords (Ctrl+Tab, Ctrl+Shift+Tab) and two clean
        // per-account keys; tests keep one refused chord (F5) to exercise the error row.
        if live_hotkeys {
            settings.hotkeys.accounts.insert(key(0), Hotkey::parse("Ctrl+Alt+1").expect("valid"));
            settings.hotkeys.accounts.insert(key(1), Hotkey::parse("Ctrl+Alt+2").expect("valid"));
        } else {
            settings.hotkeys.accounts.insert(key(1), Hotkey::parse("F5").expect("valid"));
        }
        settings.recovery.auto_relaunch = true;
        settings.notify.enabled = true;
        settings.perf.background = true;
        settings.perf.accounts.insert(key(0), perf::Client { priority: Some(Priority::AboveNormal), affinity: Some(0b1111), mute_in_background: false });
        settings.perf.accounts.insert(key(1), perf::Client { priority: None, affinity: None, mute_in_background: true });
        let monitor = layouts::MonitorRef { device: r"\\.\DISPLAY1".into(), rect: game_window::Rect::new(0, 0, 2560, 1440), work: game_window::Rect::new(0, 0, 2560, 1392), dpi: 96 };
        for (id, (w, h)) in [(key(0), (1920, 1080)), (format!("{}#2", key(1)), (1280, 720))] {
            settings.layouts.saved.insert(id, layouts::Saved {
                rect: game_window::Rect::new(0, 0, w + 16, h + 39), client: (w, h), shown: game_window::Shown::Normal,
                mode: game_window::Mode::Windowed, frame: None, monitor: monitor.clone(),
            });
        }
        let _ = store::save(&config, &settings);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let playtime = serde_json::json!({ "clients": {
            key(0): { "total_secs": 187_200 + 1_380, "sessions": 41, "last_played": now - 3_600, "character": "Aldric" },
            format!("{}#2", key(1)): { "total_secs": 26_100, "sessions": 9, "last_played": now - 86_400 * 3 },
        }});
        let file = super::playtime::path(&config);
        let _ = std::fs::create_dir_all(file.parent().expect("has a folder"));
        let _ = std::fs::write(&file, serde_json::to_vec(&playtime).unwrap_or_default());
        let user = dir.join("ffxi").join("USER").join("1a2b3c");
        let _ = std::fs::create_dir_all(&user);
        let _ = std::fs::write(user.join("mcr.dat"), vec![0u8; 7_600]);
        let sources = Sources { ffxi: vec![dir.join("ffxi")], windower: None, ashita: None };
        let archives = backups::backups_dir(&config);
        let _ = backups::create(&archives, &sources, backups::Reason::BeforeUpdate);
        let _ = backups::create(&archives, &sources, backups::Reason::Manual);
        let sender = std::sync::Mutex::new(self.sender.clone());
        let runtime = super::Runtime::new(config, std::sync::Arc::new(move |event| {
            let _ = sender.lock().unwrap_or_else(|e| e.into_inner()).send(crate::WorkerEvent::Features(event));
        }));
        runtime.set_privacy(self.settings.privacy);
        self.features = Some(runtime);
        if section == Section::Recovery {
            self.features_view.offers.insert(key(0), Decision::Offer { kind: super::recovery::EndKind::Crash, held: super::recovery::Held::Off });
        }
        let runtime = self.features.clone().expect("just set");
        self.features_view.backups = runtime.backups();
        self.features_view.backups_loaded = true;
        self.features_view.monitors = runtime.monitors();
        self.features_view.hotkeys = runtime.hotkey_status();
        if section == Section::Backups { self.features_view.confirm_restore = self.features_view.backups.first().map(|b| b.path.clone()); }
        self.features_view.open = true;
        self.features_view.section = section;
        self.features_view.touch();
    }

    /// A read of the launch profiles when one is due: the features sheet or the account editor
    /// just opened, or the detected tool folders changed while one shows. The host asks after
    /// each batch of messages; the files are read off the UI thread.
    pub(crate) fn feature_profiles_due(&mut self) -> Effect<Message> {
        let dirs: ProfileDirs = (self.launch_tools.detected.windower.clone(), self.launch_tools.detected.ashita.clone());
        let open = self.features_view.open;
        let editor = self.editor.as_ref().map(|e| e.key.clone());
        if !open && editor.is_none() {
            self.features_view.profiles_asked = None;
            return Effect::none();
        }
        let asked = (dirs.clone(), open, editor);
        if self.features_view.profiles_asked.as_ref() == Some(&asked) { return Effect::none(); }
        self.features_view.profiles_asked = Some(asked);
        Effect::perform(async move {
            let (windower, ashita) = (dirs.0.clone(), dirs.1.clone());
            let lists = tokio::task::spawn_blocking(move || profiles::Lists::load(windower.as_deref(), ashita.as_deref())).await.unwrap_or_default();
            (dirs, Arc::new(lists))
        }, |(dirs, lists)| Message::Feature(Msg::Profiles(dirs, lists)))
    }

    pub(crate) fn feature_update(&mut self, msg: Msg) -> Effect<Message> {
        self.features_view.touch();
        if let Msg::Profiles(dirs, lists) = msg {
            self.features_view.profiles = Some((dirs, lists));
            return Effect::none();
        }
        let Some(runtime) = self.features.clone() else {
            // Previews and tests have no runtime: only the sheet's own state changes.
            match msg {
                Msg::Open(open) => self.features_view.open = open,
                Msg::Section(section) => self.features_view.section = section,
                _ => {}
            }
            return Effect::none();
        };
        let mut sources = matches!(msg, Msg::BackupNow | Msg::RestoreBackup).then(|| self.feature_backup_sources());
        let view = &mut self.features_view;
        let save = |change: &dyn Fn(&mut super::FeatureSettings), view: &mut State| {
            if let Err(error) = runtime.update(|s| change(s)) { view.notice = Some((true, error)); }
        };
        match msg {
            Msg::Open(open) => {
                view.open = open;
                view.confirm_restore = None;
                if open {
                    view.notice = None;
                    view.field_errors.clear();
                    view.monitors = runtime.monitors();
                    view.hotkeys = runtime.hotkey_status();
                    if !view.backups_loaded { return Effect::done(Message::Feature(Msg::LoadBackups)); }
                }
            }
            Msg::Section(section) => {
                view.section = section;
                if section == Section::Windows { view.monitors = runtime.monitors(); }
                if section == Section::Backups && !view.backups_loaded { return Effect::done(Message::Feature(Msg::LoadBackups)); }
            }
            Msg::Remember(on) => save(&|s| s.layouts.remember = on, view),
            Msg::RestoreLayouts(on) => save(&|s| s.layouts.restore = on, view),
            Msg::LaunchAtSavedSize(on) => save(&|s| s.layouts.launch_at_saved_size = on, view),
            Msg::OrderWindows(on) => save(&|s| s.layouts.order_windows = on, view),
            Msg::Arrange(preset, monitor) => {
                let devices: Vec<String> = monitor.into_iter().collect();
                view.notice = Some(match runtime.arrange(preset, &devices) {
                    Ok(1) => (false, t("Arranged 1 window.").to_owned()),
                    Ok(count) => (false, tf("Arranged {count} windows.", &[("count", &count)])),
                    Err(error) => (true, error),
                });
            }
            Msg::RestoreWindows => {
                view.notice = Some(match runtime.restore_windows() {
                    Ok(1) => (false, t("Restored 1 window.").to_owned()),
                    Ok(count) => (false, tf("Restored {count} windows.", &[("count", &count)])),
                    Err(error) => (true, error),
                });
            }
            Msg::Forget(id) => {
                if let Some(key) = ClientKey::parse(&id) {
                    if let Err(error) = runtime.forget_layout(&key) { view.notice = Some((true, error)); }
                }
            }
            Msg::Hotkey(target, text) => {
                let id = target.id();
                let text = text.trim();
                let hotkey = if text.is_empty() { None } else {
                    match Hotkey::parse(text) {
                        Some(hotkey) => Some(hotkey),
                        None => {
                            view.field_errors.insert(id, t("Type a hotkey such as Ctrl+Alt+1.").into());
                            return Effect::none();
                        }
                    }
                };
                view.field_errors.remove(&id);
                save(&|s| match &target {
                    Target::Next => s.hotkeys.cycle = hotkey,
                    Target::Previous => s.hotkeys.cycle_back = hotkey,
                    Target::Account(key) => match hotkey {
                        Some(hotkey) => { s.hotkeys.accounts.insert(key.clone(), hotkey); }
                        None => { s.hotkeys.accounts.remove(key); }
                    },
                }, view);
                view.hotkeys = runtime.hotkey_status();
            }
            Msg::Offer(on) => save(&|s| s.recovery.offer = on, view),
            Msg::AutoRelaunch(on) => save(&|s| s.recovery.auto_relaunch = on, view),
            Msg::CancelRelaunch(key) => {
                runtime.cancel_relaunch(&key);
                view.offers.remove(&key);
            }
            Msg::DismissRecovery(key) => { view.offers.remove(&key); }
            Msg::LoadBackups => {
                let runtime = runtime.clone();
                return Effect::perform(async move { tokio::task::spawn_blocking(move || runtime.backups()).await.unwrap_or_default() },
                    |list| Message::Feature(Msg::Backups(list)));
            }
            Msg::Backups(list) => {
                view.backups = list;
                view.backups_loaded = true;
                if view.confirm_restore.as_ref().is_some_and(|path| !view.backups.iter().any(|b| &b.path == path)) { view.confirm_restore = None; }
            }
            Msg::BackupNow => {
                if view.busy { return Effect::none(); }
                view.busy = true;
                view.notice = None;
                let sources = sources.take().unwrap_or_default();
                let runtime = runtime.clone();
                return Effect::perform(async move {
                    tokio::task::spawn_blocking(move || runtime.backup_now(&sources)).await.unwrap_or_else(|_| Err(t("The backup stopped unexpectedly.").into()))
                }, |result| Message::Feature(Msg::BackupDone(result)));
            }
            Msg::BackupDone(result) => {
                view.busy = false;
                view.notice = Some(match result {
                    Ok(info) => (false, tf("Backed up {count} files.", &[("count", &info.files)])),
                    Err(error) => (true, error),
                });
                return Effect::done(Message::Feature(Msg::LoadBackups));
            }
            Msg::AskRestore(path) => {
                if !view.busy { view.confirm_restore = Some(path); }
            }
            Msg::CancelRestore => view.confirm_restore = None,
            Msg::RestoreBackup => {
                let Some(path) = view.confirm_restore.take() else { return Effect::none() };
                if view.busy { return Effect::none(); }
                view.busy = true;
                view.restoring = Some(path.clone());
                view.notice = None;
                let sources = sources.take().unwrap_or_default();
                let runtime = runtime.clone();
                return Effect::perform(async move {
                    tokio::task::spawn_blocking(move || runtime.restore(&path, &sources, backups::Parts::ALL)).await
                        .unwrap_or_else(|_| Err(t("The restore stopped unexpectedly.").into()))
                }, |result| Message::Feature(Msg::RestoreDone(result)));
            }
            Msg::RestoreDone(result) => {
                view.busy = false;
                view.restoring = None;
                view.notice = Some(match result {
                    Ok(report) => (false, tf("Restored {count} files.", &[("count", &report.restored)])),
                    Err(error) => (true, error),
                });
                return Effect::done(Message::Feature(Msg::LoadBackups));
            }
            Msg::Keep(keep) => save(&|s| s.backups.keep = keep.max(1), view),
            Msg::BeforeUpdate(on) => save(&|s| s.backups.before_update = on, view),
            Msg::Notify(on) => save(&|s| s.notify.enabled = on, view),
            Msg::NotifyMail(on) => save(&|s| s.notify.mail = on, view),
            Msg::NotifyFriends(on) => save(&|s| s.notify.friends_online = on, view),
            Msg::Priority(key, priority) => save(&|s| client(s, &key).priority = priority, view),
            Msg::Affinity(key, text) => {
                let id = format!("affinity:{key}");
                let mask = match parse_affinity(&text) {
                    Ok(mask) => mask.map(|mask| perf::effective_mask(mask, perf::system_mask()).ok_or(())),
                    Err(error) => { view.field_errors.insert(id, error); return Effect::none(); }
                };
                match mask {
                    Some(Err(())) => { view.field_errors.insert(id, t("None of these processors exist on this PC.").into()); }
                    Some(Ok(mask)) => { view.field_errors.remove(&id); save(&|s| client(s, &key).affinity = Some(mask), view); }
                    None => { view.field_errors.remove(&id); save(&|s| client(s, &key).affinity = None, view); }
                }
            }
            Msg::MuteClient(key, on) => save(&|s| client(s, &key).mute_in_background = on, view),
            Msg::BackgroundPriority(on) => save(&|s| s.perf.background = on, view),
            Msg::MuteBackground(on) => save(&|s| s.perf.mute_background = on, view),
            Msg::RecordPlaytime(on) => save(&|s| s.playtime.record = on, view),
            Msg::Discord(on) => save(&|s| s.playtime.discord = on, view),
            Msg::Profiles(..) => {}
        }
        Effect::none()
    }
}

/// An account's performance entry, created on first change; an entry back at every default is
/// dropped so the saved file stays small.
fn client<'a>(settings: &'a mut super::FeatureSettings, key: &str) -> &'a mut perf::Client {
    settings.perf.accounts.retain(|_, c| *c != perf::Client::default());
    settings.perf.accounts.entry(key.to_owned()).or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_lists_round_trip_and_bad_ones_are_refused() {
        assert_eq!(parse_affinity(""), Ok(None));
        assert_eq!(parse_affinity("0-3,6"), Ok(Some(0b100_1111)));
        assert_eq!(affinity_text(0b100_1111), "0-3,6");
        assert_eq!(affinity_text(1 << 63), "63");
        assert!(parse_affinity("3-1").is_err());
        assert!(parse_affinity("64").is_err());
        assert!(parse_affinity("a").is_err());
    }

    #[test]
    fn every_section_has_a_label() {
        for section in Section::ALL { assert!(!section.label().is_empty()); }
    }
}
