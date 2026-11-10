//! The Graphics and Sound & interface tabs as plain data the Slint page draws: every choice,
//! label and help line, and the `Msg` each control sends.
use super::{model, BackupEntry, Msg, Region, Section, State, Tone};
use crate::i18n::{t, tf};

/// What a dropdown entry sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Pick {
    Set(&'static str, u32),
    Window(u32, u32),
    Menu(u32, u32),
    Background(u32, u32),
}

impl Pick {
    pub(crate) fn msg(self) -> Msg {
        match self {
            Pick::Set(key, value) => Msg::Set(key, value),
            Pick::Window(w, h) => Msg::Window(w, h),
            Pick::Menu(w, h) => Msg::Menu(w, h),
            Pick::Background(w, h) => Msg::Background(w, h),
        }
    }
}

/// What a slider sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Slide {
    Gamma,
    Dword(&'static str),
}

impl Slide {
    pub(crate) fn msg(self, value: f32) -> Msg {
        match self {
            Slide::Gamma => Msg::Gamma(value),
            Slide::Dword(key) => Msg::Set(key, value.round().max(0.0) as u32),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Control {
    Choice { labels: Vec<String>, current: usize, picks: Vec<Pick> },
    /// A switch: on sends `Set(key, 1)`, off `Set(key, 0)`.
    Switch { on: bool, key: &'static str },
    /// `reset` is the value a double-click restores.
    Slider { target: Slide, value: f32, min: f32, max: f32, step: f32, reset: Option<f32>, shown: String },
    Info(&'static str),
    /// The screenshot folder: its (privacy-masked) value, whether it takes typing, and
    /// whether "Use default" shows.
    Folder { shown: String, editable: bool, filled: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub label: &'static str,
    pub help: &'static str,
    pub changed: bool,
    pub control: Control,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Card {
    pub title: &'static str,
    pub subtitle: &'static str,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Front {
    pub loaded: bool,
    /// The regions' labels and which one is shown.
    pub regions: Vec<(String, Region)>,
    pub region: usize,
    pub pending: usize,
    pub writable: bool,
    pub backups: Vec<BackupEntry>,
    pub location: String,
    pub notice: Option<(Tone, String)>,
    pub left: Vec<Card>,
    pub right: Vec<Card>,
}

impl Front {
    /// A region switch is refused while changes are pending (page.rs's `locked`).
    pub(crate) fn locked(&self) -> bool {
        self.pending > 0
    }
}

pub(crate) fn front(st: &State, section: Section) -> Front {
    let (left, right) = if !st.loaded {
        (Vec::new(), Vec::new())
    } else {
        match section {
            Section::Graphics => (vec![display_card(st), performance_card(st)], vec![quality_card(st)]),
            Section::Sound => (vec![sound_card(st), screenshot_card(st)], vec![interface_card(st)]),
        }
    };
    Front {
        loaded: st.loaded,
        regions: st.regions.iter().map(|region| (region.target_label(), *region)).collect(),
        region: st.regions.iter().position(|region| *region == st.region).unwrap_or(0),
        pending: st.pending().len(),
        writable: st.writable,
        backups: st.backups.clone(),
        location: tf("{path} · read by the official client, Windower and Ashita", &[("path", &st.region.location())]),
        notice: st.notice.clone(),
        left,
        right,
    }
}

fn choice<T: PartialEq + Copy>(options: Vec<Choice<T>>, selected: T, pick: impl Fn(T) -> Pick) -> Control {
    Control::Choice {
        current: options.iter().position(|o| o.value == selected).unwrap_or(0),
        labels: options.iter().map(|o| o.label.clone()).collect(),
        picks: options.iter().map(|o| pick(o.value)).collect(),
    }
}

/// page.rs's enum_dropdown: the table, plus "Other (n)" for a value it does not name.
fn enum_choice(st: &State, key: &'static str, table: &[(u32, &str)]) -> Control {
    let current = st.dword(key);
    let mut options: Vec<Choice<u32>> = table.iter().map(|(v, l)| Choice { value: *v, label: (*l).to_owned() }).collect();
    if !options.iter().any(|o| o.value == current) {
        options.push(Choice { value: current, label: tf("Other ({value})", &[("value", &current)]) });
    }
    choice(options, current, move |v| Pick::Set(key, v))
}

fn setting(label: &'static str, help: &'static str, changed: bool, control: Control) -> Row {
    Row { label, help, changed, control }
}

fn toggle(st: &State, label: &'static str, help: &'static str, key: &'static str) -> Row {
    setting(label, help, st.changed(&[key]), Control::Switch { on: st.dword(key) != 0, key })
}

fn display_card(st: &State) -> Card {
    let window = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let menu = (st.dword(model::MENU_W), st.dword(model::MENU_H));
    Card {
        title: t("Display"),
        subtitle: t("The game window and how large the interface is drawn."),
        rows: vec![
            setting(t("Window mode"), t("How the game window sits on your desktop."), st.changed(&[model::WINDOW_MODE]),
                enum_choice(st, model::WINDOW_MODE, &model::window_modes())),
            setting(t("Window resolution"), t("The size the game renders at. Your monitor's modes are listed."),
                st.changed(&[model::WINDOW_W, model::WINDOW_H]), choice(window_choices(st), window, |(w, h)| Pick::Window(w, h))),
            setting(t("Menu resolution"), t("The size menus are laid out at. A smaller size makes the interface larger."),
                st.changed(&[model::MENU_W, model::MENU_H]), choice(menu_choices(st), menu, |(w, h)| Pick::Menu(w, h))),
            toggle(st, t("Maintain aspect ratio"), t("Keep the picture's proportions instead of stretching it to the window."), model::KEEP_ASPECT),
        ],
    }
}

fn quality_card(st: &State) -> Card {
    let background = (st.dword(model::BACKGROUND_W), st.dword(model::BACKGROUND_H));
    Card {
        title: t("Image quality"),
        subtitle: t("Texture and scenery detail. Higher settings use more video memory."),
        rows: vec![
            setting(t("Background resolution"), t("Detail of the surrounding scenery. 4096 suits most modern graphics cards."),
                st.changed(&[model::BACKGROUND_W, model::BACKGROUND_H]),
                choice(background_choices(st), background, |(w, h)| Pick::Background(w, h))),
            setting(t("Mip mapping"), t("Smooths textures in the distance. Higher levels look cleaner."), st.changed(&[model::MIP_MAPPING]),
                enum_choice(st, model::MIP_MAPPING, &model::mip_levels())),
            setting(t("Texture compression"), t("Uncompressed textures are the sharpest."), st.changed(&[model::TEXTURE_COMPRESSION]),
                enum_choice(st, model::TEXTURE_COMPRESSION, &model::texture_levels())),
            setting(t("Map compression"), t("Uncompressed maps are easier to read."), st.changed(&[model::MAP_COMPRESSION]),
                enum_choice(st, model::MAP_COMPRESSION, &model::map_levels())),
            setting(t("Environment animation"), t("Moving water, foliage and other scenery."), st.changed(&[model::ENV_ANIMATION]),
                enum_choice(st, model::ENV_ANIMATION, &model::animation_levels())),
            toggle(st, t("Bump mapping"), t("Surface relief on terrain, walls and armor."), model::BUMP_MAPPING),
        ],
    }
}

fn performance_card(st: &State) -> Card {
    let gamma = model::gamma_of(st.dword(model::GAMMA));
    Card {
        title: t("Brightness & performance"),
        subtitle: t("Picture brightness and options for smoother play."),
        rows: vec![
            setting(t("Gamma"), t("Overall brightness. 0 is the game's default; double-click resets."), st.changed(&[model::GAMMA]),
                Control::Slider {
                    target: Slide::Gamma,
                    value: gamma,
                    min: -1.0,
                    max: 1.0,
                    step: 0.05,
                    reset: Some(0.0),
                    shown: if gamma.abs() < 0.005 { "0".into() } else { format!("{gamma:+.2}") },
                }),
            toggle(st, t("Graphics stabilization"), t("A compatibility option for drivers that flicker or stutter. Usually off."), model::STABILIZATION),
            setting(t("Frame rate"), t("The game has no frame-rate option; Windower and Ashita set it in their own settings."), false,
                Control::Info(t("Set by add-ons"))),
        ],
    }
}

fn sound_card(st: &State) -> Card {
    let max = st.dword(model::MAX_SOUNDS).clamp(model::MAX_SOUNDS_RANGE.0, model::MAX_SOUNDS_RANGE.1);
    Card {
        title: t("Sound"),
        subtitle: t("Music, effects and when the game plays them."),
        rows: vec![
            toggle(st, t("Sound"), t("All game music and sound effects."), model::SOUND),
            setting(t("Maximum sounds"), t("How many effects can play at once, from 12 to 20."), st.changed(&[model::MAX_SOUNDS]),
                Control::Slider {
                    target: Slide::Dword(model::MAX_SOUNDS),
                    value: max as f32,
                    min: model::MAX_SOUNDS_RANGE.0 as f32,
                    max: model::MAX_SOUNDS_RANGE.1 as f32,
                    step: 1.0,
                    reset: None,
                    shown: max.to_string(),
                }),
            toggle(st, t("Play sound in the background"), t("Keep the audio on while another window is in front."), model::SOUND_UNFOCUSED),
        ],
    }
}

fn interface_card(st: &State) -> Card {
    Card {
        title: t("Interface"),
        subtitle: t("The cursor, text and what you see as the game starts."),
        rows: vec![
            toggle(st, t("Hardware mouse cursor"), t("Use the Windows cursor, which stays smooth at any frame rate."), model::HARDWARE_MOUSE),
            setting(t("Font type"), t("How on-screen text is drawn. High quality is the sharpest."), st.changed(&[model::FONT_TYPE]),
                enum_choice(st, model::FONT_TYPE, &model::font_types())),
            toggle(st, t("Opening movie"), t("Play the introduction movie when the game starts."), model::OPENING_MOVIE),
            toggle(st, t("Simplified character creation"), t("Lighter visuals on the character creation screen."), model::SIMPLE_CREATION),
        ],
    }
}

fn screenshot_card(st: &State) -> Card {
    let dir = st.text(model::SCREENSHOT_DIR);
    Card {
        title: t("Screenshots"),
        subtitle: t("Where screenshots go and how large they are."),
        rows: vec![
            setting(t("Screenshot folder"), t("Where screenshots are saved. Leave empty to use the game's default."),
                st.changed(&[model::SCREENSHOT_DIR]),
                Control::Folder { shown: crate::privacy::mask_path(dir), editable: crate::privacy::editable(), filled: !dir.is_empty() }),
            toggle(st, t("Save at display resolution"), t("Capture at the window's full size rather than the game's base size."), model::SCREENSHOT_SCREEN_RES),
        ],
    }
}

/// A labelled choice for a dropdown.
#[derive(Clone, Debug)]
pub(crate) struct Choice<T> {
    pub(super) value: T,
    pub(super) label: String,
}

impl<T: PartialEq> PartialEq for Choice<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> std::fmt::Display for Choice<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

// ---- resolution lists ----

fn resolution_label(w: u32, h: u32) -> String {
    format!("{w} × {h}")
}

fn window_choices(st: &State) -> Vec<Choice<(u32, u32)>> {
    let current = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let mut list: Vec<Choice<(u32, u32)>> = st
        .modes
        .iter()
        .map(|&(w, h)| Choice {
            value: (w, h),
            label: if (w, h) == st.desktop {
                tf("{resolution}   ·  desktop", &[("resolution", &resolution_label(w, h))])
            } else {
                resolution_label(w, h)
            },
        })
        .collect();
    if !list.iter().any(|c| c.value == current) {
        list.insert(
            0,
            Choice {
                value: current,
                label: tf("{resolution}   ·  current", &[("resolution", &resolution_label(current.0, current.1))]),
            },
        );
    }
    list
}

fn menu_choices(st: &State) -> Vec<Choice<(u32, u32)>> {
    let (w, h) = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let current = (st.dword(model::MENU_W), st.dword(model::MENU_H));
    let mut list: Vec<Choice<(u32, u32)>> = vec![Choice { value: (0, 0), label: t("Automatic · use launch profile").into() }];
    for scale in model::MENU_SCALES {
        let (mw, mh) = model::menu_for(w, h, scale);
        if mw < model::MIN_MENU.0 || mh < model::MIN_MENU.1 || list.iter().any(|c| c.value == (mw, mh)) {
            continue;
        }
        list.push(Choice {
            value: (mw, mh),
            label: tf(
                "{resolution}  ·  interface {scale}×",
                &[("resolution", &resolution_label(mw, mh)), ("scale", &trim_float(scale))],
            ),
        });
    }
    if !list.iter().any(|c| c.value == current) {
        let scale = model::scale_of(h, current.1);
        list.insert(
            0,
            Choice {
                value: current,
                label: tf(
                    "{resolution}  ·  interface {scale}×",
                    &[
                        ("resolution", &resolution_label(current.0, current.1)),
                        ("scale", &trim_float((scale * 100.0).round() / 100.0)),
                    ],
                ),
            },
        );
    }
    list
}

fn trim_float(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn background_choices(st: &State) -> Vec<Choice<(u32, u32)>> {
    let current = (st.dword(model::BACKGROUND_W), st.dword(model::BACKGROUND_H));
    let window = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let mut list = vec![
        Choice {
            value: (1024, 1024),
            label: t("1024 × 1024  ·  low").into(),
        },
        Choice {
            value: (2048, 2048),
            label: t("2048 × 2048  ·  classic").into(),
        },
        Choice {
            value: (4096, 4096),
            label: t("4096 × 4096  ·  recommended").into(),
        },
    ];
    if !list.iter().any(|c| c.value == window) {
        list.push(Choice {
            value: window,
            label: tf("{resolution}  ·  match window", &[("resolution", &resolution_label(window.0, window.1))]),
        });
    }
    if !list.iter().any(|c| c.value == current) {
        list.insert(
            0,
            Choice {
                value: current,
                label: tf("{resolution}  ·  current", &[("resolution", &resolution_label(current.0, current.1))]),
            },
        );
    }
    list
}


/// The design preview's synthetic options (nothing on the machine is read), for tests.
#[cfg(test)]
pub(crate) fn preview_state() -> State {
    let mut s = State { preview: true, ..State::default() };
    s.load();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> State {
        preview_state()
    }

    #[test]
    fn the_front_lists_what_page_rs_shows_and_its_actions_are_page_rs_messages() {
        let mut s = loaded();
        let front = front(&s, Section::Graphics);
        assert!(front.loaded);
        assert_eq!(front.left.len(), 2);
        assert_eq!(front.right.len(), 1);
        assert_eq!(front.pending, 2, "the preview starts with two pending edits");
        let display = &front.left[0];
        assert_eq!(display.rows.len(), 4);
        let Control::Choice { labels, current, picks } = &display.rows[1].control else { panic!("window resolution is a choice") };
        assert_eq!(labels.len(), picks.len());
        assert!(matches!(picks[*current], Pick::Window(2560, 1440)));
        // Picking an entry is the message page.rs sends for it.
        let _ = super::super::update(&mut s, picks[0].msg());
        let Control::Switch { key, .. } = display.rows[3].control else { panic!("aspect ratio is a switch") };
        assert_eq!(key, model::KEEP_ASPECT);
        let sound = front_of(&s, Section::Sound);
        let Control::Slider { target, shown, .. } = &sound.left[0].rows[1].control else { panic!("maximum sounds is a slider") };
        assert_eq!(*target, Slide::Dword(model::MAX_SOUNDS));
        assert_eq!(shown, "20");
        assert!(matches!(Slide::Gamma.msg(0.25), Msg::Gamma(g) if (g - 0.25).abs() < 1e-6));
    }

    fn front_of(s: &State, section: Section) -> Front {
        front(s, section)
    }
}
