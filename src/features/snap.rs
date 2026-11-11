//! The window layout editor's geometry (Features, Windows): the monitor map, what a press on it
//! takes hold of, and where a tile being moved or resized goes. Every rectangle is in physical
//! screen pixels, as `game_window` reads them and the remembered windows keep them, so a tile
//! dragged between monitors of different scaling stays exact; the map only draws them smaller.
//!
//! While a tile is dragged:
//! - its edges take the edges of the monitor it is mostly on and of that monitor's work area (the
//!   taskbar's edge), the halves, thirds and quarters of the work area, and the other tiles'
//!   edges, so windows abut with no gap and no overlap; its centre takes the work area's or
//!   another tile's centre;
//! - a moved tile is taken only along the way it has been moved: carried straight down, it
//!   does not jump sideways to a line that happens to be near;
//! - moved with the pointer at a monitor's top edge it fills that monitor exactly (borderless
//!   full screen), at a side edge a half of it, in a corner a quarter (`Part`), on the edges no
//!   other monitor adjoins;
//! - the nearest line wins, and the stronger one when two are about as near. A line once taken
//!   is kept until the tile is well clear of it, so a tile never flickers between two lines.
//!
//! Pure and unit-tested. Nothing here runs between drags.

use super::game_window::{Monitor, Rect};

/// The smallest tile: the smallest game window Crystal starts a game at (`layouts::launch_values`).
pub(crate) const MIN_W: i32 = 320;
pub(crate) const MIN_H: i32 = 240;

/// A monitor as the editor needs it: its rectangle and its work area (less the taskbar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Screen {
    pub rect: Rect,
    pub work: Rect,
}

impl From<&Monitor> for Screen {
    fn from(m: &Monitor) -> Self { Self { rect: m.rect, work: m.work } }
}

/// The rectangle around every monitor. `None` with no monitors.
pub(crate) fn bounds(screens: &[Screen]) -> Option<Rect> {
    let first = screens.first()?.rect;
    let (left, top) = screens.iter().fold((first.x, first.y), |(x, y), s| (x.min(s.rect.x), y.min(s.rect.y)));
    let (right, bottom) = screens.iter().fold((first.right(), first.bottom()), |(x, y), s| (x.max(s.rect.right()), y.max(s.rect.bottom())));
    (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
}

/// The monitor map: `bounds` (the rectangle around every monitor) drawn `scale` map pixels to
/// the screen pixel, its top-left at the map's origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Map {
    pub bounds: Rect,
    pub scale: f64,
}

impl Map {
    /// `bounds` drawn `width` map pixels wide.
    pub(crate) fn new(bounds: Rect, width: f64) -> Self {
        Self { bounds, scale: if bounds.w > 0 && width > 0.0 { width / f64::from(bounds.w) } else { 1.0 } }
    }

    /// The map point (map pixels) as a screen point.
    #[cfg(test)]
    pub(crate) fn point(&self, x: f64, y: f64) -> (i32, i32) {
        (self.bounds.x + (x / self.scale).round() as i32, self.bounds.y + (y / self.scale).round() as i32)
    }

    /// A screen rectangle in map pixels: (x, y, width, height).
    #[cfg(test)]
    pub(crate) fn to_map(&self, r: &Rect) -> [f64; 4] {
        [f64::from(r.x - self.bounds.x) * self.scale, f64::from(r.y - self.bounds.y) * self.scale, f64::from(r.w) * self.scale, f64::from(r.h) * self.scale]
    }

    /// A map rectangle as a screen rectangle: each edge goes to its nearest screen pixel, so
    /// what `to_map` drew comes back to the pixel, and two rectangles that share an edge on the
    /// map share it on the screen.
    #[cfg(test)]
    pub(crate) fn from_map(&self, m: [f64; 4]) -> Rect {
        let (left, top) = self.point(m[0], m[1]);
        let (right, bottom) = self.point(m[0] + m[2], m[1] + m[3]);
        Rect::new(left, top, right - left, bottom - top)
    }

    /// A screen rectangle as fractions of the map (what the page draws from).
    pub(crate) fn fractions(&self, r: &Rect) -> [f32; 4] {
        let (w, h) = (f64::from(self.bounds.w.max(1)), f64::from(self.bounds.h.max(1)));
        [(f64::from(r.x - self.bounds.x) / w) as f32, (f64::from(r.y - self.bounds.y) / h) as f32, (f64::from(r.w) / w) as f32, (f64::from(r.h) / h) as f32]
    }

    /// The screen point at the fractions (`fx`, `fy`) of the map (where the page says the pointer is).
    pub(crate) fn at(&self, fx: f32, fy: f32) -> (i32, i32) {
        (self.bounds.x + (f64::from(fx) * f64::from(self.bounds.w)).round() as i32, self.bounds.y + (f64::from(fy) * f64::from(self.bounds.h)).round() as i32)
    }

    /// `px` map pixels as a distance on the screen (at least one pixel).
    pub(crate) fn reach(&self, px: f64) -> i32 { ((px / self.scale).round() as i32).max(1) }
}

/// What a press took hold of: the edges it drags, or the whole tile (none).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Grab {
    pub left: bool,
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
}

impl Grab {
    pub(crate) const MOVE: Grab = Grab { left: false, top: false, right: false, bottom: false };
    pub(crate) fn moves(self) -> bool { self == Self::MOVE }
}

/// The tile a press at `at` takes hold of, and how. The selected tile comes first (it is drawn
/// on top, and its edges can be taken from `reach` outside it too), then the others from the
/// last drawn. An edge is taken within `reach` of it, but never more than a quarter of the tile
/// in, so the middle of the smallest tile still moves it.
pub(crate) fn hit(tiles: &[Rect], selected: Option<usize>, at: (i32, i32), reach: i32) -> Option<(usize, Grab)> {
    let (x, y) = at;
    let order = selected.filter(|i| *i < tiles.len()).into_iter().chain((0..tiles.len()).rev().filter(|i| Some(*i) != selected));
    for index in order {
        let r = tiles[index];
        let out = if Some(index) == selected { reach } else { 0 };
        if x < r.x - out || x >= r.right() + out || y < r.y - out || y >= r.bottom() + out { continue; }
        let (ex, ey) = (reach.min(r.w / 4), reach.min(r.h / 4));
        let grab = Grab { left: x < r.x + ex, top: y < r.y + ey, right: x >= r.right() - ex, bottom: y >= r.bottom() - ey };
        return Some((index, grab));
    }
    None
}

