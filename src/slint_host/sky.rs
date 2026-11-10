//! Home's sky: the Vana'diel clock, worked out from the PC clock with the server's formula
//! (LandSandBoat's CVanaTime: 25 times faster than Earth from year 886, day 1, Firesday 00:00 at
//! 2002-01-01 00:00 JST),
//! and the moon's 84-day cycle. No network and no game: a single-shot timer wakes at the next
//! change the hero shows (the time is shown to ten Vana'diel minutes, 24 real seconds), sets
//! what changed and arms the next one; the rail's session clocks catch up on the same wake. It
//! lives only while the window does, and sleeps while the window is unfocused or neither the sky
//! nor a session clock is on screen.
use super::sync::put;
use super::{MainWindow, Sky};
use crate::i18n::t;
use slint::{ComponentHandle, SharedString, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// The server's clock (LandSandBoat's CVanaTime, which matches retail): Vana'diel stood at year
/// 886, day 1, Firesday 00:00 at 2002-01-01 00:00 JST, and runs 25 times faster than Earth.
const BASE_S: i64 = 1_009_810_800;
const BASE_MINUTES: i64 = 886 * YEAR_MINUTES;
const YEAR_MINUTES: i64 = 360 * DAY_MINUTES;
const DAY_MINUTES: i64 = 1440;
/// The time is shown to ten Vana'diel minutes.
const STEP_MINUTES: i64 = 10;

/// What the hero shows of Vana'diel at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Vana {
    /// Firesday 0 to Darksday 7.
    pub weekday: u8,
    pub hour: u8,
    /// Floored to ten minutes.
    pub minute: u8,
    /// 0 night, 1 dawn, 2 day, 3 dusk.
    pub phase: u8,
    /// How much of the moon is lit, 0 to 100.
    pub moon: u8,
    pub waxing: bool,
    /// Earth milliseconds until the shown time next changes.
    pub next_ms: i64,
}

/// Vana'diel minutes since its year 0 at Earth time `unix_ms`, from whole Earth seconds as the
/// server counts them: floor((s - base) / 60 * 25) + 886 years.
fn vana_minutes(unix_ms: i64) -> i64 {
    ((unix_ms.div_euclid(1000) - BASE_S) * 25).div_euclid(60) + BASE_MINUTES
}

/// The sky at a Vana'diel hour: 0 night (20:00 to 04:00), 1 dawn, 2 day (07:00 to 17:00), 3 dusk.
fn phase(hour: u8) -> u8 {
    match hour {
        4..=6 => 1,
        7..=16 => 2,
        17..=19 => 3,
        _ => 0,
    }
}

pub(super) fn at(unix_ms: i64) -> Vana {
    let minutes = vana_minutes(unix_ms);
    let day = minutes.div_euclid(DAY_MINUTES);
    let in_day = minutes.rem_euclid(DAY_MINUTES);
    let hour = (in_day / 60) as u8;
    // The moon's 84-day cycle as the server keeps it: full at 0, new at 42, full again at 84.
    let cycle = (day + 26).rem_euclid(84);
    let moon = if cycle >= 42 { (100.0 * (cycle - 42) as f64 / 42.0).round() } else { (100.0 * (1.0 - cycle as f64 / 42.0)).round() } as u8;
    // The next ten-minute step: the Earth second its first minute starts (the shown values change
    // on whole seconds), in milliseconds from now.
    let next = (minutes.div_euclid(STEP_MINUTES) + 1) * STEP_MINUTES - BASE_MINUTES;
    let next_s = BASE_S + (next * 60 + 24) / 25;
    Vana {
        weekday: day.rem_euclid(8) as u8,
        hour,
        minute: (in_day % 60 / 10 * 10) as u8,
        phase: phase(hour),
        moon,
        waxing: cycle >= 42,
        next_ms: next_s * 1000 - unix_ms,
    }
}
fn day_name(weekday: u8) -> &'static str {
    match weekday {
        0 => t("Firesday"),
        1 => t("Earthsday"),
        2 => t("Watersday"),
        3 => t("Windsday"),
        4 => t("Iceday"),
        5 => t("Lightningday"),
        6 => t("Lightsday"),
        _ => t("Darksday"),
    }
}

