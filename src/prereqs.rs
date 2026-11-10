//! Windows components FINAL FANTASY XI needs (DirectPlay, DirectX 9, VC++ x86):
//! read-only detection at startup, a card per component on Settings → System
//! with a one-click fix, and a slim warning banner for Home and Downloads.
//!
//! Detection runs while `checking` is set, so it starts with
//! the app and re-runs after every install. Nothing is installed without an
//! explicit click and a confirmation that says what will run.
mod detect;
/// The System tab's component cards as plain data for the Slint front end.
pub(crate) mod front;
mod install;
pub(crate) mod model;
mod trust;

use crate::i18n::{t, tf};
pub(crate) use detect::Component;
pub(crate) use install::wait_exit_code;
pub(crate) use trust::win_verify_trust;
use detect::{Evidence, FeatureState, Finding, Report, Status};
use crate::effect::Effect;
use install::{Event, Outcome, Phase};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Job {
    Idle,
    Running { id: u64, phase: Phase },
    Finished(Result<Outcome, String>),
    Cancelled,
}

pub(crate) struct State {
    report: Option<Report>,
    checking: bool,
    generation: u64,
    checked_at: Option<String>,
    jobs: [Job; 3],
    /// Set when an install reported "restart required" this session.
    restart: [bool; 3],
    confirm: Option<Component>,
    manual: bool,
    next_id: u64,
    /// Design preview: synthetic states, no detection or installs.
    preview: bool,
    elevated: bool,
}

impl Default for State {
    fn default() -> Self {
        let mut state = Self {
            report: None,
            checking: true,
            generation: 0,
            checked_at: None,
            jobs: [Job::Idle, Job::Idle, Job::Idle],
            restart: [false; 3],
            confirm: None,
            manual: false,
            next_id: 1,
            preview: false,
            elevated: crate::win::process::is_elevated(),
        };
        if crate::preview::flags().is_some() {
            state.load_preview(&std::env::var("XI_PREVIEW_PREREQS").unwrap_or_default());
        }
        state
    }
}

impl State {
    /// XI_PREVIEW_PREREQS: comma-separated flags. Default (or `mix`): one
    /// missing, one installing with progress, one installed. `ok` = all
    /// installed, `live` = real detection, plus `confirm`, `manual`,
    /// `restart`, `failed` overlays.
    fn load_preview(&mut self, flags: &str) {
        let has = |f: &str| flags.split(',').any(|x| x.trim().eq_ignore_ascii_case(f));
        if has("live") {
            return;
        }
        self.preview = true;
        self.checking = false;
        self.checked_at = Some("14:02".into());
        let installed = |v: Option<&str>, evidence: Evidence| Finding {
            status: Status::Installed { version: v.map(str::to_owned) },
            evidence,
        };
        let feature = |state| Evidence::Feature { state, missing: Vec::new() };
        let dx_files = || vec!["d3dx9_43.dll".to_owned(), "xinput1_3.dll".to_owned()];
        let directplay = if has("ok") {
            installed(None, feature(FeatureState::On))
        } else {
            Finding { status: Status::Missing, evidence: feature(FeatureState::Off) }
        };
        let directx = if has("ok") {
            installed(Some("9.29.952.3111"), Evidence::FilesPresent(dx_files()))
        } else {
            Finding { status: Status::Missing, evidence: Evidence::FilesMissing(dx_files()) }
        };
        let vc = installed(Some("14.44.35211"), Evidence::VcRecordAndFiles);
        self.report = Some(Report { findings: [directplay, directx, vc] });
        if !has("ok") {
            self.jobs[Component::DirectX9.index()] = Job::Running {
                id: 0,
                phase: Phase::Downloading { done: 44_130_000, total: Some(100_275_120) },
            };
        }
        if has("confirm") {
            self.confirm = Some(Component::DirectPlay);
        }
        if has("manual") {
            self.manual = true;
        }
        if has("restart") {
            self.restart[Component::DirectPlay.index()] = true;
            self.jobs[Component::DirectPlay.index()] = Job::Finished(Ok(Outcome::RestartRequired));
        }
        if has("failed") {
            self.jobs[Component::DirectX9.index()] =
                Job::Finished(Err(t("Couldn't reach Microsoft's download server. Check your internet connection.").into()));
        }
    }

