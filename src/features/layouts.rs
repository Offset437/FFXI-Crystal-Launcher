//! Window layouts: each client's remembered window (where, how big, which monitor, which mode),
//! restored when its game starts, and the "arrange all" presets.
//!
//! How Crystal avoids fighting FFXI: the game sizes its window from its own resolution values
//! (`0001`/`0002`, `0034` for the mode), which Crystal already serves to each game in-process
//! (`ffxi_settings`). With `launch_at_saved_size` on, a client whose start size the user chose
//! (`start`) starts with those values set to that size and mode (`launch_values`), so the game
//! itself opens at that size and Crystal only moves the window into place. A fullscreen game is
//! never resized: at most it is moved to its monitor.
//!
//! A game's resolution changes only when the user chose it for the next start: by placing a
//! tile for a client whose game is not running (`place`). A running game's window that is
//! moved, resized by hand, arranged or put on a tile by Apply is remembered for where it goes
//! back (`remember`, `restore_rect`), and its game still starts at its own resolution: the
//! window is a stretched picture of it, and everything the game and its add-ons place by
//! pixel (FFXIDB's minimap, the HUD) stays where it was saved. Crystal 1.2.0 to 1.2.4 served
//! every remembered window's size as the resolution, so a game restarted after Arrange all
//! came back at its tile's resolution with those positions off.
//!
//! The layout editor (the monitor map of Features, Windows) edits these same remembered windows:
//! a tile placed for a client whose game is not running is its remembered window and its start
//! size (`place`), and the presets are where the editor starts from (`arrange`).
//!
//! Everything here is pure (monitors in, rectangles out) and unit-tested; `runtime` applies it.

use super::game_window::{Frame, Mode, Monitor, Placement, Rect, Shown};
use super::ClientKey;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// Remember each client's window when it is moved, resized, minimised or left.
    pub remember: bool,
    /// Put each client's window back where it was when its game starts.
    pub restore: bool,
    /// Serve the chosen start size and mode (`start`) as the game's own resolution at launch.
    pub launch_at_saved_size: bool,
    /// The aspect ratio presets keep for each window (width / height).
    pub aspect: f32,

    /// Remembered windows by `ClientKey::id`: where each goes back. Never a resolution.
    pub saved: BTreeMap<String, Saved>,
    /// The size and mode each client's game starts at, by `ClientKey::id`: only what the user
    /// chose for the next start (`place`). Windows remembered by Crystal 1.2.0 to 1.2.4 have
    /// none, so those games start at their own resolution again.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub start: BTreeMap<String, Start>,
    /// What Restore windows puts back, by `ClientKey::id`: kept here, not in memory, because
    /// Apply also changes the remembered windows, so a game restarted after it starts
    /// arranged (stretched to its tile, at its own resolution); Restore windows still undoes it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub undo: BTreeMap<String, Arranged>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            remember: true, restore: true, launch_at_saved_size: true, aspect: 16.0 / 9.0,
            saved: BTreeMap::new(), start: BTreeMap::new(), undo: BTreeMap::new(),
        }
    }
}

/// The resolution and window mode a client's game starts at, chosen by the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Start {
    /// The client (render) size: the game's resolution.
    pub client: (i32, i32),
    pub mode: Mode,
}

impl Start {
    /// The start size a placed window gives its game, if any: none for a fullscreen one, one
    /// that covers its whole monitor (its own resolution wins: a window larger than the screen
    /// is what Windows reports for a game it scales) or one smaller than the game can be.
    fn of(saved: &Saved) -> Option<Self> {
        let (w, h) = saved.client;
        let sized = saved.mode != Mode::Fullscreen && covers(&saved.rect, &saved.monitor.rect).is_none() && w >= 320 && h >= 240;
        sized.then_some(Self { client: saved.client, mode: saved.mode })
    }
}

/// A client's window Apply moved: where it was before (before the first of several in a
/// row), and where the last Apply left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Arranged {
    pub before: Saved,
    pub arranged: Rect,
}

/// Where a monitor was, to find it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MonitorRef {
    pub device: String,
    pub rect: Rect,
    pub work: Rect,
    pub dpi: u32,
}

impl From<&Monitor> for MonitorRef {
    fn from(m: &Monitor) -> Self { Self { device: m.device.clone(), rect: m.rect, work: m.work, dpi: m.dpi } }
}

/// One client's remembered window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Saved {
    /// The normal window rectangle, frame included.
    pub rect: Rect,
    /// The client (render) size at that rectangle.
    pub client: (i32, i32),
    #[serde(default)]
    pub shown: Shown,
    #[serde(default)]
    pub mode: Mode,
    /// Its frame styles (older files have none: the frame is then left as it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
    pub monitor: MonitorRef,
}

