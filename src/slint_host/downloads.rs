//! The Downloads page's view model, the launcher settings dialog, the launcher-update strip and
//! card (self_update::banner / view) and the Windows components cards (prereqs::view): `wire`
//! turns their callbacks into core messages, `sync` pushes the core's state after each batch of messages, setting
//! only what changed.
//!
//! Privacy mode masks what identifies someone: paths whole (privacy::mask_path), identities inside free
//! text (results, errors, status), and path fields turn read-only.
#[cfg(not(test))]
use super::send;
use super::sync::{put, sync_model, Private};
use super::{DlActivity, DlInstall, DlInstaller, DlPart, DlSettings, DlStrings, Downloads, Glyph, Ink, Nav, PrereqButton, PrereqCard, PrereqSummary, UpdateLine};
use crate::i18n::{t, tf, Lang};
use crate::operation::{PartState, Progress, Stage};
use crate::patch::PatchState;
use crate::prereqs::model as prereq;
use crate::{App, MaintenanceKind, Message, Page, PickTarget, ResultTone};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

/// The installer's volumes (FFXIFullSetup_US.part1.exe … part5.rar).
const PARTS: u64 = 5;
/// The crystal gauge shows beside the sections from this window width (a strip above them
/// below it). Slint can't work it out from the page's own width: the page's layout depends on it.
const WIDE_W: f32 = 1360.0;

/// Lists of words joined once per language (a card's steps), by the words they join.
type Joined<K> = Vec<(K, SharedString)>;

/// What the callbacks act on, as of the last sync, the models the window draws from, and the
/// words that change only with the language.
#[derive(Default)]
struct State {
    lang: Option<Lang>,
    will_run: SharedString,
    part_names: Vec<SharedString>,
    confirm_steps: Joined<Vec<&'static str>>,
    manual_steps: Joined<[&'static str; 4]>,
    can_check: bool,
    can_repair: bool,
    can_install: bool,
    can_setup: bool,
    can_cancel: bool,
    installer_editable: bool,
    can_browse_installer: bool,
    install_editable: bool,
    can_find: bool,
    can_browse_game: bool,
    strip_action: Option<crate::self_update::Msg>,
    card_action: Option<crate::self_update::Msg>,
    check_again: Option<crate::prereqs::Msg>,
    /// Each card's controls, by `prereq-act`'s order.
    prereq_actions: Vec<[Option<crate::prereqs::Msg>; 6]>,
    parts: Rc<VecModel<DlPart>>,
    cards: Rc<VecModel<PrereqCard>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The window closed: the page's models go with it (nothing stays in the background).
pub(super) fn forget() {
    STATE.with(|cell| cell.borrow_mut().take());
}

/// Sends the message `f` picks from the last sync's state, after releasing it.
fn act(f: impl FnOnce(&State) -> Option<Message>) {
    if let Some(message) = STATE.with(|cell| cell.borrow().as_ref().and_then(f)) {
        send(message);
    }
}

/// Connect the callbacks of a new window (or the System cards window) and give it fresh models.
pub(super) fn wire<C: ComponentHandle>(ui: &C)
where
    for<'a> Downloads<'a>: slint::Global<'a, C>,
{
    let state = State::default();
    let dl = ui.global::<Downloads>();
    dl.set_parts(ModelRc::from(state.parts.clone()));
    dl.set_prereq_cards(ModelRc::from(state.cards.clone()));
    STATE.with(|cell| *cell.borrow_mut() = Some(state));

    // ---- the page ----
    dl.on_change_in_settings(|| send(Message::ToggleSettings));
    dl.on_toggle_troubleshooting(|| send(Message::ToggleTroubleshooting));
    dl.on_check_files(|| act(|s| s.can_check.then_some(Message::StartMaintenance(MaintenanceKind::Check))));
    dl.on_repair_files(|| act(|s| s.can_repair.then_some(Message::StartMaintenance(MaintenanceKind::Repair))));
    dl.on_cancel(|| act(|s| s.can_cancel.then_some(Message::CancelMaintenance)));
    dl.on_installer_edited(|value| act(|s| s.installer_editable.then(|| Message::InstallerPathChanged(value.into()))));
    dl.on_installer_accepted(|| send(Message::SelectInstallerPath));
    dl.on_browse_installer(|| act(|s| s.can_browse_installer.then_some(Message::Browse(PickTarget::InstallerFolder))));
    dl.on_download(|| act(|s| s.can_install.then_some(Message::StartMaintenance(MaintenanceKind::Download))));
    dl.on_extract(|| act(|s| s.can_install.then_some(Message::StartMaintenance(MaintenanceKind::Extract))));
    dl.on_run_setup(|| act(|s| s.can_setup.then_some(Message::StartMaintenance(MaintenanceKind::Setup))));

    // ---- the launcher settings dialog ----
    dl.on_close_settings(|| send(Message::ToggleSettings));
    dl.on_install_edited(|value| act(|s| s.install_editable.then(|| Message::InstallPathChanged(value.into()))));
    dl.on_install_accepted(|| send(Message::SelectInstallPath));
    dl.on_find_game(|| act(|s| s.can_find.then_some(Message::FindInstallation)));
    dl.on_browse_game(|| act(|s| s.can_browse_game.then_some(Message::Browse(PickTarget::GameFolder))));
    dl.on_open_downloads(|| send(Message::Navigate(Page::Maintenance)));
    dl.on_save_settings(|| send(Message::SaveSettings));

    // ---- launcher updates ----
    dl.on_update_strip_clicked(|| act(|s| s.strip_action.clone().map(Message::SelfUpdate)));
    dl.on_update_card_clicked(|| act(|s| s.card_action.clone().map(Message::SelfUpdate)));
    dl.on_auto_check_toggled(|on| send(Message::SelfUpdate(crate::self_update::Msg::AutoCheck(on))));

    // ---- Windows components ----
    dl.on_prereq_check_again(|| act(|s| s.check_again.clone().map(Message::Prereqs)));
    dl.on_prereq_act(|card, which| {
        act(|s| {
            let card = s.prereq_actions.get(usize::try_from(card).ok()?)?;
            card.get(usize::try_from(which).ok()?)?.clone().map(Message::Prereqs)
        })
    });
}

/// Push `app`'s state into the window. Called after every batch of messages: only what shows is
/// worked out (the page on Downloads, the dialog while open, the System cards on Settings →
/// System), and only what changed is set.
pub(super) fn sync<C: ComponentHandle>(ui: &C, app: &App)
where
    for<'a> Downloads<'a>: slint::Global<'a, C>,
    for<'a> Nav<'a>: slint::Global<'a, C>,
{
    let dl = ui.global::<Downloads>();
    let private = Private::new(app);
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(State::default);
        let lang = crate::i18n::current();
        if state.lang != Some(lang) {
            state.lang = Some(lang);
            put!(dl, get_strings, set_strings, strings());
            state.will_run = t("Will run").into();
            state.confirm_steps.clear();
            state.manual_steps.clear();
        }
        if app.page == Page::Maintenance {
            put!(dl, get_wide, set_wide, app.viewport.width >= WIDE_W);
            installation(&dl, app, state, &private);
            activity(&dl, app, state, &private);
            installer(&dl, app, state, &private);
        }
        let dialog = settings(&dl, app, state, &private);
        // The account sheet or this dialog holds Tab and the controller (Nav.modal).
        put!(ui.global::<Nav>(), get_modal, set_modal, dialog || super::view::sheet_shown(app));
        let system = app.page == Page::Settings && app.settings_tab == crate::settings_page::Tab::System;
        updates(&dl, app, state, &private, system);
        if system {
            components(&dl, app, state);
        }
    });
}

