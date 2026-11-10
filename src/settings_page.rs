//! The Settings page's tabs: the FFXI game options, the controller setup and the system
//! components check. Each tab's module owns its own state and messages.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Tab {
    #[default]
    Graphics,
    Sound,
    Controller,
    System,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Graphics, Tab::Sound, Tab::Controller, Tab::System];
    /// The suffix used by `--preview-settings-<flag>` and the preview filename.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Tab::Graphics => "graphics",
            Tab::Sound => "sound",
            Tab::Controller => "controller",
            Tab::System => "system",
        }
    }
    pub(crate) fn from_flag(flag: &str) -> Option<Tab> {
        Tab::ALL.into_iter().find(|t| t.flag() == flag)
    }
}

