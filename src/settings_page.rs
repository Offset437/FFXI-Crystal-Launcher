//! The Settings page: a tab strip over the FFXI game options, the controller
//! setup and the system components check. Each tab's module owns its own
//! state, messages and view.
use crate::i18n::{self, t, Lang};
use crate::ui::{colors, profile_menu_style, profile_pick_style, scale, BODY, RADIUS_INNER};
use crate::{App, Message};
use iced::widget::{button, column, container, pick_list, row, scrollable, text, Space};
use iced::{Alignment, Border, Element, Length};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Tab {
    #[default]
    Graphics,
    Sound,
    Controller,
    System,
    Server,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Graphics, Tab::Sound, Tab::Controller, Tab::System, Tab::Server];
    fn label(self) -> &'static str {
        match self {
            Tab::Graphics => t("Graphics"),
            Tab::Sound => t("Sound & interface"),
            Tab::Controller => t("Controller"),
            Tab::System => t("System"),
            // English only, like the private-server text it belongs with
            Tab::Server => "Server",
        }
    }
    /// The suffix used by `--preview-settings-<flag>` and the preview filename.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Tab::Graphics => "graphics",
            Tab::Sound => "sound",
            Tab::Controller => "controller",
            Tab::System => "system",
            Tab::Server => "server",
        }
    }
    pub(crate) fn from_flag(flag: &str) -> Option<Tab> {
        Tab::ALL.into_iter().find(|t| t.flag() == flag)
    }
}

pub(crate) fn view(app: &App) -> Element<'_, Message> {
    let c = colors(app.settings.light_theme);
    let s = scale(app);
    let mut tabs = row![].spacing(28.0 * s).align_y(Alignment::End);
    for tab in Tab::ALL {
        let active = app.settings_tab == tab;
        tabs = tabs.push(
            column![
                crate::controller_nav::button(
                    text(tab.label())
                        .font(BODY)
                        .size(14.0 * s)
                        .color(if active { c.accent } else { c.sub })
                )
                .padding([6.0 * s, 0.0])
                .style(|_, _| button::Style::default())
                .on_press(Message::SettingsTab(tab)),
                container(Space::new())
                    .width(Length::Fill)
                    .height(if active { 2 } else { 0 })
                    .style(move |_| container::Style {
                        background: Some(c.accent.into()),
                        border: Border {
                            radius: 1.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
            ]
            .width(Length::Shrink),
        );
    }
    let body = match app.settings_tab {
        // the DirectX wrapper above the game's own graphics options.
        Tab::Graphics => column![crate::dgvoodoo::view(app), crate::reshade::view(app), crate::game_config::view(app, crate::game_config::Section::Graphics)]
            .spacing(16.0 * s)
            .into(),
        Tab::Sound => crate::game_config::view(app, crate::game_config::Section::Sound),
        Tab::Controller => crate::gamepad::view(app),
        // Launch tools above the Windows components check.
        Tab::System => column![crate::self_update::view(app), crate::controller_nav::settings(app),
            sign_in_switch(app), crate::launch_tools::view(app), crate::prereqs::view(app), ownership_notice(app)]
            .spacing(14.0 * s)
            .into(),
        Tab::Server => crate::server_admin_page::view(app),
    };
    let header = row![
        column![
            text(t("Settings")).font(BODY).size(26.0 * s).color(c.ink),
            text(t("Game options, controller setup and the Windows components FINAL FANTASY XI needs."))
                .font(BODY)
                .size(13.0 * s)
                .color(c.sub),
        ]
        .spacing(14.0 * s)
        .width(Length::Fill),
        column![language_picker(app), privacy_switch(app)].spacing(10).align_x(Alignment::End),
    ]
    .spacing(16)
    .align_y(Alignment::End);
    column![
        header,
        tabs,
        container(Space::new())
            .width(Length::Fill)
            .height(1)
            .style(move |_| container::Style {
                background: Some(c.line.into()),
                ..Default::default()
            }),
        scrollable(container(body).padding(iced::Padding::ZERO.right(16.0 * s))).height(Length::Fill),
    ]
    .spacing(14.0 * s)
    .height(Length::Fill)
    .into()
}

fn ownership_notice(app: &App) -> Element<'_, Message> {
    let c = colors(app.settings.light_theme);
    let s = scale(app);
    column![
        text("Crystal Launcher").font(BODY).size(16.0 * s).color(c.ink),
        text(format!("Version {} · GPL-3.0-only", env!("CARGO_PKG_VERSION"))).font(BODY).size(12.0 * s).color(c.sub),
        text(t("Independent launcher. Not affiliated with or endorsed by Square Enix.")).font(BODY).size(12.0 * s).color(c.sub),
        text("FINAL FANTASY XI © 2002 - 2020 SQUARE ENIX CO., LTD. All Rights Reserved.").font(BODY).size(12.0 * s).color(c.sub),
        text("FINAL FANTASY — registered trademark of Square Enix Holdings Co., Ltd.").font(BODY).size(12.0 * s).color(c.sub),
        text(t("Code licensing does not grant rights to game artwork or trademarks. See the included LICENSE and THIRD_PARTY.md.")).font(BODY).size(12.0 * s).color(c.sub),
    ].spacing(8.0 * s).padding([12.0 * s, 0.0]).into()
}

/// Startup sign-in and signing back in after a game have separate choices.
fn sign_in_switch(app: &App) -> Element<'_, Message> {
    let c = colors(app.settings.light_theme);
    column![
        crate::controller_nav::toggler(app.settings.auto_sign_in)
            .label(t("Sign accounts in when Crystal opens"))
            .on_toggle(Message::AutoSignIn)
            .font(BODY)
            .text_size(13)
            .size(18),
        text(t("Accounts that sign in without a one-time password sign in to PlayOnline in the background when Crystal opens, so Play starts FFXI without another sign-in."))
            .font(BODY).size(12).color(c.sub),
        text(t("Crystal learns this from each account's sign-ins: a new account joins after its first Play without a one-time password. Accounts that use one sign in only when you press Play."))
            .font(BODY).size(12).color(c.sub),
        crate::controller_nav::toggler(app.settings.sign_in_after_game)
            .label(t("Sign back in after FFXI closes"))
            .on_toggle(Message::SignInAfterGame)
            .font(BODY).text_size(13).size(18),
        text(t("Off by default: closing FFXI closes its POL host. When enabled with startup sign-in, Crystal opens a fresh PlayOnline session for the next Play."))
            .font(BODY).size(12).color(c.sub),
    ]
    .spacing(10)
    .padding([12, 0])
    .into()
}

