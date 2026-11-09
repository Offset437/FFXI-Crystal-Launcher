//! Settings → Server: the admin side of a LandSandBoat server (see `server_admin`).
//!
//! Shown to everyone, but does nothing until a server's admin token is entered. Strings are
//! English like the rest of Crystal's private-server text.

use crate::backend::Config;
use crate::icons::Icon;
use crate::prereqs::{action, card, chip, divider, ok_tone, warn_tone};
use crate::server_admin::{self, GateChange, Login, Status};
use crate::ui::{colors, control, scale, text_field, Colors, Tone, BODY, SEMI};
use crate::{App, Message};
use iced::widget::{column, row, text, Space};
use iced::{Alignment, Element, Length, Subscription, Task};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
enum VerifyJob {
    Idle,
    Hashing { id: u64, done: u64, total: u64 },
    Uploading(String),
}

pub(crate) struct State {
    login: Option<Login>,
    host: String,
    token: String,
    status: Option<Status>,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    motd: String,
    verify: VerifyJob,
    verify_game: PathBuf,
    next_id: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            login: None,
            host: String::new(),
            token: String::new(),
            status: None,
            loading: false,
            error: None,
            notice: None,
            motd: String::new(),
            verify: VerifyJob::Idle,
            verify_game: PathBuf::new(),
            next_id: 1,
        }
    }
}

