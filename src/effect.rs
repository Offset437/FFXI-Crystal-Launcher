//! What `App::update` asks of the window host after a message, in terms no toolkit owns:
//! nothing, more messages, async work on the shared runtime, window commands or exit
//! (`slint_host` runs them).
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[must_use = "an effect does nothing until the front end runs it"]
pub(crate) enum Effect<M> {
    None,
    Message(M),
    /// Async work on the shared runtime; its output is the follow-up effect.
    Future(BoxFuture<Effect<M>>),
    /// Run together.
    Batch(Vec<Effect<M>>),
    /// The second starts once the first has finished (every message it produced delivered).
    Chain(Box<Effect<M>>, Box<Effect<M>>),
    Window(WindowId, WindowOp<M>),
    /// End the window loop (main() then finishes the close).
    Exit,
}

impl<M: Send + 'static> Effect<M> {
    pub(crate) fn none() -> Self { Self::None }

    pub(crate) fn done(message: M) -> Self { Self::Message(message) }

    pub(crate) fn perform<T>(future: impl Future<Output = T> + Send + 'static, f: impl FnOnce(T) -> M + Send + 'static) -> Self {
        Self::Future(Box::pin(async move { Self::Message(f(future.await)) }))
    }

    /// Async work whose outcome is itself an effect.
    pub(crate) fn future(future: impl Future<Output = Self> + Send + 'static) -> Self {
        Self::Future(Box::pin(future))
    }

    pub(crate) fn batch(effects: impl IntoIterator<Item = Self>) -> Self {
        let mut effects: Vec<Self> = effects.into_iter().filter(|e| !matches!(e, Self::None)).collect();
        match effects.len() {
            0 => Self::None,
            1 => effects.pop().unwrap_or(Self::None),
            _ => Self::Batch(effects),
        }
    }

    pub(crate) fn chain(self, next: Self) -> Self {
        match (self, next) {
            (Self::None, next) => next,
            (first, Self::None) => first,
            (first, next) => Self::Chain(Box::new(first), Box::new(next)),
        }
    }

    pub(crate) fn window(id: WindowId, op: WindowOp<M>) -> Self { Self::Window(id, op) }

    /// Each message this effect produces becomes `f`'s.
    pub(crate) fn map<N: Send + 'static>(self, f: impl Fn(M) -> N + Send + Sync + 'static) -> Effect<N> {
        self.map_shared(Arc::new(f))
    }

    fn map_shared<N: Send + 'static>(self, f: Arc<dyn Fn(M) -> N + Send + Sync>) -> Effect<N> {
        match self {
            Self::None => Effect::None,
            Self::Message(m) => Effect::Message(f(m)),
            Self::Future(future) => Effect::Future(Box::pin(async move { future.await.map_shared(f) })),
            Self::Batch(effects) => Effect::Batch(effects.into_iter().map(|e| e.map_shared(f.clone())).collect()),
            Self::Chain(first, next) => Effect::Chain(Box::new(first.map_shared(f.clone())), Box::new(next.map_shared(f))),
            Self::Window(id, op) => Effect::Window(id, op.compose(move |e| e.map_shared(f))),
            Self::Exit => Effect::Exit,
        }
    }

    /// How many separate pieces of work this starts (tests).
    #[cfg(test)]
    pub(crate) fn units(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Batch(effects) => effects.iter().map(Self::units).sum(),
            Self::Chain(first, next) => first.units() + next.units(),
            Self::Message(_) | Self::Future(_) | Self::Window(..) | Self::Exit => 1,
        }
    }

    /// Whether running this needs the runtime (a future somewhere in it).
    pub(crate) fn is_async(&self) -> bool {
        match self {
            Self::Future(_) => true,
            Self::Batch(effects) => effects.iter().any(Self::is_async),
            Self::Chain(first, next) => first.is_async() || next.is_async(),
            _ => false,
        }
    }
}

impl<M> std::fmt::Debug for Effect<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Message(_) => f.write_str("Message"),
            Self::Future(_) => f.write_str("Future"),
            Self::Batch(effects) => f.debug_list().entries(effects).finish(),
            Self::Chain(first, next) => f.debug_tuple("Chain").field(first).field(next).finish(),
            Self::Window(id, op) => f.debug_tuple("Window").field(id).field(op).finish(),
            Self::Exit => f.write_str("Exit"),
        }
    }
}

/// The launcher window, as the core knows it: a new id each time the window opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WindowId(u64);

impl WindowId {
    pub(crate) fn unique() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// A logical size in px.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub(crate) const fn new(width: f32, height: f32) -> Self { Self { width, height } }
}

/// The edge or corner a resize drag holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge { North, South, East, West, NorthEast, NorthWest, SouthEast, SouthWest }

