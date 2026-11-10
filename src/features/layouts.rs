//! Window layouts: each client's remembered window (where, how big, which monitor, which mode),
//! restored when its game starts, and the "arrange all" presets.
//!
//! How Crystal avoids fighting FFXI: the game sizes its window from its own resolution values
//! (`0001`/`0002`, `0034` for the mode), which Crystal already serves to each game in-process
//! (`ffxi_settings`). With `launch_at_saved_size` on, a client with a remembered windowed or
//! borderless layout starts with those values set to its remembered client size and mode
//! (`launch_values`), so the game itself opens at that size and Crystal only moves the window
//! into place. A fullscreen game is never resized: at most it is moved to its monitor.
//!
//! Everything here is pure (monitors in, rectangles out) and unit-tested; `runtime` applies it.

use super::game_window::{Mode, Monitor, Placement, Rect, Shown};
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
    /// Serve the remembered client size and mode as the game's own resolution at launch.
    pub launch_at_saved_size: bool,
    /// The aspect ratio presets keep for each window (width / height).
    pub aspect: f32,
    /// Put the game windows' taskbar buttons and Alt+Tab order in Home order when a Play has
    /// brought its windows up, and when the Home order changes while games run (`order`).
    pub order_windows: bool,
    /// Remembered windows by `ClientKey::id`.
    pub saved: BTreeMap<String, Saved>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { remember: true, restore: true, launch_at_saved_size: true, aspect: 16.0 / 9.0, order_windows: true, saved: BTreeMap::new() }
    }
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
    pub monitor: MonitorRef,
}

impl Saved {
    pub(crate) fn from_placement(p: &Placement) -> Self {
        Self { rect: p.rect, client: p.client, shown: p.shown, mode: p.mode, monitor: MonitorRef::from(&p.monitor) }
    }
}

/// Remember `placement` for `key`. Whether anything changed (nothing is saved for no change). A
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

/// Where to put a remembered window now. On its own unchanged monitor it goes back exactly.
/// On a monitor that changed (resolution, DPI, or a missing monitor replaced by the primary) the
/// position and size scale with the work area, are clamped inside it, and a fullscreen window is
/// placed over its new monitor whole.
pub(crate) fn restore_rect(saved: &Saved, monitors: &[Monitor]) -> Option<(Rect, Shown)> {
    let monitor = find_monitor(&saved.monitor, monitors)?;
    let shown = if saved.shown == Shown::Minimized { Shown::Normal } else { saved.shown };
    if saved.mode == Mode::Fullscreen { return Some((monitor.rect, Shown::Normal)); }
    let same = monitor.rect == saved.monitor.rect && monitor.work == saved.monitor.work && monitor.dpi == saved.monitor.dpi;
    let rect = if same { saved.rect } else {
        let (from, to) = (saved.monitor.work, monitor.work);
        let sx = f64::from(to.w) / f64::from(from.w.max(1));
        let sy = f64::from(to.h) / f64::from(from.h.max(1));
        // Uniform scale keeps the game's aspect; position keeps its share of the work area.
        let scale = sx.min(sy);
        Rect::new(
            to.x + (f64::from(saved.rect.x - from.x) * sx).round() as i32,
            to.y + (f64::from(saved.rect.y - from.y) * sy).round() as i32,
            (f64::from(saved.rect.w) * scale).round() as i32,
            (f64::from(saved.rect.h) * scale).round() as i32,
        )
    };
    Some((clamp_into(rect, &monitor.work), shown))
}

/// `rect` moved (and shrunk only if larger) so it lies inside `area`.
pub(crate) fn clamp_into(rect: Rect, area: &Rect) -> Rect {
    let w = rect.w.min(area.w).max(1);
    let h = rect.h.min(area.h).max(1);
    let x = rect.x.clamp(area.x, area.right() - w);
    let y = rect.y.clamp(area.y, area.bottom() - h);
    Rect::new(x, y, w, h)
}

/// The resolution values a client's game should start with, from its remembered window: the
/// client size (`0001`/`0002`) and mode (`0034`: 1 window, 3 borderless). Empty when nothing is
/// remembered, the setting is off, or the game was fullscreen (its own resolution wins).
pub(crate) fn launch_values(settings: &Settings, key: &ClientKey) -> Vec<(&'static str, u32)> {
    if !settings.launch_at_saved_size { return Vec::new(); }
    let Some(saved) = settings.saved.get(&key.id()).or_else(|| settings.saved.get(&ClientKey::new(key.account.clone(), None).id())) else { return Vec::new() };
    let (w, h) = saved.client;
    if w < 320 || h < 240 { return Vec::new(); }
    let mode = match saved.mode { Mode::Windowed => 1, Mode::Borderless => 3, Mode::Fullscreen => return Vec::new() };
    vec![("0001", w as u32), ("0002", h as u32), ("0034", mode)]
}

/// The arrange-all presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Preset {
    /// Every client in an even grid across the chosen monitors.
    Grid,
    /// The first client large, the others tiled beside it (or on the other monitors).
    MainLarge,
}

/// The rectangles for `count` clients on `areas` (work areas, in the order chosen), first client
/// first. Each window keeps `aspect`, as large as its cell allows, centred in the cell.
pub(crate) fn arrange(preset: Preset, count: usize, areas: &[Rect], aspect: f32) -> Vec<Rect> {
    let aspect = if aspect.is_finite() && aspect > 0.2 && aspect < 5.0 { aspect } else { 16.0 / 9.0 };
    if count == 0 || areas.is_empty() { return Vec::new(); }
    match preset {
        Preset::Grid => grid(count, areas, aspect),
        Preset::MainLarge => main_large(count, areas, aspect),
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
        Placement { rect, client: (rect.w - 16, rect.h - 39), shown: Shown::Normal, mode: Mode::Windowed, monitor: mon.clone() }
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
    fn launch_values_serve_the_remembered_client_size_and_mode() {
        let main = monitor(r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), true);
        let mut settings = Settings::default();
        let key = ClientKey::new("acct", Some(1));
        assert!(launch_values(&settings, &key).is_empty());
        remember(&mut settings, &key, &placement(Rect::new(0, 0, 1296, 759), &main));
        assert_eq!(launch_values(&settings, &key), vec![("0001", 1280), ("0002", 720), ("0034", 1)]);
        // A client with a slot falls back to the account's slotless layout.
        let mut plain = Settings::default();
        let mut border = placement(Rect::new(0, 0, 1280, 720), &main);
        border.mode = Mode::Borderless;
        border.client = (1280, 720);
        remember(&mut plain, &ClientKey::new("acct", None), &border);
        assert_eq!(launch_values(&plain, &key), vec![("0001", 1280), ("0002", 720), ("0034", 3)]);
        plain.launch_at_saved_size = false;
        assert!(launch_values(&plain, &key).is_empty());
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
    fn old_files_load_with_defaults() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert!(s.remember && s.restore && s.launch_at_saved_size);
    }
}