    /// The status to show: a pending restart from this session's install
    /// overrides a still-missing reading.
    fn status(&self, c: Component) -> Option<&Status> {
        let status = &self.report.as_ref()?.get(c).status;
        static PENDING: Status = Status::RestartPending;
        if self.restart[c.index()] && status.is_missing() {
            Some(&PENDING)
        } else {
            Some(status)
        }
    }
    fn missing(&self) -> Vec<Component> {
        Component::ALL.into_iter().filter(|c| self.status(*c).is_some_and(Status::is_missing)).collect()
    }
    fn pending(&self) -> Vec<Component> {
        Component::ALL
            .into_iter()
            .filter(|c| self.status(*c) == Some(&Status::RestartPending))
            .collect()
    }
    pub(crate) fn busy(&self) -> bool {
        self.jobs.iter().any(|j| matches!(j, Job::Running { .. }))
    }
    fn recheck(&mut self) {
        if self.preview {
            return;
        }
        self.checking = true;
        self.generation += 1;
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Detected(u64, Report),
    CheckAgain,
    Ask(Component),
    Dismiss,
    Install(Component),
    Cancel(Component),
    Job(Component, u64, Event),
    ToggleManual,
    OpenWindowsFeatures,
}

pub(crate) fn update(state: &mut State, msg: Msg) -> Effect<Msg> {
    match msg {
        Msg::Detected(generation, report) => {
            if generation == state.generation {
                // A component that now reads as present no longer needs its
                // restart note.
                for c in Component::ALL {
                    if !report.get(c).status.is_missing() && report.get(c).status != Status::RestartPending {
                        state.restart[c.index()] = false;
                    }
                }
                state.report = Some(report);
                state.checking = false;
                state.checked_at = Some(chrono::Local::now().format("%H:%M").to_string());
            }
        }
        Msg::CheckAgain => state.recheck(),
        Msg::Ask(c) => {
            if !matches!(state.jobs[c.index()], Job::Running { .. }) {
                state.confirm = Some(c);
            }
        }
        Msg::Dismiss => state.confirm = None,
        Msg::Install(c) => {
            // Only from the confirmation panel, and never twice at once.
            if state.confirm == Some(c) && !state.preview && !state.busy() {
                state.confirm = None;
                state.jobs[c.index()] = Job::Running {
                    id: state.next_id,
                    phase: if c == Component::DirectPlay {
                        Phase::Installing
                    } else {
                        Phase::Downloading { done: 0, total: None }
                    },
                };
                state.next_id += 1;
            }
        }
        Msg::Cancel(c) => {
            if let Job::Running { phase, .. } = &state.jobs[c.index()] {
                if phase.cancellable() {
                    // Stopping the job stops the download and removes the file.
                    state.jobs[c.index()] = Job::Cancelled;
                }
            }
        }
        Msg::Job(c, attempt, event) => {
            if let Job::Running { id, phase } = &mut state.jobs[c.index()] {
                if *id != attempt { return Effect::none(); }
                match event {
                    Event::Phase(next) => *phase = next,
                    Event::Finished(result) => {
                        if result == Ok(Outcome::RestartRequired) {
                            state.restart[c.index()] = true;
                        }
                        state.jobs[c.index()] = Job::Finished(result);
                        state.recheck();
                    }
                }
            }
        }
        Msg::ToggleManual => state.manual = !state.manual,
        Msg::OpenWindowsFeatures => {
            if !state.preview {
                let _ = std::process::Command::new(install::system32().join("OptionalFeatures.exe")).spawn();
            }
        }
    }
    Effect::none()
}

/// Background work the state wants running now, by identity: the host starts each one it is
/// not running yet and stops each one no longer listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Work {
    /// Detection for this generation.
    Detect(u64),
    /// This component's install job.
    Job(Component, u64),
}

/// The work to run now.
pub(crate) fn work(state: &State) -> Vec<Work> {
    if state.preview {
        return Vec::new();
    }
    let mut list = Vec::new();
    if state.checking {
        list.push(Work::Detect(state.generation));
    }
    for c in Component::ALL {
        if let Job::Running { id, .. } = state.jobs[c.index()] {
            list.push(Work::Job(c, id));
        }
    }
    list
}

