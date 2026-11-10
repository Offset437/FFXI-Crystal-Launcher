//! Named-pipe transport: the loader's client end, plus a server end the tests
//! (including tests/startup_handoff.rs) use to stand in for the launcher.
//!
//! Both ends are overlapped handles: the reply reader blocks in one kernel
//! wait (with its timeout) while the event writer writes on the same handle,
//! each with its own OVERLAPPED. No polling.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use zeroize::{Zeroize, Zeroizing};

use crate::proto::MAX_LINE;

#[derive(Debug)]
pub enum PipeError {
    Io(&'static str, io::Error),
    Timeout(&'static str),
    Closed,
    LineTooLong,
    WrongServer { expected: u32, actual: u32 },
}

impl fmt::Display for PipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipeError::Io(what, e) => write!(f, "{what}: {e}"),
            PipeError::Timeout(what) => write!(f, "timed out {what}"),
            PipeError::Closed => write!(f, "the launcher closed the pipe"),
            PipeError::LineTooLong => write!(f, "line longer than {MAX_LINE} bytes"),
            PipeError::WrongServer { expected, actual } => {
                write!(f, "pipe server is process {actual}, not the launcher {expected}")
            }
        }
    }
}

use crate::win::wide;

fn last_error(what: &'static str) -> PipeError {
    PipeError::Io(what, io::Error::last_os_error())
}

/// The other end is gone, or another error with its stage.
fn io_error(code: u32, what: &'static str) -> PipeError {
    if matches!(code, ERROR_BROKEN_PIPE | ERROR_NO_DATA) { PipeError::Closed } else { PipeError::Io(what, io::Error::from_raw_os_error(code as i32)) }
}

/// One overlapped operation: its OVERLAPPED and a manual-reset event, both
/// alive until the operation is complete.
struct Overlapped { ov: OVERLAPPED, event: HANDLE }
impl Overlapped {
    fn new() -> Result<Box<Self>, PipeError> {
        // SAFETY: an unnamed manual-reset event with default security.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() { return Err(last_error("creating a pipe event")); }
        // SAFETY: an all-zero OVERLAPPED is its documented initial state.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event;
        Ok(Box::new(Self { ov, event }))
    }
}
impl Drop for Overlapped {
    fn drop(&mut self) {
        // SAFETY: owned; every operation using it completed before the drop.
        unsafe { CloseHandle(self.event) };
    }
}

pub struct Pipe {
    h: HANDLE,
}

// SAFETY: a pipe HANDLE may be used from any thread.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.h) };
    }
}

