//! The Windows components section as plain data: the words, tones and actions each card
//! shows, and the message each control sends.
use super::detect::{self, Component, Status};
use super::install::{self, Outcome};
use super::{phase_line, Job, Msg, State};
use crate::i18n::{t, tf};

/// A palette tone, named so each front end picks its own colour (ok_tone, warn_tone, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Sub,
    Ok,
    Warn,
    Accent,
    Danger,
}

/// The icons the section uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Glyph {
    Refresh,
    Check,
    Alert,
    Download,
    Wrench,
    Close,
    ExternalLink,
}

/// A card button: its words, icon, whether it is the filled one, and its message (`None`
/// disables it).
#[derive(Clone, Debug)]
pub(crate) struct Button {
    pub label: &'static str,
    pub glyph: Glyph,
    pub emphasis: bool,
    pub message: Option<Msg>,
}

/// The confirmation panel: exactly what will run.
#[derive(Clone, Debug)]
pub(crate) struct Confirm {
    pub heading: String,
    pub steps: Vec<&'static str>,
    pub command: &'static str,
    pub permission: &'static str,
    pub dismiss: Button,
    pub install: Button,
}

/// A running install's line and bar.
#[derive(Clone, Debug)]
pub(crate) struct Progress {
    pub head: String,
    pub detail: String,
    /// Known progress 0..1; `None` draws the full, faint bar.
    pub fraction: Option<f32>,
    pub cancel: Option<Button>,
}

/// DirectPlay's manual route: the toggle, and its steps while open.
#[derive(Clone, Debug)]
pub(crate) struct Manual {
    pub open: bool,
    pub toggle: &'static str,
    pub steps: [&'static str; 4],
    pub open_features: Button,
}

/// One component's card.
#[derive(Clone, Debug)]
pub(crate) struct Card {
    pub name: &'static str,
    pub why: &'static str,
    pub badge: (Glyph, Tone),
    pub chip: String,
    pub version: String,
    pub evidence: String,
    pub button: Option<Button>,
    pub confirm: Option<Confirm>,
    pub progress: Option<Progress>,
    pub result: Option<(Glyph, Tone, String)>,
    pub manual: Option<Manual>,
}

/// The section's header card.
#[derive(Clone, Debug)]
pub(crate) struct Summary {
    pub title: &'static str,
    pub glyph: Glyph,
    pub tone: Tone,
    pub summary: String,
    pub when: String,
    pub intro: &'static str,
    pub check_again: Button,
}

pub(crate) fn summary(state: &State) -> Summary {
    let missing = state.missing();
    let pending = state.pending();
    let (summary, tone, glyph) = if state.report.is_none() {
        (t("Checking this PC…").to_owned(), Tone::Sub, Glyph::Refresh)
    } else if !missing.is_empty() {
        let n = missing.len();
        let text = if n == 1 { t("1 component needs attention").to_owned() } else { tf("{count} components need attention", &[("count", &n)]) };
        (text, Tone::Warn, Glyph::Alert)
    } else if !pending.is_empty() {
        (t("Restart Windows to finish setting up").to_owned(), Tone::Accent, Glyph::Refresh)
    } else {
        (t("Everything FINAL FANTASY XI needs is installed").to_owned(), Tone::Ok, Glyph::Check)
    };
    let when = if state.checking {
        t("Checking…").to_owned()
    } else {
        state.checked_at.as_ref().map(|time| tf("Last checked at {time}", &[("time", time)])).unwrap_or_default()
    };
    Summary {
        title: t("Windows components"),
        glyph,
        tone,
        summary,
        when,
        intro: t("FINAL FANTASY XI is a 32-bit DirectX 8 game that relies on these parts of Windows. Checking is read-only: nothing is downloaded or changed until you press Install or Enable, and every download must be signed by Microsoft before it runs."),
        check_again: Button {
            label: t("Check again"),
            glyph: Glyph::Refresh,
            emphasis: false,
            message: (!state.checking && !state.busy()).then_some(Msg::CheckAgain),
        },
    }
}

pub(crate) fn cards(state: &State) -> Vec<Card> {
    Component::ALL.into_iter().map(|component| card(state, component)).collect()
}

fn card(state: &State, comp: Component) -> Card {
    let finding = state.report.as_ref().map(|r| r.get(comp));
    let status = state.status(comp);
    let job = &state.jobs[comp.index()];
    let running = matches!(job, Job::Running { .. });
    let (badge, chip) = match status {
        None => ((Glyph::Refresh, Tone::Sub), t("Checking…")),
        Some(Status::Installed { .. }) => ((Glyph::Check, Tone::Ok), if comp == Component::DirectPlay { t("Enabled") } else { t("Installed") }),
        Some(Status::Missing) if running => ((Glyph::Download, Tone::Accent), t("Installing")),
        Some(Status::Missing) => ((Glyph::Alert, Tone::Warn), if comp == Component::DirectPlay { t("Turned off") } else { t("Missing") }),
        Some(Status::RestartPending) => ((Glyph::Refresh, Tone::Accent), t("Restart required")),
    };
    let version = match status {
        Some(Status::Installed { version: Some(v) }) => detect::shown_version(comp, v),
        _ => String::new(),
    };
    // Worded now, so it follows a language change without a new check.
    let evidence = finding.map(|f| tf("Checked: {evidence}", &[("evidence", &f.evidence.text())])).unwrap_or_default();

    let busy_elsewhere = state.busy() && !running;
    let button = match status {
        Some(Status::Missing) if !running && state.confirm == Some(comp) => None,
        Some(Status::Missing) if !running => Some(Button {
            label: comp.verb(),
            glyph: if comp == Component::DirectPlay { Glyph::Wrench } else { Glyph::Download },
            emphasis: true,
            message: (!busy_elsewhere).then_some(Msg::Ask(comp)),
        }),
        Some(_) if !running => Some(Button { label: t("Re-check"), glyph: Glyph::Refresh, emphasis: false, message: (!state.checking).then_some(Msg::CheckAgain) }),
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
        let download = plan.download.is_some();
        Confirm {
            heading,
            steps: plan.steps,
            command: plan.command,
            permission,
            dismiss: Button { label: t("Not now"), glyph: Glyph::Close, emphasis: false, message: Some(Msg::Dismiss) },
            install: Button {
                label: if download { t("Download and install") } else { t("Enable DirectPlay") },
                glyph: if download { Glyph::Download } else { Glyph::Wrench },
                emphasis: true,
                message: (!busy_elsewhere).then_some(Msg::Install(comp)),
            },
        }
    });