/// `work`'s messages, in order.
pub(crate) fn run(work: Work) -> futures_util::stream::BoxStream<'static, Msg> {
    use futures_util::StreamExt;
    match work {
        Work::Detect(generation) => futures_util::stream::once(async move {
            let report = tokio::task::spawn_blocking(detect::detect)
                .await
                .unwrap_or_else(|_| detect::detect());
            Msg::Detected(generation, report)
        })
        .boxed(),
        Work::Job(c, id) => install::job(c).map(move |event| Msg::Job(c, id, event)).boxed(),
    }
}
fn bytes(value: u64) -> String {
    let mb = value as f64 / 1_000_000.0;
    if mb >= 10.0 { format!("{mb:.0} MB") } else { format!("{mb:.1} MB") }
}

fn phase_line(phase: &Phase, c: Component) -> (String, String, Option<f32>) {
    match phase {
        Phase::Downloading { done, total } => {
            let file = install::plan(c).download.map(|d| d.0).unwrap_or("installer");
            match total {
                Some(size) if *size > 0 => {
                    let f = (*done as f64 / *size as f64).clamp(0.0, 1.0) as f32;
                    (
                        tf("Downloading {file}", &[("file", &file)]),
                        tf(
                            "{done} of {total} · {percent}%",
                            &[("done", &bytes(*done)), ("total", &bytes(*size)), ("percent", &format!("{:.0}", f * 100.0))],
                        ),
                        Some(f),
                    )
                }
                _ => (
                    tf("Downloading {file}", &[("file", &file)]),
                    tf("{done} received", &[("done", &bytes(*done))]),
                    Some(0.0),
                ),
            }
        }
        Phase::Verifying => (
            t("Checking the Microsoft signature").into(),
            t("The installer runs only if it is validly signed by Microsoft.").into(),
            None,
        ),
        Phase::Extracting => (
            t("Unpacking the DirectX runtime").into(),
            t("Into a temporary folder that is removed afterwards.").into(),
            None,
        ),
        Phase::Installing => (
            match c {
                Component::DirectPlay => t("Enabling DirectPlay").into(),
                _ => tf("Installing the {name}", &[("name", &c.name())]),
            },
            t("This can take a minute. Windows may ask for permission.").into(),
            None,
        ),
    }
}
/// The Windows components banner's words: components are missing
/// (`restart` false) or a restart finishes their setup.
pub(crate) struct BannerText {
    pub restart: bool,
    pub lead: String,
    pub rest: String,
    pub button: &'static str,
}

/// How many components are ready, of how many (Home's first-run check); None until detected.
pub(crate) fn ready_count(state: &State) -> Option<(usize, usize)> {
    state.report.as_ref()?;
    let ready = Component::ALL.into_iter().filter(|c| matches!(state.status(*c), Some(Status::Installed { .. }))).count();
    Some((ready, Component::ALL.len()))
}