impl Saved {
    pub(crate) fn from_placement(p: &Placement) -> Self {
        Self { rect: p.rect, client: p.client, shown: p.shown, mode: p.mode, frame: Some(p.frame), monitor: MonitorRef::from(&p.monitor) }
    }
}

/// Remember `placement` for `key`: where its window goes back. Its start size (`start`) is not
/// touched, so a window Crystal or the user moved or resized never changes the resolution its
/// game starts at. Whether anything changed (nothing is saved for no change). A
/// minimised window keeps its remembered normal rectangle and only notes that it was minimised
/// when that rectangle is known; a client is never remembered as starting minimised otherwise.
pub(crate) fn remember(settings: &mut Settings, key: &ClientKey, placement: &Placement) -> bool {
    let saved = Saved::from_placement(placement);
    let id = key.id();
    if saved.rect.w <= 0 || saved.rect.h <= 0 { return false; }
    // A minimised game's client area reads 0x0: keep the last real client size.
    let saved = match settings.saved.get(&id) {
        Some(old) if saved.client.0 <= 0 || saved.client.1 <= 0 => Saved { client: old.client, ..saved },
        _ => saved,
    };
    if saved.client.0 <= 0 || saved.client.1 <= 0 { return false; }
    if settings.saved.get(&id) == Some(&saved) { return false; }
    settings.saved.insert(id, saved);
    true
}

/// The monitor a remembered window belongs on now: the same device at the same place, else the
/// same device, else a monitor at the same place, else the primary. `None` with no monitors.
pub(crate) fn find_monitor<'a>(saved: &MonitorRef, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
    monitors.iter().find(|m| m.device == saved.device && m.rect == saved.rect)
        .or_else(|| monitors.iter().find(|m| m.device == saved.device))
        .or_else(|| monitors.iter().find(|m| m.rect == saved.rect))
        .or_else(|| monitors.iter().find(|m| m.primary))
        .or_else(|| monitors.first())
}

/// Where to put a remembered window now. On its own unchanged monitor it goes back exactly, to
/// the pixel, wherever it was (a window that covered the taskbar's strip covers it again). On a
/// monitor that changed (resolution, DPI, or a missing monitor replaced by the primary) the
/// position and size scale with the work area and are clamped inside it; a fullscreen window is
/// placed over its new monitor whole, and one that covered its whole monitor and more (`covers`)
/// covers the new one by the same margins.
///
/// The taskbar stays on top of a game whose window (its client area, with a caption) no longer
/// covers the monitor, by one pixel or by the taskbar's own height: a window that covered its
/// monitor is never put inside the work area.
pub(crate) fn restore_rect(saved: &Saved, monitors: &[Monitor]) -> Option<(Rect, Shown)> {
    let monitor = find_monitor(&saved.monitor, monitors)?;
    let shown = if saved.shown == Shown::Minimized { Shown::Normal } else { saved.shown };
    if saved.mode == Mode::Fullscreen { return Some((monitor.rect, Shown::Normal)); }
    let same = monitor.rect == saved.monitor.rect && monitor.work == saved.monitor.work && monitor.dpi == saved.monitor.dpi;
    if same { return Some((saved.rect, shown)); }
    if let Some((left, top, right, bottom)) = covers(&saved.rect, &saved.monitor.rect) {
        let m = monitor.rect;
        return Some((Rect::new(m.x - left, m.y - top, m.w + left + right, m.h + top + bottom), shown));
    }
    let (from, to) = (saved.monitor.work, monitor.work);
    let sx = f64::from(to.w) / f64::from(from.w.max(1));
    let sy = f64::from(to.h) / f64::from(from.h.max(1));
    // Uniform scale keeps the game's aspect; position keeps its share of the work area.
    let scale = sx.min(sy);
    let rect = Rect::new(
        to.x + (f64::from(saved.rect.x - from.x) * sx).round() as i32,
        to.y + (f64::from(saved.rect.y - from.y) * sy).round() as i32,
        (f64::from(saved.rect.w) * scale).round() as i32,
        (f64::from(saved.rect.h) * scale).round() as i32,
    );
    Some((clamp_into(rect, &monitor.work), shown))
}

/// Whether a game window as it is `now` should be left alone instead of being put at `target`:
/// it covers the whole of its monitor (as the game opened it) and `target` covers that same
/// monitor too. Moving it would at best change nothing, and for a game whose coordinates Windows
/// scales (a process that is not DPI aware on a scaled display) the move can land a pixel off,
/// which is enough for the taskbar to stay on top of it.
pub(crate) fn stays_full_screen(now: &Placement, target: &Rect) -> bool {
    now.shown == Shown::Normal && covers(&now.rect, &now.monitor.rect).is_some() && covers(target, &now.monitor.rect).is_some()
}