fn strings() -> DlStrings {
    DlStrings {
        title: t("Downloads").into(),
        description: t("Install FINAL FANTASY XI and PlayOnline, and check or repair their files.").into(),
        installation: t("Installation").into(),
        game_folder: t("Game folder").into(),
        change_in_settings: t("Change in Settings").into(),
        confirmed: t("Folder confirmed as the target for checks and changes").into(),
        confirm_first: t("Confirm this folder before starting").into(),
        ffxi: t("FINAL FANTASY XI").into(),
        playonline: t("PlayOnline").into(),
        installed_version: t("Installed version").into(),
        latest_version: t("Latest version").into(),
        version_status: t("Version status").into(),
        troubleshooting: t("Troubleshooting").into(),
        check_files: t("Check files").into(),
        repair_files: t("Repair files").into(),
        close_clients: t("Close running clients to repair").into(),
        close_games: t("Close all games before repairing.").into(),
        activity: t("Activity").into(),
        overall_update: t("Overall update").into(),
        no_operation: t("No operation running").into(),
        last_result: t("Last result").into(),
        working: t("Working…").into(),
        official_step: t("Complete or cancel this step in the official installer window.").into(),
        installer_folder: t("Installer folder").into(),
        installer_placeholder: t("Folder for official installer downloads").into(),
        browse: t("Browse…").into(),
        install_title: t("Install PlayOnline and FFXI").into(),
        step1_title: t("Download official installer").into(),
        step1_body: t("Five files, about 7.7 GB.").into(),
        download: t("Download").into(),
        step2_title: t("Extract").into(),
        extract: t("Extract").into(),
        step3_title: t("Run official setup").into(),
        step3_body: t("Select PlayOnline Viewer and FINAL FANTASY XI.").into(),
        run_setup: t("Run setup").into(),
        installer_files: t("Installer files").into(),
        parts: tf("{count} parts", &[("count", &PARTS)]).into(),
        installer_note: t("Choose the game destination in the setup wizard. Wait for extraction and setup to finish before closing Crystal.").into(),
        settings_title: t("Launcher settings").into(),
        settings_subtitle: t("Where the game lives and how the launcher looks after it.").into(),
        install_label: t("FINAL FANTASY XI installation").into(),
        install_placeholder: t("Path to the FINAL FANTASY XI folder").into(),
        install_repair: t("Install & repair").into(),
        install_repair_body: t("Check or repair this installation under Downloads → Troubleshooting.").into(),
        open_downloads: t("Open Downloads").into(),
        close: t("Close").into(),
        save_settings: t("Save settings").into(),
        launcher_updates: t("Launcher updates").into(),
        version: env!("CARGO_PKG_VERSION").into(),
        auto_check: t("Check for launcher updates when Crystal opens").into(),
        auto_check_note: t("Updates are signed and verified before installation. Running games stay open.").into(),
    }
}

/// A version is set in the monospaced face; a status word in place of one keeps the interface's.
fn mono(value: &str) -> bool {
    value.chars().any(|ch| ch.is_ascii_digit())
}

fn installation(dl: &Downloads, app: &App, state: &mut State, private: &Private) {
    let idle = app.maintenance.is_none();
    let update_blocked = app.sessions_block_update();
    let can_check = idle && !app.installation_loading && app.settings.install_selected;
    let can_repair = can_check && !update_blocked && app.vault_ready;
    state.can_check = can_check;
    state.can_repair = can_repair;

    let no_folder = app.settings.install_path.trim().is_empty();
    let folder = if no_folder { t("Not selected").to_owned() } else { crate::privacy::mask_path(&app.settings.install_path) };
    let installed = app.patch.as_ref().map_or(t("Unknown"), |p| p.local_version.as_str());
    // The official version comes from the last real version check, never from Check files.
    let latest = crate::official_versions::latest_game_text(app);
    let remembered = app
        .patch
        .as_ref()
        .filter(|p| p.state == PatchState::Unknown)
        .and_then(|p| crate::official_versions::compare(&p.local_version, app.learned_game()));
    let (state_glyph, state_ink, state_text) = match app.patch.as_ref().map(|p| p.state) {
        Some(PatchState::Current) => (Glyph::CheckCircle, Ink::Ok, t("Up to date")),
        Some(PatchState::Required) => (Glyph::Download, Ink::Accent, t("Update available")),
        Some(PatchState::Interrupted) => (Glyph::AlertCircle, Ink::Accent, t("Interrupted update")),
        _ if remembered == Some(std::cmp::Ordering::Equal) => (Glyph::CheckCircle, Ink::Ok, t("Matched at last check")),
        _ if remembered == Some(std::cmp::Ordering::Less) => (Glyph::Download, Ink::Accent, t("Update available")),
        _ if app.patch_loading => (Glyph::Refresh, Ink::Sub, t("Checking version…")),
        _ => (Glyph::AlertCircle, Ink::Sub, t("Not checked")),
    };
    let (viewer_installed, viewer_detail) =
        app.viewer.as_ref().map(|v| (v.version.local_version.as_str(), v.version.detail.as_str())).unwrap_or((t("Not checked"), ""));
    let viewer_latest = crate::official_versions::latest_playonline_text(app);
    // Each state has its glyph, as the FFXI status beside it does, so both lines share one text edge.
    let (viewer_state_glyph, viewer_state_ink, viewer_state) = if app.viewer_loading {
        (Glyph::Refresh, Ink::Sub, t("Checking…"))
    } else if app.viewer.as_ref().is_some_and(crate::viewer::Status::requires_update) {
        (Glyph::AlertCircle, Ink::Accent, t("Needs attention"))
    } else if app.viewer.as_ref().is_some_and(|v| v.version.state == PatchState::Current) {
        (Glyph::CheckCircle, Ink::Ok, t("Up to date"))
    } else {
        (Glyph::AlertCircle, Ink::Sub, t("Not verified"))
    };
    let viewer_folder = app
        .viewer
        .as_ref()
        .and_then(|v| v.folder.as_ref())
        .map(|p| crate::privacy::mask_path(&p.to_string_lossy()))
        .unwrap_or_else(|| t("Not installed").into());
    let viewer_error = app.viewer.as_ref().and_then(|v| v.core_error.as_deref()).map(|e| private.text(e)).unwrap_or_default();
    let install = DlInstall {
        folder: folder.into(),
        folder_mono: !no_folder,
        confirmed: app.settings.install_selected,
        installed_mono: mono(installed),
        installed: installed.into(),
        latest_mono: mono(&latest),
        latest: latest.into(),
        state_glyph,
        state_ink,
        state_text: state_text.into(),
        patch_detail: private.text(app.patch.as_ref().map_or("", |report| report.detail.as_str())).into(),
        viewer_folder: viewer_folder.into(),
        viewer_installed_mono: mono(viewer_installed),
        viewer_installed: viewer_installed.into(),
        viewer_latest_mono: mono(&viewer_latest),
        viewer_latest: viewer_latest.into(),
        viewer_state_glyph,
        viewer_state_ink,
        viewer_state: viewer_state.into(),
        viewer_detail: private.text(viewer_detail).into(),
        viewer_error: viewer_error.into(),
        troubleshooting_open: app.troubleshooting_open,
        can_check,
        can_repair,
        close_clients: can_check && update_blocked,
    };
    put!(dl, get_install, set_install, install);
}

// ---- the page's helpers ----