impl Pipe {
    /// Connects to `path` (`\\.\pipe\...`), retrying while the server is not up
    /// yet or busy, and checks that the server is `expected_server_pid`.
    pub fn connect(path: &str, expected_server_pid: u32, timeout: Duration) -> Result<Pipe, PipeError> {
        let name = wide(path);
        let deadline = Instant::now() + timeout;
        let pipe = loop {
            // SECURITY_IDENTIFICATION: the server may identify us, never impersonate us.
            // SAFETY: name is NUL-terminated; other pointers are null/valid.
            let h = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if h != INVALID_HANDLE_VALUE {
                break Pipe { h };
            }
            // SAFETY: plain FFI.
            let err = unsafe { GetLastError() };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(PipeError::Timeout("connecting to the launcher pipe"));
            }
            match err {
                // SAFETY: name is NUL-terminated.
                ERROR_PIPE_BUSY => unsafe {
                    WaitNamedPipeW(name.as_ptr(), left.as_millis().min(5000) as u32);
                },
                ERROR_FILE_NOT_FOUND => std::thread::sleep(Duration::from_millis(50)),
                _ => return Err(PipeError::Io("opening the launcher pipe", io::Error::from_raw_os_error(err as i32))),
            }
        };
        let actual = pipe.server_pid()?;
        if actual != expected_server_pid {
            return Err(PipeError::WrongServer { expected: expected_server_pid, actual });
        }
        Ok(pipe)
    }

    /// Creates a single-instance, local-only server end (tests only; public
    /// for the integration tests, which build the library without cfg(test)).
    pub fn create_server(path: &str) -> Result<Pipe, PipeError> {
        let name = wide(path);
        // SAFETY: name is NUL-terminated; default security (the test runs as one user).
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error("creating the pipe"));
        }
        Ok(Pipe { h })
    }

    /// Server end: blocks until a client connects.
    pub fn accept(&self) -> Result<(), PipeError> {
        let mut op = Overlapped::new()?;
        // SAFETY: an overlapped handle; `op` outlives the operation (waited below).
        if unsafe { ConnectNamedPipe(self.h, &mut op.ov) } == 0 {
            // SAFETY: plain FFI.
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => return Ok(()),
                ERROR_IO_PENDING => {}
                _ => return Err(last_error("waiting for the loader")),
            }
        }
        let mut ignored = 0;
        // SAFETY: waits for this handle's pending connect on `op`.
        if unsafe { GetOverlappedResult(self.h, &op.ov, &mut ignored, 1) } == 0 {
            return Err(last_error("waiting for the loader"));
        }
        Ok(())
    }

    pub fn server_pid(&self) -> Result<u32, PipeError> {
        let mut pid = 0;
        // SAFETY: pid is a valid out pointer.
        if unsafe { GetNamedPipeServerProcessId(self.h, &mut pid) } == 0 {
            return Err(last_error("reading the pipe server's process id"));
        }
        Ok(pid)
    }

    #[cfg(test)]
    fn client_pid(&self) -> Result<u32, PipeError> {
        let mut pid = 0;
        // SAFETY: pid is a valid out pointer.
        if unsafe { windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(self.h, &mut pid) } == 0 {
            return Err(last_error("reading the pipe client's process id"));
        }
        Ok(pid)
    }

    pub fn write_all(&self, mut data: &[u8]) -> Result<(), PipeError> {
        let mut op = Overlapped::new()?;
        while !data.is_empty() {
            let chunk = data.len().min(64 * 1024) as u32;
            // SAFETY: data is valid for `chunk` bytes and, like `op`, outlives
            // the write, which completes (GetOverlappedResult waits) before
            // either is touched again.
            if unsafe { WriteFile(self.h, data.as_ptr(), chunk, std::ptr::null_mut(), &mut op.ov) } == 0 {
                // SAFETY: plain FFI.
                let code = unsafe { GetLastError() };
                if code != ERROR_IO_PENDING { return Err(io_error(code, "writing to the pipe")); }
            }
            let mut n = 0;
            // SAFETY: waits for this write on `op`.
            if unsafe { GetOverlappedResult(self.h, &op.ov, &mut n, 1) } == 0 {
                // SAFETY: plain FFI.
                return Err(io_error(unsafe { GetLastError() }, "writing to the pipe"));
            }
            data = &data[n as usize..];
        }
        Ok(())
    }

    /// Reads what arrives first, waiting at most `timeout` (None: forever).
    /// Never returns while the kernel may still write into `out`.
    fn read_some(&self, out: &mut [u8], timeout: Option<Duration>) -> Result<usize, PipeError> {
        let mut op = Overlapped::new()?;
        let len = out.len().min(u32::MAX as usize) as u32;
        // SAFETY: `out` and `op` outlive the read: it completes or is
        // cancelled and then waited for below before this returns.
        if unsafe { ReadFile(self.h, out.as_mut_ptr(), len, std::ptr::null_mut(), &mut op.ov) } == 0 {
            // SAFETY: plain FFI.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING { return Err(io_error(code, "reading from the pipe")); }
            let ms = timeout.map_or(INFINITE, |t| t.as_millis().min(u128::from(INFINITE - 1)) as u32);
            // SAFETY: the operation's own event.
            if unsafe { WaitForSingleObject(op.event, ms) } == WAIT_TIMEOUT {
                // SAFETY: cancels only this read; the result is collected below.
                unsafe { CancelIoEx(self.h, &op.ov) };
            }
        }
        let mut n = 0;
        // SAFETY: waits until the kernel is done with `out` (a cancelled read
        // may still have completed with data in the race window).
        if unsafe { GetOverlappedResult(self.h, &op.ov, &mut n, 1) } == 0 {
            // SAFETY: plain FFI.
            match unsafe { GetLastError() } {
                ERROR_OPERATION_ABORTED if n == 0 => return Err(PipeError::Timeout("waiting for a line from the pipe")),
                ERROR_MORE_DATA => {}
                code => return Err(io_error(code, "reading from the pipe")),
            }
        }
        if n == 0 {
            return Err(PipeError::Closed);
        }
        Ok(n as usize)
    }
}

