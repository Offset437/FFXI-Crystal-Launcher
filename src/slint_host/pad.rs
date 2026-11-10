//! Spatial controller focus for the Slint window: the D-pad moves the focus to the nearest
//! focusable control that way, A presses it. Slint has no spatial focus of its own, so this
//! reads its item tree through `i-slint-core`, pinned to Slint's exact version.
//!
//! A stop is a focus scope Tab reaches (enabled) or an editable text field, that shows: not
//! clipped away, except by a scrolling list (focusing it scrolls it in). A dialog or sheet's dim
//! (a touch area over the whole window) hides every stop drawn under it, so only the dialog's
//! controls are stops while it is open. An engaged slider (one that says `accessible-expanded`)
//! takes the D-pad itself.
use i_slint_core::accessibility::AccessibleStringProperty;
use i_slint_core::input::FocusReason;
use i_slint_core::item_tree::{ItemRc, ParentItemTraversalMode};
use i_slint_core::items::{AccessibleRole, Clip, Flickable, FocusScope, TextInput, TouchArea};
use i_slint_core::window::WindowInner;
use slint::platform::{Key, WindowEvent};

/// A D-pad direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Way {
    Up,
    Down,
    Left,
    Right,
}

impl Way {
    fn across(self) -> bool { matches!(self, Way::Left | Way::Right) }
}

/// Where a control is, in the window's logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn right(self) -> f32 { self.x + self.w }
    fn bottom(self) -> f32 { self.y + self.h }
    fn middle(self) -> [f32; 2] { [self.x + self.w / 2.0, self.y + self.h / 2.0] }
}

/// Whether `to` lies `way` of `from` at all (Android's focus finder's rule).
fn ahead(way: Way, from: Rect, to: Rect) -> bool {
    match way {
        Way::Down => (from.y < to.y || from.bottom() <= to.y) && from.bottom() < to.bottom(),
        Way::Up => (from.bottom() > to.bottom() || from.y >= to.bottom()) && from.y > to.y,
        Way::Right => (from.x < to.x || from.right() <= to.x) && from.right() < to.right(),
        Way::Left => (from.right() > to.right() || from.x >= to.right()) && from.x > to.x,
    }
}

/// Whether `to` overlaps `from` across the way (a row below sharing columns with it).
fn in_beam(way: Way, from: Rect, to: Rect) -> bool {
    if way.across() { to.bottom() > from.y && to.y < from.bottom() } else { to.right() > from.x && to.x < from.right() }
}

fn gap(way: Way, from: Rect, to: Rect) -> f32 {
    match way {
        Way::Down => to.y - from.bottom(),
        Way::Up => from.y - to.bottom(),
        Way::Right => to.x - from.right(),
        Way::Left => from.x - to.right(),
    }
    .max(0.0)
}

/// Whether `a` beats `b` going `way` from `from`: one in the beam beats one out of it, else the
/// nearer, distance along the way counting thirteen times what it does across.
fn better(way: Way, from: Rect, a: Rect, b: Rect) -> bool {
    match (in_beam(way, from, a), in_beam(way, from, b)) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    let score = |r: Rect| {
        let (m, f) = (r.middle(), from.middle());
        let across = if way.across() { m[1] - f[1] } else { m[0] - f[0] };
        13.0 * gap(way, from, r).powi(2) + across.powi(2)
    };
    score(a) < score(b)
}

/// The stop to go to `way` from `from`, by its place in `stops`.
fn nearest(stops: &[Rect], from: Rect, way: Way) -> Option<usize> {
    stops.iter().enumerate()
        .filter(|(_, r)| **r != from && ahead(way, from, **r))
        .fold(None, |best: Option<(usize, Rect)>, (i, &r)| match best {
            Some((_, b)) if !better(way, from, r, b) => best,
            _ => Some((i, r)),
        })
        .map(|(i, _)| i)
}

fn is_stop(item: &ItemRc) -> bool {
    if let Some(scope) = item.downcast::<FocusScope>() {
        let scope = scope.as_pin_ref();
        return scope.enabled() && scope.focus_on_tab_navigation();
    }
    item.downcast::<TextInput>().is_some_and(|text| {
        let text = text.as_pin_ref();
        text.enabled() && !text.read_only()
    })
}

