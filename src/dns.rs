//! DNS for the stock version checks (pol-spec 07d §2; 08 §4 item 2). Lookups
//! only, through the user's own resolver: nothing here reaches Square Enix.
//! Real lookups never run in unit tests (offline).
//!
//! Callers: patch.rs (both checks, the W2U retry) and main.rs (the W20 host at Play).

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// The Viewer's patch host: its W2U at Connect (:54000) and PlayOnline updates. Stock looks
/// up exactly this name (captures stock-20261013-002122 and stock-update-20261013-004932:
/// a `pt008.pol.com` A query 0.2 s before each :54000 connection). app.dll reads it from
/// env.dat's PATCH_SERVER_DOMAIN (pol-spec 06 §1.1); this is the US install's value. An EU or
/// JP sign-in reads its own install's env.dat (`retail_region`): the official installers carry
/// `pt008.pol.com` for EU too (`EU\env.dat`) and `pt004.pol.com` for Japan.
pub(crate) const VIEWER_PATCH_HOST: &str = "pt008.pol.com";
/// FFXI's patch host: the W20 at Play (:53001), the update prompt's idle connection, the
/// file list and every download. app.dll's `pc%03d%s.pol.com` with title 1 and `W2U`; the
/// same captures show a `pc001W2U.pol.com` query (this case) 0.2 s before each W20. Both
/// names resolve to 124.150.156.107, the address `pc001.pol.com` (used before) has too.
pub(crate) const GAME_PATCH_HOST: &str = "pc001W2U.pol.com";
/// The same host as a Japanese Viewer names it. app.dll (one code base in the US, EU and JP
/// builds) fills `pc%03d%s.pol.com` with the tag its picker gives for flag 966() (app
/// 101b6d9d-101b6dc2, 1027b731): 966() is `region != 0` (set by 965), so US and EU get `W2U`
/// and Japan gets `W20`. The name resolves to the same address (looked up 2026-11-12).
pub(crate) const GAME_PATCH_HOST_JP: &str = "pc001W20.pol.com";

/// Where an exchange failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetStage { Resolve, Connect, Send, Reply }

/// One failed network step, with the real cause for the log and the UI
/// (08 §4 item 2: the W2U's DNS error 11001 was lost at pol_less.rs ~859).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetError {
    pub stage: NetStage,
    /// WSA / DNS_ERROR_* / Win32 code when the OS gave one (11001, 10051, 9003, 1460…).
    pub code: Option<i32>,
    /// The OS text, untranslated (e.g. "No such host is known.").
    pub text: String,
    /// How long the failing step took.
    pub after: Duration,
}

/// "Instant": a local failure the network itself never answered (DNS cache or
/// stack, measured 0 ms during PIA bring-up, 08 §1 row 8).
pub(crate) const INSTANT: Duration = Duration::from_millis(100);

/// Connect failures the local stack reports without the server's involvement:
/// WSAEADDRNOTAVAIL 10049, WSAENETDOWN 10050, WSAENETUNREACH 10051, WSAEHOSTUNREACH 10065.
const LOCAL_CONNECT: [i32; 4] = [10049, 10050, 10051, 10065];
/// DNS_INFO_NO_RECORDS: the name exists but has no A record.
const NO_RECORDS: i32 = 9501;
/// ERROR_TIMEOUT, for a fresh lookup that outlived its limit.
const TIMED_OUT: i32 = 1460;

