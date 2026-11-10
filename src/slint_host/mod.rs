//! The front end: the core (`App`, its effects and schedule) drawn by Slint on winit.
//!
//! * Messages go through one queue on the UI thread (`send`); each batch ends with one view
//!   sync (`view::sync`) and one schedule check.
//! * Effects: messages and window commands run inline, futures on the shared runtime
//!   (`runtime`), their outcome posted back with `slint::invoke_from_event_loop`.
//! * Background mode: closing the window drops the component, its winit window and its
//!   renderer; the event loop keeps running with no window (`run_event_loop_until_quit`), and
//!   the core trims memory 3 s later.
slint::include_modules!();

#[cfg_attr(not(test), allow(dead_code))]
mod pad;
mod dialogs;
mod downloads;
mod jobs;
mod features;
mod settings;
mod sync;
mod view;
mod friends;
#[cfg(test)]
mod window_tests;

use crate::effect::{Edge, Effect, Screenshot, Size, WindowId, WindowOp};
use crate::{App, Message};
use slint::winit_030::{winit, EventResult, WinitWindowAccessor};
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The renderer used unless SLINT_BACKEND names one: the first compiled-in of skia, femtovg,
/// software (Cargo.toml's default says which ships).
const RENDERER: &str = if cfg!(feature = "skia") { "skia" } else if cfg!(feature = "femtovg") { "femtovg" } else { "software" };

/// The layout scale: 1 at the 1440 px design width and wider (the layout reflows into extra
/// room), shrinking with the window below it to a floor at the 900 px minimum window.
pub(super) fn scale(app: &App) -> f32 {
    (app.viewport.width / 1440.0).clamp(0.8, 1.0)
}

/// Runs the window loop until the core exits.
pub(crate) fn run() -> Result<(), slint::PlatformError> {
    let renderer = std::env::var("CRYSTAL_RENDERER").unwrap_or_else(|_| RENDERER.to_owned());
    let mut selector = slint::BackendSelector::new().backend_name("winit".into());
    if std::env::var_os("SLINT_BACKEND").is_none() {
        selector = selector.renderer_name(renderer);
    }
    selector
        .with_winit_window_attributes_hook(|attributes| {
            use winit::platform::windows::{CornerPreference, WindowAttributesExtWindows};
            // Rounded corners from the start; the shadow and border colour are added by
            // window_chrome once the window has a handle.
            attributes.with_corner_preference(CornerPreference::Round)
        })
        .select()?;
    let (app, effect) = App::new();
    let host = Rc::new(Host {
        app: RefCell::new(app),
        window: RefCell::new(None),
        queue: RefCell::new(VecDeque::new()),
        draining: Cell::new(false),
        tick: Cell::new(None),
        tick_generation: Arc::new(AtomicU64::new(0)),
        gamepad: Cell::new(false),
        gamepad_generation: Arc::new(AtomicU64::new(0)),
        jobs: RefCell::new(jobs::Jobs::default()),
        repeat: slint::Timer::default(),
        account_drag: Cell::new(false),
    });
    HOST.with(|slot| *slot.borrow_mut() = Some(host.clone()));
    // The notification-area icon's commands, and a second Crystal start's open request.
    if let Some(mut commands) = crate::tray::take_commands() {
        crate::runtime::spawn(async move {
            while let Some(command) = commands.recv().await {
                post(Message::Tray(command));
            }
        });
    }
    host.run(effect);
    host.drain();
    let result = slint::run_event_loop_until_quit();
    HOST.with(|slot| slot.borrow_mut().take());
    result
}

struct Host {
    app: RefCell<App>,
    /// The launcher window while it is open.
    window: RefCell<Option<Shown>>,
    queue: RefCell<VecDeque<Message>>,
    draining: Cell<bool>,
    /// The running Tick loop's rate.
    tick: Cell<Option<Duration>>,
    /// Bumped to stop the running Tick loop (a new rate starts a new one).
    tick_generation: Arc<AtomicU64>,
    /// The controller thread runs.
    gamepad: Cell<bool>,
    /// Bumped to stop the running controller thread.
    gamepad_generation: Arc<AtomicU64>,
    /// The Windows components and dgVoodoo jobs.
    jobs: RefCell<jobs::Jobs>,
    /// A held controller direction's repeat (ControllerTick every 50 ms while it is held).
    repeat: slint::Timer,
    /// An account row is being dragged: a left release ends it, losing the focus cancels it.
    account_drag: Cell<bool>,
}