fn moon_name(percent: u8, waxing: bool) -> &'static str {
    match (percent, waxing) {
        (90.., _) => t("Full moon"),
        (57.., true) => t("Waxing gibbous"),
        (57.., false) => t("Waning gibbous"),
        (40.., true) => t("First quarter"),
        (40.., false) => t("Last quarter"),
        (11.., true) => t("Waxing crescent"),
        (11.., false) => t("Waning crescent"),
        _ => t("New moon"),
    }
}

/// The lit part of a 28-unit disc: the outer half-circle on the lit side, back along the
/// terminator (an ellipse whose width follows how much is lit).
pub(super) fn moon_path(percent: u8, waxing: bool) -> String {
    let k = f32::from(percent.min(100)) / 100.0;
    let rx = (14.0 * (1.0 - 2.0 * k).abs()).max(0.01);
    // Waxing lights the right side (the outer arc runs clockwise from the top); a gibbous moon's
    // terminator bulges away from the lit side, a crescent's towards it.
    let (outer, inner) = match (waxing, k > 0.5) {
        (true, true) => (1, 1),
        (true, false) => (1, 0),
        (false, true) => (0, 0),
        (false, false) => (0, 1),
    };
    format!("M14 0A14 14 0 0 {outer} 14 28A{rx:.2} 14 0 0 {inner} 14 0Z")
}

thread_local! {
    static TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// A crossfade's frames (running only during one).
    static FADE: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// A sky has been shown in this window (the first one comes in without a fade).
    static SHOWN: Cell<bool> = const { Cell::new(false) };
    /// The window has the focus.
    static FOCUSED: Cell<bool> = const { Cell::new(true) };
    /// The sky shows (Home, Features closed) or a rail session clock runs.
    static NEEDED: Cell<bool> = const { Cell::new(true) };
}

fn armed() -> bool {
    FOCUSED.get() && NEEDED.get()
}

/// The wake runs only while it shows something that moves: with the window focused and the
/// sky or a session clock on screen. Turning back on catches up at once.
fn set(ui: &MainWindow, flag: &'static std::thread::LocalKey<Cell<bool>>, on: bool) {
    let was = armed();
    flag.with(|cell| cell.set(on));
    match (was, armed()) {
        (false, true) => {
            // A change missed while away comes in quickly.
            update_with(ui, CATCH_UP_FADE);
            super::view::tick_clocks();
        }
        (true, false) => {
            TIMER.with(|cell| if let Some(timer) = cell.borrow().as_ref() { timer.stop(); });
            end_fade(ui);
        }
        _ => {}
    }
}

/// The window gained or lost the focus.
pub(super) fn focused(ui: &MainWindow, on: bool) {
    set(ui, &FOCUSED, on);
}

/// Whether the sky or a session clock is on screen (the view's sync).
pub(super) fn needed(ui: &MainWindow, on: bool) {
    set(ui, &NEEDED, on);
}

fn now_ms() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    now + preview_offset(now)
}

/// Debug builds only: CRYSTAL_VANA_TIME=HH:MM starts the clock at that Vana'diel time (the next
/// one from now), and it runs on from there, for previewing the four skies.
#[cfg(debug_assertions)]
fn preview_offset(now: i64) -> i64 {
    thread_local! { static OFFSET: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) }; }
    OFFSET.with(|cell| {
        if let Some(offset) = cell.get() { return offset; }
        let offset = (|| {
            let value = std::env::var("CRYSTAL_VANA_TIME").ok()?;
            let (h, m) = value.trim().split_once(':')?;
            let want = h.parse::<i64>().ok()?.rem_euclid(24) * 60 + m.parse::<i64>().ok()?.rem_euclid(60);
            let ahead = (want - vana_minutes(now).rem_euclid(DAY_MINUTES)).rem_euclid(DAY_MINUTES);
            // A Vana'diel minute is 2.4 Earth seconds; a second more lands inside it.
            Some(ahead * 2400 + 1000)
        })()
        .unwrap_or(0);
        cell.set(Some(offset));
        offset
    })
}

