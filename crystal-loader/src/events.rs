//! The pipe after the handshake. Loader -> launcher lines are queued and
//! written by one thread, so a slow launcher never stalls the owner or a game
//! callback. A second thread reads the launcher's commands.
//!
//! Both threads share one overlapped handle, each operation with its own
//! OVERLAPPED, so a pending read never holds up a write.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::log;
use crate::pipe::{LineReader, Pipe, PipeError};
use crate::proto;

struct Sink {
    tx: Sender<String>,
    thread: JoinHandle<()>,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static CLOSED: AtomicBool = AtomicBool::new(false);
static STOP_READER: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! { static CAPTURE: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) }; }

#[cfg(test)]
pub(crate) fn capture(work: impl FnOnce()) -> Vec<String> {
    CAPTURE.with(|events| *events.borrow_mut() = Some(Vec::new()));
    work();
    CAPTURE.with(|events| events.borrow_mut().take().unwrap())
}

/// True once the launcher's end of the pipe is gone.
pub fn pipe_closed() -> bool {
    #[cfg(test)]
    if TEST_CLOSED.with(|c| c.get()) { return true; }
    CLOSED.load(Ordering::Acquire)
}
// A test closes only its own thread's pipe; CLOSED is process-wide.
#[cfg(test)]
thread_local! { static TEST_CLOSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) fn set_closed_for_test(closed: bool) { TEST_CLOSED.with(|c| c.set(closed)); }

fn mark_closed(why: &str) {
    if !CLOSED.swap(true, Ordering::AcqRel) {
        log!("launcher pipe closed ({why})");
    }
}

/// Starts the writer and continues the existing reader on the connected pipe.
/// It may already hold commands following the session or credentials line.
pub fn start(pipe: Pipe, mut reader: LineReader) {
    let pipe = Arc::new(pipe);
    let (tx, rx) = mpsc::channel::<String>();
    let writer_pipe = Arc::clone(&pipe);
    let thread = std::thread::Builder::new()
        .name("crystal-events".into())
        .spawn(move || {
            for line in rx {
                if pipe_closed() {
                    continue;
                }
                match writer_pipe.write_all(line.as_bytes()) {
                    Ok(()) => {}
                    Err(PipeError::Closed) => mark_closed("write"),
                    Err(e) => {
                        log!("writing a status event failed: {e}");
                        mark_closed("write error");
                    }
                }
            }
        })
        .expect("spawn event writer");
    // A buffered command can reply as soon as the reader starts.
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink { tx, thread });
    std::thread::Builder::new()
        .name("crystal-replies".into())
        .spawn(move || {
            while !STOP_READER.load(Ordering::Acquire) {
                match reader.read_parsed(&pipe, Some(Duration::from_millis(250)), proto::parse_service_event) {
                    Ok(parsed) => match parsed {
                        Ok(Some(proto::ServiceEvent::NativeActivity)) => crate::native_auth::note_activity(),
                        Ok(Some(proto::ServiceEvent::NativeStartGame)) => crate::native_auth::request_start_game(),
                        Ok(Some(proto::ServiceEvent::NativeBatchGame(token))) => crate::native_auth::request_batch_game(&token),
                        Ok(Some(proto::ServiceEvent::NativeSignOut)) => crate::native_auth::request_sign_out(),
                        Ok(Some(proto::ServiceEvent::NativeStatus { mode, groups })) => crate::native_auth::request_status(mode, groups),
                        Ok(Some(proto::ServiceEvent::NativeSocial(request))) => crate::native_auth::request_social(request),
                        Ok(Some(proto::ServiceEvent::ViewerCheck(result))) => crate::native_auth::viewer_check_result(result),
                        Ok(Some(proto::ServiceEvent::NativePlayPending)) => crate::native_auth::note_play_pending(),
                        Ok(Some(proto::ServiceEvent::ControllerSettings { id, settings, deadline_ms })) => crate::controller_settings::submit(id, settings, deadline_ms),
                        Ok(Some(proto::ServiceEvent::GameShutdown { id, game, deadline_ms })) => crate::game_shutdown::submit(id, game, deadline_ms),
                        Ok(None) => {}
                        Err(e) => log!("ignoring a malformed launcher line: {e}"),
                    },
                    Err(PipeError::Timeout(_)) => {}
                    Err(PipeError::Closed) => {
                        mark_closed("read");
                        return;
                    }
                    Err(e) => {
                        log!("reading from the launcher failed: {e}");
                        mark_closed("read error");
                        return;
                    }
                }
            }
        })
        .expect("spawn reply reader");
}

/// Queues one line (must end in `\n`). A no-op before `start` or after `finish`.
pub fn send(line: String) {
    #[cfg(test)]
    CAPTURE.with(|events| { if let Some(events) = events.borrow_mut().as_mut() { events.push(line.clone()); } });
    if let Some(s) = SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = s.tx.send(line);
    }
}

/// Flushes queued lines (waiting at most `timeout`) and stops both threads.
pub fn finish(timeout: Duration) {
    STOP_READER.store(true, Ordering::Release);
    let sink = SINK.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(Sink { tx, thread }) = sink {
        drop(tx);
        let deadline = Instant::now() + timeout;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            log!("launcher did not drain the pipe in time; exiting anyway");
        }
    }
}
