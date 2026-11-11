//! TCP sockets shaped like the stock Viewer patcher's (app.dll 102a6f99..102a7053:
//! bind 0.0.0.0:0, SO_RCVBUF = SO_SNDBUF = 0x10000 before connect), so the
//! W2U (:54000) and W20 (:53001) version checks carry stock's SYN fingerprint
//! (window scale 1, window 32768 after the handshake), keep the default delayed
//! ACK, and close 100-115 ms after the reply instead of in the same millisecond
//! (pol-spec 07a w2u-at-connect F1/F2; 07c §3).
//!
//! Stock values, from capture\stock-20261006-045841.pcapng (both connections, per segment):
//! * SYN win 65535, options `020405b4 01030301 01010402` (MSS 1460, wscale 1, SACK
//!   permitted); every later client segment win 32768 (65536 >> 1). Crystal's std socket
//!   sent wscale 8 / win 255 (Windows autotuning), a fingerprint no Viewer produces.
//! * W2U 04:59:18: reply 103 B at +318.4 ms, client pure ACK +358.7 (40.3 ms later,
//!   the kernel's delayed ACK), client FIN +433.2 (114.7 ms after the reply).
//! * W20 05:01:34: reply +405.3, ACK +455.5 (50.1 ms), FIN +517.2 (111.9 ms after).
//! * Both ACK and FIN advertise 32716: receiver SWS avoidance holds the edge back by the
//!   104 B just read, so that value follows from the buffer size, not from unread data.
//!
//! No TCP_NODELAY and no ACK-frequency change: the Viewer sets neither (owner, 07b).
use std::io;
use std::net::{SocketAddrV4, TcpStream};
use std::os::windows::io::{FromRawSocket, RawSocket};
use std::time::{Duration, Instant};
use windows_sys::Win32::Networking::WinSock as ws;

/// app.dll 102a6fc6/102a6ff6 (SO_RCVBUF) and 102a7023/102a7053 (SO_SNDBUF): 0x10000.
pub(crate) const BUFFER: i32 = 0x10000;
/// Reply → FIN. Stock: 114.7 ms (W2U), 111.9 ms (W20). It stays well after the 40-50 ms
/// delayed ACK so the pure ACK goes out first, as in stock, and never in the reply's
/// millisecond (07a w2u-at-connect finding 4).
pub(crate) const CLOSE_AFTER_REPLY: (Duration, Duration) = (Duration::from_millis(100), Duration::from_millis(115));

#[cfg(test)]
thread_local! {
    /// Sockets opened on this thread; tests use it to show a path stays offline.
    pub(crate) static OPENED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// An unconnected socket, owned until it becomes a `TcpStream`.
pub(crate) struct Socket(ws::SOCKET);
impl Drop for Socket {
    fn drop(&mut self) { unsafe { ws::closesocket(self.0); } }
}

fn last_error() -> io::Error { io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() }) }
fn check(result: i32) -> io::Result<()> { if result == ws::SOCKET_ERROR { Err(last_error()) } else { Ok(()) } }

/// SOCKADDR_IN bytes for `address`, port in network order. IPv4 only: stock looks its patch
/// hosts up with A queries alone (`dns::resolve_a`), so a v6 address never reaches here.
#[repr(C, align(4))]
struct Raw([u8; 16]);
fn raw(address: &SocketAddrV4) -> (Raw, i32) {
    let mut out = Raw([0; 16]);
    let b = &mut out.0;
    b[..2].copy_from_slice(&ws::AF_INET.to_le_bytes());
    b[2..4].copy_from_slice(&address.port().to_be_bytes());
    b[4..8].copy_from_slice(&address.ip().octets());
    (out, 16)
}

/// WSAStartup once for this process (Winsock calls made outside std's sockets need it).
/// Reference-counted and never cleaned up, like std's own lazy WSAStartup.
pub(crate) fn startup() {
    static STARTUP: std::sync::Once = std::sync::Once::new();
    STARTUP.call_once(|| unsafe { let mut data: ws::WSADATA = std::mem::zeroed(); ws::WSAStartup(0x0202, &mut data); });
}