struct Shown {
    id: WindowId,
    ui: MainWindow,
    hwnd: isize,
}

thread_local! {
    static HOST: RefCell<Option<Rc<Host>>> = const { RefCell::new(None) };
}

fn host() -> Option<Rc<Host>> {
    HOST.with(|slot| slot.borrow().clone())
}

/// Deliver `message` to the core (UI thread).
pub(super) fn send(message: Message) {
    if let Some(host) = host() {
        host.queue.borrow_mut().push_back(message);
        host.drain();
    } else {
        #[cfg(test)]
        SENT.with(|sent| sent.borrow_mut().push(message));
    }
}

#[cfg(test)]
thread_local! {
    /// What the view sent with no host running (tests).
    static SENT: RefCell<Vec<Message>> = const { RefCell::new(Vec::new()) };
}

/// The messages sent since the last call (tests, which run no host).
#[cfg(test)]
pub(super) fn take_sent() -> Vec<Message> { SENT.with(|sent| sent.take()) }

/// Deliver `message` from any thread.
fn post(message: Message) {
    let _ = slint::invoke_from_event_loop(move || send(message));
}

/// Run `effect` on the UI thread, from any thread.
fn post_effect(effect: Effect<Message>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(host) = host() {
            host.run(effect);
            host.drain();
        }
    });
}

impl Host {
    /// Updates the core with every queued message, then syncs the view and the schedule once.
    fn drain(&self) {
        if self.draining.replace(true) { return; }
        loop {
            let Some(message) = self.queue.borrow_mut().pop_front() else { break };
            let effect = self.app.borrow_mut().update(message);
            self.run(effect);
        }
        // The launch profiles the open sheet shows, read off the UI thread when due.
        let effect = self.app.borrow_mut().feature_profiles_due();
        self.run(effect);
        self.after_update();
        self.draining.set(false);
        // A window event raised while syncing.
        if !self.queue.borrow().is_empty() { self.drain(); }
    }

    fn after_update(&self) {
        let app = self.app.borrow();
        if let Some(shown) = self.window.borrow().as_ref() {
            shown.ui.set_window_maximized(app.maximized);
            let theme = shown.ui.global::<Theme>();
            theme.set_light(app.settings.light_theme);
            theme.set_scale(scale(&app));
            view::sync(&shown.ui, &app);
            friends::sync(&shown.ui, &app);
            downloads::sync(&shown.ui, &app);
            settings::sync(&shown.ui, &app);
            features::sync(&shown.ui, &app);
            dialogs::sync(&shown.ui, &app);
        }
        let schedule = app.schedule();
        let gamepad = schedule.gamepad && crate::gamepad::watching(&app.gamepad);
        let wanted = jobs::wanted(&app);
        // The controller's queued commands.
        // They wait for a window (--preview-controller-nav queues one before it opens).
        let commands = if self.window.borrow().is_some() { app.controller_nav.take_pending() } else { Vec::new() };
        drop(app);
        self.set_tick(schedule.tick);
        self.set_gamepad(gamepad);
        self.set_repeat(schedule.controller_repeat);
        self.account_drag.set(schedule.account_drag);
        self.jobs.borrow_mut().set(&wanted, |key| jobs::start(key, post));
        // Slint animates transitions itself (Theme.calm / Theme.quick; a row's Dot fades its
        // colour), so the core sends no frame messages.
        if !commands.is_empty() {
            let window = self.window.borrow();
            if let Some(shown) = window.as_ref() {
                for command in commands {
                    if let Some(message) = pad::apply(shown.ui.window(), command) {
                        self.queue.borrow_mut().push_back(message);
                    }
                }
            }
        }
    }

    /// Starts or stops the 50 ms ControllerTick repeat (while schedule.controller_repeat is set).
    fn set_repeat(&self, on: bool) {
        if on == self.repeat.running() { return; }
        if on {
            self.repeat.start(slint::TimerMode::Repeated, Duration::from_millis(50), || send(Message::ControllerTick));
        } else {
            self.repeat.stop();
        }
    }