/// The tiles under `at`, the last drawn first.
pub(crate) fn under(tiles: &[Rect], at: (i32, i32)) -> Vec<usize> {
    (0..tiles.len()).rev().filter(|i| tiles[*i].contains(at.0, at.1)).collect()
}

/// The monitor `rect` is mostly on: the one it covers most of, else the nearest to its centre.
pub(crate) fn screen_of(rect: &Rect, screens: &[Screen]) -> Option<usize> {
    let covered = (0..screens.len()).filter_map(|i| Some((i, screens[i].rect.intersection(rect)?.area()))).max_by_key(|(i, area)| (*area, std::cmp::Reverse(*i)));
    if let Some((index, _)) = covered { return Some(index); }
    let (cx, cy) = (rect.x + rect.w / 2, rect.y + rect.h / 2);
    (0..screens.len()).min_by_key(|i| distance(&screens[*i].rect, cx, cy))
}

/// How far (`x`, `y`) is from `r` (0 inside it), squared.
fn distance(r: &Rect, x: i32, y: i32) -> i64 {
    let dx = i64::from((r.x - x).max(x - (r.right() - 1)).max(0));
    let dy = i64::from((r.y - y).max(y - (r.bottom() - 1)).max(0));
    dx * dx + dy * dy
}

/// `rect` kept on the monitors: inside the rectangle around them all (smaller only when it is
/// larger than that), and with its centre on a monitor, so a tile is never left in the corner
/// no monitor covers.
pub(crate) fn confine(rect: Rect, screens: &[Screen]) -> Rect {
    let Some(all) = bounds(screens) else { return rect };
    let mut r = super::layouts::clamp_into(rect, &all);
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    if screens.iter().any(|s| s.rect.contains(cx, cy)) { return r; }
    if let Some(near) = (0..screens.len()).min_by_key(|i| distance(&screens[*i].rect, cx, cy)).map(|i| screens[i].rect) {
        r.x += cx.clamp(near.x, near.right() - 1) - cx;
        r.y += cy.clamp(near.y, near.bottom() - 1) - cy;
    }
    r
}

/// `rect` moved by (`dx`, `dy`) and kept on the monitors (the arrow keys).
pub(crate) fn nudge(rect: Rect, dx: i32, dy: i32, screens: &[Screen]) -> Rect {
    confine(Rect::new(rect.x + dx, rect.y + dy, rect.w, rect.h), screens)
}

/// A snap line drawn while it holds the tile: across `from`..`to` at `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Guide {
    /// A vertical line (`at` is its x), else a horizontal one (`at` is its y).
    pub vertical: bool,
    pub at: i32,
    pub from: i32,
    pub to: i32,
}

/// Where the dragged tile is now, and the lines holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub rect: Rect,
    pub guides: Vec<Guide>,
}

/// How much a line pulls: the weaker lose to a stronger one about as near.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    Centre,
    Fraction,
    Edge,
}

/// A line a tile can take, along one axis. Positions are doubled so a centre has no half pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Line {
    at2: i32,
    strength: Strength,
    /// A centre line: only the tile's centre takes it.
    centre: bool,
    /// What it belongs to, across the axis (its guide is drawn over that and the tile).
    from: i32,
    to: i32,
}

/// The part of the tile on a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Start,
    End,
    Centre,
}

/// The line an axis holds between pointer moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hold {
    anchor: Anchor,
    at2: i32,
}

/// A part of a monitor a moved tile fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Full,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Part {
    /// The rectangle: the whole monitor for `Full` (so a borderless game covers it to the
    /// pixel, the taskbar's strip too), else that part of its work area.
    pub(crate) fn rect(self, screen: &Screen) -> Rect {
        let w = screen.work;
        let (half_w, half_h) = (w.w / 2, w.h / 2);
        match self {
            Part::Full => screen.rect,
            Part::Left => Rect::new(w.x, w.y, half_w, w.h),
            Part::Right => Rect::new(w.x + half_w, w.y, w.w - half_w, w.h),
            Part::TopLeft => Rect::new(w.x, w.y, half_w, half_h),
            Part::TopRight => Rect::new(w.x + half_w, w.y, w.w - half_w, half_h),
            Part::BottomLeft => Rect::new(w.x, w.y + half_h, half_w, w.h - half_h),
            Part::BottomRight => Rect::new(w.x + half_w, w.y + half_h, w.w - half_w, w.h - half_h),
        }
    }
}

/// The part of a monitor the pointer at `at` asks for: within `depth` of its top edge the whole
/// of it, of a side edge that half, in a corner that quarter. Only edges no other monitor
/// adjoins there count, so a tile carried from one monitor to the next passes freely.
fn part_at(screens: &[Screen], at: (i32, i32), depth: i32) -> Option<(usize, Part)> {
    let (x, y) = at;
    let index = (0..screens.len()).find(|i| screens[*i].rect.contains(x, y))?;
    let r = screens[index].rect;
    let other = |px: i32, py: i32| screens.iter().enumerate().any(|(i, s)| i != index && s.rect.contains(px, py));
    let near = |d: i32, reach: i32| d < reach;
    let (dl, dr, dt, db) = (x - r.x, r.right() - 1 - x, y - r.y, r.bottom() - 1 - y);
    let left = |reach| near(dl, reach) && !other(r.x - 1, y);
    let right = |reach| near(dr, reach) && !other(r.right(), y);
    let top = |reach| near(dt, reach) && !other(x, r.y - 1);
    let bottom = |reach| near(db, reach) && !other(x, r.bottom());
    let corner = |a: &dyn Fn(i32) -> bool, b: &dyn Fn(i32) -> bool| (a(depth) && b(2 * depth)) || (a(2 * depth) && b(depth));
    let part = if corner(&left, &top) { Part::TopLeft }
        else if corner(&right, &top) { Part::TopRight }
        else if corner(&left, &bottom) { Part::BottomLeft }
        else if corner(&right, &bottom) { Part::BottomRight }
        else if top(depth) { Part::Full }
        else if left(depth) { Part::Left }
        else if right(depth) { Part::Right }
        else { return None };
    Some((index, part))
}