/// Splits a pipe's byte stream into lines. The buffer is allocated once at its
/// full size (so it never reallocates and leaves copies behind) and is zeroized
/// as lines are consumed and on drop.
pub struct LineReader {
    buf: Zeroizing<Vec<u8>>,
    len: usize,
    consumed: usize,
}

impl Default for LineReader {
    fn default() -> Self {
        LineReader { buf: Zeroizing::new(vec![0u8; MAX_LINE + 1]), len: 0, consumed: 0 }
    }
}

impl LineReader {
    fn discard_consumed(&mut self) {
        if self.consumed > 0 {
            self.buf.copy_within(self.consumed..self.len, 0);
            let new_len = self.len - self.consumed;
            self.buf[new_len..self.len].zeroize();
            self.len = new_len;
            self.consumed = 0;
        }
    }

    /// Parses one owned message, then immediately wipes its source bytes while
    /// retaining any following lines already read from the byte-stream pipe.
    pub fn read_parsed<T>(&mut self, pipe: &Pipe, timeout: Option<Duration>, parse: impl FnOnce(&[u8]) -> T) -> Result<T, PipeError> {
        let parsed = parse(self.read_line(pipe, timeout)?);
        self.discard_consumed();
        Ok(parsed)
    }

    /// Returns the next line without its `\n`. `timeout: None` waits forever.
    pub fn read_line(&mut self, pipe: &Pipe, timeout: Option<Duration>) -> Result<&[u8], PipeError> {
        self.discard_consumed();
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(pos) = self.buf[..self.len].iter().position(|&b| b == b'\n') {
                self.consumed = pos + 1;
                return Ok(&self.buf[..pos]);
            }
            if self.len >= self.buf.len() {
                return Err(PipeError::LineTooLong);
            }
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            let end = self.buf.len();
            let n = pipe.read_some(&mut self.buf[self.len..end], remaining)?;
            self.len += n;
        }
    }

}