    /// Starts or stops the controller thread (gamepad::detect):
    /// every change of the controllers or their input comes back as one Gamepad message, in order.
    fn set_gamepad(&self, on: bool) {
        if self.gamepad.replace(on) == on { return; }
        let generation = self.gamepad_generation.fetch_add(1, Ordering::AcqRel) + 1;
        if !on { return; }
        let (current, still) = (self.gamepad_generation.clone(), self.gamepad_generation.clone());
        let _ = std::thread::Builder::new().name("gamepad".into()).spawn(move || {
            crate::gamepad::detect::run(
                |pads| {
                    current.load(Ordering::Acquire) == generation
                        && slint::invoke_from_event_loop(move || send(Message::Gamepad(crate::gamepad::Msg::Pads(pads)))).is_ok()
                },
                || still.load(Ordering::Acquire) == generation,
            );
        });
    }

    /// Runs `effect`: messages are queued for this drain, window commands run now, futures go
    /// to the runtime.
    fn run(&self, effect: Effect<Message>) {
        match effect {
            Effect::None => {}
            Effect::Message(message) => self.queue.borrow_mut().push_back(message),
            Effect::Future(future) => crate::runtime::spawn(async move { post_effect(future.await) }),
            Effect::Batch(effects) => effects.into_iter().for_each(|effect| self.run(effect)),
            Effect::Chain(first, next) if first.is_async() => crate::runtime::spawn(async move {
                complete(*first).await;
                post_effect(*next);
            }),
            Effect::Chain(first, next) => {
                self.run(*first);
                self.run(*next);
            }
            Effect::Window(id, op) => {
                let follow = self.window_op(id, op);
                self.run(follow);
            }
            Effect::Exit => {
                self.close_window();
                self.tick_generation.fetch_add(1, Ordering::AcqRel);
                self.gamepad_generation.fetch_add(1, Ordering::AcqRel);
                self.repeat.stop();
                self.jobs.borrow_mut().stop();
                let _ = slint::quit_event_loop();
            }
        }
    }

    /// (Re)starts the Tick loop when its rate changed.
    fn set_tick(&self, every: Duration) {
        if self.tick.replace(Some(every)) == Some(every) { return; }
        let generation = self.tick_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let current = self.tick_generation.clone();
        crate::runtime::spawn(crate::tick_gate::run(every, move || {
            let alive = current.load(Ordering::Acquire) == generation;
            async move { alive && slint::invoke_from_event_loop(|| send(Message::Tick)).is_ok() }
        }));
    }

    fn window_op(&self, id: WindowId, op: WindowOp<Message>) -> Effect<Message> {
        if let WindowOp::Open { size, maximized } = op {
            self.open_window(id, size, maximized);
            return Effect::none();
        }
        if matches!(op, WindowOp::Close) {
            if self.window.borrow().as_ref().is_some_and(|shown| shown.id == id) { self.close_window(); }
            return Effect::none();
        }
        let window = self.window.borrow();
        let Some(shown) = window.as_ref().filter(|shown| shown.id == id) else { return Effect::none() };
        let slint_window = shown.ui.window();
        match op {
            WindowOp::Open { .. } | WindowOp::Close => Effect::none(),
            WindowOp::Minimize(on) => { slint_window.set_minimized(on); Effect::none() }
            WindowOp::ToggleMaximize => { slint_window.set_maximized(!slint_window.is_maximized()); Effect::none() }
            WindowOp::Drag => { slint_window.with_winit_window(|w| { let _ = w.drag_window(); }); Effect::none() }
            WindowOp::DragResize(edge) => { slint_window.with_winit_window(|w| { let _ = w.drag_resize_window(direction(edge)); }); Effect::none() }
            WindowOp::Focus => {
                slint_window.with_winit_window(|w| w.focus_window());
                bring_forward(shown.hwnd);
                Effect::none()
            }
            WindowOp::IsMaximized(answer) => answer(slint_window.is_maximized()),
            WindowOp::NativeHandle(answer) => answer(shown.hwnd),
            WindowOp::Screenshot(answer) => match slint_window.take_snapshot() {
                Ok(shot) => answer(Screenshot { width: shot.width(), height: shot.height(), rgba: Arc::from(shot.as_bytes()) }),
                Err(_) => Effect::none(),
            },
        }
    }