/// The lines along one axis (`vertical`: lines at an x, which a tile's left edge, right edge
/// or centre takes) for a tile at `tile`: those of the monitor it is mostly on (`screen_of`; a
/// tile that only reaches onto the next monitor takes none of that one's), and of every other
/// tile. A monitor gives its own edges and its work area's, and the halves, thirds, quarters
/// and centre of its work area (where windows are tiled; one set, so no two lines lie a
/// taskbar's fraction apart).
fn lines(screens: &[Screen], others: &[Rect], vertical: bool, tile: &Rect) -> Vec<Line> {
    // (start, length) along the axis, and across it.
    let along = |r: &Rect| if vertical { (r.x, r.w) } else { (r.y, r.h) };
    let across = |r: &Rect| if vertical { (r.y, r.h) } else { (r.x, r.w) };
    let mut out = Vec::with_capacity(screens.len() * 10 + others.len() * 3);
    if let Some(screen) = screen_of(tile, screens).map(|index| &screens[index]) {
        let (from, length) = across(&screen.rect);
        let to = from + length;
        let (start, len) = along(&screen.rect);
        let (work_start, work_len) = along(&screen.work);
        for edge in [start, start + len, work_start, work_start + work_len] {
            out.push(Line { at2: 2 * edge, strength: Strength::Edge, centre: false, from, to });
        }
        for parts in [2, 3, 4] {
            for k in 1..parts {
                let at = work_start + (work_len * k + parts / 2) / parts;
                out.push(Line { at2: 2 * at, strength: Strength::Fraction, centre: false, from, to });
            }
        }
        out.push(Line { at2: 2 * work_start + work_len, strength: Strength::Centre, centre: true, from, to });
    }
    for other in others {
        let (from, length) = across(other);
        let to = from + length;
        let (start, len) = along(other);
        out.push(Line { at2: 2 * start, strength: Strength::Edge, centre: false, from, to });
        out.push(Line { at2: 2 * (start + len), strength: Strength::Edge, centre: false, from, to });
        out.push(Line { at2: 2 * start + len, strength: Strength::Centre, centre: true, from, to });
    }
    out
}

/// What taking `line` costs a tile part `distance2` (doubled pixels) from it: four a pixel, and
/// a quarter of the reach more for each step a line is weaker than an edge.
fn cost(line: &Line, distance2: i32, reach: i32) -> i32 {
    2 * distance2 + match line.strength { Strength::Edge => 0, Strength::Fraction => reach, Strength::Centre => 2 * reach }
}

/// The line one axis takes: the one it holds while the tile is within one and a half times the
/// reach of it and no other line is better by more than a quarter of the reach; else the best
/// line within reach (`cost`). `parts` are the tile's parts that may take a line, with where
/// each is (doubled).
fn solve(lines: &[Line], parts: &[(Anchor, i32)], hold: &mut Option<Hold>, reach: i32) -> Option<(Anchor, Line)> {
    let takes = |anchor: Anchor, line: &Line| line.centre == (anchor == Anchor::Centre);
    let mut fresh: Option<(i32, Anchor, Line)> = None;
    for (anchor, pos2) in parts {
        for line in lines.iter().filter(|line| takes(*anchor, line)) {
            let d2 = (line.at2 - pos2).abs();
            if d2 > 2 * reach { continue; }
            let c = cost(line, d2, reach);
            let better = fresh.as_ref().is_none_or(|(best, _, old)| (c, std::cmp::Reverse(line.strength), line.at2) < (*best, std::cmp::Reverse(old.strength), old.at2));
            if better { fresh = Some((c, *anchor, *line)); }
        }
    }
    let held = hold.and_then(|h| {
        let pos2 = parts.iter().find(|(anchor, _)| *anchor == h.anchor)?.1;
        let line = lines.iter().filter(|line| line.at2 == h.at2 && takes(h.anchor, line)).max_by_key(|line| line.strength)?;
        let d2 = (line.at2 - pos2).abs();
        (d2 <= 3 * reach).then(|| (cost(line, d2, reach), h.anchor, *line))
    });
    let taken = match (held, fresh) {
        (Some(held), Some(fresh)) if (fresh.1, fresh.2.at2) != (held.1, held.2.at2) && fresh.0 + reach < held.0 => Some(fresh),
        (Some(held), _) => Some(held),
        (None, fresh) => fresh,
    };
    *hold = taken.map(|(_, anchor, line)| Hold { anchor, at2: line.at2 });
    taken.map(|(_, anchor, line)| (anchor, line))
}

/// One drag of a tile: what was taken hold of, where the tile was, and what it holds between
/// pointer moves.
#[derive(Clone, Debug)]
pub(crate) struct Drag {
    grab: Grab,
    start: Rect,
    hold_x: Option<Hold>,
    hold_y: Option<Hold>,
    part: Option<(usize, Part)>,
    /// The pointer has been clear of every monitor edge since the press: a tile taken hold of
    /// at a monitor's edge does not fill the monitor before it has moved away from it.
    armed: bool,
    /// The pointer has gone at least half the reach sideways, and up or down, since the press:
    /// a moved tile takes lines only along a way it has been moved.
    across: bool,
    down: bool,
}

impl Drag {
    pub(crate) fn new(grab: Grab, start: Rect) -> Self {
        Self { grab, start, hold_x: None, hold_y: None, part: None, armed: false, across: false, down: false }
    }

    pub(crate) fn start(&self) -> Rect { self.start }