impl NetError {
    /// From an OS error: the code, and the OS text without std's " (os error N)".
    pub(crate) fn from_io(stage: NetStage, error: &std::io::Error, after: Duration) -> NetError {
        let code = error.raw_os_error();
        let text = error.to_string();
        let text = match code {
            Some(code) => text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text).trim().to_owned(),
            None => text,
        };
        NetError { stage, code, text, after }
    }

    fn os(stage: NetStage, code: i32, after: Duration) -> NetError {
        NetError::from_io(stage, &std::io::Error::from_raw_os_error(code), after)
    }

    /// The cause as logs and messages show it: "No such host is known. (11001)".
    pub(crate) fn describe(&self) -> String {
        match self.code {
            Some(code) => format!("{} ({code})", self.text),
            None => self.text.clone(),
        }
    }

    /// Resolution failed, or the connect failed in the local stack (`LOCAL_CONNECT`),
    /// however long it took. A W2U retry that fails this way keeps retrying (07d §2.3).
    pub(crate) fn local_failure(&self) -> bool {
        match self.stage {
            NetStage::Resolve => true,
            NetStage::Connect => self.code.is_some_and(|code| LOCAL_CONNECT.contains(&code)),
            NetStage::Send | NetStage::Reply => false,
        }
    }

    /// "Network not ready" (07d §2.1, used by the §2.3 retry): resolution failed (WSAHOST_NOT_FOUND 11001,
    /// WSATRY_AGAIN 11002, WSANO_DATA 11004, or a DnsQuery_W failure) or the
    /// connect failed locally (WSAENETDOWN 10050, WSAENETUNREACH 10051,
    /// WSAEHOSTUNREACH 10065, WSAEADDRNOTAVAIL 10049), within `INSTANT`.
    /// A timeout, a refused connect (10061, the server answered) or any reply
    /// problem is never "not ready": that W2U is merely unanswered (owner rule).
    pub(crate) fn network_not_ready(&self) -> bool {
        self.after < INSTANT && self.local_failure()
    }
}

/// Resolve `host` through the system resolver (and its cache) on a short-lived
/// thread, so the next lookup on the launch path is warm. Never blocks the caller.
/// A failed warm-up drops its own negative cache entry (`forget`), so a lookup made
/// during a network bring-up cannot fail the sign-in's later lookups of the same
/// name, as at 18:58 (08 §1 conflicts: "most likely a cached negative result from the
/// Play-start warm-up lookup"). Stock never looks these names up early; without the
/// entry the next lookup goes to the resolver exactly as stock's would. A only, like
/// every lookup here (`resolve_a`).
pub(crate) fn warm(host: &'static str) {
    #[cfg(not(test))]
    {
        let _ = std::thread::Builder::new().name("dns-warm".into()).spawn(move || {
            if resolve_a(host, 0).is_err() { forget(&[host]); }
        });
    }
    #[cfg(test)]
    let _ = host;
}