    fn open_window(&self, id: WindowId, size: Size, maximized: bool) {
        if self.window.borrow().is_some() { return; }
        let Ok(ui) = MainWindow::new() else { return };
        wire_chrome(&ui);
        view::wire(&ui);
        friends::wire(&ui);
        downloads::wire(&ui);
        settings::wire(&ui);
        features::wire(&ui);
        dialogs::wire(&ui);
        // Escape no control took goes back.
        ui.on_escape(|| send(Message::ControllerBack));
        ui.set_window_icon(window_icon());
        let window = ui.window();
        window.set_size(slint::LogicalSize::new(size.width, size.height));
        if let Some(at) = centred(size) { window.set_position(at); }
        window.on_close_requested(|| {
            send(Message::CloseRequested);
            slint::CloseRequestResponse::KeepWindowShown
        });
        window.on_winit_window_event(|window, event| {
            use winit::event::{ElementState, MouseButton, WindowEvent};
            // Keyboard and pointer input: one UserInput per ACTIVITY_INTERVAL.
            if user_input(event) {
                if let Some(message) = crate::user_activity() { post(message); }
            }
            match event {
                WindowEvent::Focused(focused) => {
                    let focused = *focused;
                    if let Some(host) = host() {
                        if let Some(shown) = host.window.borrow().as_ref() { shown.ui.global::<Theme>().set_animate(focused); }
                        // account_order::input: losing the focus cancels a drag.
                        if !focused && host.account_drag.get() { post(Message::AccountDragCancel); }
                    }
                }
                // account_order::input: a left release anywhere ends a drag.
                WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                    if host().is_some_and(|host| host.account_drag.get()) { post(Message::AccountDragFinish); }
                }
                // Escape with nothing focused reaches no scope: it goes back here.
                WindowEvent::KeyboardInput { event: key, .. } if escape(key) && !pad::has_focus(window) => {
                    post(Message::ControllerBack);
                    return EventResult::PreventDefault;
                }
                _ => {}
            }
            EventResult::Propagate
        });
        {
            let app = self.app.borrow();
            let theme = ui.global::<Theme>();
            theme.set_light(app.settings.light_theme);
            view::sync(&ui, &app);
            friends::sync(&ui, &app);
            downloads::sync(&ui, &app);
            settings::sync(&ui, &app);
            features::sync(&ui, &app);
            dialogs::sync(&ui, &app);
        }
        if ui.show().is_err() { return; }
        if maximized { ui.window().set_maximized(true); }
        let hwnd = native_handle(ui.window());
        *self.window.borrow_mut() = Some(Shown { id, ui, hwnd });
        if let Some(shown) = self.window.borrow().as_ref() {
            shown.ui.window().with_winit_window(|w| w.focus_window());
            bring_forward(hwnd);
        }
        self.queue.borrow_mut().push_back(Message::WindowOpened(id));
    }

    /// Hides and drops the window: its winit window, surface and renderer go with it.
    fn close_window(&self) {
        let Some(shown) = self.window.borrow_mut().take() else { return };
        let _ = shown.ui.hide();
        let id = shown.id;
        drop(shown);
        view::forget();
        friends::forget();
        downloads::forget();
        settings::forget();
        features::forget();
        dialogs::forget();
        self.queue.borrow_mut().push_back(Message::WindowClosed(id));
    }
}

/// Drives an effect to its end from the runtime (a chain's first part): messages and window
/// commands go to the UI thread, and window commands' answers are awaited.
fn complete(effect: Effect<Message>) -> crate::effect::BoxFuture<()> {
    Box::pin(async move {
        match effect {
            Effect::None => {}
            Effect::Message(message) => post(message),
            Effect::Future(future) => complete(future.await).await,
            Effect::Batch(effects) => {
                let mut running = tokio::task::JoinSet::new();
                for effect in effects { running.spawn(complete(effect)); }
                while running.join_next().await.is_some() {}
            }
            Effect::Chain(first, next) => {
                complete(*first).await;
                complete(*next).await;
            }
            Effect::Window(id, op) => {
                let (answer, answered) = tokio::sync::oneshot::channel();
                let _ = slint::invoke_from_event_loop(move || {
                    let follow = host().map_or(Effect::none(), |host| host.window_op(id, op));
                    let _ = answer.send(follow);
                });
                if let Ok(follow) = answered.await { complete(follow).await; }
            }
            Effect::Exit => post_effect(Effect::Exit),
        }
    })
}