    /// The tile with the pointer (`dx`, `dy`) from where it was pressed, now at `pointer`.
    /// `others` are the tiles it can line up with, `reach` how near a line takes it (screen
    /// pixels), and `snap` false places it freely (Alt).
    pub(crate) fn step(&mut self, dx: i32, dy: i32, pointer: (i32, i32), screens: &[Screen], others: &[Rect], reach: i32, snap: bool) -> Step {
        let reach = reach.max(1);
        let raw = self.raw(dx, dy, screens);
        if !snap {
            (self.hold_x, self.hold_y, self.part) = (None, None, None);
            return Step { rect: raw, guides: Vec::new() };
        }
        if self.grab.moves() {
            if let Some(step) = self.fill(pointer, screens, reach) { return step; }
        }
        let (vertical, horizontal) = (lines(screens, others, true, &raw), lines(screens, others, false, &raw));
        let mut rect = raw;
        let (x, y) = if self.grab.moves() {
            self.across |= 2 * dx.abs() >= reach;
            self.down |= 2 * dy.abs() >= reach;
            let parts_x = [(Anchor::Start, 2 * raw.x), (Anchor::End, 2 * raw.right()), (Anchor::Centre, 2 * raw.x + raw.w)];
            let parts_y = [(Anchor::Start, 2 * raw.y), (Anchor::End, 2 * raw.bottom()), (Anchor::Centre, 2 * raw.y + raw.h)];
            let x = solve(&vertical, if self.across { &parts_x[..] } else { &[] }, &mut self.hold_x, reach);
            let y = solve(&horizontal, if self.down { &parts_y[..] } else { &[] }, &mut self.hold_y, reach);
            if let Some((anchor, line)) = x { rect.x += offset(anchor, &line, raw.x, raw.w); }
            if let Some((anchor, line)) = y { rect.y += offset(anchor, &line, raw.y, raw.h); }
            rect = confine(rect, screens);
            (x, y)
        } else {
            let all = bounds(screens);
            let (g, edge) = (self.grab, |on: bool, anchor: Anchor, pos: i32| on.then_some((anchor, 2 * pos)));
            let parts_x: Vec<_> = [edge(g.left, Anchor::Start, raw.x), edge(g.right, Anchor::End, raw.right())].into_iter().flatten().collect();
            let parts_y: Vec<_> = [edge(g.top, Anchor::Start, raw.y), edge(g.bottom, Anchor::End, raw.bottom())].into_iter().flatten().collect();
            let mut x = solve(&vertical, &parts_x, &mut self.hold_x, reach);
            let mut y = solve(&horizontal, &parts_y, &mut self.hold_y, reach);
            if let Some((anchor, line)) = x {
                let (left, right) = if anchor == Anchor::Start { (line.at2 / 2, raw.right()) } else { (raw.x, line.at2 / 2) };
                if right - left >= MIN_W && all.is_none_or(|a| left >= a.x && right <= a.right()) { (rect.x, rect.w) = (left, right - left); } else { (x, self.hold_x) = (None, None); }
            }
            if let Some((anchor, line)) = y {
                let (top, bottom) = if anchor == Anchor::Start { (line.at2 / 2, raw.bottom()) } else { (raw.y, line.at2 / 2) };
                if bottom - top >= MIN_H && all.is_none_or(|a| top >= a.y && bottom <= a.bottom()) { (rect.y, rect.h) = (top, bottom - top); } else { (y, self.hold_y) = (None, None); }
            }
            (x, y)
        };
        // A line is drawn only while the tile is still on it (keeping it on the monitors may
        // have taken it off).
        let on = |anchor: Anchor, line: &Line, start: i32, len: i32| match anchor {
            Anchor::Start => 2 * start == line.at2,
            Anchor::End => 2 * (start + len) == line.at2,
            Anchor::Centre => (2 * start + len - line.at2).abs() <= 1,
        };
        let mut guides = Vec::new();
        if let Some((anchor, line)) = x.filter(|(anchor, line)| on(*anchor, line, rect.x, rect.w)) {
            let _ = anchor;
            guides.push(Guide { vertical: true, at: line.at2.div_euclid(2), from: line.from.min(rect.y), to: line.to.max(rect.bottom()) });
        }
        if let Some((anchor, line)) = y.filter(|(anchor, line)| on(*anchor, line, rect.y, rect.h)) {
            let _ = anchor;
            guides.push(Guide { vertical: false, at: line.at2.div_euclid(2), from: line.from.min(rect.x), to: line.to.max(rect.right()) });
        }
        Step { rect, guides }
    }

    /// The tile where the pointer alone puts it: moved, or its taken edges moved, kept on the
    /// monitors and no smaller than the smallest tile.
    fn raw(&self, dx: i32, dy: i32, screens: &[Screen]) -> Rect {
        let s = self.start;
        if self.grab.moves() { return confine(Rect::new(s.x + dx, s.y + dy, s.w, s.h), screens); }
        let all = bounds(screens).unwrap_or(Rect::new(i32::MIN / 4, i32::MIN / 4, i32::MAX / 2, i32::MAX / 2));
        let (mut left, mut top, mut right, mut bottom) = (s.x, s.y, s.right(), s.bottom());
        if self.grab.left { left = (left + dx).max(all.x).min(right - MIN_W); }
        if self.grab.right { right = (right + dx).min(all.right()).max(left + MIN_W); }
        if self.grab.top { top = (top + dy).max(all.y).min(bottom - MIN_H); }
        if self.grab.bottom { bottom = (bottom + dy).min(all.bottom()).max(top + MIN_H); }
        Rect::new(left, top, right - left, bottom - top)
    }

    /// The part of a monitor the moved tile fills with the pointer at `pointer`, if it asks
    /// for one. A part once taken is kept until the pointer is half as far again from the edge.
    fn fill(&mut self, pointer: (i32, i32), screens: &[Screen], reach: i32) -> Option<Step> {
        let depth = (reach * 3 / 4).max(1);
        let now = part_at(screens, pointer, depth);
        if !self.armed {
            self.armed = now.is_none();
            self.part = None;
            return None;
        }
        let kept = self.part.filter(|kept| part_at(screens, pointer, depth * 3 / 2 + 1) == Some(*kept));
        self.part = kept.or(now);
        let (index, part) = self.part?;
        let rect = part.rect(&screens[index]);
        (self.hold_x, self.hold_y) = (None, None);
        let guides = vec![
            Guide { vertical: true, at: rect.x, from: rect.y, to: rect.bottom() },
            Guide { vertical: true, at: rect.right(), from: rect.y, to: rect.bottom() },
            Guide { vertical: false, at: rect.y, from: rect.x, to: rect.right() },
            Guide { vertical: false, at: rect.bottom(), from: rect.x, to: rect.right() },
        ];
        Some(Step { rect, guides })
    }
}