/// `host`'s IPv4 addresses through the system resolver and its cache, asking for A records
/// only: getaddrinfo with AF_INET, as stock's lookups of its patch hosts were (captures
/// 2026-10-13: every pt008.pol.com and pc001W2U.pol.com query was type A). std's and
/// tokio's lookups use AF_UNSPEC, which on this machine sends an AAAA query beside each A
/// query that stock never sends. An IPv4 literal resolves locally, without a query.
pub(crate) fn resolve_a(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddrV4>> {
    use windows_sys::Win32::Networking::WinSock::{FreeAddrInfoW, GetAddrInfoW, ADDRINFOW, AF_INET, IPPROTO_TCP, SOCKADDR_IN, SOCK_STREAM};
    crate::stock_socket::startup();
    let name = crate::win::wide(host);
    // SAFETY: zero is a valid ADDRINFOW (all integers and null pointers).
    let mut hints: ADDRINFOW = unsafe { std::mem::zeroed() };
    hints.ai_family = AF_INET as i32;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_protocol = IPPROTO_TCP;
    let mut list: *mut ADDRINFOW = std::ptr::null_mut();
    // SAFETY: `name` is NUL-terminated and outlives the call; `list` is freed below.
    let status = unsafe { GetAddrInfoW(name.as_ptr(), std::ptr::null(), &hints, &mut list) };
    if status != 0 { return Err(std::io::Error::from_raw_os_error(status)); }
    let mut found = Vec::new();
    let mut at = list;
    while !at.is_null() {
        // SAFETY: a node of the list GetAddrInfoW returned, not yet freed.
        let info = unsafe { &*at };
        if info.ai_family == AF_INET as i32 && !info.ai_addr.is_null() && info.ai_addrlen >= std::mem::size_of::<SOCKADDR_IN>() {
            // SAFETY: an AF_INET entry's address is a SOCKADDR_IN of ai_addrlen bytes.
            let address = unsafe { &*(info.ai_addr as *const SOCKADDR_IN) };
            // IN_ADDR is stored in network byte order.
            let ip = Ipv4Addr::from(unsafe { address.sin_addr.S_un.S_addr }.to_ne_bytes());
            found.push(std::net::SocketAddrV4::new(ip, port));
        }
        at = info.ai_next;
    }
    // SAFETY: the list GetAddrInfoW allocated, freed once.
    unsafe { FreeAddrInfoW(list) };
    if found.is_empty() { return Err(std::io::Error::from_raw_os_error(NO_RECORDS)); }
    Ok(found)
}

/// `resolve_a` for a "host:port" server string.
pub(crate) fn resolve_server(server: &str) -> std::io::Result<Vec<std::net::SocketAddrV4>> {
    let (host, port) = server.rsplit_once(':')
        .and_then(|(host, port)| Some((host, port.parse::<u16>().ok()?)))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{server}: no port")))?;
    resolve_a(host, port)
}

/// A fresh A lookup that skips the Windows DNS client cache (DnsQuery_W,
/// DNS_TYPE_A, DNS_QUERY_BYPASS_CACHE), bounded by `limit` (the resolver's own
/// retries can take ~10 s; past `limit` this returns a Resolve error and the
/// lookup thread is left to finish). Used only for W2U retries, so a negative
/// answer cached by a warm-up cannot fail them (08 §1 conflicts: cached negative).
pub(crate) fn resolve_fresh(host: &str, limit: Duration) -> Result<Vec<Ipv4Addr>, NetError> {
    resolve_fresh_with(host, limit, query_a)
}

/// `resolve_fresh` with the lookup injected (tests: never real DNS).
fn resolve_fresh_with(host: &str, limit: Duration, query: fn(&str) -> Result<Vec<Ipv4Addr>, i32>) -> Result<Vec<Ipv4Addr>, NetError> {
    let started = Instant::now();
    let (sender, answer) = std::sync::mpsc::channel();
    let name = host.to_owned();
    let spawned = std::thread::Builder::new().name("dns-fresh".into())
        .spawn(move || { let _ = sender.send(query(&name)); });
    if let Err(error) = spawned { return Err(NetError::from_io(NetStage::Resolve, &error, started.elapsed())); }
    match answer.recv_timeout(limit) {
        Ok(Ok(addresses)) if !addresses.is_empty() => Ok(addresses),
        Ok(Ok(_)) => Err(NetError::os(NetStage::Resolve, NO_RECORDS, started.elapsed())),
        Ok(Err(code)) => Err(NetError::os(NetStage::Resolve, code, started.elapsed())),
        // Past the limit (or the thread died): the lookup finishes on its own.
        Err(_) => Err(NetError::os(NetStage::Resolve, TIMED_OUT, started.elapsed())),
    }
}

/// DnsQuery_W(host, DNS_TYPE_A, DNS_QUERY_BYPASS_CACHE): the A records, or the
/// DNS_ERROR_* / Win32 code. CNAMEs in the answer are skipped.
#[cfg(not(test))]
fn query_a(host: &str) -> Result<Vec<Ipv4Addr>, i32> {
    use windows_sys::Win32::NetworkManagement::Dns::{
        DnsFree, DnsFreeRecordList, DnsQuery_W, DNS_QUERY_BYPASS_CACHE, DNS_RECORDA, DNS_TYPE_A,
    };
    let name = crate::win::wide(host);
    let mut records: *mut DNS_RECORDA = std::ptr::null_mut();
    // windows-sys types the out-list as DNS_RECORDA; DnsQuery_W fills DNS_RECORDW, whose
    // layout differs only in pName (not read here).
    let status = unsafe {
        DnsQuery_W(name.as_ptr(), DNS_TYPE_A, DNS_QUERY_BYPASS_CACHE, std::ptr::null_mut(), &mut records, std::ptr::null_mut())
    };
    let mut found = Vec::new();
    let mut at = records;
    while status == 0 && !at.is_null() {
        let record = unsafe { &*at };
        // IP4_ADDRESS is stored in network byte order.
        if record.wType == DNS_TYPE_A { found.push(Ipv4Addr::from(unsafe { record.Data.A.IpAddress }.to_ne_bytes())); }
        at = record.pNext;
    }
    if !records.is_null() { unsafe { DnsFree(records as *const _, DnsFreeRecordList) }; }
    if status != 0 { Err(status as i32) } else { Ok(found) }
}
#[cfg(test)]
fn query_a(_host: &str) -> Result<Vec<Ipv4Addr>, i32> {
    // DNS_ERROR_RCODE_SERVER_FAILURE: unit tests stay offline (07d §6 S2).
    Err(9002)
}

/// Drop `hosts` from the Windows DNS client cache (DnsFlushResolverCacheEntry_W,
/// dnsapi.dll; local only, nothing is sent). The negative-cache rule of 07d §2.1:
/// whether a DNS_QUERY_BYPASS_CACHE answer replaces the client's cache entry cannot
/// be verified offline (no live lookups, round-2 hard rules), so after a W2U that
/// needed a retry the patch hosts' entries are flushed instead. That works either
/// way: the W20's lookup then goes to the resolver. Best effort; failures are ignored.
pub(crate) fn forget(hosts: &[&str]) {
    #[cfg(not(test))]
    flush_entries(hosts);
    #[cfg(test)]
    let _ = hosts;
}

#[cfg(not(test))]
fn flush_entries(hosts: &[&str]) {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    type Flush = unsafe extern "system" fn(*const u16) -> i32;
    // Exported by dnsapi.dll but absent from windows-sys (undocumented; ipconfig's
    // sibling of DnsFlushResolverCache), so it is looked up once from System32.
    static FLUSH: std::sync::OnceLock<Option<Flush>> = std::sync::OnceLock::new();
    let flush = *FLUSH.get_or_init(|| unsafe {
        let dll = crate::win::wide("dnsapi.dll");
        let module = LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if module.is_null() { return None; }
        GetProcAddress(module, c"DnsFlushResolverCacheEntry_W".as_ptr().cast())
            .map(|proc| std::mem::transmute::<unsafe extern "system" fn() -> isize, Flush>(proc))
    });
    let Some(flush) = flush else { return };
    for host in hosts {
        let name = crate::win::wide(host);
        unsafe { flush(name.as_ptr()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(stage: NetStage, code: Option<i32>, ms: u64) -> NetError {
        NetError { stage, code, text: String::new(), after: Duration::from_millis(ms) }
    }

    #[test]
    fn network_not_ready_follows_the_classification_table() {
        // Resolution failures and local connect failures within INSTANT (07d §2.1).
        for code in [Some(11001), Some(11002), Some(11004), Some(9003), Some(9002), None] {
            assert!(error(NetStage::Resolve, code, 0).network_not_ready(), "{code:?}");
            assert!(error(NetStage::Resolve, code, 99).network_not_ready(), "{code:?}");
        }
        for code in LOCAL_CONNECT {
            assert!(error(NetStage::Connect, Some(code), 1).network_not_ready(), "{code}");
        }
        // Slow: the resolver or the network answered.
        assert!(!error(NetStage::Resolve, Some(11001), 100).network_not_ready());
        assert!(!error(NetStage::Resolve, Some(11001), 150).network_not_ready());
        assert!(!error(NetStage::Connect, Some(10051), 400).network_not_ready());
        // Never: refused (the server answered), timeouts, other connect errors, send/reply.
        for code in [Some(10061), Some(10060), None, Some(10054)] {
            assert!(!error(NetStage::Connect, code, 0).network_not_ready(), "{code:?}");
        }
        assert!(!error(NetStage::Connect, None, 10_003).network_not_ready());
        for stage in [NetStage::Send, NetStage::Reply] {
            for code in [Some(10051), Some(11001), Some(10054), None] {
                assert!(!error(stage, code, 0).network_not_ready(), "{stage:?} {code:?}");
            }
        }
        // The retry loop's wider rule: any resolve failure, local connect codes, any duration.
        assert!(error(NetStage::Resolve, Some(1460), 10_000).local_failure());
        assert!(error(NetStage::Connect, Some(10065), 3_000).local_failure());
        assert!(!error(NetStage::Connect, Some(10061), 0).local_failure());
    }

    #[test]
    fn os_errors_keep_their_text_and_code_without_stds_suffix() {
        let e = NetError::from_io(NetStage::Resolve, &std::io::Error::from_raw_os_error(11001), Duration::ZERO);
        assert_eq!(e.code, Some(11001));
        assert!(!e.text.contains("os error") && !e.text.is_empty(), "{}", e.text);
        assert_eq!(e.describe(), format!("{} (11001)", e.text));
        let timeout = NetError::from_io(NetStage::Connect, &std::io::ErrorKind::TimedOut.into(), Duration::ZERO);
        assert_eq!((timeout.code, timeout.describe().as_str()), (None, "timed out"));
    }

    /// IPv4 literals only: unit tests never send a real query.
    #[test]
    fn patch_hosts_resolve_through_getaddrinfo_to_ipv4() {
        assert_eq!(resolve_a("124.150.156.107", 53001).unwrap(), [std::net::SocketAddrV4::new(Ipv4Addr::new(124, 150, 156, 107), 53001)]);
        assert_eq!(resolve_server("127.0.0.1:54000").unwrap(), [std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 54000)]);
        assert!(resolve_server("pc001W2U.pol.com").is_err(), "a server string needs its port");
        // Every lookup of a patch host goes through resolve_a (A only); the AF_UNSPEC
        // resolvers appear nowhere outside tests.
        for (file, source) in [("patch.rs", include_str!("patch.rs")), ("updater.rs", include_str!("updater.rs")), ("dns.rs", include_str!("dns.rs"))] {
            let production = source.split("#[cfg(test)]\nmod tests").next().unwrap().split("#[cfg(test)]\r\nmod tests").next().unwrap();
            assert!(!production.contains("lookup_host("), "{file}");
            assert!(production.matches("to_socket_addrs()").count() == usize::from(file == "patch.rs"), "{file}: only the test-only exchange may use it");
        }
    }

    #[test]
    fn a_fresh_lookup_is_bounded_and_reports_dns_codes() {
        let ok = resolve_fresh_with("pc001.pol.com", Duration::from_secs(5), |_| Ok(vec![Ipv4Addr::new(124, 150, 156, 107)]));
        assert_eq!(ok, Ok(vec![Ipv4Addr::new(124, 150, 156, 107)]));
        let nxdomain = resolve_fresh_with("pc001.pol.com", Duration::from_secs(5), |_| Err(9003)).unwrap_err();
        assert_eq!((nxdomain.stage, nxdomain.code), (NetStage::Resolve, Some(9003)));
        assert!(nxdomain.network_not_ready());
        let empty = resolve_fresh_with("pc001.pol.com", Duration::from_secs(5), |_| Ok(Vec::new())).unwrap_err();
        assert_eq!(empty.code, Some(NO_RECORDS));
        // A lookup that outlives its limit returns at the limit; the thread finishes alone.
        let started = Instant::now();
        let slow = resolve_fresh_with("pc001.pol.com", Duration::from_millis(50), |_| {
            std::thread::sleep(Duration::from_millis(600)); Ok(vec![Ipv4Addr::LOCALHOST])
        }).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        assert_eq!((slow.stage, slow.code), (NetStage::Resolve, Some(TIMED_OUT)));
        // Tests never resolve for real.
        assert_eq!(resolve_fresh("pc001.pol.com", Duration::from_secs(1)).unwrap_err().code, Some(9002));
    }
}