#[cfg(not(debug_assertions))]
fn preview_offset(_now: i64) -> i64 {
    0
}

/// Shows Vana'diel now and wakes again at the next change.
pub(super) fn update(ui: &MainWindow) {
    update_with(ui, FADE_SPAN);
}

/// A change of sky crossfades over this long while the window is watched, and over the short
/// one when it catches up after the window was away.
const FADE_SPAN: Duration = Duration::from_millis(5000);
const CATCH_UP_FADE: Duration = Duration::from_millis(1200);
/// The crossfade's frame interval.
const FADE_FRAME: Duration = Duration::from_millis(16);

fn update_with(ui: &MainWindow, span: Duration) {
    let vana = at(now_ms());
    let before = ui.global::<Sky>().get_phase();
    show(ui, vana);
    let changed = SHOWN.replace(true) && before != i32::from(vana.phase);
    if !armed() { return; }
    if changed { crossfade(ui, before, span); }
    let weak = ui.as_weak();
    TIMER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let timer = slot.get_or_insert_with(Timer::default);
        timer.start(TimerMode::SingleShot, Duration::from_millis(vana.next_ms.max(1) as u64), move || {
            if let Some(ui) = weak.upgrade() {
                update(&ui);
                // The rail's session clocks ride the same wake.
                super::view::tick_clocks();
            }
        });
    });
}

/// Sets what the hero shows of `vana` (only what changed).
fn show(ui: &MainWindow, vana: Vana) {
    let sky = ui.global::<Sky>();
    put!(sky, get_phase, set_phase, i32::from(vana.phase));
    put!(sky, get_weekday, set_weekday, i32::from(vana.weekday));
    put!(sky, get_day, set_day, SharedString::from(day_name(vana.weekday)));
    put!(sky, get_time, set_time, SharedString::from(format!("{:02}:{:02}", vana.hour, vana.minute)));
    put!(sky, get_moon, set_moon, SharedString::from(format!("{} {}%", moon_name(vana.moon, vana.waxing), vana.moon)));
    put!(sky, get_moon_path, set_moon_path, SharedString::from(moon_path(vana.moon, vana.waxing)));
}

/// How far a crossfade `elapsed` into `span` has come (eased in and out), or `None` once done.
fn fade_at(elapsed: Duration, span: Duration) -> Option<f32> {
    let t = elapsed.as_secs_f32() / span.as_secs_f32().max(f32::EPSILON);
    (t < 1.0).then(|| t * t * (3.0 - 2.0 * t))
}

/// The sky `from` fades out under the one now set: frames run only for the fade.
fn crossfade(ui: &MainWindow, from: i32, span: Duration) {
    let sky = ui.global::<Sky>();
    sky.set_under(from);
    sky.set_fade(0.0);
    let weak = ui.as_weak();
    let start = Instant::now();
    FADE.with(|cell| {
        cell.borrow_mut().get_or_insert_with(Timer::default).start(TimerMode::Repeated, FADE_FRAME, move || {
            let Some(ui) = weak.upgrade() else { return };
            match fade_at(start.elapsed(), span) {
                Some(fade) => ui.global::<Sky>().set_fade(fade),
                None => end_fade(&ui),
            }
        });
    });
}

/// The new sky stands alone; the frames stop.
fn end_fade(ui: &MainWindow) {
    let sky = ui.global::<Sky>();
    sky.set_under(-1);
    sky.set_fade(1.0);
    FADE.with(|cell| if let Some(timer) = cell.borrow().as_ref() { timer.stop(); });
}