/// How far a moved tile shifts so its `anchor` lies on `line`.
fn offset(anchor: Anchor, line: &Line, start: i32, len: i32) -> i32 {
    match anchor {
        Anchor::Start => line.at2 / 2 - start,
        Anchor::End => line.at2 / 2 - (start + len),
        Anchor::Centre => (line.at2 - (2 * start + len)).div_euclid(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4K monitor with a 72 px taskbar at its foot.
    fn main() -> Screen { Screen { rect: Rect::new(0, 0, 3840, 2160), work: Rect::new(0, 0, 3840, 2088) } }
    /// A 1080p monitor to its right (at 100 % scaling: the pair is a mixed-DPI desktop).
    fn side() -> Screen { Screen { rect: Rect::new(3840, 0, 1920, 1080), work: Rect::new(3840, 0, 1920, 1040) } }
    /// As the map draws them: 12 screen pixels to the map pixel, lines taken within 7 map pixels.
    const REACH: i32 = 84;

    fn moved(start: Rect, dx: i32, dy: i32, screens: &[Screen], others: &[Rect]) -> Step {
        // The pointer in the middle of the tile, clear of every edge at the press.
        let at = (start.x + start.w / 2, start.y + start.h / 2);
        let mut drag = Drag::new(Grab::MOVE, start);
        drag.step(0, 0, at, screens, others, REACH, true);
        drag.step(dx, dy, (at.0 + dx, at.1 + dy), screens, others, REACH, true)
    }

    fn resized(start: Rect, grab: Grab, dx: i32, dy: i32, screens: &[Screen], others: &[Rect]) -> Step {
        Drag::new(grab, start).step(dx, dy, (0, 0), screens, others, REACH, true)
    }

    const RIGHT: Grab = Grab { right: true, ..Grab::MOVE };
    const CORNER: Grab = Grab { right: true, bottom: true, ..Grab::MOVE };

    #[test]
    fn a_tile_takes_the_monitors_edges_and_the_taskbars_edge() {
        let screens = [main()];
        let tile = Rect::new(900, 700, 1280, 720);
        // Carried to 30 px from the left edge and 40 from the top: both taken, to the pixel.
        let step = moved(tile, -870, -660, &screens, &[]);
        assert_eq!(step.rect, Rect::new(0, 0, 1280, 720));
        assert_eq!(step.guides, vec![
            Guide { vertical: true, at: 0, from: 0, to: 2160 },
            Guide { vertical: false, at: 0, from: 0, to: 3840 },
        ]);
        // Its foot near the taskbar's edge sits on it (the work area), not under it...
        let step = moved(tile, 0, 2088 - 1420 - 25, &screens, &[]);
        assert_eq!(step.rect.bottom(), 2088);
        // ...and nearer the monitor's own foot, on that.
        let step = moved(tile, 0, 2160 - 1420 - 10, &screens, &[]);
        assert_eq!(step.rect.bottom(), 2160);
        // The right edge of the monitor, by the tile's right edge.
        let step = moved(tile, 3840 - 2180 - 50, 0, &screens, &[]);
        assert_eq!(step.rect.right(), 3840);
    }

    #[test]
    fn edges_take_halves_thirds_and_quarters_and_a_whole_monitor_is_exact() {
        let screens = [main()];
        let tile = Rect::new(0, 0, 1000, 800);
        // The right edge dragged to within reach of each line lands on it exactly.
        for (line, name) in [(1920, "half"), (1280, "third"), (2560, "two thirds"), (960, "quarter"), (2880, "three quarters")] {
            let step = resized(tile, RIGHT, line - 1000 + 30, 0, &screens, &[]);
            assert_eq!(step.rect, Rect::new(0, 0, line, 800), "{name}");
            assert_eq!(step.guides, vec![Guide { vertical: true, at: line, from: 0, to: 2160 }], "{name}");
        }
        // Thirds of a width three does not divide are whole pixels that add up.
        let odd = [Screen { rect: Rect::new(0, 0, 2560, 1440), work: Rect::new(0, 0, 2560, 1440) }];
        let third = resized(Rect::new(0, 0, 700, 500), RIGHT, 853 - 700 + 9, 0, &odd, &[]).rect;
        assert_eq!(third.right(), 853);
        let two = resized(Rect::new(853, 0, 700, 500), RIGHT, 1707 - 1553 - 9, 0, &odd, &[]).rect;
        assert_eq!((two.x, two.right()), (853, 1707));
        // The corner dragged to the half line and the taskbar's edge: the left half of the work area.
        let step = resized(tile, CORNER, 1920 - 1000 - 40, 2088 - 800 + 30, &screens, &[]);
        assert_eq!(step.rect, Rect::new(0, 0, 1920, 2088));
        // To the monitor's own corner: the whole monitor, exactly (borderless full screen).
        let step = resized(tile, CORNER, 3840 - 1000 - 20, 2160 - 800 - 5, &screens, &[]);
        assert_eq!(step.rect, main().rect);
        assert_eq!(super::super::layouts::covers(&step.rect, &main().rect), Some((0, 0, 0, 0)));
    }

    #[test]
    fn tiles_abut_and_line_up_with_no_gap_or_overlap() {
        let screens = [main()];
        let other = Rect::new(1000, 300, 1280, 720);
        let tile = Rect::new(2500, 1200, 1280, 720);
        // Carried to 20 px right of the other and 15 px below its top: it abuts and lines up.
        let step = moved(tile, 2300 - 2500, 315 - 1200, &screens, &[other]);
        assert_eq!(step.rect, Rect::new(2280, 300, 1280, 720));
        assert_eq!(step.rect.x, other.right(), "no gap");
        assert!(step.rect.intersection(&other).is_none(), "no overlap");
        assert_eq!(step.guides, vec![
            Guide { vertical: true, at: 2280, from: 300, to: 1020 },
            Guide { vertical: false, at: 300, from: 1000, to: 3560 },
        ]);
        // Under it: its top on the other's foot, their left edges in line.
        let step = moved(tile, 1010 - 2500, 1000 - 1200, &screens, &[other]);
        assert_eq!(step.rect, Rect::new(1000, 1020, 1280, 720));
        // A smaller tile's centre takes another's centre (no edge of it is near a line).
        let large = Rect::new(1100, 620, 1000, 500);
        let small = Rect::new(2600, 1500, 400, 160);
        let step = moved(small, 1600 - 200 + 30 - 2600, 870 - 80 + 5 - 1500, &screens, &[large]);
        assert_eq!((step.rect.x + 200, step.rect.y + 80), (1600, 870), "{:?}", step.rect);
        assert_eq!(step.guides, vec![
            Guide { vertical: true, at: 1600, from: 620, to: 1120 },
            Guide { vertical: false, at: 870, from: 1100, to: 2100 },
        ]);
        // Carried straight down it is not pulled sideways to a line that happens to be near.
        let near = Rect::new(940, 300, 1280, 720);
        assert_eq!(moved(near, 0, 500, &screens, &[]).rect.x, 940, "the quarter line 20 px away is left alone");
        assert_eq!(moved(near, 45, 500, &screens, &[]).rect.x, 960, "moved sideways too, it takes it");
        // A resized edge stops on the other tile's edge.
        let step = resized(Rect::new(100, 300, 700, 720), RIGHT, 1000 - 800 + 33, 0, &screens, &[other]);
        assert_eq!(step.rect.right(), other.x);
    }

    #[test]
    fn a_line_out_of_reach_is_not_taken_and_alt_places_freely() {
        let screens = [main()];
        let tile = Rect::new(900, 700, 1280, 720);
        // Just inside the reach it is taken, just outside it is not.
        assert_eq!(moved(tile, -900 + REACH, 0, &screens, &[]).rect.x, 0);
        assert_eq!(moved(tile, -900 + REACH + 1, 0, &screens, &[]).rect.x, REACH + 1);
        // Alt: the tile goes where the pointer puts it, with no line drawn.
        let mut drag = Drag::new(Grab::MOVE, tile);
        let free = drag.step(-870, -660, (670, 400), &screens, &[], REACH, false);
        assert_eq!(free, Step { rect: Rect::new(30, 40, 1280, 720), guides: Vec::new() });
        // Let go of Alt mid-drag and it snaps; held again, it is free again.
        assert_eq!(drag.step(-870, -660, (670, 400), &screens, &[], REACH, true).rect, Rect::new(0, 0, 1280, 720));
        assert_eq!(drag.step(-870, -660, (670, 400), &screens, &[], REACH, false).rect, Rect::new(30, 40, 1280, 720));
        // A resize is free the same way.
        assert_eq!(Drag::new(RIGHT, tile).step(-250, 0, (0, 0), &screens, &[], REACH, false).rect, Rect::new(900, 700, 1030, 720));
    }

    #[test]
    fn a_line_once_taken_is_held_so_the_tile_never_flickers() {
        let screens = [main()];
        let tile = Rect::new(900, 700, 1280, 720);
        let at = |dx: i32| (1540 + dx, 1060);
        let mut drag = Drag::new(Grab::MOVE, tile);
        drag.step(0, 0, at(0), &screens, &[], REACH, true);
        // Taken at the reach...
        assert_eq!(drag.step(-900 + REACH, 0, at(-900 + REACH), &screens, &[], REACH, true).rect.x, 0);
        // ...and kept while the pointer wanders back out past it, up to half as far again.
        for away in [REACH + 1, REACH + 20, REACH * 3 / 2] {
            assert_eq!(drag.step(-900 + away, 0, at(-900 + away), &screens, &[], REACH, true).rect.x, 0, "{away}");
        }
        // Beyond that it lets go, and is not taken again until within the reach.
        assert_eq!(drag.step(-900 + REACH * 3 / 2 + 1, 0, at(0), &screens, &[], REACH, true).rect.x, REACH * 3 / 2 + 1);
        assert_eq!(drag.step(-900 + REACH + 1, 0, at(0), &screens, &[], REACH, true).rect.x, REACH + 1);
        // Two tiles' edges 60 px apart, the pointer trembling by a pixel around the point
        // between them: whichever was taken first stays.
        let others = [Rect::new(1600, 900, 500, 400), Rect::new(2160, 900, 500, 400)];
        for first in [-1, 1] {
            let mut drag = Drag::new(RIGHT, Rect::new(0, 0, 1000, 800));
            let mut seen = Vec::new();
            for tremble in [first, -first, first, -first, first] {
                seen.push(drag.step(1130 + tremble, 0, (0, 0), &screens, &others, REACH, true).rect.right());
            }
            assert!(seen.iter().all(|edge| *edge == seen[0]), "{seen:?}");
            assert_eq!(seen[0], if first < 0 { 2100 } else { 2160 });
        }
    }

    #[test]
    fn the_nearest_line_wins_and_the_stronger_when_two_are_as_near() {
        // The quarter line of the monitor (at 960) against another tile's edge at 990.
        let screens = [main()];
        let other = Rect::new(990, 1200, 600, 400);
        let tile = Rect::new(0, 0, 700, 500);
        // Midway between the quarter line (960) and the tile's edge (990): the edge is stronger.
        assert_eq!(resized(tile, RIGHT, 975 - 700, 0, &screens, &[other]).rect.right(), 990);
        // Clearly nearer the quarter line: it wins, weaker as it is.
        assert_eq!(resized(tile, RIGHT, 950 - 700, 0, &screens, &[other]).rect.right(), 960);
        // A centre is the weakest: an edge about as near beats it.
        let wide = Rect::new(2000, 1500, 1000, 400);
        let step = moved(wide, 1420 - 2000 + 30, 0, &screens, &[]);
        assert_eq!(step.rect.x + 500, 1920, "centred on the monitor: {:?}", step.rect);
        let edge = Rect::new(1445, 0, 300, 300);
        let step = moved(wide, 1420 - 2000 + 30, 0, &screens, &[edge]);
        assert_eq!(step.rect.x, 1445, "the other tile's edge, 5 px from the tile's own, over the centre line 30 px away");
    }

    #[test]
    fn a_tile_crosses_monitors_of_different_scaling_in_exact_pixels() {
        let screens = [main(), side()];
        let tile = Rect::new(2000, 500, 960, 540);
        // Onto the 1080p monitor: its left edge takes that monitor's half line, its top the top.
        let step = moved(tile, 4800 - 2000 + 25, -500 + 20, &screens, &[]);
        assert_eq!(step.rect, Rect::new(4800, 0, 960, 540));
        assert_eq!(screen_of(&step.rect, &screens), Some(1));
        // Astride the two it belongs to the one it is mostly on, and takes the shared edge.
        let step = moved(tile, 3840 - 2000 - 30, 0, &screens, &[]);
        assert_eq!(step.rect.x, 3840);
        let astride = Rect::new(3840 - 500, 500, 960, 540);
        assert_eq!(screen_of(&astride, &screens), Some(0));
        // The corner neither monitor covers (under the small one): a tile carried there is
        // brought back onto a monitor, and never leaves the rectangle around them.
        let step = moved(tile, 4500 - 2000, 1700 - 500, &screens, &[]);
        let centre = (step.rect.x + step.rect.w / 2, step.rect.y + step.rect.h / 2);
        assert!(screens.iter().any(|s| s.rect.contains(centre.0, centre.1)), "{:?}", step.rect);
        let far = Drag::new(Grab::MOVE, tile).step(90_000, -90_000, (0, 0), &screens, &[], REACH, false).rect;
        assert_eq!(bounds(&screens).unwrap().intersection(&far), Some(far));
        // The small monitor's edges and fractions are its own pixels, and a tile on it takes
        // none of the large one's (whose quarter line, at 522, is nearer here).
        let step = resized(Rect::new(3840, 0, 700, 500), CORNER, 960 - 700 - 8, 520 - 500 + 6, &screens, &[]);
        assert_eq!(step.rect, Rect::new(3840, 0, 960, 520), "its half line and the half of its work area");
        assert_eq!(bounds(&screens), Some(Rect::new(0, 0, 5760, 2160)));
        assert_eq!(bounds(&[]), None);
    }

    #[test]
    fn a_tile_moved_to_a_monitors_edge_fills_it_or_a_half_or_a_quarter() {
        let screens = [main(), side()];
        let tile = Rect::new(900, 700, 1280, 720);
        let carry = |to: (i32, i32)| {
            let from = (1540, 1060);
            let mut drag = Drag::new(Grab::MOVE, tile);
            drag.step(0, 0, from, &screens, &[], REACH, true);
            drag.step(to.0 - from.0, to.1 - from.1, to, &screens, &[], REACH, true).rect
        };
        // The top edge: the whole monitor, to the pixel, the taskbar's strip included.
        assert_eq!(carry((1900, 10)), main().rect);
        assert_eq!(carry((4700, 5)), side().rect, "the monitor the pointer is on");
        // The left edge: the left half of the work area. Corners: quarters.
        assert_eq!(carry((8, 1000)), Rect::new(0, 0, 1920, 2088));
        assert_eq!(carry((20, 30)), Rect::new(0, 0, 1920, 1044));
        assert_eq!(carry((30, 2150)), Rect::new(0, 1044, 1920, 1044));
        assert_eq!(carry((5750, 500)), Rect::new(4800, 0, 960, 1040), "the small monitor's right half");
        assert_eq!(carry((5750, 1070)), Rect::new(4800, 520, 960, 520));
        // The edge the two monitors share fills nothing: the tile passes from one to the other.
        assert_eq!(carry((3835, 600)).w, 1280);
        assert_eq!(carry((3845, 600)).w, 1280);
        // The foot of a monitor fills nothing either.
        assert_eq!(carry((1900, 2150)).w, 1280);
        // A part once filled is kept while the pointer trembles at the edge of it.
        let mut drag = Drag::new(Grab::MOVE, tile);
        drag.step(0, 0, (1540, 1060), &screens, &[], REACH, true);
        let depth = REACH * 3 / 4;
        assert_eq!(drag.step(0, 0, (1900, depth - 1), &screens, &[], REACH, true).rect, main().rect);
        assert_eq!(drag.step(0, 0, (1900, depth + 10), &screens, &[], REACH, true).rect, main().rect);
        assert_eq!(drag.step(360, -900, (1900, depth * 3 / 2 + 5), &screens, &[], REACH, true).rect.w, 1280, "let go");
        // Taken hold of at the top edge itself (a tile that sits there): nothing fills until the
        // pointer has been away from the edge once.
        let top = Rect::new(900, 0, 1280, 720);
        let mut drag = Drag::new(Grab::MOVE, top);
        assert_eq!(drag.step(0, 0, (1500, 8), &screens, &[], REACH, true).rect, top);
        assert_eq!(drag.step(40, 0, (1540, 8), &screens, &[], REACH, true).rect.w, 1280);
        assert_eq!(drag.step(40, 300, (1540, 308), &screens, &[], REACH, true).rect.w, 1280);
        assert_eq!(drag.step(40, 0, (1540, 8), &screens, &[], REACH, true).rect, main().rect);
        // Alt fills nothing.
        let mut drag = Drag::new(Grab::MOVE, tile);
        drag.step(0, 0, (1540, 1060), &screens, &[], REACH, true);
        assert_eq!(drag.step(360, -1050, (1900, 10), &screens, &[], REACH, false).rect.w, 1280);
    }

    #[test]
    fn a_resize_keeps_the_smallest_tile_and_stays_on_the_monitors() {
        let screens = [main()];
        let tile = Rect::new(900, 700, 1280, 720);
        let left = Grab { left: true, top: true, ..Grab::MOVE };
        // Pushed past its own far edges: the smallest tile, the far edges where they were.
        let step = Drag::new(left, tile).step(5000, 5000, (0, 0), &screens, &[], REACH, false);
        assert_eq!(step.rect, Rect::new(2180 - MIN_W, 1420 - MIN_H, MIN_W, MIN_H));
        // Pulled off the monitor: it ends at the monitor's edge.
        let step = Drag::new(left, tile).step(-5000, -5000, (0, 0), &screens, &[], REACH, true);
        assert_eq!(step.rect, Rect::new(0, 0, 2180, 1420));
        let step = Drag::new(CORNER, tile).step(9000, 9000, (0, 0), &screens, &[], REACH, false);
        assert_eq!((step.rect.right(), step.rect.bottom()), (3840, 2160));
        // The arrow keys move it by pixels, and stop at the monitors' edge.
        assert_eq!(nudge(tile, -1, 10, &screens), Rect::new(899, 710, 1280, 720));
        assert_eq!(nudge(Rect::new(0, 0, 1280, 720), -1, -10, &screens), Rect::new(0, 0, 1280, 720));
    }

    #[test]
    fn a_press_takes_the_tile_under_it_by_its_middle_or_its_edges() {
        let tiles = [Rect::new(0, 0, 1920, 1080), Rect::new(1000, 500, 1280, 720), Rect::new(3000, 0, 400, 300)];
        // The last drawn is on top; its middle moves it.
        assert_eq!(hit(&tiles, None, (1500, 800), 60), Some((1, Grab::MOVE)));
        assert_eq!(hit(&tiles, None, (500, 300), 60), Some((0, Grab::MOVE)));
        assert_eq!(hit(&tiles, None, (2500, 1500), 60), None);
        // The selected one comes first where they overlap.
        assert_eq!(hit(&tiles, Some(0), (1500, 800), 60), Some((0, Grab::MOVE)));
        // Edges and corners, from just inside.
        assert_eq!(hit(&tiles, None, (1010, 800), 60), Some((1, Grab { left: true, ..Grab::MOVE })));
        assert_eq!(hit(&tiles, None, (2270, 1210), 60), Some((1, CORNER)));
        assert_eq!(hit(&tiles, None, (1500, 510), 60), Some((1, Grab { top: true, ..Grab::MOVE })));
        // The selected tile's edge can be taken from just outside it too; another's cannot.
        assert_eq!(hit(&tiles, Some(1), (2300, 800), 60), Some((1, RIGHT)));
        assert_eq!(hit(&tiles, None, (2300, 800), 60), None);
        // A small tile keeps a middle that moves it, however far the reach.
        assert_eq!(hit(&tiles, None, (3200, 150), 500), Some((2, Grab::MOVE)));
        assert_eq!(hit(&tiles, None, (3050, 150), 500), Some((2, Grab { left: true, ..Grab::MOVE })));
        // Every tile under a point, the top one first (a click goes down through a stack).
        assert_eq!(under(&tiles, (1500, 800)), vec![1, 0]);
        assert_eq!(under(&tiles, (2500, 1500)), Vec::<usize>::new());
    }

    #[test]
    fn the_map_and_the_screen_agree_to_the_pixel() {
        let screens = [main(), side(), Screen { rect: Rect::new(-1440, -961, 1440, 2560), work: Rect::new(-1440, -961, 1440, 2512) }];
        let all = bounds(&screens).unwrap();
        assert_eq!(all, Rect::new(-1440, -961, 7200, 3121));
        // Whatever width the map is drawn at, a rectangle drawn on it reads back exactly: every
        // monitor (a tile that fills one is that monitor's rectangle), every work area, and
        // tiles of awkward sizes at awkward places.
        let rects: Vec<Rect> = screens.iter().flat_map(|s| [s.rect, s.work]).chain([Rect::new(-1433, -957, 1277, 719), Rect::new(3841, 1079, 321, 241), Rect::new(5759, 2159, 1, 1)]).collect();
        for width in [180.0, 448.0, 519.5, 688.0, 1023.25] {
            let map = Map::new(all, width);
            for rect in &rects {
                assert_eq!(map.from_map(map.to_map(rect)), *rect, "{rect:?} at {width}");
                let [fx, fy, fw, fh] = map.fractions(rect);
                assert_eq!(map.at(fx, fy), (rect.x, rect.y), "{rect:?} at {width}");
                assert_eq!(map.at(fx + fw, fy + fh), (rect.right(), rect.bottom()), "{rect:?} at {width}");
            }
            // Two monitors that share an edge share it on the map.
            let (a, b) = (map.to_map(&main().rect), map.to_map(&side().rect));
            assert!((a[0] + a[2] - b[0]).abs() < 1e-9);
            // A map pixel is `1 / scale` screen pixels, and the reach is never nothing.
            assert_eq!(map.reach(7.0), (7.0 * 7200.0 / width).round() as i32);
            assert_eq!(Map::new(all, 90_000.0).reach(0.01), 1);
        }
        // The pointer at a fraction of the map is a screen point.
        let map = Map::new(all, 448.0);
        assert_eq!(map.at(0.0, 0.0), (-1440, -961));
        assert_eq!(map.at(1.0, 1.0), (5760, 2160));
        assert_eq!(map.point(448.0, 0.0), (5760, -961));
    }

    #[test]
    fn a_tile_belongs_to_the_monitor_it_is_mostly_on() {
        let screens = [main(), side()];
        assert_eq!(screen_of(&Rect::new(100, 100, 800, 600), &screens), Some(0));
        assert_eq!(screen_of(&side().rect, &screens), Some(1));
        // Off every monitor: the nearest.
        assert_eq!(screen_of(&Rect::new(6000, 1500, 400, 300), &screens), Some(1));
        assert_eq!(screen_of(&Rect::new(-900, 100, 400, 300), &screens), Some(0));
        assert_eq!(screen_of(&Rect::new(0, 0, 10, 10), &[]), None);
        assert_eq!(Part::Full.rect(&main()), main().rect);
        assert_eq!(Part::Right.rect(&main()), Rect::new(1920, 0, 1920, 2088));
    }
}