fn dl_bytes(value: u64) -> String {
    let v = value as f64;
    if v >= 1e9 {
        format!("{:.2} GB", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.0} MB", v / 1e6)
    } else {
        format!("{value} B")
    }
}

pub(super) fn dl_stage(kind: MaintenanceKind, stage: Stage) -> String {
    match stage {
        Stage::InstallerMetadata => t("Checking the official installer files").into(),
        Stage::InstallerDownload => t("Downloading the official installer").into(),
        Stage::Manifest => t("Reading the official file manifest").into(),
        Stage::SignOut => t("Signing out idle PlayOnline sessions before updating").into(),
        Stage::Check => t("Checking local files").into(),
        Stage::Download => t("Downloading official files").into(),
        Stage::Verify => t("Verifying downloaded files").into(),
        Stage::Apply => t("Applying files").into(),
        Stage::Finish => t("Finalizing update…").into(),
        Stage::None | Stage::Waiting => kind.label().to_owned(),
    }
}

/// The operation's detail line in the current language: an installer volume's step is worded
/// from its data, anything else is shown as reported.
fn dl_detail(progress: &Progress) -> String {
    let Some(part) = progress.part else { return progress.detail.clone() };
    let name = part.name;
    match part.state {
        PartState::Checking => tf("Checking {name}", &[("name", &name)]),
        PartState::Downloading { done: 0, .. } => tf("Downloading {name}", &[("name", &name)]),
        PartState::Downloading { done, expected } => {
            tf("Downloading {name}: {done} of {total}", &[("name", &name), ("done", &dl_bytes(done)), ("total", &dl_bytes(expected))])
        }
        PartState::AlreadyComplete => tf("Already complete: {name}", &[("name", &name)]),
        PartState::Complete => tf("Complete: {name}", &[("name", &name)]),
        PartState::Changed => tf("{name} changed; downloading the official file again", &[("name", &name)]),
    }
}

/// The "Last result" icon and colour, from the result's tone (never its words).
fn result_look(tone: ResultTone) -> (Glyph, Ink) {
    match tone {
        ResultTone::Stopped => (Glyph::AlertCircle, Ink::Danger),
        ResultTone::Unverified => (Glyph::AlertCircle, Ink::Sub),
        ResultTone::Done => (Glyph::CheckCircle, Ink::Ok),
    }
}

fn activity(dl: &Downloads, app: &App, state: &mut State, private: &Private) {
    let cancelled = app.maintenance_cancelled;
    let mut view = DlActivity { gauge: -1.0, ..DlActivity::default() };
    state.can_cancel = false;
    if let Some(kind) = app.maintenance {
        let progress = &app.maintenance_progress;
        let has_overall = progress.overall.is_some();
        view.running = true;
        view.cancellable = kind.cancellable();
        view.cancel_label = (if cancelled { t("Cancelling…") } else { t("Cancel") }).into();
        view.cancel_enabled = !cancelled;
        state.can_cancel = kind.cancellable() && !cancelled;
        view.has_overall = has_overall;
        if let Some(overall) = &progress.overall {
            view.overall_fraction = overall.fraction.unwrap_or(0.0).clamp(0.0, 1.0) as f32;
            match overall.remaining_percent() {
                // The header shows the share; only an unknown total needs a line.
                Some(_) => view.overall_known = true,
                None => view.overall_line = t("Calculating total update size…").into(),
            }
            if !cancelled {
                view.estimate = match overall.remaining {
                    Some(duration) if duration.is_zero() && !overall.complete => t("Finalizing update…").to_owned(),
                    Some(duration) => tf("{time} left", &[("time", &crate::play_update::remaining_time(duration))]),
                    None => t("Estimating total time remaining…").to_owned(),
                }
                .into();
            }
        }
        view.step = if has_overall { tf("Current step: {step}", &[("step", &dl_stage(kind, progress.stage))]) } else { dl_stage(kind, progress.stage) }.into();
        view.kind = kind.label().into();
        view.detail = dl_detail(progress).into();
        if progress.total > 0 {
            view.has_total = true;
            view.fraction = (progress.done as f64 / progress.total as f64).clamp(0.0, 1.0) as f32;
            let amount = if progress.bytes {
                tf("{done} of {total}", &[("done", &dl_bytes(progress.done)), ("total", &dl_bytes(progress.total))])
            } else {
                tf("{done} of {total}", &[("done", &progress.done), ("total", &progress.total)])
            };
            // The share is the header's figure: the amount alone here.
            view.amount = amount.into();
        } else if progress.detail.is_empty() {
            view.detail = SharedString::new();
        }
        if !has_overall {
            // Time left only: the share is the header's figure.
            if kind.cancellable() && !cancelled {
                view.remaining = match progress.remaining {
                    Some(duration) => tf("{time} left", &[("time", &crate::play_update::remaining_time(duration))]),
                    None => t("Estimating time remaining…").to_owned(),
                }
                .into();
            }
        }
        view.official_note = !kind.cancellable();
        // The gauge follows the whole operation's figure, else the step's; no figure lights nothing.
        let known = match &progress.overall {
            Some(overall) => overall.fraction.map(|f| f.clamp(0.0, 1.0) as f32),
            None => view.has_total.then_some(view.fraction),
        };
        view.gauge = known.unwrap_or(0.0);
        if let Some(fraction) = known {
            view.gauge_label = format!("{}%", (fraction * 100.0).round() as u32).into();
        }
    }
    if !app.maintenance_result.is_empty() {
        let (glyph, ink) = result_look(app.maintenance_tone);
        view.result = private.text(&app.maintenance_result).into();
        view.result_glyph = glyph;
        view.result_ink = ink;
    }
    if !app.status.is_empty() && app.status != app.maintenance_result {
        view.status = private.text(&app.status).into();
    }
    put!(dl, get_activity, set_activity, view);
}

/// Per-volume state of the official installer download, from the operation's
/// progress (it names the volume being transferred). `None` outside the installer stages.
fn parts(progress: &Progress, running: bool, names: &[SharedString]) -> Option<Vec<DlPart>> {
    let metadata = progress.stage == Stage::InstallerMetadata;
    if !metadata && progress.stage != Stage::InstallerDownload {
        return None;
    }
    let part = progress.part.filter(|_| !metadata);
    let current = part.map_or(0, |p| u64::from(p.number));
    let current_complete = part.is_some_and(|p| matches!(p.state, PartState::Complete | PartState::AlreadyComplete));
    let all_done = !metadata && progress.total > 0 && progress.done >= progress.total;
    let current_fraction = match part.map(|p| p.state) {
        Some(PartState::Downloading { done, expected }) if expected > 0 => (done as f64 / expected as f64).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    };
    let rows = (1..=PARTS)
        .zip(names)
        .map(|(n, name)| {
            let (fraction, state) = if metadata {
                if n <= progress.done {
                    (0.0, t("Checked").to_owned())
                } else if n == progress.done + 1 && running {
                    (0.0, t("Checking").to_owned())
                } else {
                    (0.0, t("Waiting").to_owned())
                }
            } else if all_done || n < current || (n == current && current_complete) {
                (1.0, t("Complete").to_owned())
            } else if n == current {
                let pct = (current_fraction * 100.0).round();
                (current_fraction, if running { format!("{pct:.0}%") } else { tf("Stopped at {percent}%", &[("percent", &format!("{pct:.0}"))]) })
            } else {
                (0.0, if running { t("Waiting") } else { t("Not downloaded") }.to_owned())
            };
            DlPart { name: name.clone(), state: state.into(), fraction, done: fraction >= 1.0 }
        })
        .collect();
    Some(rows)
}

