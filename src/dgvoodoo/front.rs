//! The dgVoodoo card as plain data the Slint page draws.
use super::{mb, Job, VERSION};
use crate::i18n::{t, tf};
use crate::App;

/// The chip's colour: the accent while downloading, the Graphics tab's success tone once
/// installed, muted otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chip {
    Accent,
    Ok,
    Sub,
}

/// What the card's action row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Actions {
    /// Open dgVoodoo settings, and Remove (`remove` false while downloading).
    Installed { remove: bool },
    /// Turned on, the download failed and nothing is installed: Try again.
    Retry,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Front {
    pub on: bool,
    pub chip: String,
    pub chip_tone: Chip,
    /// Downloading: the line, its size detail and the fraction (0 while the size is unknown).
    pub progress: Option<(String, String, f32)>,
    pub error: Option<String>,
    pub actions: Option<Actions>,
    pub note: String,
}

pub(crate) fn front(app: &App) -> Front {
    let state = &app.dgvoodoo;
    let on = app.settings.dgvoodoo;
    let (chip, chip_tone) = match &state.job {
        Job::Running { done, total: Some(total), .. } if *total > 0 => {
            (tf("Downloading {percent}%", &[("percent", &(done.saturating_mul(100) / total))]), Chip::Accent)
        }
        Job::Running { .. } => (t("Downloading…").into(), Chip::Accent),
        _ if state.installed => (tf("Installed v{version}", &[("version", &VERSION)]), Chip::Ok),
        _ => (t("Not installed").into(), Chip::Sub),
    };
    let progress = match &state.job {
        Job::Running { done, total, .. } => {
            let fraction = total.filter(|t| *t > 0).map(|t| (*done as f64 / t as f64).clamp(0.0, 1.0) as f32);
            let detail = match total {
                Some(total) => tf("{done} of {total}", &[("done", &mb(*done)), ("total", &mb(*total))]),
                None => tf("{done} received", &[("done", &mb(*done))]),
            };
            Some((tf("Downloading dgVoodoo {version}", &[("version", &VERSION)]), detail, fraction.unwrap_or(0.0)))
        }
        _ => None,
    };
    let error = match &state.job {
        Job::Failed(error) => Some(error.clone()),
        _ => None,
    };
    let busy = matches!(state.job, Job::Running { .. });
    let failed_and_on = on && matches!(state.job, Job::Failed(_)) && !state.installed;
    let actions = if state.installed {
        Some(Actions::Installed { remove: !busy })
    } else if failed_and_on {
        Some(Actions::Retry)
    } else {
        None
    };
    Front {
        on,
        chip,
        chip_tone,
        progress,
        error,
        actions,
        note: tf("dgVoodoo {version} · settings are kept in Crystal's folder", &[("version", &VERSION)]),
    }
}