/// A capture of the window's pixels: RGBA, row by row, at physical size.
#[derive(Clone, Debug)]
pub(crate) struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

/// A window command's answer: the effect its value leads to.
type Answer<T, M> = Box<dyn FnOnce(T) -> Effect<M> + Send>;

/// A command for the launcher window. Those that answer carry what to do with the answer.
pub(crate) enum WindowOp<M> {
    /// Open the window at `size` (maximised when `maximized`), focused.
    Open { size: Size, maximized: bool },
    /// Close it: the renderer and its device go with it.
    Close,
    Minimize(bool),
    ToggleMaximize,
    /// A window move by the pointer, from the title bar.
    Drag,
    DragResize(Edge),
    /// Restore it if minimised and bring it to the front.
    Focus,
    IsMaximized(Answer<bool, M>),
    /// Its Win32 handle (0 when it has none).
    NativeHandle(Answer<isize, M>),
    Screenshot(Answer<Screenshot, M>),
}

impl<M: Send + 'static> WindowOp<M> {
    /// An answering command whose answer is the message `f` makes of it.
    pub(crate) fn is_maximized(f: impl FnOnce(bool) -> M + Send + 'static) -> Self { Self::IsMaximized(Box::new(move |v| Effect::Message(f(v)))) }
    pub(crate) fn native_handle(f: impl FnOnce(isize) -> M + Send + 'static) -> Self { Self::NativeHandle(Box::new(move |v| Effect::Message(f(v)))) }
    pub(crate) fn screenshot(f: impl FnOnce(Screenshot) -> M + Send + 'static) -> Self { Self::Screenshot(Box::new(move |v| Effect::Message(f(v)))) }

    fn compose<N: Send + 'static>(self, f: impl FnOnce(Effect<M>) -> Effect<N> + Send + 'static) -> WindowOp<N> {
        match self {
            Self::Open { size, maximized } => WindowOp::Open { size, maximized },
            Self::Close => WindowOp::Close,
            Self::Minimize(on) => WindowOp::Minimize(on),
            Self::ToggleMaximize => WindowOp::ToggleMaximize,
            Self::Drag => WindowOp::Drag,
            Self::DragResize(edge) => WindowOp::DragResize(edge),
            Self::Focus => WindowOp::Focus,
            Self::IsMaximized(a) => WindowOp::IsMaximized(Box::new(move |v| f(a(v)))),
            Self::NativeHandle(a) => WindowOp::NativeHandle(Box::new(move |v| f(a(v)))),
            Self::Screenshot(a) => WindowOp::Screenshot(Box::new(move |v| f(a(v)))),
        }
    }
}
impl<M> std::fmt::Debug for WindowOp<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Open { .. } => "Open",
            Self::Close => "Close",
            Self::Minimize(_) => "Minimize",
            Self::ToggleMaximize => "ToggleMaximize",
            Self::Drag => "Drag",
            Self::DragResize(_) => "DragResize",
            Self::Focus => "Focus",
            Self::IsMaximized(_) => "IsMaximized",
            Self::NativeHandle(_) => "NativeHandle",
            Self::Screenshot(_) => "Screenshot",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(effect: Effect<u32>) -> Vec<u32> {
        match effect {
            Effect::None => vec![],
            Effect::Message(m) => vec![m],
            Effect::Future(future) => messages(crate::runtime::get().block_on(future)),
            Effect::Batch(effects) => effects.into_iter().flat_map(messages).collect(),
            Effect::Chain(first, next) => messages(*first).into_iter().chain(messages(*next)).collect(),
            Effect::Window(..) | Effect::Exit => vec![],
        }
    }

    #[test]
    fn batch_drops_nothing_and_flattens_singletons() {
        assert!(matches!(Effect::<u32>::batch([Effect::none(), Effect::none()]), Effect::None));
        assert!(matches!(Effect::batch([Effect::none(), Effect::done(1)]), Effect::Message(1)));
        assert_eq!(messages(Effect::batch([Effect::done(1), Effect::perform(async { 2 }, |v| v)])), vec![1, 2]);
    }

    #[test]
    fn map_reaches_every_message() {
        let effect = Effect::batch([Effect::done(1), Effect::perform(async { 2 }, |v| v)]).chain(Effect::done(3));
        assert_eq!(messages(effect.map(|m| m * 10)), vec![10, 20, 30]);
    }

    #[test]
    fn a_window_answer_is_mapped() {
        let id = WindowId::unique();
        let Effect::Window(_, WindowOp::IsMaximized(answer)) = Effect::window(id, WindowOp::is_maximized(u32::from)).map(|m| m + 7) else { panic!() };
        assert_eq!(messages(answer(true)), vec![8]);
    }
}
