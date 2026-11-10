//! Settings → System's Windows components cards as plain data the Slint page draws: every line
//! and button, and the `Msg` each button sends.
use super::{detect, install, phase_line, Component, Job, Msg, State, Status};
use crate::i18n::{t, tf};
use install::Outcome;

/// A status colour (crate::palette::ok_tone / warn_tone / danger_tone, the accent, the muted ink).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Tone {
    #[default]
    Sub,
    Ok,
    Warn,
    Danger,
    Accent,
}

/// An icon the cards use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Glyph {
    #[default]
    Refresh,
    CheckCircle,
    AlertCircle,
    Download,
    Wrench,
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Button {
    pub label: &'static str,
    pub glyph: Glyph,
    pub primary: bool,
    pub enabled: bool,
    pub msg: MsgOf,
}

/// A button's message, comparable (Msg carries reports that are not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MsgOf {
    CheckAgain,
    Ask(Component),
    Dismiss,
    Install(Component),
    Cancel(Component),
    ToggleManual,
    OpenWindowsFeatures,
}

impl MsgOf {
    pub(crate) fn msg(self) -> Msg {
        match self {
            MsgOf::CheckAgain => Msg::CheckAgain,
            MsgOf::Ask(c) => Msg::Ask(c),
            MsgOf::Dismiss => Msg::Dismiss,
            MsgOf::Install(c) => Msg::Install(c),
            MsgOf::Cancel(c) => Msg::Cancel(c),
            MsgOf::ToggleManual => Msg::ToggleManual,
            MsgOf::OpenWindowsFeatures => Msg::OpenWindowsFeatures,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Confirm {
    pub heading: String,
    pub steps: Vec<&'static str>,
    pub command: &'static str,
    pub permission: &'static str,
    pub not_now: Button,
    pub confirm: Button,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Progress {
    pub head: String,
    pub detail: String,
    /// None: indeterminate (a full, faded bar).
    pub fraction: Option<f32>,
    pub cancel: Option<Button>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Card {
    pub name: &'static str,
    pub glyph: Glyph,
    pub tone: Tone,
    pub label: &'static str,
    pub version: Option<String>,
    pub why: &'static str,
    pub checked: Option<String>,
    pub button: Option<Button>,
    pub confirm: Option<Confirm>,
    pub progress: Option<Progress>,
    pub result: Option<(Glyph, Tone, String)>,
    /// DirectPlay's manual route: whether it is open.
    pub manual: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Front {
    pub summary: String,
    pub tone: Tone,
    pub glyph: Glyph,
    pub when: String,
    pub check_again: Button,
    pub cards: Vec<Card>,
}

/// The four steps of DirectPlay's manual route.
pub(crate) fn manual_steps() -> [&'static str; 4] {
    [
        t("Open “Turn Windows features on or off” (the button below, or search for it in Start)."),
        t("Expand Legacy Components."),
        t("Tick DirectPlay and press OK."),
        t("Restart if Windows asks, then press Check again here."),
    ]
}

pub(crate) fn front(state: &State) -> Front {
    let missing = state.missing();
    let pending = state.pending();
    let (summary, tone, glyph) = if state.report.is_none() {
        (t("Checking this PC…").to_owned(), Tone::Sub, Glyph::Refresh)
    } else if !missing.is_empty() {
        let n = missing.len();
        (
            if n == 1 { t("1 component needs attention").to_owned() } else { tf("{count} components need attention", &[("count", &n)]) },
            Tone::Warn,
            Glyph::AlertCircle,
        )
    } else if !pending.is_empty() {
        (t("Restart Windows to finish setting up").to_owned(), Tone::Accent, Glyph::Refresh)
    } else {
        (t("Everything FINAL FANTASY XI needs is installed").to_owned(), Tone::Ok, Glyph::CheckCircle)
    };
    let when = if state.checking {
        t("Checking…").to_owned()
    } else {
        state.checked_at.as_ref().map(|time| tf("Last checked at {time}", &[("time", time)])).unwrap_or_default()
    };
    Front {
        summary,
        tone,
        glyph,
        when,
        check_again: Button {
            label: t("Check again"),
            glyph: Glyph::Refresh,
            primary: false,
            enabled: !state.checking && !state.busy(),
            msg: MsgOf::CheckAgain,
        },
        cards: Component::ALL.into_iter().map(|comp| card(state, comp)).collect(),
    }
}

fn card(state: &State, comp: Component) -> Card {
    let finding = state.report.as_ref().map(|r| r.get(comp));
    let status = state.status(comp);
    let job = &state.jobs[comp.index()];
    let running = matches!(job, Job::Running { .. });
    let (glyph, tone, label) = match status {
        None => (Glyph::Refresh, Tone::Sub, t("Checking…")),
        Some(Status::Installed { .. }) => (Glyph::CheckCircle, Tone::Ok, if comp == Component::DirectPlay { t("Enabled") } else { t("Installed") }),
        Some(Status::Missing) if running => (Glyph::Download, Tone::Accent, t("Installing")),
        Some(Status::Missing) => (Glyph::AlertCircle, Tone::Warn, if comp == Component::DirectPlay { t("Turned off") } else { t("Missing") }),
        Some(Status::RestartPending) => (Glyph::Refresh, Tone::Accent, t("Restart required")),
    };
    let version = match status {
        Some(Status::Installed { version: Some(v) }) => Some(detect::shown_version(comp, v).to_string()),
        _ => None,
    };
    let busy_elsewhere = state.busy() && !running;
    let button = match status {
        Some(Status::Missing) if !running && state.confirm == Some(comp) => None,
        Some(Status::Missing) if !running => Some(Button {
            label: comp.verb(),
            glyph: if comp == Component::DirectPlay { Glyph::Wrench } else { Glyph::Download },
            primary: true,
            enabled: !busy_elsewhere,
            msg: MsgOf::Ask(comp),
        }),
        Some(_) if !running => Some(Button { label: t("Re-check"), glyph: Glyph::Refresh, primary: false, enabled: !state.checking, msg: MsgOf::CheckAgain }),
        _ => None,
    };
    let confirm = (state.confirm == Some(comp) && !running).then(|| {
        let plan = install::plan(comp);
        let heading = match plan.download {
            Some((file, size)) => tf(
                "{action} {name} — downloads {file} ({size})",
                &[("action", &comp.verb()), ("name", &comp.full_name()), ("file", &file), ("size", &size)],
            ),
            None => tf("{action} {name}", &[("action", &comp.verb()), ("name", &comp.full_name())]),
        };
        let permission = if busy_elsewhere {
            t("Another component is being installed. This can start when it finishes.")
        } else if state.elevated {
            t("Runs with the launcher's administrator rights. Close FINAL FANTASY XI first.")
        } else {
            t("Windows will ask for administrator permission. Close FINAL FANTASY XI first.")
        };
        Confirm {
            heading,
            steps: plan.steps.clone(),
            command: plan.command,
            permission,
            not_now: Button { label: t("Not now"), glyph: Glyph::Close, primary: false, enabled: true, msg: MsgOf::Dismiss },
            confirm: Button {
                label: if plan.download.is_some() { t("Download and install") } else { t("Enable DirectPlay") },
                glyph: if plan.download.is_some() { Glyph::Download } else { Glyph::Wrench },
                primary: true,
                enabled: !busy_elsewhere,
                msg: MsgOf::Install(comp),
            },
        }
    });
    let progress = match job {
        Job::Running { phase, .. } => {
            let (head, detail, fraction) = phase_line(phase, comp);
            Some(Progress {
                head,
                detail,
                fraction,
                cancel: phase.cancellable().then_some(Button {
                    label: t("Cancel"),
                    glyph: Glyph::Close,
                    primary: false,
                    enabled: true,
                    msg: MsgOf::Cancel(comp),
                }),
            })
        }
        _ => None,
    };
    let result = match job {
        Job::Finished(Ok(Outcome::Done)) => Some((
            Glyph::CheckCircle,
            Tone::Ok,
            match status {
                Some(Status::Installed { .. }) => tf("{name} is ready.", &[("name", &comp.name())]),
                _ => t("The installer finished, but the component still reads as missing. Restart Windows, then Check again.").into(),
            },
        )),
        Job::Finished(Ok(Outcome::RestartRequired)) => Some((
            Glyph::Refresh,
            Tone::Accent,
            tf("Restart Windows to finish setting up {name}, then press Check again.", &[("name", &comp.name())]),
        )),
        Job::Finished(Err(error)) => Some((Glyph::AlertCircle, Tone::Danger, error.clone())),
        Job::Cancelled => Some((Glyph::Close, Tone::Sub, t("Download cancelled. Nothing was installed.").into())),
        _ => None,
    }
    .filter(|_| state.confirm != Some(comp));
    Card {
        name: comp.name(),
        glyph,
        tone,
        label,
        version,
        why: comp.why(),
        checked: finding.map(|f| tf("Checked: {evidence}", &[("evidence", &f.evidence.text())])),
        button,
        confirm,
        progress,
        result,
        manual: (comp == Component::DirectPlay && status.is_some_and(Status::is_missing)).then_some(state.manual),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_mix_reads_as_the_cards_show_it() {
        let mut state = State {
            report: None,
            checking: false,
            generation: 0,
            checked_at: None,
            jobs: [Job::Idle, Job::Idle, Job::Idle],
            restart: [false; 3],
            confirm: None,
            manual: false,
            next_id: 1,
            preview: false,
            elevated: true,
        };
        state.load_preview("confirm,manual");
        let front = front(&state);
        assert_eq!(front.tone, Tone::Warn);
        assert_eq!(front.cards.len(), 3);
        let directplay = &front.cards[0];
        assert!(directplay.button.is_none(), "the confirmation replaces the button");
        let confirm = directplay.confirm.as_ref().unwrap();
        assert_eq!(confirm.confirm.msg, MsgOf::Install(Component::DirectPlay));
        assert!(!confirm.confirm.enabled, "another component is downloading");
        assert_eq!(directplay.manual, Some(true));
        let directx = &front.cards[1];
        let progress = directx.progress.as_ref().unwrap();
        assert!(progress.fraction.is_some_and(|f| f > 0.4 && f < 0.5));
        assert_eq!(progress.cancel.as_ref().unwrap().msg, MsgOf::Cancel(Component::DirectX9));
        assert!(front.cards[2].version.is_some());
        assert!(matches!(MsgOf::ToggleManual.msg(), Msg::ToggleManual));
    }
}
