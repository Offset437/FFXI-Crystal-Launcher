//! The Launcher updates card's status line and action as plain data the Slint page draws.
use super::{shared_status, Msg, State};
use crate::i18n::t;
use crate::App;

/// The card's action, comparable (Msg carries releases that are not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Check,
    Download,
    Apply,
    Postpone,
}

impl Action {
    pub(crate) fn msg(self) -> Msg {
        match self {
            Action::Check => Msg::Check,
            Action::Download => Msg::Download,
            Action::Apply => Msg::Apply,
            Action::Postpone => Msg::Postpone,
        }
    }
}

fn comparable(action: Option<(&'static str, Msg)>) -> Option<(&'static str, Action)> {
    action.and_then(|(label, msg)| {
        Some((label, match msg {
            Msg::Check => Action::Check,
            Msg::Download => Action::Download,
            Msg::Apply => Action::Apply,
            Msg::Postpone => Action::Postpone,
            _ => return None,
        }))
    })
}

/// The status line and the action button `view` shows.
pub(crate) fn status(app: &App) -> (String, Option<(&'static str, Action)>) {
    let (status, action) = match &app.self_update {
        State::Available(_) | State::Downloading | State::Applying => shared_status(app).unwrap_or_default(),
        State::Idle => (t("Launcher updates have not been checked.").into(), Some((t("Check for launcher updates"), Msg::Check))),
        State::Checking => (t("Checking for launcher updates…").into(), None),
        State::Current => (t("Crystal Launcher is up to date.").into(), Some((t("Check again"), Msg::Check))),
        State::NotPublished => (t("No launcher releases have been published yet.").into(), Some((t("Check again"), Msg::Check))),
        State::Ready(_) => super::model::ready_card(app),
        State::Error(error) => (error.clone(), Some((t("Try again"), Msg::Check))),
    };
    (status, comparable(action))
}