/// How far `rect` reaches past each side of `monitor` (left, top, right, bottom) when it covers
/// the whole of it: a borderless window a pixel larger than the screen, or a windowed game whose
/// frame is pushed off it so its picture fills the screen. `None` when any of the monitor shows.
pub(crate) fn covers(rect: &Rect, monitor: &Rect) -> Option<(i32, i32, i32, i32)> {
    let (left, top) = (monitor.x - rect.x, monitor.y - rect.y);
    let (right, bottom) = (rect.right() - monitor.right(), rect.bottom() - monitor.bottom());
    (monitor.w > 0 && monitor.h > 0 && left >= 0 && top >= 0 && right >= 0 && bottom >= 0).then_some((left, top, right, bottom))
}

/// The monitor `rect` belongs on: the one it is mostly on, else the nearest. `None` with no monitors.
pub(crate) fn monitor_for<'a>(rect: &Rect, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
    let screens: Vec<super::snap::Screen> = monitors.iter().map(super::snap::Screen::from).collect();
    super::snap::screen_of(rect, &screens).map(|index| &monitors[index])
}

/// The remembered window of a client whose tile the layout editor put at `rect`, given what was
/// remembered for it (`old`). Only the place changes: a window with a caption keeps it, and its
/// frame's share of the rectangle, so the game starts at the size that fits `rect`; any other
/// is borderless, and exactly over its monitor it is that monitor's full screen. `None` with no
/// monitors.
pub(crate) fn placed(old: Option<&Saved>, rect: Rect, monitors: &[Monitor]) -> Option<Saved> {
    if rect.w <= 0 || rect.h <= 0 { return None; }
    let monitor = monitor_for(&rect, monitors)?;
    let windowed = old.is_some_and(|s| s.mode == Mode::Windowed);
    let border = old.filter(|_| windowed).map_or((0, 0), |s| ((s.rect.w - s.client.0).max(0), (s.rect.h - s.client.1).max(0)));
    let mode = if windowed { Mode::Windowed } else if rect == monitor.rect { Mode::Fullscreen } else { Mode::Borderless };
    Some(Saved {
        rect, client: ((rect.w - border.0).max(1), (rect.h - border.1).max(1)), shown: Shown::Normal, mode,
        frame: old.and_then(|s| s.frame), monitor: MonitorRef::from(monitor),
    })
}

/// Remember `rect` as `key`'s window (`placed`; a client with a slot starts from its account's
/// slotless window when it has none of its own), and its size as the size its game starts at
/// (`Start::of`): the one place a start size is chosen. Whether anything changed.
pub(crate) fn place(settings: &mut Settings, key: &ClientKey, rect: Rect, monitors: &[Monitor]) -> bool {
    let id = key.id();
    let old = settings.saved.get(&id).or_else(|| settings.saved.get(&ClientKey::new(key.account.clone(), None).id()));
    let Some(saved) = placed(old, rect, monitors) else { return false };
    let start = Start::of(&saved);
    if settings.saved.get(&id) == Some(&saved) && settings.start.get(&id) == start.as_ref() { return false; }
    match start {
        Some(start) => settings.start.insert(id.clone(), start),
        None => settings.start.remove(&id),
    };
    settings.saved.insert(id, saved);
    true
}

/// Forget `key`'s remembered window and its start size.
pub(crate) fn forget(settings: &mut Settings, key: &ClientKey) {
    let id = key.id();
    settings.saved.remove(&id);
    settings.start.remove(&id);
}

/// `rect` moved (and shrunk only if larger) so it lies inside `area`.
pub(crate) fn clamp_into(rect: Rect, area: &Rect) -> Rect {
    let w = rect.w.min(area.w).max(1);
    let h = rect.h.min(area.h).max(1);
    let x = rect.x.clamp(area.x, area.right() - w);
    let y = rect.y.clamp(area.y, area.bottom() - h);
    Rect::new(x, y, w, h)
}

/// The resolution values a client's game should start with, from the start size the user chose
/// for it (`start`; a client with a slot and none of its own uses its account's): the client
/// size (`0001`/`0002`) and mode (`0034`: 1 window, 3 borderless). Empty when none was chosen
/// or the setting is off: the game then starts at its own resolution, whatever window is
/// remembered for it. A remembered window is never a resolution: it may be a tile Apply
/// stretched a running game to, or a size Windows scaled.
pub(crate) fn launch_values(settings: &Settings, key: &ClientKey) -> Vec<(&'static str, u32)> {
    if !settings.launch_at_saved_size { return Vec::new(); }
    let Some(start) = settings.start.get(&key.id()).or_else(|| settings.start.get(&ClientKey::new(key.account.clone(), None).id())) else { return Vec::new() };
    let (w, h) = start.client;
    if w < 320 || h < 240 { return Vec::new(); }
    let mode = match start.mode { Mode::Windowed => 1, Mode::Borderless => 3, Mode::Fullscreen => return Vec::new() };
    vec![("0001", w as u32), ("0002", h as u32), ("0034", mode)]
}