/// The window chrome's callbacks: the title bar's core messages.
fn wire_chrome(ui: &MainWindow) {
    use crate::i18n::t;
    ui.set_minimize_label(t("Minimize").into());
    ui.set_maximize_label(t("Maximize").into());
    ui.set_restore_label(t("Restore").into());
    ui.set_close_label(t("Close").into());
    ui.on_minimize_clicked(|| send(Message::MinimizeWindow));
    ui.on_maximize_clicked(|| send(Message::MaximizeWindow));
    ui.on_close_clicked(|| send(Message::CloseRequested));
    ui.on_title_pressed(|| send(Message::TitlePressed));
    ui.on_title_moved(|| send(Message::TitleMoved));
    ui.on_title_released(|| send(Message::TitleReleased));
    ui.on_resize_pressed(|edge| {
        const EDGES: [Edge; 8] = [Edge::North, Edge::South, Edge::East, Edge::West, Edge::NorthEast, Edge::NorthWest, Edge::SouthEast, Edge::SouthWest];
        if let Some(edge) = usize::try_from(edge).ok().and_then(|i| EDGES.get(i)) { send(Message::ResizeWindow(*edge)); }
    });
    ui.on_resized(|width, height| send(Message::WindowResized(Size::new(width, height))));
}

/// Whether the event is keyboard or pointer input (what counts as activity): a key or button press, the wheel, or the pointer moving.
fn user_input(event: &winit::event::WindowEvent) -> bool {
    use winit::event::{ElementState, WindowEvent};
    match event {
        WindowEvent::KeyboardInput { event, .. } => event.state == ElementState::Pressed,
        WindowEvent::MouseInput { state, .. } => *state == ElementState::Pressed,
        WindowEvent::MouseWheel { .. } | WindowEvent::CursorMoved { .. } => true,
        _ => false,
    }
}

/// A first press of Escape (not a key repeat).
fn escape(key: &winit::event::KeyEvent) -> bool {
    use winit::keyboard::{Key, NamedKey};
    key.state == winit::event::ElementState::Pressed && !key.repeat && key.logical_key == Key::Named(NamedKey::Escape)
}

/// The taskbar and Alt+Tab icon: assets/crystal-icon-64.rgba (64 x 64).
fn window_icon() -> slint::Image {
    const SIDE: u32 = 64;
    let rgba = include_bytes!("../../assets/crystal-icon-64.rgba");
    slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, SIDE, SIDE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::event::{DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};

    #[test]
    fn pointer_and_key_presses_count_as_activity_through_the_shared_throttle() {
        let device = DeviceId::dummy();
        let moved = WindowEvent::CursorMoved { device_id: device, position: winit::dpi::PhysicalPosition::new(4.0, 5.0) };
        let wheel = WindowEvent::MouseWheel { device_id: device, delta: MouseScrollDelta::LineDelta(0.0, 1.0), phase: TouchPhase::Moved };
        let press = WindowEvent::MouseInput { device_id: device, state: ElementState::Pressed, button: MouseButton::Left };
        let release = WindowEvent::MouseInput { device_id: device, state: ElementState::Released, button: MouseButton::Left };
        assert!(user_input(&moved) && user_input(&wheel) && user_input(&press));
        assert!(!user_input(&release) && !user_input(&WindowEvent::Focused(true)), "only keyboard and pointer input counts");
        // The throttle is the one every front end shares: at most one UserInput per interval.
        let first = crate::user_activity();
        let second = crate::user_activity();
        assert!(second.is_none() || first.is_none(), "two reports inside one interval");
    }
}

fn direction(edge: Edge) -> winit::window::ResizeDirection {
    use winit::window::ResizeDirection;
    match edge {
        Edge::North => ResizeDirection::North,
        Edge::South => ResizeDirection::South,
        Edge::East => ResizeDirection::East,
        Edge::West => ResizeDirection::West,
        Edge::NorthEast => ResizeDirection::NorthEast,
        Edge::NorthWest => ResizeDirection::NorthWest,
        Edge::SouthEast => ResizeDirection::SouthEast,
        Edge::SouthWest => ResizeDirection::SouthWest,
    }
}

fn native_handle(window: &slint::Window) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

/// The window's top-left for `size` centred on the primary monitor's work area (physical px).
fn centred(size: Size) -> Option<slint::PhysicalPosition> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    let mut area = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut area as *mut RECT).cast(), 0) } == 0 { return None; }
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let (w, h) = ((size.width * scale) as i32, (size.height * scale) as i32);
    Some(slint::PhysicalPosition::new(
        area.left + ((area.right - area.left - w) / 2).max(0),
        area.top + ((area.bottom - area.top - h) / 2).max(0),
    ))
}

/// A window opened from the background (the tray, a second start) comes to the front.
fn bring_forward(hwnd: isize) {
    if hwnd == 0 { return; }
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd as _); }
}