fn installer(dl: &Downloads, app: &App, state: &mut State, private: &Private) {
    let idle = app.maintenance.is_none();
    let editable = crate::privacy::editable();
    state.installer_editable = editable;
    state.can_browse_installer = idle;
    state.can_install = idle;
    state.can_setup = idle && !app.has_sessions();
    if state.part_names.is_empty() {
        state.part_names = (1..=PARTS).map(|n| format!("FFXIFullSetup_US.part{n}.{}", if n == 1 { "exe" } else { "rar" }).into()).collect();
    }
    let rows = parts(&app.maintenance_progress, app.maintenance == Some(MaintenanceKind::Download), &state.part_names);
    let view = DlInstaller {
        editable,
        browse_enabled: idle,
        error: app.installer_error.as_deref().map(|e| private.text(e)).unwrap_or_default().into(),
        can_install: state.can_install,
        can_setup: state.can_setup,
        show_parts: rows.is_some(),
    };
    put!(dl, get_installer, set_installer, view);
    sync_model(&state.parts, rows.unwrap_or_default());
    put!(dl, get_installer_path, set_installer_path, crate::privacy::mask_path(&app.settings.installer_path).into());
}

/// The launcher settings dialog; whether it shows. Its contents are worked out only while it does.
fn settings(dl: &Downloads, app: &App, state: &mut State, private: &Private) -> bool {
    // The dialog shows when nothing stacked above it (close, delete, the account sheet, import)
    // is open.
    let open = app.show_settings && !app.confirm_close && app.pending_delete.is_none() && app.editor.is_none() && !app.account_import.open;
    put!(dl, get_settings_open, set_settings_open, open);
    if !open {
        return false;
    }
    let idle = app.maintenance.is_none();
    state.install_editable = crate::privacy::editable();
    state.can_find = idle && !app.installation_loading;
    state.can_browse_game = idle;
    let view = DlSettings {
        find_label: (if app.installation_loading { t("Finding…") } else { t("Find game") }).into(),
        find_enabled: state.can_find,
        browse_enabled: idle,
        editable: state.install_editable,
        confirmed: app.settings.install_selected,
        status: private.text(&app.status).into(),
    };
    put!(dl, get_settings, set_settings, view);
    put!(dl, get_install_path, set_install_path, crate::privacy::mask_path(&app.settings.install_path).into());
    true
}

/// The strip under the title bar (every page), and Settings → System's card (`card`: shown).
fn updates(dl: &Downloads, app: &App, state: &mut State, private: &Private, card: bool) {
    let strip = crate::self_update::model::banner(app);
    put!(dl, get_update_banner, set_update_banner, strip.is_some());
    let strip = strip.unwrap_or(crate::self_update::model::Line { text: String::new(), action: None });
    state.strip_action = strip.action.as_ref().map(|(_, msg)| msg.clone());
    let (pill, busy) = crate::self_update::model::pill(app).unwrap_or_default();
    put!(dl, get_update_strip, set_update_strip, UpdateLine { text: strip.text.into(), action: strip.action.map_or("", |(label, _)| label).into(), pill: pill.into(), busy });
    if !card {
        return;
    }
    let card = crate::self_update::model::card(app);
    state.card_action = card.action.as_ref().map(|(_, msg)| msg.clone());
    put!(dl, get_update_card, set_update_card, UpdateLine { text: private.text(&card.text).into(), action: card.action.map_or("", |(label, _)| label).into(), ..Default::default() });
    put!(dl, get_auto_check, set_auto_check, app.settings.launcher_auto_updates);
}

fn glyph(which: prereq::Glyph) -> Glyph {
    match which {
        prereq::Glyph::Refresh => Glyph::Refresh,
        prereq::Glyph::Check => Glyph::CheckCircle,
        prereq::Glyph::Alert => Glyph::AlertCircle,
        prereq::Glyph::Download => Glyph::Download,
        prereq::Glyph::Wrench => Glyph::Wrench,
        prereq::Glyph::Close => Glyph::Close,
        prereq::Glyph::ExternalLink => Glyph::ExternalLink,
    }
}

/// prereqs' tones (ok, warn, danger; the accent; sub).
fn tone(which: prereq::Tone) -> Ink {
    match which {
        prereq::Tone::Sub => Ink::Sub,
        prereq::Tone::Ok => Ink::OkTone,
        prereq::Tone::Warn => Ink::WarnTone,
        prereq::Tone::Accent => Ink::Accent,
        prereq::Tone::Danger => Ink::DangerTone,
    }
}

/// `words` joined by `join`, from `cache` when they were joined before (the language's words
/// are static, so the same words give the same text).
fn joined<K: PartialEq + Clone>(cache: &mut Joined<K>, words: &K, join: impl FnOnce() -> String) -> SharedString {
    if let Some((_, text)) = cache.iter().find(|(key, _)| key == words) {
        return text.clone();
    }
    let text = SharedString::from(join());
    cache.push((words.clone(), text.clone()));
    text
}

/// A card button, and the message it sends (kept for its callback).
fn button(button: Option<&prereq::Button>) -> (PrereqButton, Option<crate::prereqs::Msg>) {
    match button {
        Some(b) => (
            PrereqButton { show: true, label: b.label.into(), glyph: glyph(b.glyph), emphasis: b.emphasis, enabled: b.message.is_some() },
            b.message.clone(),
        ),
        None => (PrereqButton::default(), None),
    }
}

fn components(dl: &Downloads, app: &App, state: &mut State) {
    let summary = prereq::summary(&app.prereqs);
    let (check_again, message) = button(Some(&summary.check_again));
    state.check_again = message;
    put!(
        dl,
        get_prereq_summary,
        set_prereq_summary,
        PrereqSummary {
            title: summary.title.into(),
            glyph: glyph(summary.glyph),
            tone: tone(summary.tone),
            summary: summary.summary.into(),
            when: summary.when.into(),
            intro: summary.intro.into(),
            check_again,
        }
    );
    state.prereq_actions.clear();
    let mut rows = Vec::with_capacity(3);
    for card in prereq::cards(&app.prereqs) {
        let (main, main_msg) = button(card.button.as_ref());
        let (dismiss, dismiss_msg) = button(card.confirm.as_ref().map(|c| &c.dismiss));
        let (install, install_msg) = button(card.confirm.as_ref().map(|c| &c.install));
        let (cancel, cancel_msg) = button(card.progress.as_ref().and_then(|p| p.cancel.as_ref()));
        let (open_features, features_msg) = button(card.manual.as_ref().map(|m| &m.open_features));
        let toggle_msg = card.manual.as_ref().map(|_| crate::prereqs::Msg::ToggleManual);
        state.prereq_actions.push([main_msg, dismiss_msg, install_msg, cancel_msg, toggle_msg, features_msg]);
        let (result_glyph, result_tone, result) = card.result.clone().unwrap_or((prereq::Glyph::Check, prereq::Tone::Sub, String::new()));
        rows.push(PrereqCard {
            name: card.name.into(),
            why: card.why.into(),
            badge_glyph: glyph(card.badge.0),
            tone: tone(card.badge.1),
            chip: card.chip.into(),
            version: card.version.into(),
            evidence: card.evidence.into(),
            button: main,
            confirm: card.confirm.is_some(),
            heading: card.confirm.as_ref().map(|c| c.heading.clone()).unwrap_or_default().into(),
            steps: card.confirm.as_ref().map(|c| joined(&mut state.confirm_steps, &c.steps, || c.steps.iter().map(|step| format!("•  {step}")).collect::<Vec<_>>().join("\n"))).unwrap_or_default(),
            will_run: state.will_run.clone(),
            command: card.confirm.as_ref().map_or("", |c| c.command).into(),
            permission: card.confirm.as_ref().map_or("", |c| c.permission).into(),
            dismiss,
            install,
            running: card.progress.is_some(),
            progress_head: card.progress.as_ref().map(|p| p.head.clone()).unwrap_or_default().into(),
            progress_detail: card.progress.as_ref().map(|p| p.detail.clone()).unwrap_or_default().into(),
            progress: card.progress.as_ref().map_or(-1.0, |p| p.fraction.unwrap_or(-1.0)),
            cancel,
            has_result: card.result.is_some(),
            result_glyph: glyph(result_glyph),
            result_tone: tone(result_tone),
            // Unmasked: the note names no one.
            result: result.into(),
            manual: card.manual.is_some(),
            manual_open: card.manual.as_ref().is_some_and(|m| m.open),
            manual_toggle: card.manual.as_ref().map_or("", |m| m.toggle).into(),
            manual_steps: card
                .manual
                .as_ref()
                .map(|m| joined(&mut state.manual_steps, &m.steps, || m.steps.iter().enumerate().map(|(n, step)| format!("{}.  {step}", n + 1)).collect::<Vec<_>>().join("\n")))
                .unwrap_or_default(),
            open_features,
        });
    }
    sync_model(&state.cards, rows);
}

