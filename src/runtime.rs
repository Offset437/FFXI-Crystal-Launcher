//! The one async runtime: every effect's future, network refresh and timer runs here.
//! Multi-threaded with IO and time on (`Runtime::new`), as the launcher has always run its
//! requests, so their order and timing are unchanged.
use futures_util::{Stream, StreamExt};
use std::sync::OnceLock;

/// The process-wide runtime, created on first use.
pub(crate) fn get() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("the async runtime starts"))
}

/// Run `future` on the runtime; its output is dropped.
pub(crate) fn spawn(future: impl std::future::Future<Output = ()> + Send + 'static) {
    drop(get().spawn(future));
}

/// What `work` sends on its channel (of `size` slots), as a stream that ends when `work` has
/// finished and everything it sent has been read. Dropping the stream drops `work`.
pub(crate) fn channel<T, F>(size: usize, work: impl FnOnce(futures_channel::mpsc::Sender<T>) -> F) -> impl Stream<Item = T>
where
    F: std::future::Future<Output = ()>,
{
    let (sender, receiver) = futures_channel::mpsc::channel(size);
    let runner = futures_util::stream::once(work(sender)).filter_map(|()| async { None });
    futures_util::stream::select(receiver, runner)
}