/// The arrange-all presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Preset {
    /// Every client in an even grid across the chosen monitors.
    Grid,
    /// The first client large, the others tiled beside it (or on the other monitors).
    MainLarge,
    /// Every client over the whole of a monitor (borderless full screen), one on top of the
    /// other where several share a monitor.
    FullScreen,
}

impl Preset {
    /// The part of `monitor` this preset arranges: all of it for full screen (the taskbar's
    /// strip too, so a borderless game covers it to the pixel), else its work area.
    pub(crate) fn area(self, monitor: &Monitor) -> Rect {
        if self == Preset::FullScreen { monitor.rect } else { monitor.work }
    }
}

/// The rectangles for `count` clients on `areas` (`Preset::area` of each monitor, in the order
/// chosen), first client first. Each window keeps `aspect`, as large as its cell allows, centred
/// in the cell; a full-screen one is its area exactly.
pub(crate) fn arrange(preset: Preset, count: usize, areas: &[Rect], aspect: f32) -> Vec<Rect> {
    let aspect = if aspect.is_finite() && aspect > 0.2 && aspect < 5.0 { aspect } else { 16.0 / 9.0 };
    if count == 0 || areas.is_empty() { return Vec::new(); }
    match preset {
        Preset::Grid => grid(count, areas, aspect),
        Preset::MainLarge => main_large(count, areas, aspect),
        Preset::FullScreen => areas.iter().zip(share(count, areas)).flat_map(|(area, n)| std::iter::repeat_n(*area, n)).collect(),
    }
}

/// How many of `count` windows each area takes: in proportion to its area, every area used
/// before any gets two (so 2 clients on 2 monitors get one each).
fn share(count: usize, areas: &[Rect]) -> Vec<usize> {
    let mut shares = vec![0usize; areas.len()];
    let total: i64 = areas.iter().map(Rect::area).sum::<i64>().max(1);
    for _ in 0..count {
        // The area with the most room per window it already has.
        let next = (0..areas.len()).max_by(|&a, &b| {
            let room = |i: usize| areas[i].area() as f64 / total as f64 / (shares[i] + 1) as f64;
            room(a).total_cmp(&room(b)).then(b.cmp(&a))
        }).unwrap_or(0);
        shares[next] += 1;
    }
    shares
}

fn grid(count: usize, areas: &[Rect], aspect: f32) -> Vec<Rect> {
    let mut out = Vec::with_capacity(count);
    for (area, n) in areas.iter().zip(share(count, areas)) {
        out.extend(tile(n, area, aspect));
    }
    out
}

/// `n` equal cells on `area`, choosing the column count that gives the largest windows.
fn tile(n: usize, area: &Rect, aspect: f32) -> Vec<Rect> {
    if n == 0 || area.w <= 0 || area.h <= 0 { return Vec::new(); }
    let best = (1..=n).map(|cols| {
        let rows = n.div_ceil(cols);
        let (cw, ch) = (area.w as f32 / cols as f32, area.h as f32 / rows as f32);
        let w = cw.min(ch * aspect);
        (cols, rows, w)
    }).max_by(|a, b| a.2.total_cmp(&b.2).then(b.0.cmp(&a.0))).unwrap_or((1, n, 0.0));
    let (cols, rows, _) = best;
    let (cw, ch) = (area.w / cols as i32, area.h / rows as i32);
    (0..n).map(|i| {
        let (col, row) = ((i % cols) as i32, (i / cols) as i32);
        fit(Rect::new(area.x + col * cw, area.y + row * ch, cw, ch), aspect)
    }).collect()
}

/// The largest `aspect` rectangle centred in `cell`.
fn fit(cell: Rect, aspect: f32) -> Rect {
    let w = (cell.w as f32).min(cell.h as f32 * aspect).floor() as i32;
    let h = (w as f32 / aspect).floor() as i32;
    Rect::new(cell.x + (cell.w - w) / 2, cell.y + (cell.h - h) / 2, w.max(1), h.max(1))
}

