//! Window shutdown owns cleanup independently of the UI event loop. Start it
//! before destroying the last window, then join it after the UI runtime ends.

use std::{io, sync::Mutex, thread::{self, JoinHandle}};

type CloseResult = Result<(), String>;

struct Coordinator {
    cleanup: Mutex<Option<JoinHandle<CloseResult>>>,
}

impl Coordinator {
    const fn new() -> Self { Self { cleanup: Mutex::new(None) } }

    fn start(&self, cleanup: impl FnOnce() -> CloseResult + Send + 'static) -> io::Result<()> {
        let mut pending = self.cleanup.lock().unwrap_or_else(|error| error.into_inner());
        if pending.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Launcher shutdown cleanup is already running."));
        }
        *pending = Some(thread::Builder::new().name("launcher-close".into()).spawn(cleanup)?);
        Ok(())
    }

    fn finish(&self) -> CloseResult {
        let pending = self.cleanup.lock().unwrap_or_else(|error| error.into_inner()).take();
        match pending {
            Some(cleanup) => cleanup.join().unwrap_or_else(|_| Err("Launcher shutdown cleanup stopped unexpectedly.".into())),
            None => Ok(()),
        }
    }
}

static CLOSE: Coordinator = Coordinator::new();

/// Returns as soon as the cleanup thread is owned; closing the window does not
/// depend on that thread or on a UI task being polled.
pub(crate) fn start(cleanup: impl FnOnce() -> CloseResult + Send + 'static) -> io::Result<()> {
    CLOSE.start(cleanup)
}

/// Called after the event loop returns, keeping this process alive through graceful
/// native sign-out and the session workers' final Windower profile saves.
pub(crate) fn finish() -> CloseResult { CLOSE.finish() }

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::{mpsc, Arc}, time::Duration};

    #[test]
    fn cleanup_survives_ui_executor_shutdown_and_finish_waits_for_its_completion() {
        let coordinator = Arc::new(Coordinator::new());
        let (started, running) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            coordinator.start(move || {
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                completed.send(()).unwrap();
                Ok(())
            }).unwrap();
        });
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        // The UI can end while the same cleanup thread still owns its work.
        drop(runtime);
        assert!(matches!(completion.try_recv(), Err(mpsc::TryRecvError::Empty)));

        let (finishing, joining) = mpsc::channel();
        let (finished, joined) = mpsc::channel();
        let waiter = thread::spawn(move || {
            finishing.send(()).unwrap();
            finished.send(coordinator.finish()).unwrap();
        });
        joining.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(joined.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release.send(()).unwrap();
        joined.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn an_owned_cleanup_is_not_replaced_and_its_failure_reaches_process_shutdown() {
        let coordinator = Coordinator::new();
        assert!(coordinator.finish().is_ok());
        let (release, released) = mpsc::channel();
        coordinator.start(move || {
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            Err("synthetic cleanup failure".into())
        }).unwrap();
        assert_eq!(coordinator.start(|| Ok(())).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        release.send(()).unwrap();
        assert_eq!(coordinator.finish(), Err("synthetic cleanup failure".into()));
        assert!(coordinator.finish().is_ok());
    }
}