/// What the Windows components banner says, if it shows.
pub(crate) fn banner_text(state: &State) -> Option<BannerText> {
    let missing = state.missing();
    let pending = state.pending();
    if let Some(first) = missing.first() {
        let lead = match missing.len() {
            1 => first.name().to_owned(),
            2 => tf("{first} and {second}", &[("first", &missing[0].name()), ("second", &missing[1].name())]),
            _ => tf(
                "{first}, {second} and {third}",
                &[("first", &missing[0].name()), ("second", &missing[1].name()), ("third", &missing[2].name())],
            ),
        };
        let rest = if missing.len() == 1 {
            tf(" is missing — {consequence}", &[("consequence", &first.consequence())])
        } else if missing.iter().any(|m| m.critical()) {
            t(" are missing — FINAL FANTASY XI can't start without DirectPlay.").to_owned()
        } else {
            t(" are missing — parts of FINAL FANTASY XI won't work.").to_owned()
        };
        let button = if state.busy() { t("View") } else { t("Fix") };
        Some(BannerText { restart: false, lead, rest, button })
    } else {
        let first = pending.first()?;
        Some(BannerText {
            restart: true,
            lead: t("Restart Windows").to_owned(),
            rest: tf(" to finish setting up {name}.", &[("name", &first.name())]),
            button: t("Details"),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(statuses: [Status; 3]) -> State {
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
        let [a, b, c] = statuses;
        let f = |status| Finding { status, evidence: Evidence::VcNothing };
        state.report = Some(Report { findings: [f(a), f(b), f(c)] });
        state
    }
    fn ok() -> Status {
        Status::Installed { version: None }
    }
    fn missing() -> Status {
        Status::Missing
    }

    #[test]
    fn install_requires_confirmation() {
        let mut state = state_with([missing(), ok(), ok()]);
        let _ = update(&mut state, Msg::Install(Component::DirectPlay));
        assert_eq!(state.jobs[0], Job::Idle, "no install without the confirmation panel");
        let _ = update(&mut state, Msg::Ask(Component::DirectPlay));
        let _ = update(&mut state, Msg::Install(Component::DirectPlay));
        assert!(matches!(state.jobs[0], Job::Running { .. }));
        assert_eq!(state.confirm, None);
    }

    #[test]
    fn one_install_at_a_time() {
        let mut state = state_with([missing(), missing(), ok()]);
        let _ = update(&mut state, Msg::Ask(Component::DirectPlay));
        let _ = update(&mut state, Msg::Install(Component::DirectPlay));
        let _ = update(&mut state, Msg::Ask(Component::DirectX9));
        let _ = update(&mut state, Msg::Install(Component::DirectX9));
        assert_eq!(state.jobs[1], Job::Idle);
    }

    #[test]
    fn cancel_only_while_downloading() {
        let mut state = state_with([ok(), missing(), ok()]);
        state.jobs[1] = Job::Running { id: 1, phase: Phase::Installing };
        let _ = update(&mut state, Msg::Cancel(Component::DirectX9));
        assert!(matches!(state.jobs[1], Job::Running { .. }));
        state.jobs[1] = Job::Running { id: 1, phase: Phase::Downloading { done: 1, total: Some(2) } };
        let _ = update(&mut state, Msg::Cancel(Component::DirectX9));
        assert_eq!(state.jobs[1], Job::Cancelled);
    }

    #[test]
    fn finished_install_rechecks_and_tracks_restart() {
        let mut state = state_with([missing(), ok(), ok()]);
        state.jobs[0] = Job::Running { id: 1, phase: Phase::Installing };
        let _ = update(&mut state, Msg::Job(Component::DirectPlay, 1, Event::Finished(Ok(Outcome::RestartRequired))));
        assert!(state.checking);
        assert_eq!(state.generation, 1);
        // Still missing on disk until the restart: shown as restart pending.
        assert_eq!(state.status(Component::DirectPlay), Some(&Status::RestartPending));
        assert!(state.missing().is_empty());
        assert_eq!(state.pending(), vec![Component::DirectPlay]);
        // A stale detection result is ignored.
        let report = state.report.clone().unwrap();
        let _ = update(&mut state, Msg::Detected(0, report.clone()));
        assert!(state.checking);
        // Once it reads installed, the restart note clears.
        let mut fixed = report;
        fixed.findings[0].status = ok();
        let _ = update(&mut state, Msg::Detected(1, fixed));
        assert!(!state.checking);
        assert!(!state.restart[0]);
    }

    #[test]
    fn preview_mix_has_each_state() {
        let mut state = state_with([ok(), ok(), ok()]);
        state.load_preview("");
        assert!(state.status(Component::DirectPlay).is_some_and(Status::is_missing));
        assert!(matches!(state.jobs[1], Job::Running { .. }));
        assert!(matches!(state.status(Component::VcRedist), Some(Status::Installed { .. })));
        let mut all = state_with([ok(), ok(), ok()]);
        all.load_preview("ok");
        assert!(all.missing().is_empty());
    }

    #[test]
    fn delayed_job_events_do_not_finish_or_change_a_retried_download() {
        let mut state = state_with([ok(), missing(), ok()]);
        let component = Component::DirectX9;
        let _ = update(&mut state, Msg::Ask(component));
        let _ = update(&mut state, Msg::Install(component));
        let _ = update(&mut state, Msg::Cancel(component));
        let _ = update(&mut state, Msg::Ask(component));
        let _ = update(&mut state, Msg::Install(component));
        let retry = state.jobs[1].clone();
        let _ = update(&mut state, Msg::Job(component, 1, Event::Phase(Phase::Installing)));
        let _ = update(&mut state, Msg::Job(component, 1, Event::Finished(Ok(Outcome::Done))));
        assert_eq!(state.jobs[1], retry);
        assert!(!state.checking);
        let _ = update(&mut state, Msg::Job(component, 2, Event::Finished(Ok(Outcome::Done))));
        assert!(state.checking);
        assert!(!state.busy());
    }
}
