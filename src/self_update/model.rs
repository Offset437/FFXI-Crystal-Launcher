//! The launcher-update banner and Settings card as plain data: the words and the one action
//! each shows, and the message it sends.
use super::{restart_action, shared_status, Msg, State};
use crate::i18n::{t, tf};
use crate::App;

/// A line and its optional button (label, message).
#[derive(Clone, Debug)]
pub(crate) struct Line {
    pub text: String,
    pub action: Option<(&'static str, Msg)>,
}

/// The strip under the title bar, when an update is found, downloading, ready or applying.
pub(crate) fn banner(app: &App) -> Option<Line> {
    let (text, action) = match &app.self_update {
        State::Ready(_) => (t("The launcher update is ready.").into(), restart_action(app)),
        _ => shared_status(app)?,
    };
    Some(Line { text, action })
}

/// The rail's pill beside the brand while the banner shows: a few words, and whether work is
/// under way (a spinner instead of the arrow).
pub(crate) fn pill(app: &App) -> Option<(String, bool)> {
    Some(match &app.self_update {
        State::Available(release) => (tf("{version} available", &[("version", &release.payload.version)]), false),
        State::Downloading => (t("Updating").into(), true),
        State::Ready(stage) => (tf("{version} ready", &[("version", &stage.release.payload.version)]), false),
        State::Applying => (t("Restarting").into(), true),
        _ => return None,
    })
}

/// The Launcher updates card's status line and button (Settings → System).
pub(crate) fn card(app: &App) -> Line {
    let (text, action) = match &app.self_update {
        State::Available(_) | State::Downloading | State::Applying => shared_status(app).unwrap_or_default(),
        State::Idle => (t("Launcher updates have not been checked.").into(), Some((t("Check for launcher updates"), Msg::Check))),
        State::Checking => (t("Checking for launcher updates…").into(), None),
        State::Current => (t("Crystal Launcher is up to date.").into(), Some((t("Check again"), Msg::Check))),
        State::NotPublished => (t("No launcher releases have been published yet.").into(), Some((t("Check again"), Msg::Check))),
        State::Ready(stage) => (
            tf("Crystal Launcher {version} is ready to install.", &[("version", &stage.release.payload.version)]),
            restart_action(app),
        ),
        State::Error(error) => (error.clone(), Some((t("Try again"), Msg::Check))),
    };
    Line { text, action }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_and_banner_follow_the_state() {
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        app.self_update = State::Idle;
        assert!(banner(&app).is_none());
        let line = card(&app);
        assert!(matches!(line.action, Some((_, Msg::Check))));
        app.self_update = State::Downloading;
        let strip = banner(&app).expect("banner while downloading");
        assert!(strip.action.is_none());
        assert_eq!(strip.text, card(&app).text);
        app.self_update = State::Error("network down".into());
        assert!(banner(&app).is_none());
        assert_eq!(card(&app).text, "network down");
    }
}