fn main_large(count: usize, areas: &[Rect], aspect: f32) -> Vec<Rect> {
    let first = areas[0];
    if count == 1 { return vec![fit(first, aspect)]; }
    if areas.len() > 1 {
        // The main client has the first monitor; the others share the rest as a grid.
        let mut out = vec![fit(first, aspect)];
        out.extend(grid(count - 1, &areas[1..], aspect));
        return out;
    }
    // One monitor: the main window takes the left three quarters, the alts a column on the right.
    let alts = count - 1;
    let main_w = first.w * 3 / 4;
    let main = Rect { x: first.x, ..fit(Rect::new(first.x, first.y, main_w, first.h), aspect) };
    let column = Rect::new(main.right(), first.y, first.right() - main.right(), first.h);
    let mut out = vec![main];
    out.extend(tile(alts, &column, aspect));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(device: &str, rect: Rect, primary: bool) -> Monitor {
        Monitor { device: device.into(), rect, work: Rect::new(rect.x, rect.y, rect.w, rect.h - 40), dpi: 96, primary }
    }

    fn placement(rect: Rect, mon: &Monitor) -> Placement {
        Placement { rect, client: (rect.w - 16, rect.h - 39), shown: Shown::Normal, mode: Mode::Windowed, frame: Frame::default(), monitor: mon.clone() }
    }

    #[test]
    fn a_window_is_remembered_once_and_goes_back_exactly_on_its_unchanged_monitor() {
        let main = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), true);
        let side = monitor(r"\\.\DISPLAY2", Rect::new(1920, 0, 2560, 1440), false);
        let mut settings = Settings::default();
        let key = ClientKey::new("acct", Some(2));
        let p = placement(Rect::new(2000, 100, 1296, 759), &side);
        assert!(remember(&mut settings, &key, &p));
        assert!(!remember(&mut settings, &key, &p), "no change, nothing to save");
        let saved = &settings.saved["acct#2"];
        assert_eq!(restore_rect(saved, &[main.clone(), side.clone()]), Some((Rect::new(2000, 100, 1296, 759), Shown::Normal)));
        // A minimised capture keeps the last real client size.
        let mut minimised = p.clone();
        minimised.shown = Shown::Minimized;
        minimised.client = (0, 0);
        assert!(remember(&mut settings, &key, &minimised));
        assert_eq!(settings.saved["acct#2"].client, (1280, 720));
        // ...and a client never starts minimised.
        assert_eq!(restore_rect(&settings.saved["acct#2"], &[main, side]).unwrap().1, Shown::Normal);
    }

    #[test]
    fn a_missing_monitor_falls_back_to_the_primary_scaled_and_inside_its_work_area() {
        let main = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), true);
        let side = monitor(r"\\.\DISPLAY2", Rect::new(1920, 0, 2560, 1440), false);
        let mut settings = Settings::default();
        let key = ClientKey::new("acct", None);
        remember(&mut settings, &key, &placement(Rect::new(1920 + 1280, 700, 1280, 700), &side));
        let (rect, _) = restore_rect(&settings.saved["acct"], &[main.clone()]).unwrap();
        assert!(main.work.intersection(&rect) == Some(rect), "{rect:?} inside {:?}", main.work);
        assert_eq!((rect.w, rect.h), (951, 520), "uniform scale min(1920/2560, 1040/1400) from a 2560x1400 work area");
        // A DPI change on the same monitor also counts as changed and stays inside.
        let mut scaled = main.clone();
        scaled.dpi = 144;
        scaled.work = Rect::new(0, 0, 1920, 1020);
        remember(&mut settings, &key, &placement(Rect::new(100, 100, 800, 600), &main));
        let (rect, _) = restore_rect(&settings.saved["acct"], &[scaled.clone()]).unwrap();
        assert!(scaled.work.intersection(&rect) == Some(rect));
        // Monitors renumbered: the same rectangle under another device name is still found.
        let renamed = monitor(r"\\.\DISPLAY7", main.rect, true);
        assert_eq!(find_monitor(&MonitorRef::from(&main), &[side.clone(), renamed.clone()]), Some(&renamed));
        assert_eq!(restore_rect(&settings.saved["acct"], &[]), None);
    }

    #[test]
    fn fullscreen_is_never_resized_only_placed_on_its_monitor() {
        let main = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), true);
        let mut p = placement(main.rect, &main);
        p.mode = Mode::Fullscreen;
        p.client = (1920, 1080);
        let mut settings = Settings::default();
        let key = ClientKey::new("acct", None);
        remember(&mut settings, &key, &p);
        assert_eq!(restore_rect(&settings.saved["acct"], &[main.clone()]), Some((main.rect, Shown::Normal)));
        assert!(launch_values(&settings, &key).is_empty(), "a fullscreen game keeps its own resolution");
    }

    #[test]
    fn a_window_that_covers_its_monitor_goes_back_to_the_pixel() {
        // The taskbar stays on top of a game that no longer covers the screen: none of these may
        // come back a pixel off, or inside the work area (the screen less the taskbar).
        let key = ClientKey::new("acct", None);
        // Monitors left of and above the primary have negative coordinates.
        for rect in [Rect::new(0, 0, 3840, 2160), Rect::new(-2560, -200, 2560, 1440), Rect::new(1920, 0, 1920, 1080), Rect::new(0, -1080, 1920, 1080)] {
            let mon = monitor(r"\\.\DISPLAY2", rect, false);
            assert!(mon.work != mon.rect, "this monitor has a taskbar");
            // FFXI's borderless window at the monitor's size.
            let mut settings = Settings::default();
            let mut full = placement(mon.rect, &mon);
            (full.mode, full.client) = (Mode::Fullscreen, (rect.w, rect.h));
            remember(&mut settings, &key, &full);
            assert_eq!(restore_rect(&settings.saved["acct"], &[mon.clone()]), Some((mon.rect, Shown::Normal)), "{rect:?}");
            // A borderless window one pixel larger all round, and a windowed game with its frame
            // pushed off the screen so its picture fills it: both reach under the taskbar.
            for over in [Rect::new(rect.x - 1, rect.y - 1, rect.w + 2, rect.h + 2), Rect::new(rect.x - 8, rect.y - 31, rect.w + 16, rect.h + 39)] {
                let mut p = placement(over, &mon);
                (p.mode, p.client) = (Mode::Borderless, (rect.w, rect.h));
                remember(&mut settings, &key, &p);
                assert_eq!(restore_rect(&settings.saved["acct"], &[mon.clone()]), Some((over, Shown::Normal)), "{over:?} on {rect:?}");
                assert!(covers(&over, &mon.rect).is_some());
                // Its size is not served back as the game's resolution (a game Windows scales
                // reads larger than the screen, and would start larger still each time).
                assert!(launch_values(&settings, &key).is_empty(), "{over:?}");
                // As the game opens it over its whole monitor it is left alone, whatever covering
                // rectangle is remembered; a remembered tile, or another monitor, still moves it.
                assert!(stays_full_screen(&full, &over) && stays_full_screen(&p, &mon.rect) && stays_full_screen(&p, &over));
                assert!(!stays_full_screen(&p, &Rect::new(rect.x + 16, rect.y + 16, 928, 522)));
                assert!(!stays_full_screen(&p, &Rect::new(rect.x + 10_000, rect.y, rect.w, rect.h)));
                assert!(!stays_full_screen(&placement(Rect::new(rect.x + 16, rect.y + 16, 928, 522), &mon), &mon.rect), "a tile goes back to full screen");
                assert!(!stays_full_screen(&Placement { shown: Shown::Minimized, ..p.clone() }, &over));
                // The monitor changed (another resolution, the taskbar moved): the window still
                // covers all of it, by the same margins.
                let changed = monitor(r"\\.\DISPLAY2", Rect::new(rect.x, rect.y, 2560, 1600), false);
                let (back, _) = restore_rect(&settings.saved["acct"], &[changed.clone()]).unwrap();
                assert_eq!(covers(&back, &changed.rect), covers(&over, &mon.rect), "{back:?} on {:?}", changed.rect);
            }
        }
        // An ordinary window reaching under the taskbar also goes back exactly on its unchanged
        // monitor; only on a changed one is it brought inside the work area.
        let mon = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), true);
        let mut settings = Settings::default();
        let low = Rect::new(600, 500, 1296, 580);
        remember(&mut settings, &key, &placement(low, &mon));
        assert_eq!(restore_rect(&settings.saved["acct"], &[mon.clone()]), Some((low, Shown::Normal)));
        assert_eq!(covers(&low, &mon.rect), None);
        assert_eq!(covers(&mon.work, &mon.rect), None, "the work area does not cover the monitor");
        assert_eq!(covers(&mon.rect, &mon.rect), Some((0, 0, 0, 0)));
        let smaller = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1280, 720), true);
        let (inside, _) = restore_rect(&settings.saved["acct"], &[smaller.clone()]).unwrap();
        assert_eq!(smaller.work.intersection(&inside), Some(inside));
    }

    #[test]
    fn launch_values_serve_only_the_start_size_the_user_chose() {
        let main = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), true);
        let monitors = [main.clone()];
        let mut settings = Settings::default();
        let key = ClientKey::new("acct", Some(1));
        assert!(launch_values(&settings, &key).is_empty());
        // The game runs windowed at its own 1920x1080 and is put on a tile (Arrange all, Apply)
        // or resized by hand: the window is remembered, to go back there, and the game still
        // starts at its own resolution. Served as the resolution, as in 1.2.0 to 1.2.4, the
        // next start would be at 944x501 and everything placed by pixel (FFXIDB's minimap) off.
        remember(&mut settings, &key, &placement(Rect::new(0, 0, 960, 540), &main));
        assert_eq!(settings.saved["acct#1"].client, (944, 501));
        assert!(settings.start.is_empty());
        assert!(launch_values(&settings, &key).is_empty(), "a remembered window is never a resolution");
        // The file a Crystal 1.2.0 to 1.2.4 left (remembered windows, no start sizes) reads the same.
        let old: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(old.start.is_empty() && launch_values(&old, &key).is_empty());
        // The user places the closed game's tile: that is its start size, frame taken off.
        assert!(place(&mut settings, &key, Rect::new(0, 0, 1296, 759), &monitors));
        assert_eq!(settings.start["acct#1"], Start { client: (1280, 720), mode: Mode::Windowed });
        assert_eq!(launch_values(&settings, &key), vec![("0001", 1280), ("0002", 720), ("0034", 1)]);
        // Running again, moved and stretched to another tile: remembered there, same start size.
        assert!(remember(&mut settings, &key, &placement(Rect::new(960, 0, 960, 540), &main)));
        assert_eq!(settings.saved["acct#1"].rect, Rect::new(960, 0, 960, 540));
        assert_eq!(launch_values(&settings, &key), vec![("0001", 1280), ("0002", 720), ("0034", 1)]);
        // It survives the settings file, and is forgotten with the window.
        let again: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(again, settings);
        forget(&mut settings, &key);
        assert!(settings.saved.is_empty() && settings.start.is_empty() && launch_values(&settings, &key).is_empty());
        // A client with a slot falls back to the account's slotless start size.
        let mut plain = Settings::default();
        assert!(place(&mut plain, &ClientKey::new("acct", None), Rect::new(0, 0, 1280, 720), &monitors));
        assert_eq!(launch_values(&plain, &key), vec![("0001", 1280), ("0002", 720), ("0034", 3)]);
        plain.launch_at_saved_size = false;
        assert!(launch_values(&plain, &key).is_empty());
        // A tile smaller than the game can be is a window to go back to, with no start size.
        let mut small = Settings::default();
        assert!(place(&mut small, &key, Rect::new(0, 0, 300, 200), &monitors));
        assert!(small.start.is_empty() && launch_values(&small, &key).is_empty());
    }

    #[test]
    fn grid_fits_every_client_without_overlap_and_keeps_aspect() {
        let areas = [Rect::new(0, 0, 1920, 1040), Rect::new(1920, 0, 2560, 1400)];
        for n in 1..=12 {
            let rects = arrange(Preset::Grid, n, &areas, 16.0 / 9.0);
            assert_eq!(rects.len(), n);
            for (i, a) in rects.iter().enumerate() {
                assert!(areas.iter().any(|area| area.intersection(a) == Some(*a)), "{n}: {a:?} inside a monitor");
                assert!(((a.w as f32 / a.h as f32) - 16.0 / 9.0).abs() < 0.02, "{n}: aspect of {a:?}");
                for b in &rects[i + 1..] { assert!(a.intersection(b).is_none(), "{n}: {a:?} overlaps {b:?}"); }
            }
        }
        // Two clients on two monitors: one each, each filling its monitor's width or height.
        let two = arrange(Preset::Grid, 2, &areas, 16.0 / 9.0);
        assert!(two[0].x < 1920 && two[1].x >= 1920);
        // Four on one 1080p monitor: a 2x2 grid of 960x540.
        let four = arrange(Preset::Grid, 4, &[Rect::new(0, 0, 1920, 1080)], 16.0 / 9.0);
        assert!(four.iter().all(|r| (r.w, r.h) == (960, 540)), "{four:?}");
        assert!(arrange(Preset::Grid, 0, &areas, 1.7).is_empty() && arrange(Preset::Grid, 3, &[], 1.7).is_empty());
    }

    #[test]
    fn main_large_puts_the_first_client_large_and_tiles_the_rest() {
        let one = [Rect::new(0, 0, 1920, 1080)];
        let rects = arrange(Preset::MainLarge, 4, &one, 16.0 / 9.0);
        assert_eq!(rects.len(), 4);
        assert!(rects[1..].iter().all(|r| r.area() < rects[0].area() && r.x >= rects[0].right()));
        for (i, a) in rects.iter().enumerate() {
            assert!(one[0].intersection(a) == Some(*a));
            for b in &rects[i + 1..] { assert!(a.intersection(b).is_none(), "{a:?} overlaps {b:?}"); }
        }
        // Two monitors: the main client fills the first, the others share the second.
        let two = [Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1920, 1080)];
        let rects = arrange(Preset::MainLarge, 3, &two, 16.0 / 9.0);
        assert_eq!(rects[0], Rect::new(0, 0, 1920, 1080));
        assert!(rects[1..].iter().all(|r| r.x >= 1920));
        assert_eq!(arrange(Preset::MainLarge, 1, &one, 16.0 / 9.0), vec![Rect::new(0, 0, 1920, 1080)]);
    }

    #[test]
    fn full_screen_puts_every_client_over_a_whole_monitor() {
        let main = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 3840, 2160), true);
        let side = monitor(r"\\.\DISPLAY2", Rect::new(3840, 0, 1920, 1080), false);
        assert_eq!(Preset::FullScreen.area(&main), main.rect, "the taskbar's strip too");
        assert_eq!(Preset::Grid.area(&main), main.work);
        assert_eq!(Preset::MainLarge.area(&main), main.work);
        // One monitor: all five over it, exactly.
        assert_eq!(arrange(Preset::FullScreen, 5, &[main.rect], 16.0 / 9.0), vec![main.rect; 5]);
        // Two alike: each has one before either has two. A much smaller one has its share by size.
        let twin = Rect::new(3840, 0, 3840, 2160);
        assert_eq!(arrange(Preset::FullScreen, 3, &[main.rect, twin], 16.0 / 9.0), vec![main.rect, main.rect, twin]);
        let both = arrange(Preset::FullScreen, 5, &[main.rect, side.rect], 16.0 / 9.0);
        assert_eq!(both, vec![main.rect, main.rect, main.rect, main.rect, side.rect]);
        assert!(arrange(Preset::FullScreen, 0, &[main.rect], 1.7).is_empty());
    }

    #[test]
    fn a_tile_placed_in_the_editor_is_the_clients_remembered_window() {
        // A mixed-DPI desktop: a 4K monitor at 150 % and a 1080p one at 100 %.
        let main = Monitor { dpi: 144, ..monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 3840, 2160), true) };
        let side = monitor(r"\\.\DISPLAY2", Rect::new(3840, 0, 1920, 1080), false);
        let monitors = [main.clone(), side.clone()];
        let mut settings = Settings::default();
        let key = ClientKey::new("acct", Some(2));
        // Nothing remembered yet: a borderless window at the tile, on the monitor it is on.
        let tile = Rect::new(4800, 0, 960, 540);
        assert!(place(&mut settings, &key, tile, &monitors));
        assert!(!place(&mut settings, &key, tile, &monitors), "no change, nothing to save");
        let saved = settings.saved["acct#2"].clone();
        assert_eq!((saved.rect, saved.client, saved.mode, saved.shown, saved.frame), (tile, (960, 540), Mode::Borderless, Shown::Normal, None));
        assert_eq!(saved.monitor, MonitorRef::from(&side), "the 1080p monitor, with its own scaling");
        assert_eq!(restore_rect(&saved, &monitors), Some((tile, Shown::Normal)), "it starts exactly there");
        assert_eq!(launch_values(&settings, &key), vec![("0001", 960), ("0002", 540), ("0034", 3)]);
        // Moved over the whole of the other monitor: that monitor's full screen, to the pixel.
        assert!(place(&mut settings, &key, main.rect, &monitors));
        let full = settings.saved["acct#2"].clone();
        assert_eq!((full.rect, full.mode, full.monitor.dpi), (main.rect, Mode::Fullscreen, 144));
        assert_eq!(restore_rect(&full, &monitors), Some((main.rect, Shown::Normal)));
        assert!(launch_values(&settings, &key).is_empty(), "a game over its whole monitor keeps its own resolution");
        assert!(settings.start.is_empty(), "the tile's start size went with it");
        // A windowed game keeps its caption and its frame: the tile is the window, frame included.
        let mut windowed = Settings::default();
        remember(&mut windowed, &key, &placement(Rect::new(100, 100, 1296, 759), &main));
        assert!(place(&mut windowed, &key, Rect::new(0, 0, 1920, 1044), &monitors));
        let saved = &windowed.saved["acct#2"];
        assert_eq!((saved.rect, saved.client, saved.mode, saved.frame), (Rect::new(0, 0, 1920, 1044), (1904, 1005), Mode::Windowed, Some(Frame::default())));
        // A client with a slot and nothing of its own starts from its account's window.
        let mut shared = Settings::default();
        remember(&mut shared, &ClientKey::new("acct", None), &placement(Rect::new(100, 100, 1296, 759), &main));
        assert!(place(&mut shared, &key, Rect::new(3840, 0, 1296, 759), &monitors));
        assert_eq!(shared.saved["acct#2"].mode, Mode::Windowed);
        assert_eq!(shared.saved["acct"].rect, Rect::new(100, 100, 1296, 759), "the account's own stays");
        // It survives the settings file.
        let text = serde_json::to_string(&shared).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), shared);
        // No monitors, or no size: nothing is remembered.
        assert!(!place(&mut settings, &key, tile, &[]));
        assert!(!place(&mut settings, &key, Rect::new(0, 0, 0, 100), &monitors));
        assert_eq!(monitor_for(&Rect::new(3000, 500, 1280, 720), &monitors), Some(&main), "astride two: the one it is mostly on");
    }
}