/// Tests deliver the page's messages here instead of to a core.
#[cfg(test)]
fn send(message: Message) {
    tests::SENT.with(|sent| sent.borrow_mut().push(message));
}

/// Headless tests on the real windows (software renderer, nothing on screen): what the page
/// shows for a state, the message each control sends, privacy masking, and that the keyboard and
/// the controller walker reach the page's controls. CRYSTAL_SLINT_SHOTS=<folder> also saves the
/// Settings → System cards as PNGs.
#[cfg(test)]
mod tests {
    use super::super::{pad, MainWindow, SystemCards, Theme};
    use super::*;
    use crate::self_update::{Msg as UpdateMsg, State as UpdateState};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Key, WindowEvent};
    use slint::Model;

    thread_local! {
        pub(super) static SENT: RefCell<Vec<Message>> = const { RefCell::new(Vec::new()) };
    }

    fn take() -> Vec<Message> {
        SENT.with(|sent| std::mem::take(&mut *sent.borrow_mut()))
    }

    struct Headless(Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    /// This test thread's software window (Slint keeps one platform per thread).
    fn headless(width: u32, height: u32) -> Rc<MinimalSoftwareWindow> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::set_platform(Box::new(Headless(window.clone()))).expect("one platform per test thread");
        window
    }

    fn downloads_app() -> App {
        crate::i18n::set(Lang::En);
        let mut app = crate::tests::app();
        app.page = Page::Maintenance;
        app.viewport = crate::effect::Size::new(1440.0, 900.0);
        app
    }

    /// The main window on the Downloads page, wired and synced from `app`.
    fn page(app: &App) -> MainWindow {
        let ui = MainWindow::new().unwrap();
        super::super::view::wire(&ui);
        wire(&ui);
        resync(&ui, app);
        ui.show().unwrap();
        take();
        ui
    }

    fn resync(ui: &MainWindow, app: &App) {
        super::super::view::sync(ui, app);
        sync(ui, app);
    }

    /// Downloads with a download of the official installer running (preview's --preview-busy).
    fn downloading(app: &mut App) {
        app.maintenance = Some(MaintenanceKind::Download);
        let part = crate::operation::Part {
            number: 3,
            name: "FFXIFullSetup_US.part3.rar",
            state: PartState::Downloading { done: 678_760_000, expected: 1_610_612_736 },
        };
        app.maintenance_progress = Progress {
            stage: Stage::InstallerDownload,
            done: 3_900_000_000,
            total: 7_734_210_712,
            detail: part.english(),
            part: Some(part),
            bytes: true,
            remaining: Some(std::time::Duration::from_secs(115)),
            overall: None,
        };
    }

    #[test]
    fn the_page_shows_the_state_and_every_control_sends_its_message() {
        let _window = headless(1440, 900);
        let mut app = downloads_app();
        app.settings.install_path = r"C:\Games\FINAL FANTASY XI".into();
        app.settings.install_selected = true;
        app.troubleshooting_open = true;
        let ui = page(&app);
        let dl = ui.global::<Downloads>();
        assert_eq!(dl.get_strings().title, t("Downloads"));
        let install = dl.get_install();
        assert_eq!(install.folder, r"C:\Games\FINAL FANTASY XI");
        assert!(install.folder_mono && install.confirmed && install.troubleshooting_open);
        assert!(install.can_check && install.can_repair, "a confirmed folder, vault ready, no sessions");
        assert!(!dl.get_activity().running && dl.get_activity().gauge < 0.0, "idle shows the whole, unlit crystal");
        assert!(!dl.get_installer().show_parts && dl.get_parts().row_count() == 0);

        dl.invoke_change_in_settings();
        dl.invoke_toggle_troubleshooting();
        dl.invoke_check_files();
        dl.invoke_repair_files();
        dl.invoke_installer_edited(r"D:\Installer".into());
        dl.invoke_installer_accepted();
        dl.invoke_browse_installer();
        dl.invoke_download();
        dl.invoke_extract();
        dl.invoke_run_setup();
        dl.invoke_cancel();
        let sent = take();
        assert_eq!(sent.len(), 10, "Cancel sends nothing with nothing running");
        assert!(matches!(sent[0], Message::ToggleSettings));
        assert!(matches!(sent[1], Message::ToggleTroubleshooting));
        assert!(matches!(sent[2], Message::StartMaintenance(MaintenanceKind::Check)));
        assert!(matches!(sent[3], Message::StartMaintenance(MaintenanceKind::Repair)));
        assert!(matches!(&sent[4], Message::InstallerPathChanged(path) if path == r"D:\Installer"));
        assert!(matches!(sent[5], Message::SelectInstallerPath));
        assert!(matches!(sent[6], Message::Browse(PickTarget::InstallerFolder)));
        assert!(matches!(sent[7], Message::StartMaintenance(MaintenanceKind::Download)));
        assert!(matches!(sent[8], Message::StartMaintenance(MaintenanceKind::Extract)));
        assert!(matches!(sent[9], Message::StartMaintenance(MaintenanceKind::Setup)));

        // A running download: Activity leads with it, the parts show, the steps wait.
        downloading(&mut app);
        resync(&ui, &app);
        let activity = dl.get_activity();
        assert!(activity.running && activity.cancellable && activity.cancel_enabled && activity.has_total);
        assert_eq!(activity.step, t("Downloading the official installer"));
        assert!(!activity.amount.contains('%'), "the share is the header's: {}", activity.amount);
        assert!((activity.gauge - 0.5).abs() < 0.01 && activity.gauge_label == "50%", "the crystal follows the step: {} {}", activity.gauge, activity.gauge_label);
        // With an overall figure the crystal shows that one; with none known it is lit to nothing.
        app.maintenance_progress.overall = Some(crate::operation::OverallProgress { fraction: Some(0.28), remaining: None, complete: false });
        resync(&ui, &app);
        assert_eq!((dl.get_activity().gauge_label.as_str(), dl.get_activity().gauge > 0.27), ("28%", true));
        app.maintenance_progress.overall = Some(crate::operation::OverallProgress { fraction: None, remaining: None, complete: false });
        resync(&ui, &app);
        assert_eq!((dl.get_activity().gauge, dl.get_activity().gauge_label.as_str()), (0.0, ""));
        app.maintenance_progress.overall = None;
        resync(&ui, &app);
        assert!(activity.detail.contains("part3.rar"));
        let parts: Vec<DlPart> = dl.get_parts().iter().collect();
        assert_eq!(parts.len(), 5);
        assert!(parts[0].done && parts[1].done && !parts[2].done && !parts[3].done);
        assert_eq!(parts[2].state, "42%");
        assert_eq!(parts[4].state, t("Waiting"));
        let installer = dl.get_installer();
        assert!(installer.show_parts && !installer.can_install && !installer.browse_enabled);
        assert!(!dl.get_install().can_check);
        dl.invoke_download();
        dl.invoke_check_files();
        dl.invoke_browse_installer();
        dl.invoke_cancel();
        let sent = take();
        assert_eq!(sent.len(), 1, "only Cancel acts while a download runs");
        assert!(matches!(sent[0], Message::CancelMaintenance));
        app.maintenance_cancelled = true;
        resync(&ui, &app);
        assert_eq!(dl.get_activity().cancel_label, t("Cancelling…"));
        dl.invoke_cancel();
        assert!(take().is_empty());
    }

    #[test]
    fn privacy_mode_masks_paths_and_identities_but_keeps_messages_readable() {
        let _window = headless(1440, 900);
        let mut app = downloads_app();
        crate::privacy::set_profile_dir_for_test(r"C:\Users\Jane");
        app.settings.installer_path = r"C:\Users\Jane\Downloads".into();
        app.settings.install_path = r"C:\Users\Jane\Games\FFXI".into();
        app.installer_error = Some("Close all games before running setup.".into());
        app.maintenance_result = "Main account: update finished".into();
        app.status = r"Selected installer download folder: C:\Users\Jane\Downloads".into();
        app.show_settings = true;
        let report = |detail: &str| crate::patch::PatchReport {
            state: PatchState::Current,
            local_version: "30251105_0".into(),
            remote_version: String::new(),
            detail: detail.into(),
            files_verified: None,
        };
        app.viewer = Some(crate::viewer::Status {
            folder: Some(r"C:\Users\Jane\PlayOnline\SquareEnix\PlayOnlineViewer".into()),
            version: report("Main account: viewer checked"),
            core_error: None,
        });
        app.patch = Some(report("Main account: files checked"));
        crate::privacy::set(true);
        let ui = page(&app);
        let dl = ui.global::<Downloads>();
        let shown = [
            dl.get_installer_path().to_string(),
            dl.get_install_path().to_string(),
            dl.get_install().folder.to_string(),
            dl.get_activity().result.to_string(),
            dl.get_activity().status.to_string(),
            dl.get_installer().error.to_string(),
            dl.get_settings().status.to_string(),
            dl.get_install().viewer_folder.to_string(),
            dl.get_install().patch_detail.to_string(),
            dl.get_install().viewer_detail.to_string(),
        ];
        crate::privacy::set(false);
        for text in &shown {
            assert!(!text.contains("Jane") && !text.contains("Main account"), "{text:?} is readable with privacy mode on");
        }
        assert!(shown[3].contains("update finished"), "{shown:?}");
        assert!(shown[8].contains("files checked"), "{shown:?}");
        assert_eq!(shown[5], "Close all games before running setup.");
        assert!(shown[0].starts_with(r"C:\"), "the drive stays readable: {:?}", shown[0]);
        assert!(!dl.get_installer().editable && !dl.get_settings().editable, "path fields are read-only");
        dl.invoke_installer_edited(r"C:\x".into());
        dl.invoke_install_edited(r"C:\x".into());
        assert!(take().is_empty(), "a masked field never writes its mask back");
    }

    #[test]
    fn the_launcher_settings_dialog_shows_under_the_dialogs_above_it() {
        let _window = headless(1440, 900);
        let mut app = downloads_app();
        let ui = page(&app);
        let dl = ui.global::<Downloads>();
        assert!(!dl.get_settings_open());
        app.show_settings = true;
        resync(&ui, &app);
        assert!(dl.get_settings_open());
        assert_eq!(dl.get_settings().find_label, t("Find game"));
        dl.invoke_find_game();
        dl.invoke_browse_game();
        dl.invoke_install_edited(r"D:\FFXI".into());
        dl.invoke_install_accepted();
        dl.invoke_open_downloads();
        dl.invoke_save_settings();
        dl.invoke_close_settings();
        let sent = take();
        assert!(matches!(sent[0], Message::FindInstallation));
        assert!(matches!(sent[1], Message::Browse(PickTarget::GameFolder)));
        assert!(matches!(&sent[2], Message::InstallPathChanged(path) if path == r"D:\FFXI"));
        assert!(matches!(sent[3], Message::SelectInstallPath));
        assert!(matches!(sent[4], Message::Navigate(Page::Maintenance)));
        assert!(matches!(sent[5], Message::SaveSettings));
        assert!(matches!(sent[6], Message::ToggleSettings));
        // Finding: the button says so and waits.
        app.installation_loading = true;
        resync(&ui, &app);
        assert_eq!(dl.get_settings().find_label, t("Finding…"));
        dl.invoke_find_game();
        assert!(take().is_empty());
        // The account sheet sits above it.
        app.editor = Some(crate::Editor::blank());
        resync(&ui, &app);
        assert!(!dl.get_settings_open());
    }

    #[test]
    fn launcher_updates_show_the_strip_and_card_with_their_actions() {
        let _window = headless(1440, 900);
        let mut app = downloads_app();
        // The card is worked out on Settings → System; the strip on every page.
        app.page = Page::Settings;
        app.settings_tab = crate::settings_page::Tab::System;
        app.self_update = UpdateState::Current;
        let ui = page(&app);
        let dl = ui.global::<Downloads>();
        assert!(!dl.get_update_banner());
        assert_eq!(dl.get_update_card().action, t("Check again"));
        dl.invoke_update_card_clicked();
        dl.invoke_update_strip_clicked();
        dl.invoke_auto_check_toggled(false);
        let sent = take();
        assert_eq!(sent.len(), 2, "the hidden strip has no action");
        assert!(matches!(sent[0], Message::SelfUpdate(UpdateMsg::Check)));
        assert!(matches!(sent[1], Message::SelfUpdate(UpdateMsg::AutoCheck(false))));
        app.self_update = UpdateState::Downloading;
        resync(&ui, &app);
        assert!(dl.get_update_banner());
        assert_eq!(dl.get_update_strip().text, t("Downloading and verifying the launcher update…"));
        assert_eq!(dl.get_update_strip().action, "");
        assert_eq!(dl.get_update_card().action, "");
    }

    /// Keyboard Tab and the controller walker reach every control on the page: each press sends
    /// its message (the core is not updated, so nothing moves while walking).
    #[test]
    fn the_keyboard_and_the_controller_reach_every_downloads_control() {
        let window = headless(1440, 900);
        let mut app = downloads_app();
        app.settings.install_selected = true;
        app.troubleshooting_open = true;
        let ui = page(&app);
        let w = ui.window();
        // A press on a dropdown (the footer's) opens its menu: a click on the page's title,
        // outside it, closes it again so the walk goes on over the page.
        let press = || {
            pad::press(w);
            let at = slint::LogicalPosition::new(70.0, 66.0);
            w.dispatch_event(WindowEvent::PointerPressed { position: at, button: slint::platform::PointerEventButton::Left });
            w.dispatch_event(WindowEvent::PointerReleased { position: at, button: slint::platform::PointerEventButton::Left });
            take()
        };
        let tab: SharedString = Key::Tab.into();
        let mut sent = Vec::new();
        for _ in 0..120 {
            w.dispatch_event(WindowEvent::KeyPressed { text: tab.clone() });
            w.dispatch_event(WindowEvent::KeyReleased { text: tab.clone() });
            sent.extend(press());
        }
        let reached = |sent: &[Message]| -> Vec<&'static str> {
            let mut names: Vec<&'static str> = sent
                .iter()
                .filter_map(|m| match m {
                    Message::ToggleSettings => Some("change in settings"),
                    Message::ToggleTroubleshooting => Some("troubleshooting"),
                    Message::StartMaintenance(MaintenanceKind::Check) => Some("check"),
                    Message::StartMaintenance(MaintenanceKind::Repair) => Some("repair"),
                    Message::SelectInstallerPath => Some("installer folder"),
                    Message::Browse(PickTarget::InstallerFolder) => Some("browse"),
                    Message::StartMaintenance(MaintenanceKind::Download) => Some("download"),
                    Message::StartMaintenance(MaintenanceKind::Extract) => Some("extract"),
                    Message::StartMaintenance(MaintenanceKind::Setup) => Some("setup"),
                    _ => None,
                })
                .collect();
            names.sort_unstable();
            names.dedup();
            names
        };
        let all = vec!["browse", "change in settings", "check", "download", "extract", "installer folder", "repair", "setup", "troubleshooting"];
        assert_eq!(reached(&sent), all, "Tab");

        // The walker: to the top right, down the installer card a few stops, across to the main
        // column and down it. The walker prefers what lies straight ahead (Android's rule), so
        // the main column is entered from beside it, or from below.
        let mut sent = Vec::new();
        let sweep = |depth: usize| {
            [pad::Way::Up; 40].into_iter().chain([pad::Way::Right; 6]).chain(std::iter::repeat_n(pad::Way::Down, depth)).chain([pad::Way::Left]).chain([pad::Way::Down; 30])
        };
        // And across from the account rail at its left (Play at the foot, then each step up it)
        // into the main column's left-aligned controls, and up them.
        let from_rail = |ups: usize| {
            [pad::Way::Down; 40].into_iter().chain([pad::Way::Left; 8]).chain(std::iter::repeat_n(pad::Way::Up, ups)).chain([pad::Way::Right]).chain([pad::Way::Up; 30])
        };
        // And right from each stop down the right-hand side (a field's Browse button).
        let across = |depth: usize| {
            [pad::Way::Up; 40].into_iter().chain([pad::Way::Right; 6]).chain(std::iter::repeat_n(pad::Way::Down, depth)).chain([pad::Way::Right])
        };
        for way in (1..8).flat_map(sweep).chain((0..8).flat_map(from_rail)).chain((1..10).flat_map(across)) {
            pad::step(w, way);
            let _ = window.draw_if_needed(|_| {});
            sent.extend(press());
        }
        let reached = reached(&sent);
        assert_eq!(reached, all, "the walker");
    }

    /// The dialog's controls, by the message each sends.
    fn dialog_control(message: &Message) -> Option<&'static str> {
        match message {
            Message::FindInstallation => Some("find"),
            Message::Browse(PickTarget::GameFolder) => Some("browse"),
            Message::SelectInstallPath => Some("path"),
            Message::Navigate(Page::Maintenance) => Some("open downloads"),
            Message::ToggleSettings => Some("close"),
            Message::SaveSettings => Some("save"),
            _ => None,
        }
    }

    /// With the launcher settings dialog open, Tab and the controller reach its controls and
    /// nothing under it: the page, the title bar and the play bar are out of reach.
    #[test]
    fn the_settings_dialog_holds_the_keyboard_and_the_controller() {
        let window = headless(1440, 900);
        let mut app = downloads_app();
        app.settings.install_selected = true;
        app.troubleshooting_open = true;
        app.show_settings = true;
        let ui = page(&app);
        let w = ui.window();
        let _ = window.draw_if_needed(|_| {});
        assert!(ui.global::<Nav>().get_modal());
        // The dialog is 600 px wide in the middle of the window: every stop is inside it.
        let inside = |r: [f32; 4]| r[0] >= 420.0 && r[0] + r[2] <= 1020.0;
        let stops = pad::stop_rects(w);
        assert_eq!(stops.len(), 7, "close, path, Find game, Browse, Open Downloads, Close, Save settings: {stops:?}");
        assert!(stops.iter().all(|r| inside(*r)), "{stops:?}");
        assert!(pad::focus_rect(w).is_some_and(inside), "the dialog takes the focus when it opens");

        let tab: SharedString = Key::Tab.into();
        let mut sent = Vec::new();
        for _ in 0..20 {
            w.dispatch_event(WindowEvent::KeyPressed { text: tab.clone() });
            w.dispatch_event(WindowEvent::KeyReleased { text: tab.clone() });
            assert!(pad::focus_rect(w).is_some_and(inside), "Tab left the dialog");
            pad::press(w);
            sent.extend(take());
        }
        let ways = [pad::Way::Up, pad::Way::Right, pad::Way::Down, pad::Way::Left];
        for way in ways.iter().cycle().take(40).copied().chain([pad::Way::Down; 6]).chain([pad::Way::Left; 6]).chain([pad::Way::Up; 6]) {
            pad::step(w, way);
            assert!(pad::focus_rect(w).is_some_and(inside), "the walker left the dialog going {way:?}");
            pad::press(w);
            sent.extend(take());
        }
        let stray: Vec<_> = sent.iter().filter(|m| dialog_control(m).is_none()).collect();
        assert!(stray.is_empty(), "a control under the dialog was pressed: {stray:?}");
        let mut reached: Vec<_> = sent.iter().filter_map(dialog_control).collect();
        reached.sort_unstable();
        reached.dedup();
        assert_eq!(reached, ["browse", "close", "find", "open downloads", "path", "save"]);
    }

    /// The account sheet holds them too: every stop is on the sheet's panel at the right.
    #[test]
    fn the_account_sheet_holds_the_keyboard_and_the_controller() {
        let window = headless(1440, 900);
        let mut app = downloads_app();
        app.editor = Some(crate::Editor::blank());
        let ui = page(&app);
        let w = ui.window();
        let _ = window.draw_if_needed(|_| {});
        assert!(ui.global::<Nav>().get_modal());
        let on_panel = |r: [f32; 4]| r[0] >= 1440.0 - 464.0;
        let stops = pad::stop_rects(w);
        assert!(stops.len() > 5 && stops.iter().all(|r| on_panel(*r)), "{stops:?}");
        assert!(pad::focus_rect(w).is_some_and(on_panel), "the sheet takes the focus when it opens");
        let tab: SharedString = Key::Tab.into();
        for _ in 0..40 {
            w.dispatch_event(WindowEvent::KeyPressed { text: tab.clone() });
            w.dispatch_event(WindowEvent::KeyReleased { text: tab.clone() });
            assert!(pad::focus_rect(w).is_some_and(on_panel), "Tab left the sheet");
        }
        // Closed, the page is in reach again.
        app.editor = None;
        resync(&ui, &app);
        let _ = window.draw_if_needed(|_| {});
        assert!(!ui.global::<Nav>().get_modal());
        assert!(pad::stop_rects(w).iter().any(|r| !on_panel(*r)));
    }

    /// Steps the controller through the System cards window row by row, pressing each stop.
    fn walk_system_cards(window: &MinimalSoftwareWindow, w: &slint::Window) -> Vec<Message> {
        let mut sent = Vec::new();
        // Straight down the window; then down again with a step left after each, and with a step
        // right (a side step with nothing beside it lands in another row: the walker's rule).
        let pass = |side: Option<pad::Way>| [pad::Way::Up; 30].into_iter().chain((0..15).flat_map(move |_| [pad::Way::Down].into_iter().chain(side)));
        let ways = pass(None).chain(pass(Some(pad::Way::Left))).chain(pass(Some(pad::Way::Right)));
        for way in ways {
            pad::step(w, way);
            let _ = window.draw_if_needed(|_| {});
            pad::press(w);
            sent.extend(take());
        }
        sent
    }

    /// What a System cards message is, for an assertion's message (Message's Debug is redacted).
    fn names(sent: &[Message]) -> Vec<String> {
        let mut names: Vec<String> = sent
            .iter()
            .map(|m| match m {
                Message::Prereqs(msg) => format!("{msg:?}"),
                Message::SelfUpdate(msg) => format!("{msg:?}"),
                _ => "other".into(),
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    #[test]
    fn system_cards_carry_the_components_and_their_actions() {
        let window = headless(900, 1500);
        let mut app = downloads_app();
        // The cards are worked out only while Settings → System shows.
        app.page = Page::Settings;
        app.settings_tab = crate::settings_page::Tab::System;
        app.prereqs = crate::prereqs::model::preview_state("ok,confirm");
        app.self_update = UpdateState::Idle;
        let cards = SystemCards::new().unwrap();
        wire(&cards);
        sync(&cards, &app);
        cards.show().unwrap();
        let dl = cards.global::<Downloads>();
        let rows: Vec<PrereqCard> = dl.get_prereq_cards().iter().collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].confirm && rows[0].install.enabled && rows[0].dismiss.show);
        assert!(rows[1].button.show && !rows[1].running);
        dl.invoke_prereq_act(0, 2);
        dl.invoke_prereq_act(0, 1);
        dl.invoke_prereq_act(1, 0);
        dl.invoke_prereq_act(1, 3);
        dl.invoke_prereq_act(9, 0);
        dl.invoke_prereq_check_again();
        let sent = take();
        assert_eq!(sent.len(), 4, "no Cancel without a download, nothing for a missing card");
        use crate::prereqs::{Component, Msg};
        assert!(matches!(sent[0], Message::Prereqs(Msg::Install(Component::DirectPlay))));
        assert!(matches!(sent[1], Message::Prereqs(Msg::Dismiss)));
        assert!(matches!(sent[2], Message::Prereqs(Msg::CheckAgain)));
        assert!(matches!(sent[3], Message::Prereqs(Msg::CheckAgain)));

        // The controller reaches every control: the auto-check switch, the update card's button,
        // Check again, a card's button, Not now and Install.
        let w = cards.window();
        let sent = walk_system_cards(&window, w);
        let has = |want: fn(&Message) -> bool| sent.iter().any(want);
        assert!(has(|m| matches!(m, Message::SelfUpdate(UpdateMsg::AutoCheck(_)))), "auto-check: {:?}", names(&sent));
        assert!(dl.get_update_card().action.is_empty() || has(|m| matches!(m, Message::SelfUpdate(msg) if !matches!(msg, UpdateMsg::AutoCheck(_)))), "the update card's button: {:?}", names(&sent));
        assert!(has(|m| matches!(m, Message::Prereqs(Msg::Install(Component::DirectPlay)))), "Install: {:?}", names(&sent));
        assert!(has(|m| matches!(m, Message::Prereqs(Msg::Dismiss))), "Not now: {:?}", names(&sent));
        assert!(sent.iter().filter(|m| matches!(m, Message::Prereqs(Msg::CheckAgain))).count() >= 2, "Check again and the DirectX card's button: {:?}", names(&sent));

        // The preview's mix: DirectPlay off with its manual route, DirectX downloading.
        app.prereqs = crate::prereqs::model::preview_state("manual");
        sync(&cards, &app);
        let rows: Vec<PrereqCard> = dl.get_prereq_cards().iter().collect();
        assert!(rows[0].manual && rows[0].manual_open && rows[0].button.show && !rows[0].button.enabled, "one install at a time");
        assert!(rows[1].running && rows[1].cancel.show && (rows[1].progress - 0.44).abs() < 0.01);
        assert!(!dl.get_prereq_summary().check_again.enabled);
        dl.invoke_prereq_act(0, 0);
        dl.invoke_prereq_act(0, 4);
        dl.invoke_prereq_act(0, 5);
        dl.invoke_prereq_act(1, 3);
        let sent = take();
        assert_eq!(sent.len(), 3);
        assert!(matches!(sent[0], Message::Prereqs(Msg::ToggleManual)));
        assert!(matches!(sent[1], Message::Prereqs(Msg::OpenWindowsFeatures)));
        assert!(matches!(sent[2], Message::Prereqs(Msg::Cancel(Component::DirectX9))));
        // The manual toggle, Open Windows Features and Cancel are in the controller's reach.
        let _ = window.draw_if_needed(|_| {});
        let sent = walk_system_cards(&window, w);
        let has = |want: fn(&Message) -> bool| sent.iter().any(want);
        assert!(has(|m| matches!(m, Message::Prereqs(Msg::ToggleManual))), "the manual toggle: {:?}", names(&sent));
        assert!(has(|m| matches!(m, Message::Prereqs(Msg::OpenWindowsFeatures))), "Open Windows Features: {:?}", names(&sent));
        assert!(has(|m| matches!(m, Message::Prereqs(Msg::Cancel(Component::DirectX9)))), "Cancel: {:?}", names(&sent));

        if let Some(folder) = std::env::var_os("CRYSTAL_SLINT_SHOTS") {
            let folder = std::path::PathBuf::from(folder);
            // No focus ring left over from the walks above.
            cards.global::<Nav>().set_keyboard(false);
            let shoot = |name: &str| {
                window.request_redraw();
                let shot = cards.window().take_snapshot().expect("snapshot");
                let image = imagecrate::RgbaImage::from_raw(shot.width(), shot.height(), shot.as_bytes().to_vec()).expect("pixels");
                image.save(folder.join(name)).expect("save");
            };
            shoot("system-cards-dark.png");
            cards.global::<Theme>().set_light(true);
            shoot("system-cards-light.png");
            crate::i18n::set(Lang::Ja);
            app.prereqs = crate::prereqs::model::preview_state("confirm");
            sync(&cards, &app);
            shoot("system-cards-light-ja.png");
            cards.global::<Theme>().set_light(false);
            shoot("system-cards-dark-ja.png");
            crate::i18n::set(Lang::En);
        }
    }

    #[test]
    fn japanese_words_come_from_the_table() {
        let _window = headless(1440, 900);
        let mut app = downloads_app();
        crate::i18n::set(Lang::Ja);
        app.troubleshooting_open = true;
        let ui = page(&app);
        let strings = ui.global::<Downloads>().get_strings();
        crate::i18n::set(Lang::En);
        for (english, shown) in [("Downloads", &strings.title), ("Troubleshooting", &strings.troubleshooting), ("Repair files", &strings.repair_files), ("Run setup", &strings.run_setup), ("Launcher settings", &strings.settings_title)] {
            assert_ne!(shown.as_str(), english, "{english} has no Japanese entry");
        }
    }
}