/// The window closed: the clock stops with it.
pub(super) fn forget() {
    TIMER.with(|cell| cell.borrow_mut().take());
    FADE.with(|cell| cell.borrow_mut().take());
    SHOWN.with(|cell| cell.set(false));
    FOCUSED.with(|cell| cell.set(true));
    NEEDED.with(|cell| cell.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's clock at known points: its base (886/1/1, Firesday 00:00, the moon 10 %
    /// waning) and 2026-11-10 13:40 UTC (1516/7/19, Lightsday 14:40, the moon 62 % waxing).
    #[test]
    fn the_clock_is_the_servers() {
        let date = |unix_ms: i64| {
            let m = vana_minutes(unix_ms);
            (m / YEAR_MINUTES, m % YEAR_MINUTES / 43_200 + 1, m % 43_200 / DAY_MINUTES + 1)
        };
        let base = BASE_S * 1000;
        assert_eq!(date(base), (886, 1, 1));
        let v = at(base);
        assert_eq!((v.weekday, v.hour, v.minute, v.phase, v.moon, v.waxing), (0, 0, 0, 0, 10, false));
        let now = 1_794_318_000_000;
        assert_eq!(date(now), (1516, 7, 19));
        let v = at(now);
        assert_eq!((v.weekday, v.hour, v.minute, v.phase, v.moon, v.waxing), (6, 14, 40, 2, 62, true));
        // 14:49 a moment later (shown as 14:40, the time steps by ten minutes).
        assert_eq!(vana_minutes(now + 22_000) % DAY_MINUTES, 14 * 60 + 49);
        assert_eq!(at(now + 22_000).minute, 40);
    }

    /// 25x speed from whole Earth seconds; the phases by hour; the wake lands on the second the
    /// shown time changes.
    #[test]
    fn vana_clock_runs_25_times_faster() {
        let base = BASE_S * 1000;
        // One Earth hour is 25 Vana'diel hours: one day and one hour later.
        let later = at(base + 3_600_000);
        assert_eq!((later.hour, later.weekday), (1, 1));
        // 04:00, 07:00, 17:00 and 20:00 start dawn, day, dusk and night.
        for (hour, phase) in [(3, 0), (4, 1), (6, 1), (7, 2), (16, 2), (17, 3), (19, 3), (20, 0), (23, 0)] {
            assert_eq!(at(base + hour * 3_600_000 / 25).phase, phase, "{hour}:00");
        }
        // Ten Vana'diel minutes are 24 Earth seconds: at 00:07 the next step is 7.2 s away, on
        // the whole second where the server's minutes reach 00:10.
        let mid = at(base + 7 * 2_400);
        assert_eq!((mid.hour, mid.minute), (0, 0));
        assert_eq!(mid.next_ms, 24_000 - 7 * 2_400);
        assert_eq!(at(base + 7 * 2_400 + mid.next_ms).minute, 10);
        assert_eq!(at(base + 7 * 2_400 + mid.next_ms - 1).minute, 0);
        // Mid-second: the wake still lands on the second.
        let off = at(base + 7 * 2_400 + 500);
        assert_eq!((base + 7 * 2_400 + 500 + off.next_ms) % 1000, 0);
    }

    /// The crossfade eases in and out over its span, then ends.
    #[test]
    fn a_crossfade_eases_and_ends() {
        let span = FADE_SPAN;
        assert_eq!(fade_at(Duration::ZERO, span), Some(0.0));
        assert_eq!(fade_at(span / 2, span), Some(0.5));
        let early = fade_at(span / 10, span).unwrap();
        assert!(early > 0.0 && early < 0.1, "slow to start: {early}");
        let late = fade_at(span * 9 / 10, span).unwrap();
        assert!(late > 0.9 && late < 1.0, "slow to land: {late}");
        assert_eq!(fade_at(span, span), None);
        assert_eq!(fade_at(CATCH_UP_FADE, CATCH_UP_FADE), None);
    }

    #[test]
    fn moon_waxes_and_wanes_over_84_vana_days() {
        // Vana'diel day d is at d * 57.6 Earth minutes from the base, day 886 * 360 + d.
        let day = |d: i64| BASE_S * 1000 + d * 3_456_000 + 1_000;
        // The base day is 38 days into the cycle (full at 0, new at 42): 4 days on it is new,
        // 46 days on full.
        assert_eq!((at(day(4)).moon, at(day(46)).moon), (0, 100));
        let waning = at(day(56));
        assert_eq!((waning.moon, waning.waxing), (76, false));
        let waxing = at(day(36));
        assert_eq!((waxing.moon, waxing.waxing), (76, true));
        assert_eq!(at(day(46 + 84)).moon, 100, "the cycle repeats");
    }
    /// An Earth moment when Vana'diel shows this weekday and time with the moon nearest `moon`
    /// on its waxing or waning side (the week and the moon's cycle don't meet every pairing).
    fn moment(weekday: u8, hour: u8, minute: u8, moon: u8, waxing: bool) -> i64 {
        let in_day = (i64::from(hour) * 60 + i64::from(minute)) * 60_000 / 25 + 1_000;
        (0..168)
            .map(|day| BASE_S * 1000 + day * 3_456_000 + in_day)
            .filter(|&ms| {
                let v = at(ms);
                (v.weekday, v.hour, v.minute, v.waxing) == (weekday, hour, minute, waxing)
            })
            .min_by_key(|&ms| at(ms).moon.abs_diff(moon))
            .expect("that Vana'diel moment exists")
    }

    #[test]
    fn preview_moments_are_found() {
        let v = at(moment(5, 21, 40, 62, true));
        assert_eq!((v.weekday, v.hour, v.minute, v.phase, v.waxing), (5, 21, 40, 0, true));
        assert!(v.moon.abs_diff(62) <= 5, "{v:?}");
    }

    /// What the hero shows, set outright: the week and the moon's cycle don't meet at every
    /// pairing the renders show.
    fn vana(weekday: u8, hour: u8, minute: u8, moon: u8, waxing: bool) -> Vana {
        Vana { weekday, hour, minute, phase: phase(hour), moon, waxing, next_ms: 0 }
    }

    /// The headless window the hero's screenshots are taken in, and where they go.
    struct Stage {
        window: std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
        ui: MainWindow,
        dir: std::path::PathBuf,
    }

    impl Stage {
        fn new(dir: std::path::PathBuf) -> Self {
            crate::privacy::set(false);
            crate::i18n::set(crate::i18n::Lang::En);
            let window = super::super::sync::headless::window(1440, 900);
            let ui = MainWindow::new().unwrap();
            super::super::view::wire(&ui);
            ui.show().unwrap();
            Self { window, ui, dir }
        }

        /// Home at `width` x `height` and display scale `scale`, under `vana`'s sky
        /// `from`: mid-crossfade from that sky, this far in.
        #[allow(clippy::too_many_arguments)]
        fn shot(&self, name: &str, app: &mut crate::App, width: u32, height: u32, scale: f32, light: bool, vana: Vana, from: Option<(u8, f32)>) {
            let (window, ui) = (&self.window, &self.ui);
            window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: scale });
            let (pw, ph) = ((width as f32 * scale) as u32, (height as f32 * scale) as u32);
            window.set_size(slint::PhysicalSize::new(pw, ph));
            app.viewport = crate::effect::Size::new(width as f32, height as f32);
            let theme = ui.global::<super::super::Theme>();
            theme.set_light(light);
            theme.set_scale(super::super::scale(app));
            theme.set_small(super::super::small(app));
            // The first sync shows the PC clock's Vana'diel; the render's goes over it.
            super::super::view::sync(ui, app);
            show(ui, vana);
            let sky = ui.global::<Sky>();
            let (under, fade) = from.map_or((-1, 1.0), |(p, f)| (i32::from(p), f));
            sky.set_under(under);
            sky.set_fade(fade);
            let mut pixels = vec![slint::Rgb8Pixel::default(); (pw * ph) as usize];
            for _ in 0..2 {
                window.request_redraw();
                window.draw_if_needed(|renderer| { renderer.render(&mut pixels, pw as usize); });
            }
            let bytes: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            std::fs::create_dir_all(&self.dir).unwrap();
            imagecrate::save_buffer_with_format(self.dir.join(format!("{name}.png")), &bytes, pw, ph, imagecrate::ExtendedColorType::Rgb8, imagecrate::ImageFormat::Png).unwrap();
        }
    }

    /// CRYSTAL_HERO_SHOTS=<dir>: Home's hero as the renders show it (crystal-design-v12b,
    /// a-skyline: home-dark at Lightningday 21:40 under a 62 % waxing moon, home-light at Iceday
    /// 10:20 under a 21 % waning one, sky-times' four phases, the small window), at 1x and at
    /// 150 % display scale.
    #[test]
    fn hero_screenshots() {
        let Some(dir) = std::env::var_os("CRYSTAL_HERO_SHOTS").map(std::path::PathBuf::from) else { return };
        let stage = Stage::new(dir);
        let mut app = crate::tests::app();
        app.page = crate::Page::Home;
        let shot = |name: &str, app: &mut crate::App, width: u32, height: u32, scale: f32, light: bool, vana: Vana, from: Option<(u8, f32)>| {
            stage.shot(name, app, width, height, scale, light, vana, from);
        };
        let night = vana(5, 21, 40, 62, true);
        let light = vana(4, 10, 20, 21, false);
        for (suffix, scale) in [("", 1.0), ("@1.5x", 1.5), ("@2x", 2.0)] {
            shot(&format!("home-dark{suffix}"), &mut app, 1440, 900, scale, false, night, None);
            shot(&format!("home-light{suffix}"), &mut app, 1440, 900, scale, true, light, None);
            shot(&format!("home-small-light{suffix}"), &mut app, 1000, 700, scale, true, light, None);
            shot(&format!("home-small-dark{suffix}"), &mut app, 1000, 700, scale, false, night, None);
            for (name, hour, minute) in [("night", 21, 40), ("dawn", 5, 30), ("day", 12, 0), ("dusk", 18, 30)] {
                shot(&format!("sky-{name}{suffix}"), &mut app, 1440, 900, scale, false, vana(5, hour, minute, 62, true), None);
                shot(&format!("small-{name}{suffix}"), &mut app, 1000, 700, scale, false, vana(5, hour, minute, 62, true), None);
            }
            // Mid-crossfade: dusk to night, night to dawn, dawn to day, a third and halfway in.
            for (name, from, hour, minute) in [("dusk-night", 3, 20, 0), ("night-dawn", 0, 4, 0), ("dawn-day", 1, 7, 0)] {
                for fade in [0.33, 0.5] {
                    let at = vana(5, hour, minute, 62, true);
                    shot(&format!("fade-{name}-{}{suffix}", (fade * 100.0) as u32), &mut app, 1440, 900, scale, false, at, Some((from, fade)));
                }
            }
        }
    }

    /// The render's waxing gibbous 62%: the right half lit, the terminator 3.4 units left of centre.
    #[test]
    fn moon_path_matches_the_render() {
        assert_eq!(moon_path(62, true), "M14 0A14 14 0 0 1 14 28A3.36 14 0 0 1 14 0Z");
        assert!(moon_path(30, false).starts_with("M14 0A14 14 0 0 0 14 28"), "waning lights the left");
        assert!(moon_path(0, true).contains("A14.00 14"), "a new moon's terminator meets the edge");
    }
}