    let progress = match job {
        Job::Running { phase, .. } => {
            let (head, detail, fraction) = phase_line(phase, comp);
            let cancel = phase.cancellable().then_some(Button { label: t("Cancel"), glyph: Glyph::Close, emphasis: false, message: Some(Msg::Cancel(comp)) });
            Some(Progress { head, detail, fraction, cancel })
        }
        _ => None,
    };

    let result = match job {
        Job::Finished(Ok(Outcome::Done)) => Some((
            Glyph::Check,
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
        Job::Finished(Err(error)) => Some((Glyph::Alert, Tone::Danger, error.clone())),
        Job::Cancelled => Some((Glyph::Close, Tone::Sub, t("Download cancelled. Nothing was installed.").into())),
        _ => None,
    }
    .filter(|_| state.confirm != Some(comp));

    let manual = (comp == Component::DirectPlay && status.is_some_and(Status::is_missing)).then(|| Manual {
        open: state.manual,
        toggle: t("How to enable it manually"),
        steps: [
            t("Open “Turn Windows features on or off” (the button below, or search for it in Start)."),
            t("Expand Legacy Components."),
            t("Tick DirectPlay and press OK."),
            t("Restart if Windows asks, then press Check again here."),
        ],
        open_features: Button { label: t("Open Windows Features"), glyph: Glyph::ExternalLink, emphasis: false, message: Some(Msg::OpenWindowsFeatures) },
    });

    Card { name: comp.name(), why: comp.why(), badge, chip: chip.to_owned(), version, evidence, button, confirm, progress, result, manual }
}

/// The design preview's synthetic states (prereqs::State::load_preview's flags), for the front
/// end's tests.
#[cfg(test)]
pub(crate) fn preview_state(flags: &str) -> State {
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
        elevated: false,
    };
    state.load_preview(flags);
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::detect::{Evidence, Finding, Report};

    fn state(statuses: [Status; 3]) -> State {
        let mut state = State {
            report: None,
            checking: false,
            generation: 0,
            checked_at: Some("14:02".into()),
            jobs: [Job::Idle, Job::Idle, Job::Idle],
            restart: [false; 3],
            confirm: None,
            manual: false,
            next_id: 1,
            preview: false,
            elevated: true,
        };
        let [a, b, c] = statuses;
        let f = |status| Finding { status, evidence: Evidence::VcNothing };
        state.report = Some(Report { findings: [f(a), f(b), f(c)] });
        state
    }

    /// Each card offers: Fix on a missing component, the
    /// confirmation's Install only from the panel, Cancel only while downloading.
    #[test]
    fn cards_carry_their_actions() {
        crate::i18n::set(crate::i18n::Lang::En);
        let ok = || Status::Installed { version: None };
        let mut s = state([Status::Missing, ok(), ok()]);
        let first = &cards(&s)[0];
        assert!(matches!(first.button.as_ref().and_then(|b| b.message.clone()), Some(Msg::Ask(Component::DirectPlay))));
        assert!(first.manual.is_some() && first.confirm.is_none());
        assert_eq!(first.chip, t("Turned off"));
        assert!(matches!(cards(&s)[1].button.as_ref().and_then(|b| b.message.clone()), Some(Msg::CheckAgain)));
        s.confirm = Some(Component::DirectPlay);
        let first = &cards(&s)[0];
        assert!(first.button.is_none(), "the panel replaces the button");
        let confirm = first.confirm.as_ref().expect("confirmation panel");
        assert!(matches!(confirm.install.message, Some(Msg::Install(Component::DirectPlay))));
        assert!(matches!(confirm.dismiss.message, Some(Msg::Dismiss)));
        s.confirm = None;
        s.jobs[1] = Job::Running { id: 1, phase: install::Phase::Downloading { done: 5_000_000, total: Some(10_000_000) } };
        let dx = &cards(&s)[1];
        let progress = dx.progress.as_ref().expect("progress");
        assert_eq!(progress.fraction, Some(0.5));
        assert!(matches!(progress.cancel.as_ref().and_then(|b| b.message.clone()), Some(Msg::Cancel(Component::DirectX9))));
        assert!(summary(&s).check_again.message.is_none(), "no re-check while installing");
        assert!(cards(&s)[0].button.as_ref().is_some_and(|b| b.message.is_none()), "one install at a time");
    }
}