/// A fresh pipe path with 128 random bits, in the contract's naming scheme.
pub fn random_pipe_path() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hex = String::new();
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i ^ u64::from(std::process::id()));
        h.write_u128(Instant::now().elapsed().as_nanos());
        hex.push_str(&format!("{:016x}", h.finish()));
    }
    format!(r"\\.\pipe\crystal-loader-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coalesced_startup_handoff(parked: bool) {
        use crate::{b64, proto::{self, FirstLine, ServiceEvent}};
        use std::sync::mpsc;

        let credentials = format!("\"native_auth\":{{\"pol_id\":\"{}\",\"pol_password\":\"{}\",\"se_id\":\"{}\",\"se_secret\":\"{}\",\"otp\":null,\"profile_key\":\"{}\"}}",
            b64::encode(b"ABCD1234"), b64::encode(b"fake password"), b64::encode(b"fakeuser"), b64::encode(&[0x51; 20]), "a".repeat(40));
        let startup = if parked {
            format!("{{\"type\":\"park\",\"v\":1,\"region\":\"US\",\"lang\":\"US\"}}\n{{\"type\":\"credentials\",\"start_game\":false,{credentials}}}\n")
        } else {
            format!("{{\"type\":\"session\",\"v\":1,\"region\":\"US\",\"lang\":\"US\",\"start_game\":false,{credentials}}}\n")
        };
        let commands = concat!("{\"type\":\"native_sign_out\"}\n", "{\"type\":\"native_play_pending\"}\n",
            "{\"type\":\"controller_settings\",\"request_id\":42,\"deadline_ms\":123456,\"settings\":{\"padmode000\":\"1,1,0,0,1,1\",\"padsin000\":\"1,2,3\"}}\n");
        let payload = format!("{startup}{commands}");
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let (sent, written) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            server.accept().unwrap();
            server.write_all(payload.as_bytes()).unwrap(); // one byte-stream write
            sent.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        let client = Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap();
        written.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut reader = LineReader::default();
        let first = reader.read_parsed(&client, Some(Duration::from_secs(2)), proto::parse_first_line).unwrap().unwrap();
        let session = match first {
            FirstLine::Session(session) => { assert!(!parked); session }
            FirstLine::Park(park) => {
                assert!(parked);
                let (credentials, _) = crate::park::await_credentials(std::process::id(), Instant::now(), Duration::ZERO,
                    &mut |bytes| client.write_all(bytes),
                    &mut || reader.read_parsed(&client, None, proto::parse_credentials)).unwrap();
                park.into_session(credentials)
            }
            FirstLine::WindowerStartup(_) => panic!("this transport fixture sends only a session or park"),
        };
        assert!(!session.start_game);
        // The same kernel read must have included the commands, and no bytes
        // from the consumed credentials may remain in the allocated buffer.
        assert_eq!(&reader.buf[..reader.len], commands.as_bytes());
        assert!(reader.buf[reader.len..].iter().all(|&byte| byte == 0));
        assert_eq!(reader.consumed, 0);
        drop(session);

        // Continue the real parser on the reader thread with no further writes.
        std::thread::spawn(move || {
            let next = |reader: &mut LineReader| reader.read_parsed(&client, Some(Duration::from_secs(2)), proto::parse_service_event).unwrap().unwrap().unwrap();
            assert!(matches!(next(&mut reader), ServiceEvent::NativeSignOut));
            assert!(matches!(next(&mut reader), ServiceEvent::NativePlayPending));
            assert!(matches!(next(&mut reader), ServiceEvent::ControllerSettings { id: 42, deadline_ms: 123456, settings } if settings.len() == 2));
            assert_eq!((reader.len, reader.consumed), (0, 0));
            assert!(reader.buf.iter().all(|&byte| byte == 0));
        }).join().unwrap();
        release.send(()).unwrap();
        writer.join().unwrap();
    }

    #[test]
    fn cold_and_parked_sessions_preserve_coalesced_commands_and_wipe_consumed_credentials() {
        coalesced_startup_handoff(false);
        coalesced_startup_handoff(true);
    }

    #[test]
    fn lines_cross_reads_and_server_pid_is_checked() {
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let t = std::thread::spawn(move || {
            server.accept().unwrap();
            assert_eq!(server.client_pid().unwrap(), std::process::id());
            server.write_all(b"one\ntw").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            server.write_all(b"o\nthree\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let client = Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap();
        let mut r = LineReader::default();
        assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"one");
        assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"two");
        assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"three");
        t.join().unwrap();
        assert!(matches!(r.read_line(&client, Some(Duration::from_secs(1))), Err(PipeError::Closed)));
    }

    #[test]
    fn a_timed_out_read_loses_nothing_and_writes_proceed_while_a_read_waits() {
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let (go, wait) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            server.accept().unwrap();
            let mut r = LineReader::default();
            // The client writes while its own reader is blocked in a read.
            assert_eq!(r.read_line(&server, Some(Duration::from_secs(5))).unwrap(), b"from client");
            wait.recv().unwrap();
            server.write_all(b"late\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let client = std::sync::Arc::new(Pipe::connect(&path, std::process::id(), Duration::from_secs(5)).unwrap());
        let reader = {
            let client = std::sync::Arc::clone(&client);
            std::thread::spawn(move || {
                let mut r = LineReader::default();
                let started = Instant::now();
                assert!(matches!(r.read_line(&client, Some(Duration::from_millis(50))), Err(PipeError::Timeout(_))));
                assert!(started.elapsed() >= Duration::from_millis(45));
                go.send(()).unwrap();
                assert_eq!(r.read_line(&client, Some(Duration::from_secs(5))).unwrap(), b"late");
            })
        };
        client.write_all(b"from client\n").unwrap();
        reader.join().unwrap();
        t.join().unwrap();
    }

    #[test]
    fn wrong_server_pid_is_refused() {
        let path = random_pipe_path();
        let server = Pipe::create_server(&path).unwrap();
        let t = std::thread::spawn(move || {
            let _ = server.accept();
            std::thread::sleep(Duration::from_millis(100));
        });
        let err = Pipe::connect(&path, std::process::id().wrapping_add(1), Duration::from_secs(5)).err().unwrap();
        assert!(matches!(err, PipeError::WrongServer { .. }));
        t.join().unwrap();
    }

    #[test]
    fn missing_pipe_times_out() {
        let err = Pipe::connect(&random_pipe_path(), 1, Duration::from_millis(200)).err().unwrap();
        assert!(matches!(err, PipeError::Timeout(_)));
    }
}