fn rect(item: &ItemRc) -> Rect {
    let geometry = item.geometry();
    let at = item.map_to_native_window(geometry.origin);
    Rect { x: at.x, y: at.y, w: geometry.size.width, h: geometry.size.height }
}

/// Where `item` is, if it shows.
fn shown(item: &ItemRc) -> Option<Rect> {
    let at = rect(item);
    if at.w < 1.0 || at.h < 1.0 { return None; }
    let mut seen = at;
    let mut above = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(parent) = above {
        if let Some(clip) = parent.downcast::<Clip>() {
            let clip = clip.as_pin_ref();
            if clip.clip() {
                let edge = rect(&parent);
                let cut = seen.right() <= edge.x || seen.x >= edge.right() || seen.bottom() <= edge.y || seen.y >= edge.bottom();
                if clip.is_visibility_clip() || cut { return None; }
            }
        } else if parent.downcast::<Flickable>().is_some() {
            seen = rect(&parent);
        }
        above = parent.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    Some(at)
}

/// Whether `item` is a dim over the whole window: an enabled touch area that covers it.
fn blocks(item: &ItemRc, window: Rect) -> bool {
    let Some(area) = item.downcast::<TouchArea>() else { return false };
    if !area.as_pin_ref().enabled() || !item.is_visible() { return false; }
    let at = rect(item);
    at.x <= window.x + 1.0 && at.y <= window.y + 1.0 && at.right() >= window.right() - 1.0 && at.bottom() >= window.bottom() - 1.0
}

/// The modal panel `item` sits in: its nearest groupbox. The side sheets (ui/form.slint's
/// SideSheet) are the window's only groupboxes, and each is modal over the page under it.
fn modal_of(item: &ItemRc) -> Option<ItemRc> {
    let mut above = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(parent) = above {
        if parent.accessible_role() == AccessibleRole::Groupbox { return Some(parent); }
        above = parent.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    None
}

/// Every stop of the window (of its open popup, while one is; of the open side sheet, while one
/// is), in Tab order. Items come in the order they are drawn, so a dim hides every stop found
/// before it, and nothing under an open side sheet's dimmed page can be focused or pressed.
fn stops(window: &slint::Window) -> Vec<(ItemRc, Rect)> {
    const MOST_ITEMS: usize = 50_000;
    let inner = WindowInner::from_pub(window);
    // A dialog or list shown since the last frame exists only once its items are made.
    inner.ensure_tree_instantiated();
    let size = window.size().to_logical(window.scale_factor());
    let whole = Rect { x: 0.0, y: 0.0, w: size.width, h: size.height };
    let popup = inner.active_popups().last().map(|popup| popup.component.clone());
    let in_popup = popup.is_some();
    let start = ItemRc::new_root(popup.unwrap_or_else(|| inner.component()));
    let mut found = Vec::new();
    let mut item = start.clone();
    for _ in 0..MOST_ITEMS {
        if is_stop(&item) {
            if let Some(at) = shown(&item) { found.push((item.clone(), at, modal_of(&item))); }
        } else if !in_popup && blocks(&item, whole) {
            found.clear();
        }
        item = item.next_focus_item();
        if item == start { break; }
    }
    // The last sheet in the tree is drawn on top.
    let modal = found.iter().rev().find_map(|(_, _, modal)| modal.clone());
    found.into_iter().filter(|(_, _, m)| modal.is_none() || *m == modal).map(|(item, at, _)| (item, at)).collect()
}

thread_local! {
    /// The control whose list the last press opened, while that list may still be open.
    static OPENER: std::cell::RefCell<Option<ItemRc>> = const { std::cell::RefCell::new(None) };
}

/// Whether the focused control is a slider being adjusted (it takes the D-pad).
fn engaged(item: &ItemRc) -> bool {
    let mut at = Some(item.clone());
    // The slider names itself on its root, a step or two above its focus scope.
    for _ in 0..3 {
        let Some(item) = at else { return false };
        if item.is_accessible() && item.accessible_role() != AccessibleRole::None {
            return item.accessible_role() == AccessibleRole::Slider
                && item.accessible_string_property(AccessibleStringProperty::Expanded).is_some_and(|on| on == "true");
        }
        at = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    false
}

fn key(window: &slint::Window, key: Key) {
    let text: slint::SharedString = key.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

/// The open side sheet's panel, as (x, y, width, height), for tests of pages.
#[cfg(test)]
pub(crate) fn modal_rect(window: &slint::Window) -> Option<(f32, f32, f32, f32)> {
    let inner = WindowInner::from_pub(window);
    let start = ItemRc::new_root(inner.component());
    let mut item = start.clone();
    for _ in 0..50_000 {
        if let Some(modal) = is_stop(&item).then(|| modal_of(&item)).flatten() {
            let r = rect(&modal);
            return Some((r.x, r.y, r.w, r.h));
        }
        item = item.next_focus_item();
        if item == start { break; }
    }
    None
}

/// The text of each editable field the walker can reach, in Tab order (tests of pages).
#[cfg(test)]
pub(crate) fn field_texts(window: &slint::Window) -> Vec<String> {
    stops(window).iter().filter_map(|(item, _)| item.downcast::<TextInput>().map(|t| t.as_pin_ref().text().to_string())).collect()
}

/// Focuses the `n`-th editable field the walker can reach (tests of pages).
#[cfg(test)]
pub(crate) fn focus_field(window: &slint::Window, n: usize) {
    let found: Vec<ItemRc> = stops(window).into_iter().filter(|(item, _)| item.downcast::<TextInput>().is_some()).map(|(item, _)| item).collect();
    WindowInner::from_pub(window).set_focus_item(&found[n], true, FocusReason::TabNavigation);
}

/// Where each stop the walker can reach now starts: (x, y), for tests of pages.
#[cfg(test)]
pub(crate) fn stop_origins(window: &slint::Window) -> Vec<(f32, f32)> { stops(window).iter().map(|(_, r)| (r.x, r.y)).collect() }

/// Moves the focus `way`: to the nearest stop that way, or to the first stop when nothing (or
/// nothing reachable, such as a control under a dialog) has the focus; after a list it opened
/// closes, from the control that opened it. An engaged slider gets the direction as an arrow key
/// instead. Whether anything happened.
pub(crate) fn step(window: &slint::Window, way: Way) -> bool {
    let inner = WindowInner::from_pub(window);
    let current = inner.focus_item.borrow().upgrade();
    if current.as_ref().is_some_and(engaged) {
        key(window, match way {
            Way::Up => Key::UpArrow,
            Way::Down => Key::DownArrow,
            Way::Left => Key::LeftArrow,
            Way::Right => Key::RightArrow,
        });
        return true;
    }
    let found = stops(window);
    if current.is_some() && inner.active_popups().is_empty() { OPENER.with(|o| o.borrow_mut().take()); }
    // A list closed under the focus (a pick, a click outside it, Escape): go on from the control
    // that opened it.
    let resume = || OPENER.with(|o| o.borrow_mut().take()).and_then(|opener| found.iter().position(|(stop, _)| *stop == opener));
    let at = match &current {
        Some(item) => found.iter().position(|(stop, _)| stop == item),
        None => resume(),
    };
    let target = match at {
        Some(at) => {
            let rects: Vec<Rect> = found.iter().map(|(_, r)| *r).collect();
            // Back from a list with nothing that way: the control that opened it takes the focus.
            nearest(&rects, rects[at], way).or(current.is_none().then_some(at))
        }
        None => (!found.is_empty()).then_some(0),
    };
    let Some(target) = target else { return false };
    inner.set_focus_item(&found[target].0, true, FocusReason::TabNavigation);
    true
}

/// Where every stop is (x, y, width, height), for tests that check what the walker can reach.
#[cfg(test)]
pub(crate) fn stop_rects(window: &slint::Window) -> Vec<[f32; 4]> {
    stops(window).into_iter().map(|(_, r)| [r.x, r.y, r.w, r.h]).collect()
}

/// Where the focused item is, if anything has the focus.
#[cfg(test)]
pub(crate) fn focus_rect(window: &slint::Window) -> Option<[f32; 4]> {
    let item = WindowInner::from_pub(window).focus_item.borrow().upgrade()?;
    let r = rect(&item);
    Some([r.x, r.y, r.w, r.h])
}

/// Moves the focus to the next stop in Tab order (`forward`) or the previous one, wrapping:
/// Tab inside a dialog, which the window's own Tab would take to the page under it.
pub(crate) fn cycle(window: &slint::Window, forward: bool) -> bool {
    let inner = WindowInner::from_pub(window);
    let found = stops(window);
    if found.is_empty() { return false; }
    let current = inner.focus_item.borrow().upgrade();
    let target = match current.as_ref().and_then(|item| found.iter().position(|(stop, _)| stop == item)) {
        Some(at) if forward => (at + 1) % found.len(),
        Some(at) => (at + found.len() - 1) % found.len(),
        None if forward => 0,
        None => found.len() - 1,
    };
    inner.set_focus_item(&found[target].0, true, FocusReason::TabNavigation);
    true
}

/// Presses the focused control (A): Return, as the keyboard does. Only a reachable stop is
/// pressed (never a control under an open dialog). Whether it pressed.
pub(crate) fn press(window: &slint::Window) -> bool {
    let inner = WindowInner::from_pub(window);
    let current = inner.focus_item.borrow().upgrade();
    let Some(current) = current else { return false };
    if !stops(window).iter().any(|(stop, _)| *stop == current) { return false; }
    let popups = inner.active_popups().len();
    key(window, Key::Return);
    // A list it opened starts at its chosen entry (or its first).
    if inner.active_popups().len() > popups {
        OPENER.with(|o| *o.borrow_mut() = Some(current.clone()));
        let found = stops(window);
        let chosen = found.iter().position(|(item, _)| selected(item)).unwrap_or(0);
        if let Some((item, _)) = found.get(chosen) {
            inner.set_focus_item(item, true, FocusReason::TabNavigation);
        }
    }
    true
}

/// Carries out one queued controller command (controller_nav::State::poll's): directions move the
/// focus, A presses it; the rest are core messages, returned for the host to send.
pub(crate) fn apply(window: &slint::Window, command: crate::controller_nav::Command) -> Option<crate::Message> {
    use crate::controller_nav::{Command, Direction};
    use crate::Message;
    match command {
        Command::Move(direction) => {
            step(window, match direction {
                Direction::Up => Way::Up,
                Direction::Down => Way::Down,
                Direction::Left => Way::Left,
                Direction::Right => Way::Right,
            });
            None
        }
        Command::Select => {
            press(window);
            None
        }
        Command::Back => Some(Message::ControllerBack),
        Command::Page(step) => Some(Message::ControllerPage(step)),
        Command::Play => Some(Message::ControllerPlay),
    }
}

/// Whether any item has the keyboard focus.
pub(crate) fn has_focus(window: &slint::Window) -> bool {
    WindowInner::from_pub(window).focus_item.borrow().upgrade().is_some()
}

/// Whether `item` belongs to a list entry that is the chosen one.
fn selected(item: &ItemRc) -> bool {
    let mut at = Some(item.clone());
    for _ in 0..3 {
        let Some(item) = at else { return false };
        if item.is_accessible() && item.accessible_role() == AccessibleRole::ListItem {
            return item.accessible_string_property(AccessibleStringProperty::ItemSelected).is_some_and(|on| on == "true");
        }
        at = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    false
}

/// The accessible name of the control `item` belongs to: its own, or its nearest named parent's.
#[cfg(test)]
pub(crate) fn name_of(item: &ItemRc) -> String {
    let mut at = Some(item.clone());
    while let Some(item) = at {
        if let Some(label) = item.accessible_string_property(AccessibleStringProperty::Label).filter(|l| !l.is_empty()) {
            return label.into();
        }
        at = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    String::new()
}

/// The names of every stop, in Tab order.
#[cfg(test)]
pub(crate) fn names(window: &slint::Window) -> Vec<String> {
    stops(window).iter().map(|(item, _)| name_of(item)).collect()
}

/// The focused control's name.
#[cfg(test)]
pub(crate) fn focused(window: &slint::Window) -> Option<String> {
    WindowInner::from_pub(window).focus_item.borrow().upgrade().map(|item| name_of(&item))
}

/// Focuses the first stop named `name`. Whether there was one.
#[cfg(test)]
pub(crate) fn focus_named(window: &slint::Window, name: &str) -> bool {
    let inner = WindowInner::from_pub(window);
    let Some((item, _)) = stops(window).into_iter().find(|(item, _)| name_of(item) == name) else { return false };
    inner.set_focus_item(&item, true, FocusReason::TabNavigation);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect { Rect { x, y, w, h } }

    #[test]
    fn the_focus_goes_to_the_nearest_stop_that_way() {
        // A row of three buttons over a wide field, and a far button off to the right.
        let stops = [r(0.0, 0.0, 80.0, 30.0), r(100.0, 0.0, 80.0, 30.0), r(200.0, 0.0, 80.0, 30.0), r(0.0, 50.0, 280.0, 30.0), r(600.0, 200.0, 80.0, 30.0)];
        assert_eq!(nearest(&stops, stops[0], Way::Right), Some(1));
        assert_eq!(nearest(&stops, stops[1], Way::Right), Some(2));
        assert_eq!(nearest(&stops, stops[2], Way::Down), Some(3), "the field below, in the beam, beats the nearer-diagonal far one");
        assert_eq!(nearest(&stops, stops[3], Way::Up), Some(1), "the button straight above the field's middle");
        assert_eq!(nearest(&stops, stops[0], Way::Left), None);
        assert_eq!(nearest(&stops, stops[3], Way::Right), Some(4));
    }

    slint::slint! {
        export component PadGrid inherits Window {
            width: 400px;
            height: 200px;
            out property <string> focused-name;
            for name[i] in ["a", "b", "c"]: Rectangle {
                x: 20px + i * 120px;
                y: 20px;
                width: 100px;
                height: 30px;
                scope := FocusScope {
                    focus-changed-event => { if self.has-focus { root.focused-name = name; } }
                    key-pressed(event) => { if event.text == Key.Return { root.focused-name = name + "!"; return accept; } return reject; }
                }
            }
            Rectangle {
                x: 20px;
                y: 100px;
                width: 340px;
                height: 30px;
                FocusScope {
                    focus-changed-event => { if self.has-focus { root.focused-name = "field"; } }
                }
            }
        }
    }

    /// The walker drives a real Slint component (software renderer, no window on screen).
    #[test]
    fn the_walker_moves_and_presses_in_a_real_component() {
        use slint::ComponentHandle;
        let _window = super::super::sync::headless::window(400, 200);
        let ui = PadGrid::new().unwrap();
        ui.show().unwrap();
        let w = ui.window();
        assert!(step(w, Way::Right));
        assert_eq!(ui.get_focused_name(), "a", "nothing focused: the first stop");
        step(w, Way::Right);
        assert_eq!(ui.get_focused_name(), "b");
        step(w, Way::Down);
        assert_eq!(ui.get_focused_name(), "field");
        step(w, Way::Up);
        assert_eq!(ui.get_focused_name(), "b", "back up to the button over the field's middle");
        press(w);
        assert_eq!(ui.get_focused_name(), "b!");
    }

    /// The queued controller commands drive the walker and become core messages.
    #[test]
    fn queued_controller_commands_move_press_and_publish() {
        use crate::controller_nav::{Command, Direction};
        use crate::Message;
        use slint::ComponentHandle;
        let _window = super::super::sync::headless::window(400, 200);
        let ui = PadGrid::new().unwrap();
        ui.show().unwrap();
        let w = ui.window();
        assert!(!has_focus(w));
        assert!(apply(w, Command::Move(Direction::Right)).is_none());
        assert_eq!(ui.get_focused_name(), "a");
        assert!(has_focus(w));
        apply(w, Command::Move(Direction::Right));
        assert!(apply(w, Command::Select).is_none());
        assert_eq!(ui.get_focused_name(), "b!");
        assert!(matches!(apply(w, Command::Page(1)), Some(Message::ControllerPage(1))));
        assert!(matches!(apply(w, Command::Page(-1)), Some(Message::ControllerPage(-1))));
        assert!(matches!(apply(w, Command::Play), Some(Message::ControllerPlay)));
        assert!(matches!(apply(w, Command::Back), Some(Message::ControllerBack)));
        // The core's queue, drained as the host drains it.
        let mut nav = crate::controller_nav::State::default();
        nav.preview_focus();
        assert_eq!(nav.take_pending(), vec![Command::Move(Direction::Down)]);
        assert!(nav.take_pending().is_empty());
    }
}
