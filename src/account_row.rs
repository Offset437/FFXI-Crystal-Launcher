//! What an account row says: its state in priority order and the line, progress, code field
//! and action each state shows (Home's rows and the account sheet's status).
use crate::backend::AccountMode;
use crate::i18n::{t, tf};
use crate::{App, Message};

/// A session that is signed in to PlayOnline and idle: its Play starts the game at once.
pub(crate) fn signed_in(row: &crate::AccountRow) -> bool {
    row.pid.is_some() && !row.is_playing() && !row.busy && !row.shutting_down && !row.game_start_pending()
}
/// What one account row shows: its state, the detail after it, an optional
/// progress bar, the inline one-time-password field and its action.
pub(crate) struct RowView {
    pub label: String,
    pub detail: String,
    pub progress: Option<f32>,
    pub otp: bool,
    /// The row's action: label, message, soft accent (true) or quiet.
    pub action: Option<(String, Message, bool)>,
}
/// The local clock time `at` happened, as "11:16".
fn clock_at(at: std::time::Instant) -> String {
    let ago = chrono::Duration::from_std(at.elapsed()).unwrap_or_default();
    (chrono::Local::now() - ago).format("%H:%M").to_string()
}
/// Whether the row is an account that needs a one-time code before it can sign in.
pub(crate) fn needs_code(app: &App, item: &crate::AccountRow) -> bool {
    item.account.mode == AccountMode::Retail
        && (app.settings.account_otp.contains(&item.key) || !item.otp.is_empty())
}
/// Which state an account row shows, in priority order (its colour needs only this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowState { Closing, Waiting, Updating, ViewerConfirm, Playing, Starting, SigningIn, SignedIn, Private, NeedsCode, Problem, Idle }
pub(crate) fn row_state(app: &App, item: &crate::AccountRow) -> RowState {
    if item.shutting_down || item.signing_out {
        RowState::Closing
    } else if app.is_waiting(&item.key) {
        RowState::Waiting
    } else if app.play_update_of(&item.key).is_some() {
        RowState::Updating
    } else if app.play_updates.viewer_confirm.as_ref().is_some_and(|(key, _)| *key == item.key) {
        RowState::ViewerConfirm
    } else if item.is_playing() {
        RowState::Playing
    } else if item.game_start_pending() {
        RowState::Starting
    } else if item.busy {
        RowState::SigningIn
    } else if signed_in(item) {
        RowState::SignedIn
    } else if item.account.mode == AccountMode::Private {
        RowState::Private
    } else if needs_code(app, item) && !item.busy && item.pid.is_none() {
        RowState::NeedsCode
    } else if item.problem {
        RowState::Problem
    } else {
        RowState::Idle
    }
}
pub(crate) fn row_view(app: &App, item: &crate::AccountRow) -> RowView {
    let idle = !item.busy && item.pid.is_none();
    let state = row_state(app, item);
    let view = |label: String, detail: String| RowView {
        label,
        detail,
        progress: None,
        otp: false,
        action: None,
    };
    // Maintenance's update of this row's game folder, after the row's own state.
    let watch = || crate::play_update::watch_line(app, &item.key);
    match state {
        RowState::Closing => view(item.status_text().into_owned(), String::new()),
        RowState::Waiting => {
            let mut row = view(app.wait_headline().into(), app.wait_detail());
            row.action = Some((t("Cancel").into(), Message::CancelWait(vec![item.key.clone()]), false));
            if let Some(update) = watch() {
                (row.detail, row.progress) = (update.headline, update.fraction);
                // Cancel next to the download's progress stops the download; the wait's own
                // Cancel is back once it is done.
                if let Some(generation) = update.cancel { row.action = Some((t("Cancel").into(), Message::CancelPlayUpdate(generation), false)); }
            }
            row
        }
        RowState::Updating => {
            let Some(update) = crate::play_update::line(app, &item.key) else { return view(t("Not signed in").into(), String::new()) };
            let mut row = view(update.headline, String::new());
            row.progress = update.fraction;
            row.action = update.cancel.map(|generation| (t("Cancel").to_owned(), Message::CancelPlayUpdate(generation), false));
            row
        }
        RowState::ViewerConfirm => {
            let mut row = view(item.status_text().into_owned(), String::new());
            row.action = Some((t("Sign out and update").into(), Message::ConfirmViewerUpdate, true));
            row
        }
        RowState::Playing => {
            let since = item
                .play_timing
                .as_ref()
                .map(|timing| tf("since {time}", &[("time", &clock_at(timing.started()))]))
                .unwrap_or_default();
            view(t("In game").into(), since)
        }
        RowState::Starting => {
            let step = if item.status.is_empty() { t("Signing in").into() } else { item.status_text().into_owned() };
            view(t("Starting").into(), step)
        }
        RowState::SigningIn => view(t("Signing in").into(), item.status_text().into_owned()),
        RowState::SignedIn => {
            let mut row = view(t("Signed in").into(), String::new());
            if item.can_play() && app.vault_ready && !app.launch_blocked() {
                row.action = Some((t("Play now").into(), Message::Launch(item.key.clone()), true));
            }
            if let Some(update) = watch() {
                (row.detail, row.progress) = (update.headline, update.fraction);
                if let Some(generation) = update.cancel { row.action = Some((t("Cancel").into(), Message::CancelPlayUpdate(generation), false)); }
            }
            row
        }
        RowState::Private => view(t("Private server").into(), t("client temporarily unavailable").into()),
        RowState::NeedsCode => {
            let mut row = view(
                t("Needs a one-time password").into(),
                if item.problem { item.status_text().into_owned() } else { String::new() },
            );
            row.otp = true;
            let can_sign_in = idle && app.vault_ready && !app.launch_blocked();
            if can_sign_in && !item.otp.trim().is_empty() {
                row.action = Some((t("Sign in").into(), Message::SignIn(item.key.clone()), true));
            }
            row
        }
        RowState::Problem => {
            let mut row = view(item.status_text().into_owned(), String::new());
            row.otp = idle;
            if item.log.is_some() && idle {
                row.action = Some((t("View log").into(), Message::OpenLog(item.key.clone()), false));
            }
            row
        }
        RowState::Idle => {
            let mut row = view(t("Not signed in").into(), watch().map(|update| update.headline).unwrap_or_default());
            if idle && app.vault_ready && !app.launch_blocked() && item.can_play() {
                row.action = Some((t("Sign in").into(), Message::SignIn(item.key.clone()), false));
            }
            row
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signed-in account plays at once, an OTP account asks for its code and signs in only
    /// once one is typed, an idle one offers a background sign-in.
    #[test]
    fn home_rows_show_each_state_with_its_action() {
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        let keys: Vec<String> = app.accounts.iter().map(|row| row.key.clone()).collect();
        app.accounts[0].pid = Some(7);
        app.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        app.settings.account_otp.insert(keys[1].clone());
        let signed = row_view(&app, &app.accounts[0]);
        assert!(signed_in(&app.accounts[0]));
        assert_eq!(signed.label, t("Signed in"));
        assert!(matches!(&signed.action, Some((_, Message::Launch(key), true)) if *key == keys[0]));
        let otp = row_view(&app, &app.accounts[1]);
        assert!(otp.otp && otp.action.is_none() && otp.label == t("Needs a one-time password"));
        app.accounts[1].otp = "123456".into();
        assert!(matches!(row_view(&app, &app.accounts[1]).action, Some((_, Message::SignIn(_), true))));
        let idle = row_view(&app, &app.accounts[2]);
        assert!(!idle.otp && matches!(idle.action, Some((_, Message::SignIn(_), false))));
        // In game is playing, not signed in.
        app.accounts[0].game_running = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
        assert!(app.accounts[0].is_playing() && !signed_in(&app.accounts[0]));
    }
}