impl State {
    /// The saved server and token, if any. Unit tests never read the real data folder.
    pub(crate) fn load(config: &Config) -> Self {
        let mut state = Self::default();
        if cfg!(test) {
            return state;
        }
        if let Some(login) = server_admin::load_login(config) {
            state.host = login.host.clone();
            state.login = Some(login);
        }
        state
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    HostInput(String),
    TokenInput(String),
    Connect,
    Forget,
    Refresh,
    Loaded(Result<Status, String>),
    SetUpdateApproved(bool),
    SetMaintenance(bool),
    MotdInput(String),
    SaveMotd,
    ClearOverrides,
    Changed(Result<Status, String>),
    Xiloader(&'static str, Option<String>),
    XiloaderQueued(Result<(), String>),
    GenerateVerify,
    VerifyProgress(u64, u64, u64),
    VerifyBuilt(u64, Result<(String, Vec<u8>), String>),
    VerifyUploaded(Result<String, String>),
    /// Startup's manifest refresh for the update hold finished.
    ManifestsRefreshed,
}

fn sa(msg: Msg) -> Message {
    Message::ServerAdmin(msg)
}

/// Startup: read every known server's manifest again, so update holds are current.
pub(crate) fn refresh_manifests(config: &Config) -> Task<Message> {
    let experiment = config.experiment.clone();
    Task::perform(crate::server_manifest::refresh_known(experiment), |_| sa(Msg::ManifestsRefreshed))
}

fn fetch(login: &Login) -> Task<Message> {
    Task::perform(server_admin::fetch_status(login.clone()), |r| sa(Msg::Loaded(r)))
}

fn change(state: &mut State, change: GateChange) -> Task<Message> {
    let Some(login) = state.login.clone() else { return Task::none() };
    state.loading = true;
    state.notice = None;
    Task::perform(server_admin::change_gate(login, change), |r| sa(Msg::Changed(r)))
}

/// The FFXI folder a verification manifest is built from: the one selected in Crystal.
fn game_folder(app: &App) -> PathBuf {
    if app.settings.install_path.trim().is_empty() {
        app.config.game_directory()
    } else {
        PathBuf::from(&app.settings.install_path)
    }
}

pub(crate) fn update(app: &mut App, msg: Msg) -> Task<Message> {
    let verify_game = game_folder(app);
    let state = &mut app.server_admin;
    match msg {
        Msg::HostInput(value) => state.host = value.trim().to_owned(),
        Msg::TokenInput(value) => state.token = value.trim().to_owned(),
        Msg::Connect => {
            let token = if state.token.is_empty() {
                state.login.as_ref().map(|l| l.token.clone()).unwrap_or_default()
            } else {
                std::mem::take(&mut state.token)
            };
            if let Err(error) = server_admin::admin_url(&state.host, "status") {
                state.error = Some(error);
                return Task::none();
            }
            if token.len() < 32 {
                state.error = Some("Enter the admin token from launcher.ADMIN_TOKEN on the server (32 characters or more).".into());
                return Task::none();
            }
            let login = Login { host: state.host.clone(), token };
            if let Err(error) = server_admin::save_login(&app.config, &login) {
                state.error = Some(error);
                return Task::none();
            }
            state.login = Some(login.clone());
            state.loading = true;
            state.error = None;
            return fetch(&login);
        }
        Msg::Forget => {
            server_admin::forget_login(&app.config);
            *state = State::default();
        }
        Msg::Refresh => {
            if let Some(login) = state.login.clone() {
                state.loading = true;
                return fetch(&login);
            }
        }
        Msg::Loaded(result) | Msg::Changed(result) => {
            state.loading = false;
            match result {
                Ok(status) => {
                    state.motd = status.gate.motd.clone();
                    state.status = Some(status);
                    state.error = None;
                    // This PC's copy of the manifest follows at once (it holds this PC's updates too)
                    return refresh_manifests(&app.config);
                }
                Err(error) => state.error = Some(error),
            }
        }
        Msg::SetUpdateApproved(on) => {
            return change(state, GateChange { update_approved: Some(Some(on)), ..Default::default() });
        }
        Msg::SetMaintenance(on) => {
            return change(state, GateChange { maintenance: Some(Some(on)), ..Default::default() });
        }
        Msg::MotdInput(value) => state.motd = value,
        Msg::SaveMotd => {
            let motd = state.motd.trim().to_owned();
            return change(state, GateChange { motd: Some(Some(motd)), ..Default::default() });
        }
        Msg::ClearOverrides => {
            return change(state, GateChange { update_approved: Some(None), maintenance: Some(None), motd: Some(None) });
        }
        Msg::Xiloader(action, version) => {
            let Some(login) = state.login.clone() else { return Task::none() };
            state.loading = true;
            state.notice = None;
            return Task::perform(server_admin::xiloader_request(login, action, version), |r| sa(Msg::XiloaderQueued(r)));
        }
        Msg::XiloaderQueued(result) => {
            state.loading = false;
            match result {
                Ok(()) => {
                    state.notice = Some("Sent. The server switches xiloader within a few seconds; refresh to see it.".into());
                    if let Some(login) = state.login.clone() {
                        // The request runs on the server; show its result once it's had a moment
                        return Task::perform(
                            async move {
                                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                                server_admin::fetch_status(login).await
                            },
                            |r| sa(Msg::Loaded(r)),
                        );
                    }
                }
                Err(error) => state.error = Some(error),
            }
        }
        Msg::GenerateVerify => {
            if state.verify == VerifyJob::Idle && state.login.is_some() {
                state.verify = VerifyJob::Hashing { id: state.next_id, done: 0, total: 0 };
                state.verify_game = verify_game;
                state.next_id += 1;
                state.error = None;
                state.notice = None;
            }
        }
        Msg::VerifyProgress(id, done, total) => {
            if matches!(state.verify, VerifyJob::Hashing { id: running, .. } if running == id) {
                state.verify = VerifyJob::Hashing { id, done, total };
            }
        }
        Msg::VerifyBuilt(id, result) => {
            if !matches!(state.verify, VerifyJob::Hashing { id: running, .. } if running == id) {
                return Task::none();
            }
            match (result, state.login.clone()) {
                (Ok((version, json)), Some(login)) => {
                    state.verify = VerifyJob::Uploading(version.clone());
                    return Task::perform(server_admin::upload_verify(login, version.clone(), json), move |r| {
                        sa(Msg::VerifyUploaded(r.map(|_| version.clone())))
                    });
                }
                (Err(error), _) => {
                    state.verify = VerifyJob::Idle;
                    state.error = Some(error);
                }
                (Ok(_), None) => state.verify = VerifyJob::Idle,
            }
        }
        Msg::VerifyUploaded(result) => {
            state.verify = VerifyJob::Idle;
            match result {
                Ok(version) => {
                    state.notice = Some(format!("Uploaded the verification manifest for {version}."));
                    if let Some(login) = state.login.clone() {
                        return fetch(&login);
                    }
                }
                Err(error) => state.error = Some(error),
            }
        }
        Msg::ManifestsRefreshed => {}
    }
    Task::none()
}

/// Hashes the FFXI folder off the interface thread while a verification manifest is built.
pub(crate) fn subscription(state: &State) -> Subscription<Message> {
    let VerifyJob::Hashing { id, .. } = state.verify else { return Subscription::none() };
    Subscription::run_with((id, state.verify_game.clone()), |(id, game)| {
        let (id, game) = (*id, game.clone());
        iced::stream::channel(16, async move |mut out: iced::futures::channel::mpsc::Sender<Message>| {
            use iced::futures::SinkExt;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let work = tokio::task::spawn_blocking(move || {
                let mut shown = u64::MAX;
                server_admin::generate_verify(&game, |done, total| {
                    // A message per percent, not per megabyte
                    let percent = if total == 0 { 0 } else { done.saturating_mul(100) / total };
                    if percent != shown {
                        shown = percent;
                        let _ = tx.send((done, total));
                    }
                })
            });
            tokio::pin!(work);
            let result = loop {
                tokio::select! {
                    result = &mut work => break result.unwrap_or_else(|_| Err("Building the verification manifest stopped unexpectedly.".into())),
                    Some((done, total)) = rx.recv() => { let _ = out.send(sa(Msg::VerifyProgress(id, done, total))).await; }
                }
            };
            let _ = out.send(sa(Msg::VerifyBuilt(id, result))).await;
        })
    })
}

// ---- view ----

fn heading<'a>(c: Colors, title: &'a str, detail: &'a str) -> Element<'a, Message> {
    column![
        text(title).font(SEMI).size(15).color(c.ink),
        text(detail).font(BODY).size(12).color(c.sub),
    ]
    .spacing(4)
    .into()
}

fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

pub(crate) fn view(app: &App) -> Element<'_, Message> {
    let c = colors(app.settings.light_theme);
    let s = scale(app);
    let state = &app.server_admin;
    let connected = state.login.is_some();
    let busy = state.loading;

    // Connection
    let token_hint = if connected { "Saved — enter a new token to replace it" } else { "Admin token (launcher.ADMIN_TOKEN)" };
    let connection = column![
        heading(
            c,
            "Server admin",
            "Run your LandSandBoat server's launcher side from here: hold or approve FFXI updates, post a message, switch xiloader and publish the verification manifest. Needs the admin token set on the server as launcher.ADMIN_TOKEN.",
        ),
        row![
            text_field("Server, e.g. ffxi.example.com", &crate::privacy::mask(&state.host))
                .on_input_maybe(crate::privacy::editable().then_some(|v| sa(Msg::HostInput(v))))
                .width(Length::FillPortion(2)),
            text_field(token_hint, &state.token)
                .secure(true)
                .on_input(|v| sa(Msg::TokenInput(v)))
                .on_submit(sa(Msg::Connect))
                .width(Length::FillPortion(3)),
        ]
        .spacing(10),
        row![
            control(c, Tone::Primary, Some(Icon::Send), if connected { "Save and refresh" } else { "Connect" }, (!busy).then(|| sa(Msg::Connect)), None),
            control(c, Tone::Secondary, Some(Icon::Refresh), "Refresh", (connected && !busy).then(|| sa(Msg::Refresh)), None),
            Space::new().width(Length::Fill),
            control(c, Tone::DangerLink, Some(Icon::Trash), "Forget this server", connected.then(|| sa(Msg::Forget)), None),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    ]
    .spacing(12.0 * s);

    let mut body = column![connection].spacing(16.0 * s);
    if let Some(error) = &state.error {
        body = body.push(text(error.as_str()).font(BODY).size(12.5).color(warn_tone(c)));
    }
    if let Some(notice) = &state.notice {
        body = body.push(text(notice.as_str()).font(BODY).size(12.5).color(ok_tone(c)));
    }

    let Some(status) = &state.status else {
        if busy {
            body = body.push(text("Connecting…").font(BODY).size(12.5).color(c.sub));
        }
        return card(c, body, s);
    };
    let enabled = !busy;

    // Overview
    body = body.push(divider(c));
    body = body.push(
        row![
            chip(format!("{} online", status.players_online), ok_tone(c)),
            text(format!("Client {} · LSB {}", status.client_version, status.lsb_commit)).font(BODY).size(12.5).color(c.sub),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    );

    // Players: updates, maintenance, message
    let gate = &status.gate;
    let mut approve = crate::controller_nav::toggler(gate.update_approved)
        .label("Let players update FINAL FANTASY XI")
        .font(BODY)
        .text_size(13)
        .size(18);
    if enabled {
        approve = approve.on_toggle(|v| sa(Msg::SetUpdateApproved(v)));
    }
    let mut maintenance = crate::controller_nav::toggler(gate.maintenance)
        .label("Show the maintenance notice in launchers")
        .font(BODY)
        .text_size(13)
        .size(18);
    if enabled && !gate.login_maint_mode {
        maintenance = maintenance.on_toggle(|v| sa(Msg::SetMaintenance(v)));
    }
    let mut players = column![
        heading(c, "Players", "Changes reach every launcher on its next check; no restart or reload_settings."),
        approve,
        text("Turn this off when Square Enix patches and LSB doesn't support the new client yet. Crystal then holds its FFXI update for this server's players. Turn it back on once the server is updated.")
            .font(BODY)
            .size(12)
            .color(c.sub),
        maintenance,
    ]
    .spacing(10.0 * s);
    if gate.login_maint_mode {
        players = players.push(
            text("login.MAINT_MODE is on in the server's settings, so logins are locked and the notice stays on.")
                .font(BODY)
                .size(12)
                .color(warn_tone(c)),
        );
    }
    players = players.push(
        row![
            text_field("Message shown in launchers (empty for none)", &state.motd)
                .on_input(|v| sa(Msg::MotdInput(v)))
                .on_submit(sa(Msg::SaveMotd))
                .width(Length::Fill),
            control(c, Tone::Secondary, None, "Save message", (enabled && state.motd.trim() != gate.motd.trim()).then(|| sa(Msg::SaveMotd)), None),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    );
    if !gate.overridden.is_empty() {
        players = players.push(
            row![
                text(format!("Set from here: {}. Clearing goes back to settings/launcher.lua.", gate.overridden.join(", ")))
                    .font(BODY)
                    .size(12)
                    .color(c.sub)
                    .width(Length::Fill),
                control(c, Tone::Secondary, None, "Use server settings", enabled.then(|| sa(Msg::ClearOverrides)), None),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
    }
    body = body.push(divider(c)).push(players);

    // xiloader
    let loader = &status.xiloader;
    let live = match &loader.live {
        Some(live) => format!("Live: {} ({})", if live.version.is_empty() { "unknown version" } else { &live.version }, short(&live.sha256)),
        None => "Live: none (the server can't read its xiloader)".to_owned(),
    };
    let mut xiloader = column![
        heading(c, "xiloader", "New releases are staged by the server's daily check; players get one only once it's published."),
        text(live).font(BODY).size(13).color(c.ink),
    ]
    .spacing(10.0 * s);
    let can_request = enabled && !loader.request_pending;
    let mut actions = row![].spacing(10).align_y(Alignment::Center);
    for version in &loader.staged {
        actions = actions.push(control(
            c,
            Tone::Primary,
            Some(Icon::Download),
            format!("Publish {version}"),
            can_request.then(|| sa(Msg::Xiloader("promote", Some(version.clone())))),
            None,
        ));
    }
    if !loader.archived.is_empty() {
        actions = actions.push(control(c, Tone::Secondary, None, "Roll back", can_request.then(|| sa(Msg::Xiloader("rollback", None))), None));
    }
    if loader.staged.is_empty() && loader.archived.is_empty() {
        actions = actions.push(text("Nothing staged.").font(BODY).size(12.5).color(c.sub));
    }
    xiloader = xiloader.push(actions);
    if loader.request_pending {
        xiloader = xiloader.push(
            text("A request is waiting on the server. If it doesn't finish, check that xiloader-request.path is enabled there.")
                .font(BODY)
                .size(12)
                .color(warn_tone(c)),
        );
    }
    if let Some(held) = &loader.held {
        xiloader = xiloader.push(text(format!("Held back after a rollback: {held} and older.")).font(BODY).size(12).color(c.sub));
    }
    if let Some(result) = &loader.last_result {
        xiloader = xiloader.push(
            text(format!("Last request: {}", result.message))
                .font(BODY)
                .size(12)
                .color(if result.ok { c.sub } else { warn_tone(c) }),
        );
    }
    body = body.push(divider(c)).push(xiloader);

    // Verification manifest
    let present = status.verify.current_present;
    let mut verify = column![
        heading(c, "Client verification", "Built from this PC's FINAL FANTASY XI folder; it should be patched to exactly the version the server expects."),
        row![
            chip(
                if present { format!("{} ready", status.client_version) } else { format!("{} missing", status.client_version) },
                if present { ok_tone(c) } else { warn_tone(c) }
            ),
            crate::privacy::path(game_folder(app).display().to_string(), 12.0, BODY, c.sub),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    ]
    .spacing(10.0 * s);
    verify = verify.push(match &state.verify {
        VerifyJob::Idle => action(c, Icon::Send, if present { "Build and upload again" } else { "Build and upload" }, !present, enabled.then(|| sa(Msg::GenerateVerify))),
        VerifyJob::Hashing { done, total, .. } => {
            let percent = if *total == 0 { 0 } else { done.saturating_mul(100) / total };
            text(format!("Hashing game files… {percent}% (several minutes for a full install)")).font(BODY).size(12.5).color(c.accent).into()
        }
        VerifyJob::Uploading(version) => text(format!("Uploading {version}…")).font(BODY).size(12.5).color(c.accent).into(),
    });
    body = body.push(divider(c)).push(verify);

    card(c, body, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_shortened_for_display() {
        assert_eq!(short("68A1DE4112046AFF9F0E"), "68A1DE411204");
        assert_eq!(short("ab"), "ab");
    }
}
