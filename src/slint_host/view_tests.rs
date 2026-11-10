//! Home's helpers: news dates, tabs and headline wrapping, and the Play button's state.
use super::*;

#[test]
fn japanese_text_wraps_by_em_width_inside_the_card() {
    // Full-width characters are a whole em, so a 20-em line holds 20 of them.
    let title = "フリートライアルのレベル上限が75に！プレイ期間が無制限になりました！";
    let lines = wrap_lines(title, 20.0 * 16.0, 16.0, 3);
    assert!(lines.len() >= 2 && lines.len() <= 3, "{lines:?}");
    for line in &lines {
        assert!(text_em(line) <= 20.0 + 1e-3, "{line}");
    }
    assert_eq!(lines.concat().replace(' ', ""), title);
    assert!(text_em("ああ") > text_em("aa") * 1.5);
}

#[test]
fn short_titles_stay_whole_and_long_ones_end_in_an_ellipsis() {
    let whole = wrap_lines("Free Trial Level Cap Raised to Level 75 and Time Limit Removed", 280.0, 16.0, 3);
    assert!(whole.len() <= 3);
    assert_eq!(whole.join(" "), "Free Trial Level Cap Raised to Level 75 and Time Limit Removed");
    let cut = wrap_lines(&"word ".repeat(60), 200.0, 16.0, 3);
    assert_eq!(cut.len(), 3);
    assert!(cut[2].ends_with('…'));
    for line in &cut {
        assert!(text_em(line) <= 200.0 / 16.0);
    }
}

#[test]
fn unbroken_text_is_split_rather_than_overflowing() {
    let lines = wrap_lines(&"x".repeat(80), 100.0, 10.0, 0);
    assert!(lines.len() > 1);
    assert!(lines.iter().all(|line| text_em(line) <= 10.0));
}

/// Dates are worded for the UI language and keep the source's own day.
#[test]
fn news_dates_are_short_and_worded_for_the_ui_language() {
    crate::i18n::set(Lang::En);
    for (raw, shown) in [
        ("Sep. 26, 2026 22:00 [PDT] From: FINAL FANTASY XI", "Sep 26"),
        ("Sep. 7, 2026 22:00 [PDT] From: FINAL FANTASY XI", "Sep 7"),
        ("Mon, 05 Oct 2026 03:00:00 +0900", "Oct 5"),
        ("2026-10-05T03:00:00Z", "Oct 5"),
        ("Wed, 30 Sep 2026 17:00:00 JST", "Sep 30"),
        ("2026.09.28(月) 23:50 From: プレイオンライン", "Sep 28"),
        ("Unknown", "Unknown"),
    ] {
        assert_eq!(news_date(raw), shown, "{raw}");
    }
    crate::i18n::set(Lang::Ja);
    for (raw, shown) in [
        ("Wed, 30 Sep 2026 17:00:00 JST", "9月30日"),
        ("2026.09.28(月) 23:50 From: プレイオンライン", "9月28日"),
        ("2026.10.01(木) 00:10 From: ファイナルファンタジーXI", "10月1日"),
        ("Sep. 26, 2026 22:00 [PDT] From: FINAL FANTASY XI", "9月26日"),
        ("Mon, 05 Oct 2026 03:00:00 +0900", "10月5日"),
        ("2026-10-05T03:00:00Z", "10月5日"),
        // 00:10 JST on Oct 1 stays Oct 1: the day is the source's, not this PC's.
        ("Thu, 01 Oct 2026 00:10:00 JST", "10月1日"),
        ("Unknown", "Unknown"),
        ("", ""),
    ] {
        assert_eq!(news_date(raw), shown, "{raw}");
    }
    crate::i18n::set(Lang::En);
}

#[test]
fn japanese_titles_fall_into_the_same_tabs() {
    let item = |title: &str| crate::services::NewsItem { title: title.into(), ..Default::default() };
    crate::i18n::set(Lang::Ja);
    for (title, update, event, label) in [
        ("10月バージョンアップのお知らせ", true, false, "Update"),
        ("フリートライアルのレベル上限が75に！ プレイ期間が無制限になりました！", true, false, "Update"),
        ("ファイナルファンタジーXI 更新のお知らせ(9/10)", true, false, "Update"),
        ("モーグリの新米冒険者＆復帰者応援キャンペーン ", false, true, "Event"),
        ("ヴァナ・ディールの異変を再調査せよ ", false, true, "Event"),
        ("「真夏のノマドモグボナンザ2026」当せん番号発表！", false, true, "Community"),
        // Neither an update nor an event: the PlayOnline kind (its own default picture).
        ("「第2回 『FFXI』冒険者意識調査」を実施します！", false, false, "PlayOnline"),
        ("全ワールド メンテナンス作業のお知らせ(11/12)", false, false, "Maintenance"),
    ] {
        let item = item(title);
        assert_eq!(news_matches(&item, NewsTab::Updates), update, "{title}");
        assert_eq!(news_matches(&item, NewsTab::Events), event, "{title}");
        assert!(news_matches(&item, NewsTab::Latest), "{title}");
        assert_eq!(Category::of(&item).look().0, t(label), "{title}");
    }
    crate::i18n::set(Lang::En);
    assert!(news_matches(&item("Free Trial Level Cap Raised to Level 75"), NewsTab::Updates));
    assert!(news_matches(&item("More Strange Happenings in Vana'diel"), NewsTab::Events));
    assert_eq!(Category::of(&item("Mog Bonanza results")), Category::Community);
}

/// Every kind is reachable, each with its own colour and default picture; a title the words don't
/// place is PlayOnline, not Event (1.1.0 called every such post an Event).
#[test]
fn news_kinds_map_to_their_colour_and_default_picture() {
    crate::i18n::set(Lang::En);
    let item = |title: &str| crate::services::NewsItem { title: title.into(), ..Default::default() };
    for (title, kind, ink, art) in [
        ("November version update", Category::Update, Ink::Ok, 0),
        ("Autumn Adventures campaign", Category::Event, Ink::Info, 1),
        ("All worlds maintenance on Wednesday 12 November", Category::Maintenance, Ink::Danger, 2),
        // A maintenance notice about an update is maintenance.
        ("Emergency maintenance for the version update", Category::Maintenance, Ink::Danger, 2),
        ("Nomad Mog Bonanza: entries are open", Category::Community, Ink::Warn, 3),
        ("Adventurer survey results", Category::PlayOnline, Ink::Sub, 4),
    ] {
        assert_eq!(Category::of(&item(title)), kind, "{title}");
        let (_, shown_ink, shown_art) = kind.look();
        assert_eq!((shown_ink, shown_art), (ink, art), "{title}");
    }
}

#[test]
fn play_button_follows_running_and_launching_accounts() {
    assert!(play_state(false, 0, 1) == PlayState::Play);
    assert!(play_state(false, 0, 0) == PlayState::Play);
    assert!(play_state(true, 0, 0) == PlayState::Starting);
    assert!(play_state(true, 0, 1) == PlayState::Play);
    assert!(play_state(false, 1, 0) == PlayState::Playing);
    // Some running, others still launchable: Play stays a launch button.
    assert!(play_state(false, 1, 2) == PlayState::Play);
}
