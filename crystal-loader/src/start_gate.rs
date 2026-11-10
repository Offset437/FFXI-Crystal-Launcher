//! A short, process-owned barrier immediately before GameStart. No POL calls.
use std::time::Duration;
use windows_sys::Win32::{Foundation::{CloseHandle, HANDLE, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::{CreateEventW, OpenEventW, SetEvent, OpenProcess, GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, WaitForMultipleObjects, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, EVENT_MODIFY_STATE}};

const SYNCHRONIZE: u32 = 0x0010_0000;
pub const LIMIT: Duration = Duration::from_secs(30);
struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle { fn drop(&mut self) { unsafe { CloseHandle(self.0); } } }
impl Handle {
    fn signaled(&self) -> bool { unsafe { WaitForSingleObject(self.0, 0) == 0 } }
    fn signal(&self) -> Result<(), String> { if unsafe { SetEvent(self.0) } != 0 { Ok(()) } else { Err("Could not signal the game-start gate.".into()) } }
}
fn created(handle: HANDLE) -> Option<u64> {
    let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
    let p = times.as_mut_ptr();
    (unsafe { GetProcessTimes(handle, p, p.add(1), p.add(2), p.add(3)) } != 0)
        .then(|| (times[0].dwHighDateTime as u64) << 32 | times[0].dwLowDateTime as u64)
}
fn event(id: &str, suffix: &str, create: bool) -> Result<Handle, String> {
    let name: Vec<u16> = format!("Local\\CrystalBatch-{id}-{suffix}").encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { if create { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) }
        else { OpenEventW(SYNCHRONIZE | EVENT_MODIFY_STATE, 0, name.as_ptr()) } };
    if handle.is_null() { Err("Could not open the game-start gate.".into()) } else { Ok(Handle(handle)) }
}
fn token_parts(token: &str) -> Result<(&str, u32, u64, u8), String> {
    let parts: Vec<_> = token.split('-').collect();
    if parts.len() != 4 || parts[0].len() != 32 || !parts[0].bytes().all(|b| b.is_ascii_hexdigit())
        || parts[1].len() != 8 || parts[2].len() != 16 || parts[3].len() != 2 { return Err("Invalid game-start gate.".into()); }
    Ok((parts[0], u32::from_str_radix(parts[1], 16).map_err(|_| "Invalid gate owner.")?,
        u64::from_str_radix(parts[2], 16).map_err(|_| "Invalid gate owner.")?,
        u8::from_str_radix(parts[3], 16).map_err(|_| "Invalid gate member.")?))
}
// The launcher uses Coordinator (and only opens a Participant to check a
// token); the loader uses Participant. Each crate compiles the whole file.
#[cfg_attr(not(test), allow(dead_code))]
pub struct Coordinator { release: Handle, cancel: Handle, ready: Vec<Handle>, started: Vec<Handle> }
#[cfg_attr(not(test), allow(dead_code))]
impl Coordinator {
    pub fn new(id: &str, count: usize) -> Result<(Self, Vec<String>), String> {
        if count == 0 || count > 32 || id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("Invalid game-start batch.".into()); }
        let owner = unsafe { GetCurrentProcessId() };
        let birth = created(unsafe { GetCurrentProcess() }).ok_or("Could not identify batch owner.")?;
        let mut gate = Self { release: event(id, "release", true)?, cancel: event(id, "cancel", true)?, ready: Vec::new(), started: Vec::new() };
        let mut tokens = Vec::new();
        for i in 0..count {
            gate.ready.push(event(id, &format!("ready-{i}"), true)?);
            gate.started.push(event(id, &format!("started-{i}"), true)?);
            tokens.push(format!("{id}-{owner:08x}-{birth:016x}-{i:02x}"));
        }
        Ok((gate, tokens))
    }
    pub fn ready(&self) -> bool { self.ready.iter().all(Handle::signaled) }
    pub fn started(&self) -> bool { self.started.iter().all(Handle::signaled) }
    pub fn canceled(&self) -> bool { self.cancel.signaled() }
    pub fn release(&self) -> Result<(), String> { if self.canceled() || !self.ready() { return Err("A game is not ready.".into()); } self.release.signal() }
}
impl Drop for Coordinator { fn drop(&mut self) { let _ = self.cancel.signal(); } }

