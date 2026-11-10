//! The parked warm host (pol-spec 07d §1.3; 07a parked-warm-host, the
//! conservative variant "park only up to polcore creation"). After a `park`
//! line the game thread creates polcore up to the validated function table and
//! stops there: no bootstrap, no 870/871, no member select, no Open, no
//! request and no polcore entry call of any kind, so nothing is networked. The
//! main thread sends `parked`, then waits for `credentials` with no timeout;
//! from them on the host runs exactly the cold sign-in.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::pipe::PipeError;
use crate::{log, proto};

/// While parked the game thread dispatches its window messages at least this
/// often (COM, the injector's windows) and does nothing else (07d §1.3 step 3).
pub(crate) const PUMP: Duration = Duration::from_millis(100);

/// Game thread, parked: `pump` (window messages only), then wait up to
/// `interval` for the Play, until it arrives. None when the main thread gave
/// up (the launcher closed the pipe, or its credentials line was rejected).
pub(crate) fn wait_for_play<T>(play: &Receiver<T>, interval: Duration, pump: &mut dyn FnMut()) -> Option<T> {
    loop {
        pump();
        match play.recv_timeout(interval) {
            Ok(play) => return Some(play),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Main thread, steps 4-8 once polcore is ready: `parked` first (credentials
/// already sent wait in the pipe, so a Play during the park is used right
/// after it), then the credentials line with no timeout. `Err(exit code)`
/// once handled: a closed pipe exits with nothing sent; a malformed line gets
/// an `error` line (never quoting the input) and EXIT_PROTOCOL. Either way
/// nothing reached Square Enix and no polcore entry was called.
pub(crate) fn await_credentials(
    pid: u32,
    park_started: Instant,
    polcore: Duration,
    write: &mut dyn FnMut(&[u8]) -> Result<(), PipeError>,
    read: &mut dyn FnMut() -> Result<Result<proto::Unpark, proto::ProtoError>, PipeError>,
) -> Result<(proto::Unpark, Duration), i32> {
    let closed = || {
        log!("stage: the launcher closed the pipe while parked; exiting");
        crate::EXIT_PIPE
    };
    match write(proto::parked_line(pid, polcore.as_millis() as u64).as_bytes()) {
        Ok(()) => {}
        Err(PipeError::Closed) => return Err(closed()),
        Err(e) => {
            log!("error: sending parked: {e}");
            return Err(crate::EXIT_PIPE);
        }
    }
    let parked = Instant::now();
    log!("stage: parked: polcore ready in {:.2}s (instance {:.2}s); waiting for credentials (no timeout)",
        parked.duration_since(park_started).as_secs_f64(), polcore.as_secs_f64());
    match read() {
        Ok(Ok(unpark)) => {
            let waited = parked.elapsed();
            log!("stage: credentials after {:.1}s parked", waited.as_secs_f64());
            Ok((unpark, waited))
        }
        Ok(Err(e)) => {
            let message = format!("credentials rejected: {e}");
            log!("error: {message}");
            let _ = write(proto::error_line(&message).as_bytes());
            Err(crate::EXIT_PROTOCOL)
        }
        Err(PipeError::Closed) => Err(closed()),
        Err(e) => {
            log!("error: reading the credentials: {e}");
            Err(crate::EXIT_PIPE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn credentials() -> String {
        use crate::b64::encode;
        format!("{{\"type\":\"credentials\",\"start_game\":false,\"keep_session\":false,\"viewer_gaps\":false,\"auto_away_minutes\":20,\
            \"native_auth\":{{\"pol_id\":\"{}\",\"pol_password\":\"{}\",\"se_id\":\"{}\",\"se_secret\":\"{}\",\"otp\":\"{}\",\"profile_key\":\"{}\"}}}}",
            encode(b"ABCD1234"), encode(b"fake password"), encode(b"fakeuser"), encode(&[0x51; 20]), encode(b"123456"), "a".repeat(40))
    }

    /// The pipe as both sides saw it: `parked` before any read, then the read.
    fn run(lines: Vec<Result<String, PipeError>>) -> (Result<(proto::Unpark, Duration), i32>, Vec<String>) {
        let order = std::cell::RefCell::new(Vec::new());
        let mut lines = lines.into_iter();
        let result = await_credentials(77, Instant::now(), Duration::from_millis(431),
            &mut |bytes| { order.borrow_mut().push(String::from_utf8(bytes.to_vec()).unwrap()); Ok(()) },
            &mut || {
                order.borrow_mut().push("read".into());
                lines.next().unwrap().map(|line| proto::parse_credentials(line.as_bytes()))
            });
        (result, order.into_inner())
    }

    #[test]
    fn parked_goes_out_before_the_credentials_are_read_and_they_unpark_the_host() {
        let (result, order) = run(vec![Ok(credentials())]);
        assert_eq!(order, ["{\"type\":\"parked\",\"pid\":77,\"polcore_ms\":431}\n", "read"]);
        let (unpark, _) = result.unwrap();
        assert!(!unpark.start_game && unpark.auto_away_minutes == 20);
        assert_eq!(&unpark.native_auth.pol_id, b"ABCD1234\0");
    }

    #[test]
    fn a_closed_pipe_exits_quietly_and_a_malformed_line_gets_an_error_line_without_its_input() {
        let (result, order) = run(vec![Err(PipeError::Closed)]);
        assert_eq!(result.err(), Some(crate::EXIT_PIPE));
        assert_eq!(order.len(), 2, "parked, then the read; nothing else is written");
        for bad in [credentials().replace("\"credentials\"", "\"session\""), credentials().replace(",\"keep_session\"", ",\"region\":\"US\",\"keep_session\""), "{".into()] {
            let (result, order) = run(vec![Ok(bad)]);
            assert_eq!(result.err(), Some(crate::EXIT_PROTOCOL));
            assert_eq!(order.len(), 3);
            assert!(order[2].starts_with("{\"type\":\"error\"") && order[2].contains("credentials rejected"), "{}", order[2]);
            assert!(!order[2].contains("fake password") && !order[2].contains("ABCD1234"));
        }
        // A launcher that is gone before `parked` gets nothing more.
        let mut reads = 0;
        let result = await_credentials(1, Instant::now(), Duration::ZERO, &mut |_| Err(PipeError::Closed),
            &mut || { reads += 1; Err(PipeError::Closed) });
        assert_eq!((result.err(), reads), (Some(crate::EXIT_PIPE), 0));
    }

    #[test]
    fn the_parked_game_thread_only_pumps_until_the_play_or_the_main_thread_gives_up() {
        let (tx, rx) = mpsc::channel();
        let mut pumps = 0;
        let mut tx = Some(tx);
        let play = wait_for_play(&rx, Duration::from_millis(1), &mut || {
            pumps += 1;
            if pumps == 5 { tx.take().unwrap().send(7).unwrap(); }
        });
        assert_eq!((play, pumps), (Some(7), 5));
        let (tx, rx) = mpsc::channel::<u8>();
        drop(tx);
        let mut pumps = 0;
        assert_eq!(wait_for_play(&rx, PUMP, &mut || pumps += 1), None);
        assert_eq!(pumps, 1);
    }
}