/// Privacy mode, under the language: like it, it applies to every page. The
/// title bar's eye switches the same setting.
fn privacy_switch(app: &App) -> Element<'_, Message> {
    let s = scale(app);
    let switch = crate::controller_nav::toggler(app.settings.privacy)
        .label(t("Privacy mode"))
        .on_toggle(Message::SetPrivacy)
        .size(18)
        .text_size(13.0 * s)
        .font(BODY)
        .spacing(10);
    crate::ui::hint(
        container(switch).padding([2, 0]),
        t("Blurs account names, IDs, friends, characters and messages, for screenshots. Nothing saved changes."),
        iced::widget::tooltip::Position::Left,
    )
}

/// The launcher's own language, beside the page title: it applies to every
/// page, not to one tab, and never changes the game's language. Each choice
/// is named in its own language.
fn language_picker(app: &App) -> Element<'_, Message> {
    let light = app.settings.light_theme;
    let c = colors(light);
    let s = scale(app);
    let base = profile_pick_style(c.well, Border { color: c.line, width: 1.0, radius: RADIUS_INNER.into() }, false, light);
    row![
        text(t("Language")).font(BODY).size(13.0 * s).color(c.sub),
        crate::controller_nav::pick_list(Lang::ALL, Some(i18n::current()), Message::SetLanguage)
            .font(BODY)
            .text_size(14)
            .padding([8, 12])
            .width(150)
            .style(move |theme, state| pick_list::Style { handle_color: c.sub, ..base(theme, state) })
            .menu_style(profile_menu_style(light)),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}