#[cfg_attr(not(test), allow(dead_code))]
pub struct Participant { release: Handle, cancel: Handle, ready: Handle, started: Handle, owner: Handle }
impl Participant {
    pub fn open(token: &str) -> Result<Self, String> {
        let (id, pid, birth, index) = token_parts(token)?;
        let owner = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if owner.is_null() { return Err("The game-start batch ended.".into()); }
        let owner = Handle(owner);
        if created(owner.0) != Some(birth) { return Err("The game-start batch owner changed.".into()); }
        let this = Self { release: event(id, "release", false)?, cancel: event(id, "cancel", false)?,
            ready: event(id, &format!("ready-{index}"), false)?, started: event(id, &format!("started-{index}"), false)?, owner };
        this.check()?;
        Ok(this)
    }
    fn check(&self) -> Result<(), String> {
        if self.cancel.signaled() || self.owner.signaled() { Err("The game-start batch was canceled.".into()) } else { Ok(()) }
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn wait(&self) -> Result<(), String> { self.wait_for(LIMIT) }
    #[cfg_attr(not(test), allow(dead_code))]
    fn wait_for(&self, timeout: Duration) -> Result<(), String> {
        self.check()?;
        self.ready.signal()?;
        // One kernel wait on whichever fires first. Cancel and owner death come
        // before release, so (lowest index wins) they keep their priority.
        let handles = [self.cancel.0, self.owner.0, self.release.0];
        let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        match unsafe { WaitForMultipleObjects(3, handles.as_ptr(), 0, ms) } {
            r if r == WAIT_OBJECT_0 || r == WAIT_OBJECT_0 + 1 => Err("The game-start batch was canceled.".into()),
            r if r == WAIT_OBJECT_0 + 2 => { self.started.signal()?; Ok(()) }
            WAIT_TIMEOUT => { let _ = self.cancel.signal(); Err("Timed out waiting for the other games.".into()) }
            _ => Err("Could not wait on the game-start gate.".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn gate(n: usize) -> (Coordinator, Vec<String>) {
        let nounce = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Coordinator::new(&format!("{:016x}{nounce:016x}", unsafe { GetCurrentProcessId() }), n).unwrap()
    }
    #[test] fn mixed_ready_members_wait_for_delayed_member() {
        let (gate, tokens) = gate(2);
        let first = Participant::open(&tokens[0]).unwrap();
        let second = Participant::open(&tokens[1]).unwrap();
        let a = std::thread::spawn(move || first.wait());
        std::thread::sleep(Duration::from_millis(20));
        assert!(!gate.ready()); assert!(gate.release().is_err()); assert!(!a.is_finished());
        let b = std::thread::spawn(move || second.wait());
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while !gate.ready() { assert!(std::time::Instant::now() < until); std::thread::yield_now(); }
        gate.release().unwrap();
        a.join().unwrap().unwrap(); b.join().unwrap().unwrap(); assert!(gate.started());
    }
    #[test] fn cancellation_and_timeout_never_release_pending_game() {
        let (gate, tokens) = gate(1);
        let member = Participant::open(&tokens[0]).unwrap();
        assert!(member.wait_for(Duration::ZERO).is_err()); assert!(!gate.started());
        drop(gate);
        assert!(member.wait().is_err()); assert!(Participant::open(&tokens[0]).is_err());
    }
    #[test] fn malformed_or_reused_owner_is_rejected() {
        assert!(Participant::open("bad").is_err());
        let (_gate, tokens) = gate(1);
        let mut pieces: Vec<_> = tokens[0].split('-').collect(); pieces[2] = "0000000000000000";
        assert!(Participant::open(&pieces.join("-")).is_err());
    }
    #[test]
    #[ignore = "offline owner-death fixture"]
    fn owner_fixture() {
        let (_gate, tokens) = gate(1);
        println!("GATE {}", tokens[0]);
        std::thread::sleep(Duration::from_secs(10));
    }
    #[test] fn owner_process_death_cancels_without_a_release() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::owner_fixture"), "--ignored", "--nocapture"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let participant = loop {
            let mut line = String::new(); assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if let Some(token) = line.trim().strip_prefix("GATE ") { break Participant::open(token).unwrap(); }
        };
        child.kill().unwrap(); child.wait().unwrap();
        assert!(participant.wait_for(Duration::from_secs(1)).is_err());
    }
}