impl Socket {
    /// socket → bind to the wildcard address, port 0 → SO_RCVBUF/SO_SNDBUF 64 KB, in
    /// app.dll's order. Both buffers must be set before connect: the SYN's window and
    /// scale are fixed from SO_RCVBUF when it goes out.
    pub(crate) fn configured() -> io::Result<Socket> {
        startup();
        // Overlapped like std's sockets; not inheritable, so a loader started meanwhile
        // cannot hold the connection open past closesocket.
        let s = unsafe { ws::WSASocketW(ws::AF_INET as i32, ws::SOCK_STREAM, ws::IPPROTO_TCP, std::ptr::null(), 0,
            ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT) };
        if s == ws::INVALID_SOCKET { return Err(last_error()); }
        let socket = Socket(s);
        #[cfg(test)] OPENED.with(|n| n.set(n.get() + 1));
        let (name, len) = raw(&SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0));
        check(unsafe { ws::bind(s, name.0.as_ptr().cast(), len) })?;
        for option in [ws::SO_RCVBUF, ws::SO_SNDBUF] {
            check(unsafe { ws::setsockopt(s, ws::SOL_SOCKET, option, (&BUFFER as *const i32).cast(), 4) })?;
        }
        Ok(socket)
    }

    #[cfg(test)]
    pub(crate) fn option(&self, name: i32) -> io::Result<i32> { option(self.0, name) }

    /// Non-blocking connect bounded by `limit` (app.dll connects non-blocking too), then
    /// back to blocking for std's reads and writes.
    pub(crate) fn connect(self, address: &SocketAddrV4, limit: Duration) -> io::Result<TcpStream> {
        let s = self.0;
        let mut on = 1u32;
        check(unsafe { ws::ioctlsocket(s, ws::FIONBIO, &mut on) })?;
        let (name, len) = raw(address);
        if unsafe { ws::connect(s, name.0.as_ptr().cast(), len) } == ws::SOCKET_ERROR {
            let error = unsafe { ws::WSAGetLastError() };
            if error != ws::WSAEWOULDBLOCK { return Err(io::Error::from_raw_os_error(error)); }
            // select, not WSAPoll: WSAPoll misses refused connects before Windows 10 2004.
            let mut write = ws::FD_SET { fd_count: 1, fd_array: [0; 64] };
            let mut fail = ws::FD_SET { fd_count: 1, fd_array: [0; 64] };
            write.fd_array[0] = s; fail.fd_array[0] = s;
            let micros = limit.as_micros().clamp(1, i32::MAX as u128 * 1_000_000) as u64;
            let timeout = ws::TIMEVAL { tv_sec: (micros / 1_000_000) as i32, tv_usec: (micros % 1_000_000) as i32 };
            match unsafe { ws::select(0, std::ptr::null_mut(), &mut write, &mut fail, &timeout) } {
                ws::SOCKET_ERROR => return Err(last_error()),
                0 => return Err(io::ErrorKind::TimedOut.into()),
                _ => {}
            }
            let pending = option(s, ws::SO_ERROR)?;
            if pending != 0 { return Err(io::Error::from_raw_os_error(pending)); }
            if fail.fd_count != 0 || write.fd_count == 0 { return Err(io::ErrorKind::ConnectionRefused.into()); }
        }
        let mut off = 0u32;
        check(unsafe { ws::ioctlsocket(s, ws::FIONBIO, &mut off) })?;
        std::mem::forget(self);
        Ok(unsafe { TcpStream::from_raw_socket(s as RawSocket) })
    }
}

fn option(s: ws::SOCKET, name: i32) -> io::Result<i32> {
    let (mut value, mut len) = (0i32, 4i32);
    check(unsafe { ws::getsockopt(s, ws::SOL_SOCKET, name, (&mut value as *mut i32).cast(), &mut len) })?;
    Ok(value)
}

/// A stock-shaped connection to `address` within `limit`.
pub(crate) fn connect(address: &SocketAddrV4, limit: Duration) -> io::Result<TcpStream> {
    Socket::configured()?.connect(address, limit)
}

/// closesocket (graceful: FIN, no linger change) once the stock delay since the last
/// reply byte has passed. Returns only after the socket is closed.
pub(crate) fn close_after_reply(stream: TcpStream, replied: Instant) {
    std::thread::sleep(close_delay().saturating_sub(replied.elapsed()));
    drop(stream);
}

/// Uniform in `CLOSE_AFTER_REPLY`, so the close is not a fixed machine constant.
pub(crate) fn close_delay() -> Duration {
    uniform(CLOSE_AFTER_REPLY)
}

/// A delay uniform in `(low, high)`, so a stock pause is never one machine constant.
pub(crate) fn uniform((low, high): (Duration, Duration)) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let span = high.saturating_sub(low).as_micros() as u64 + 1;
    // A fresh RandomState is a new per-process key; no RNG dependency for one jitter.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    low + Duration::from_micros(hasher.finish() % span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawSocket;

    #[test]
    fn buffers_are_64k_and_bound_before_connect_and_kept_after() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = Socket::configured().unwrap();
        assert_eq!((socket.option(ws::SO_RCVBUF).unwrap(), socket.option(ws::SO_SNDBUF).unwrap()), (BUFFER, BUFFER));
        // Bound already (wildcard, ephemeral port), as app.dll's bind at 102a6f99.
        let mut name = Raw([0; 16]); let mut len = 16;
        assert_eq!(unsafe { ws::getsockname(socket.0, name.0.as_mut_ptr().cast(), &mut len) }, 0);
        assert_eq!((&name.0[..2], &name.0[4..8]), (&ws::AF_INET.to_le_bytes()[..], &[0u8; 4][..]));
        assert_ne!(u16::from_be_bytes([name.0[2], name.0[3]]), 0);
        let std::net::SocketAddr::V4(address) = listener.local_addr().unwrap() else { unreachable!() };
        let stream = socket.connect(&address, Duration::from_secs(5)).unwrap();
        let s = stream.as_raw_socket() as ws::SOCKET;
        assert_eq!((option(s, ws::SO_RCVBUF).unwrap(), option(s, ws::SO_SNDBUF).unwrap()), (BUFFER, BUFFER));
        assert!(!stream.nodelay().unwrap(), "Nagle stays on, as in the Viewer");
        let (mut peer, _) = listener.accept().unwrap();
        (&stream).write_all(b"x").unwrap();
        let mut byte = [0]; peer.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }

    #[test]
    fn a_refused_connect_fails_instead_of_waiting_out_the_limit() {
        // A loopback port with no listener: bind one, take its port, drop it.
        let std::net::SocketAddr::V4(free) = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap() else { unreachable!() };
        let started = Instant::now();
        assert!(connect(&free, Duration::from_secs(10)).is_err());
        assert!(started.elapsed() < Duration::from_secs(9));
    }

    #[test]
    fn close_delay_stays_in_the_stock_window() {
        let (low, high) = CLOSE_AFTER_REPLY;
        for _ in 0..200 { let d = close_delay(); assert!(d >= low && d <= high, "{d:?}"); }
        assert!(low >= Duration::from_millis(60) && high <= Duration::from_millis(115));
    }
}
