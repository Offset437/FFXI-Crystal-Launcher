//! Viewer social operations under the existing native session owner.
//! Original functions retain packing, paths, transport, tickets and errors.
//!
//! Mail follows the Viewer's own per-frame mail jobs, not the generic request
//! driver (native_jobs, app 1003e9b4): list app 10056b40, download/ack app
//! 10056fe7, send and receipt app 10055f00. Those jobs differ from 1003e9b4:
//! a job fails on its third failure (`inc; cmp 3; jge`), -256 re-issues
//! without counting, the list times out after 60 s of wall clock (slot 696)
//! and retries on a 30 s timer, and downloads run concurrently, one state
//! step per job per frame. Put Friend List is an ordinary 1003e9b4 request.
use crate::{b64, events, log, native_auth::Api, native_jobs, proto::NativeSocialRequest};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub static REQUEST: Mutex<Option<NativeSocialRequest>> = Mutex::new(None);
pub(crate) static CATALOG_ACCESS: Mutex<()> = Mutex::new(());
pub const HEADER_LEN: usize = crate::wire::SOCIAL_HEADER_LEN;
/// One listed message: the 0x48 header, then +0x4c the body pointer 326
/// fills (core 10027ac4 stride; app 100515eb allocates max*0x50).
pub const RECORD_LEN: usize = 0x50;
pub const HEADER_COUNT: usize = 32;
/// app 10056f04: [104e5154] = 0xc8.
const LIST_MAX: usize = 200;
/// Subject 0x80 + BEL + text 0xfff + NUL + attachment <= 0x100000 (core 1001a8f6) + metadata.
const MAX_BODY: usize = 0x102000;
/// Body follows the header at +0xa8 in every send buffer (app 100513b0).
const BODY_AT: usize = 0xa8;
/// The Viewer's 3600-frame mail limits at their stock cold wall time (112.5 s; 07d §6 S1 D).
fn mail_frames() -> u32 { native_jobs::mail_frames() }
const MAIL_ATTEMPTS: u32 = 3;
const LIST_PENDING: Duration = Duration::from_secs(60);
const LIST_RETRY: Duration = Duration::from_secs(30);
const PUSH_LIMIT: usize = 64;
/// Local save attempts for a pushed message before it is given up.
const SAVE_ATTEMPTS: u32 = 3;
/// Core 100236a0 refuses list A edits for list B accounts with this error.
const BLOCKED: i32 = -7181;

/// The Viewer mail receiver's state, shared by the core thread (receiver) and
/// the session owner. Process-wide in the loader; per test thread in tests, so
/// parallel session tests cannot flip another test's listening or queue.
struct MailState {
    listening: AtomicBool,
    changed: AtomicBool,
    pushed: Mutex<Vec<Pushed>>,
    /// Pushes the receiver could not copy (no body, oversized, queue full); logged by the owner.
    drops: AtomicU32,
}
impl MailState {
    const fn new() -> Self {
        Self { listening: AtomicBool::new(false), changed: AtomicBool::new(false), pushed: Mutex::new(Vec::new()), drops: AtomicU32::new(0) }
    }
}
#[cfg(not(test))]
static MAIL: MailState = MailState::new();
#[cfg(not(test))]
fn mail() -> &'static MailState { &MAIL }
#[cfg(test)]
thread_local! { static MAIL: &'static MailState = Box::leak(Box::new(MailState::new())); }
#[cfg(test)]
fn mail() -> &'static MailState { MAIL.with(|m| *m) }

struct Pushed {
    header: [u8; HEADER_LEN],
    body: Zeroizing<Vec<u8>>,
    /// Its receipt is already queued (a retried save never answers twice).
    answered: bool,
    attempts: u32,
}

/// The Viewer receiver (app 10056b05), registered through 331 directly or as
/// entry 4 of the 871 callback table. Core 1001bcd2/3 calls it with one
/// pointer to a 0x50 record whose +0x4c is the body (app 10056816 reads it).
/// It only copies and queues: no IO, request, pump or wait on the core thread.
pub unsafe extern "C" fn mail_arrived(record: usize) {
    if !mail().listening.load(Ordering::Acquire) || record == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(record as *const u8, RECORD_LEN);
    let flags = u16::from_le_bytes([bytes[0x3e], bytes[0x3f]]);
    // app 10056b0a: bits 12-13 == 1 is a receipt for one of our own sends;
    // only an ordinary message (0) is answered (10056256) and saved (10056816).
    if (flags >> 12) & 3 != 0 {
        return;
    }
    mail().changed.store(true, Ordering::Release);
    let length = u32::from_le_bytes([bytes[0x38], bytes[0x39], bytes[0x3a], bytes[0x3b]]) as usize;
    let body = u32::from_le_bytes([bytes[0x4c], bytes[0x4d], bytes[0x4e], bytes[0x4f]]) as usize;
    if body == 0 || length == 0 || length > MAX_BODY {
        mail().drops.fetch_add(1, Ordering::AcqRel);
        return;
    }
    let mut header = [0u8; HEADER_LEN];
    header.copy_from_slice(&bytes[..HEADER_LEN]);
    let body = Zeroizing::new(std::slice::from_raw_parts(body as *const u8, length).to_vec());
    let mut queue = mail().pushed.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() < PUSH_LIMIT {
        queue.push(Pushed { header, body, answered: false, attempts: 0 });
    } else {
        mail().drops.fetch_add(1, Ordering::AcqRel);
    }
}

/// Whether `mail_arrived` accepts messages. Owners that register it through
/// slot 871 enable it while the Viewer owns the core (between games).
/// Messages already queued stay queued for the owner's next delivery frame.
/// Remove the native callback before disabling acceptance at game handoff.
pub fn set_listening(listening: bool) {
    mail().listening.store(listening, Ordering::Release);
    if !listening {
        mail().changed.store(false, Ordering::Release);
    }
}

/// Forget queued pushes without storing them (signing out of the account).
pub fn discard_pushed() {
    mail().pushed.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// Whether a message arrived since the last call (owners report it once).
pub fn take_mail_changed() -> bool {
    mail().changed.swap(false, Ordering::AcqRel)
}

/// Direct 331 registration, kept for tests of the receiver; the owner
/// registers it as 871 entry 4 like the Viewer.
#[cfg(test)]
pub struct MailNotifications { api: Api }
#[cfg(test)]
impl MailNotifications {
    pub fn start(api: &Api) -> Self {
        mail().changed.store(false, Ordering::Release);
        set_listening(true);
        unsafe { api.f::<unsafe extern "C" fn(usize)>(331, 1)(mail_arrived as *const () as usize) };
        Self { api: api.clone() }
    }
    pub fn take_changed(&self) -> bool {
        mail().changed.swap(false, Ordering::AcqRel)
    }
}
#[cfg(test)]
impl Drop for MailNotifications {
    fn drop(&mut self) {
        set_listening(false);
        unsafe { self.api.f::<unsafe extern "C" fn(usize)>(331, 1)(0) };
    }
}

/// Keep one Service for the whole signed-in session.
#[derive(Default)]
pub struct Service {
    headers: Vec<StoredHeader>,
    /// app [104e512c]: consecutive list failures since the last accepted start.
    list_errors: u32,
    /// app list state 6: re-list when this passes (10056b6a, 30 s).
    list_retry: Option<Instant>,
}

/// Incoming mail progresses independently of a list, send or status request.
/// The owner services it after each native pump, including while another job
/// is pending. It never starts a list or pumps the core itself.
#[derive(Default)]
pub(crate) struct PushDelivery {
    /// Receipts waiting for 333 (send job state 7, retried every frame).
    receipts: Vec<Zeroizing<Vec<u64>>>,
    /// Pushed messages whose local save failed; retried on later frames.
    unsaved: Vec<Pushed>,
}
#[derive(Clone)]
struct StoredHeader {
    header: [u8; HEADER_LEN],
    folder: u8,
    read: bool,
}
impl Service {
    #[cfg(test)]
    pub(crate) fn retry_mail_now(&mut self) {
        self.list_retry = Some(Instant::now());
    }
    #[cfg(test)]
    fn contains(&self, header: &[u8; HEADER_LEN]) -> bool {
        self.headers.iter().any(|h| &h.header == header)
    }
    #[cfg(test)]
    fn remember(&mut self, bytes: &[u8]) {
        self.headers = bytes
            .as_chunks::<HEADER_LEN>().0.iter()
            .filter(|h| h.iter().any(|b| *b != 0))
            .map(|h| StoredHeader { header: *h, folder: 0, read: false })
            .collect();
    }
}

pub(crate) fn result(request: &NativeSocialRequest, code: i32, payload: Option<&[u8]>) {
    let extra = payload.map_or(String::new(), |bytes| {
        format!(",\"payload\":\"{}\"", b64::encode(bytes))
    });
    events::send(format!("{{\"type\":\"native_social_result\",\"id\":\"{}\",\"action\":\"{}\",\"code\":{code}{extra}}}\n", request.id(), request.action()));
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn bytes_of(words: &[u64]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 8) }
}
fn bytes_of_mut(words: &mut [u64]) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 8) }
}

fn friend(api: &Api, alternate: bool, slot: u16, expected: u64) -> Result<[u8; 0xb0], i32> {
    if slot as usize >= if alternate { 100 } else { 200 } {
        return Err(-7175);
    }
    let mut record = [0; 0xb0];
    let code = api.call(if alternate { 169 } else { 167 }, &[slot as usize, record.as_mut_ptr() as usize]);
    if code < 0 {
        return Err(code);
    }
    if record[0x98] & 1 == 0 || u64_at(&record, 0) != expected {
        return Err(-7189);
    }
    Ok(record)
}

/// Record +0x9c bits 13..18: the handle the entry targets (app 1003ee10).
fn record_handle(record: &[u8]) -> u8 {
    ((u32_at(record, 0x9c) >> 13) & 0x3f) as u8
}

/// app 1003ddba(0, target): list A entry for this account and handle.
fn in_friend_list(api: &Api, uid: u64, handle: u8) -> bool {
    let _catalog = CATALOG_ACCESS.lock().unwrap_or_else(|e| e.into_inner());
    (0..200).any(|slot| {
        let mut record = [0u8; 0xb0];
        let code = api.call(167, &[slot, record.as_mut_ptr() as usize]);
        code >= 0 && record[0x98] & 1 != 0 && u64_at(&record, 0) == uid && record_handle(&record) == handle
    })
}

/// app 1003e911: a member of one of my groups. The Viewer's group cache is
/// 394's 0x3028-byte copies: state (+0xc >> 1) & 7, 64 members from +0x28 at
/// 0xc0 each (uid +0, in use +8 bit 0, handle (u64 +0x20 >> 45) & 0x3f).
fn in_group(api: &Api, uid: u64, handle: u8) -> bool {
    const GROUP: usize = 0x3028;
    let groups = api.zero(385);
    if groups <= 0 {
        return false;
    }
    let mut buffer = vec![0u8; 4 * GROUP];
    let copied = api.call(394, &[buffer.as_mut_ptr() as usize, 1]);
    buffer
        .as_chunks::<GROUP>().0.iter()
        .take(copied.clamp(0, groups).min(4) as usize)
        .filter(|group| !matches!((u32_at(&group[..], 0xc) >> 1) & 7, 1 | 2))
        .flat_map(|group| group[0x28..].as_chunks::<0xc0>().0)
        .any(|m| m[8] & 1 != 0 && u64_at(m, 0) == uid && ((u64_at(m, 0x20) >> 45) & 0x3f) as u8 == handle)
}

/// app 10051a85(id, 8): four capitals, four digits, and the first letter is
/// the checksum of the other seven (1005199a).
fn member_id(id: &[u8]) -> bool {
    id.len() == 8
        && id[..4].iter().all(u8::is_ascii_uppercase)
        && id[4..].iter().all(u8::is_ascii_digit)
        && (id[1..].iter().map(|b| *b as u32).sum::<u32>() % 26) as u8 + b'A' == id[0]
}

/// app 10051bbc: an ordinary message (kind 0) from a member who is neither a
/// friend nor in one of my groups is acknowledged without downloading it.
fn discarded_unread(api: &Api, header: &[u8]) -> bool {
    if u16_at(header, 0x3e) & 0xf80 != 0 {
        return false;
    }
    let mut id = [0u8; 32];
    api.call(316, &[u32_at(header, 0) as usize, u32_at(header, 4) as usize, id.as_mut_ptr() as usize]); // void
    if !member_id(&id[..8]) {
        return false;
    }
    let (uid, handle) = (u64_at(header, 0), header[0x3c]);
    !in_friend_list(api, uid, handle) && !in_group(api, uid, handle)
}

pub(crate) fn cp932(text: &str, limit: usize) -> Result<Zeroizing<Vec<u8>>, i32> {
    use windows_sys::Win32::Globalization::{WideCharToMultiByte, WC_NO_BEST_FIT_CHARS};
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(-5120);
    }
    let wide = Zeroizing::new(text.encode_utf16().collect::<Vec<_>>());
    if wide.is_empty() {
        return Ok(Zeroizing::new(vec![0]));
    }
    let mut substituted = 0;
    let n = unsafe {
        WideCharToMultiByte(
            932,
            WC_NO_BEST_FIT_CHARS,
            wide.as_ptr(),
            wide.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut substituted,
        )
    };
    if n <= 0 || n as usize > limit || substituted != 0 {
        return Err(-5120);
    }
    let mut out = Zeroizing::new(vec![0; n as usize + 1]);
    let actual = unsafe {
        WideCharToMultiByte(
            932,
            WC_NO_BEST_FIT_CHARS,
            wide.as_ptr(),
            wide.len() as i32,
            out.as_mut_ptr(),
            n,
            std::ptr::null(),
            &mut substituted,
        )
    };
    if actual != n || substituted != 0 {
        return Err(-5120);
    }
    Ok(out)
}

/// app 1005506e: the header subject is at most 15 bytes. Longer subjects are
/// cut at a character boundary by 10003181 (a byte >= 0x80 or 0x7f with a
/// following byte, and CR LF, count as two), and the cut sets [0x42] bit 12.
fn subject15(subject: &[u8]) -> (&[u8], bool) {
    if subject.len() <= 15 {
        return (subject, false);
    }
    let (mut at, mut used) = (0, 0);
    while at < subject.len() {
        let next = subject.get(at + 1).copied().unwrap_or(0);
        let width = match subject[at] {
            b if (b == 0x7f || b >= 0x80) && next != 0 => 2,
            b'\r' if next == b'\n' => 2,
            _ => 1,
        };
        if used + width > 15 {
            break;
        }
        used += width;
        at += width;
    }
    (&subject[..used], true)
}

/// app 10055e50..10055ef5, single recipient, kinds 0/4/5: header +0x3e bit 14
/// (immediate delivery) when the body is at most 0x180 bytes and the
/// recipient's Viewer cache entry (built from this record at app 1003edbb)
/// shows a PlayOnline application (1000/1001) for the account or its linked
/// character, a nonzero presence status, and flags bit 12.
fn immediate_delivery(record: &[u8], length: usize) -> bool {
    let flags = u64_at(record, 8);
    let application = ((flags >> 33) & 0x3ff) as u32;
    let selected = 0x18 + ((flags >> 17) & 7) as usize * 16;
    let linked = if flags & (1 << 16) != 0 && record[selected] & 1 != 0 {
        (u16_at(record, selected + 2) & 0x3ff) as u32
    } else {
        application
    };
    let viewer = application.wrapping_sub(1000) <= 1 || linked.wrapping_sub(1000) <= 1;
    // app 1003efc5..1003f061: status 1 = mode 1 while online (flags bit 11),
    // 2 = modes 2 and 3, otherwise 0.
    let status = match (flags >> 13) & 7 {
        1 => u32::from(flags & (1 << 11) != 0),
        2 | 3 => 2,
        _ => 0,
    };
    length <= 0x180 && flags & (1 << 12) != 0 && viewer && status != 0
}

/// app 10055c0e..10055d95 on the job's header copy, single recipient.
/// [0x42]: clear bits 8-11 and 13, bits 1..7 = 966() (the same 100181c0 value
/// 270 already stored), bit 12 = the Viewer's own subject cut. Bit 13 is set
/// from setting (4,3) only on the multi-recipient path (10055d80 is reached
/// only when a recipient list exists), so a normal send leaves it clear.
/// [0x3e]: clear bits 12-13, bit 14 = immediate delivery.
fn viewer_fixup(header: &mut [u8], truncated: bool, immediate: bool) {
    let mut options = u16_at(header, 0x42) & !0x2f00;
    options = (options & !0x1000) | (u16::from(truncated) << 12);
    header[0x42..0x44].copy_from_slice(&options.to_le_bytes());
    let mut flags = u16_at(header, 0x3e) & 0xcfff & !0x4000;
    if immediate {
        flags |= 0x4000;
    }
    header[0x3e..0x40].copy_from_slice(&flags.to_le_bytes());
}

/// The sender name the Viewer uses for handle `h` (app 1003e509: 173(h) +8).
fn handle_name(api: &Api, handle: i32) -> Option<[u8; 16]> {
    if !(0..64).contains(&handle) {
        return None;
    }
    let mut record = [0u8; 0x28];
    let code = api.call(173, &[handle as usize, record.as_mut_ptr() as usize]);
    if code < 0 || record[0] & 1 == 0 {
        return None;
    }
    let mut name = [0u8; 16];
    name[..15].copy_from_slice(&record[8..23]);
    Some(name)
}

struct Message {
    own: u64,
    to: u64,
    name: [u8; 16],
    sender_handle: u8,
    recipient_handle: u8,
}

/// app 10055a5c..10055ad5: normal composed message, kind 0, no attachment.
/// `subject`/`text` are NUL-terminated CP932. Returns the send buffer
/// (header, then the body at +0xa8), its body length and the subject cut.
fn pack_message(api: &Api, m: &Message, subject: &[u8], text: &[u8]) -> Result<(Zeroizing<Vec<u64>>, usize, bool), i32> {
    let (Some((0, plain_subject)), Some((0, plain_text))) = (subject.split_last(), text.split_last()) else {
        return Err(-5120);
    };
    // "subject BEL text NUL": core 272 copies <= 0x80 subject and < 0x1000 text bytes.
    if plain_subject.len() > 0x80 || plain_text.len() >= 0xfff {
        return Err(-5120);
    }
    let length = plain_subject.len() + 1 + plain_text.len() + 1;
    let (cut, truncated) = subject15(plain_subject);
    let mut short = [0u8; 0x81];
    short[..cut.len()].copy_from_slice(cut);
    let mut buffer = Zeroizing::new(vec![0u64; (BODY_AT + length).div_ceil(8)]);
    let header = buffer.as_mut_ptr() as usize;
    unsafe {
        let built = api.f::<unsafe extern "C" fn(usize, u32, u32, u32, u32, u32, usize, usize, u32, u32, u32, u32, u32) -> i32>(270, 13)(
            header, 0, m.own as u32, (m.own >> 32) as u32, m.to as u32, (m.to >> 32) as u32,
            m.name.as_ptr() as usize, short.as_ptr() as usize,
            u32::from(m.sender_handle), u32::from(m.recipient_handle), 1000, 0, length as u32);
        let packed = api.f::<unsafe extern "C" fn(usize, usize, u32, usize, usize) -> i32>(272, 5)(
            subject.as_ptr() as usize, text.as_ptr() as usize, 0, 0, header + BODY_AT);
        if built != length as i32 || packed != length as i32 {
            return Err(-5120);
        }
    }
    Ok((buffer, length, truncated))
}

/// app 10055f00 states 2-5 for one composed message (single recipient).
fn send_job(api: &Api, buffer: &mut [u64], frame: &mut dyn FnMut() -> bool) -> i32 {
    let (mut state, mut ticket, mut frames, mut failures, mut error) = (2, -1, 0u32, 0u32, 0);
    // 07d §4.7: request lines; the end line (with the latest error) on return.
    let mut request = native_jobs::RequestLog::start("mail-send");
    loop {
        match state {
            2 => {
                request.attempts += 1;
                let started = api.one(324, buffer.as_mut_ptr() as usize);
                if started < 0 {
                    error = started;
                    state = 4;
                } else {
                    (ticket, frames, state) = (started, 0, 3);
                }
            }
            3 => {
                frames += 1;
                let polled = api.one(325, ticket as usize);
                (request.polls, request.result) = (request.polls + 1, polled);
                if polled > 0 {
                    return polled;
                }
                if polled < 0 {
                    (error, state) = (polled, 4);
                } else if frames > mail_frames() {
                    cancel(api, ticket);
                    (error, state) = (native_jobs::TIMED_OUT, 4);
                }
            }
            // Once Play is requested no new attempt starts (07a
            // presign-no-cancel-freeze 1), a full-table reissue included.
            4 | 5 if crate::native_auth::play_requested() => {
                request.result = error;
                return error;
            }
            4 => { request.result = error; match error {
                -256 => state = 2,
                // 1005373b, 10052131 and 10052272 end the job: the message
                // is returned or marked undeliverable, never retried.
                -5320..=-5301 | -5322 | -5327 | -262 => return error,
                _ => {
                    failures += 1;
                    if failures < MAIL_ATTEMPTS && api.one(1107, error as usize) != 0 {
                        (frames, state) = (0, 5);
                    } else {
                        return error;
                    }
                }
            } }
            _ => {
                frames += 1;
                if frames > mail_frames() {
                    state = 2;
                }
            }
        }
        if !frame() {
            if state == 3 {
                cancel(api, ticket);
            }
            request.result = native_jobs::ABORTED;
            return native_jobs::ABORTED;
        }
    }
}

fn cancel(api: &Api, ticket: i32) {
    api.call(328, &[ticket as usize]); // void
}

fn send_mail(
    api: &Api,
    alternate: bool,
    slot: u16,
    uid: u64,
    subject: &str,
    message: &str,
    frame: &mut dyn FnMut() -> bool,
) -> Result<(i32, Option<i32>), i32> {
    // List B is the block list (core 100236a0); the Viewer never mails it.
    if alternate {
        return Err(BLOCKED);
    }
    let catalog = CATALOG_ACCESS.lock().unwrap_or_else(|e| e.into_inner());
    let target = friend(api, false, slot, uid)?;
    let subject = cp932(subject, 127)?;
    let text = cp932(message, 4095)?;
    // app 10055982..100559c3: the current handle (190) and its own name.
    let handle = api.zero(190);
    let name = handle_name(api, handle).ok_or(-7186)?;
    let own = unsafe { api.f::<unsafe extern "C" fn() -> u64>(342, 0)() };
    let m = Message { own, to: uid, name, sender_handle: handle as u8, recipient_handle: record_handle(&target) };
    let (mut buffer, length, truncated) = pack_message(api, &m, &subject, &text)?;
    drop(catalog);
    viewer_fixup(&mut bytes_of_mut(&mut buffer)[..HEADER_LEN], truncated, immediate_delivery(&target, length));
    let header: [u8; HEADER_LEN] = bytes_of(&buffer)[..HEADER_LEN].try_into().unwrap();
    let outbox = native_path(api, &header, 286)?;
    write_file(&outbox, &bytes_of(&buffer)[BODY_AT..BODY_AT + length])?;
    let code = send_job(api, &mut buffer, frame);
    if code <= 0 {
        return Ok((code, None));
    }
    let header = bytes_of(&buffer)[..HEADER_LEN].try_into().unwrap();
    let archive = native_path(api, &header, 287).and_then(|sent| move_file(&outbox, &sent));
    // A local archive error must never tell the user to send an already
    // delivered message again. Report native delivery and storage separately.
    Ok((code, archive.err()))
}

/// app 10056256 + 10051556: the automatic receipt for a pushed message. The
/// original header with sender and recipient swapped, my handle's name,
/// kind 0, application 1000, body "\x07\0", [0x3e] bit 12 and 14 set.
fn receipt(api: &Api, original: &[u8; HEADER_LEN]) -> Zeroizing<Vec<u64>> {
    let mut buffer = Zeroizing::new(vec![0u64; (BODY_AT + 2).div_ceil(8)]);
    let bytes = bytes_of_mut(&mut buffer);
    bytes[..HEADER_LEN].copy_from_slice(original);
    bytes[..8].copy_from_slice(&original[8..16]);
    bytes[8..16].copy_from_slice(&original[..8]);
    bytes[0x3c] = original[0x3d];
    bytes[0x3d] = original[0x3c];
    // 1003e509(my handle) else the current handle's name (1001d02e).
    if let Some(name) = handle_name(api, i32::from(original[0x3d])).or_else(|| handle_name(api, api.zero(190))) {
        bytes[0x10..0x20].copy_from_slice(&name);
    }
    bytes[0x1f] = 0;
    bytes[0x38..0x3c].copy_from_slice(&2u32.to_le_bytes());
    let flags = ((u16_at(bytes, 0x3e) & 0xff80 & 0xf07f) & !0x2000) | 0x1000 | 0x4000;
    bytes[0x3e..0x40].copy_from_slice(&flags.to_le_bytes());
    bytes[0x40..0x42].copy_from_slice(&1000u16.to_le_bytes());
    bytes[BODY_AT] = 7;
    bytes[BODY_AT + 1] = 0;
    buffer
}

/// app 10056816 checks before storing: not kind 31, a BEL, and a NUL within
/// the declared length.
fn storable(header: &[u8], body: &[u8]) -> bool {
    u16_at(header, 0x3e) & 0xf80 != 0xf80
        && body.iter().position(|b| *b == 0).is_some_and(|end| body[..end].contains(&7))
}

fn move_file(source: &[u8], target: &[u8]) -> Result<(), i32> {
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExA, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    if unsafe {
        MoveFileExA(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(-10240)
    } else {
        Ok(())
    }
}

fn message_file(path: &[u8], write: bool) -> Result<std::fs::File, i32> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileA, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
        },
    };
    if path.first() == Some(&0) || !path.contains(&0) {
        return Err(-5120);
    }
    // 923 enqueues a close; it neither joins IO nor releases the file before
    // returning. Launcher storage uses synchronous Win32 IO with an owned
    // handle. The native paths and complete bodies are unchanged, and every
    // buffer and handle finishes before an acknowledgement or rename. FFXI's
    // original asynchronous file service remains untouched.
    let handle = unsafe {
        CreateFileA(
            path.as_ptr(),
            if write { GENERIC_WRITE } else { GENERIC_READ },
            if write { 0 } else { FILE_SHARE_READ },
            std::ptr::null(),
            if write { CREATE_ALWAYS } else { OPEN_EXISTING },
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(-10241);
    }
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}
fn read_file(path: &[u8], length: usize) -> Result<Zeroizing<Vec<u8>>, i32> {
    use std::io::Read;
    let mut bytes = Zeroizing::new(vec![0; length]);
    message_file(path, false)?
        .read_exact(&mut bytes)
        .map_err(|_| -10241)?;
    Ok(bytes)
}
/// Complete and flushed before any acknowledgement depends on it.
fn write_file(path: &[u8], bytes: &[u8]) -> Result<(), i32> {
    use std::io::Write;
    let mut file = message_file(path, true)?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|_| -10241)
}
fn aligned_header(bytes: &[u8; HEADER_LEN]) -> [u64; HEADER_LEN / 8] {
    let mut header = [0u64; HEADER_LEN / 8];
    for (out, word) in header.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *out = u64::from_le_bytes(*word);
    }
    header
}
fn native_path(api: &Api, bytes: &[u8; HEADER_LEN], entry: usize) -> Result<[u8; 512], i32> {
    let mut header = aligned_header(bytes);
    let mut path = [0; 512];
    api.two(
        entry,
        header.as_mut_ptr() as usize,
        path.as_mut_ptr() as usize,
    );
    if path[0] == 0 || !path.contains(&0) {
        return Err(-5120);
    }
    Ok(path)
}

/// Opening a stored message is local, as in the Viewer: read the file the
/// automatic download saved and rename it to clear the unread bit. `None`
/// means a kind this text view cannot show (system or attachment formats);
/// it is still marked read.
fn read_mail(api: &Api, stored: &mut StoredHeader) -> Result<Option<Zeroizing<Vec<u8>>>, i32> {
    let bytes = &stored.header;
    let length = u32_at(bytes, 0x38) as usize;
    if length == 0 || length > MAX_BODY {
        return Err(-5120);
    }
    let mut local = *bytes;
    if stored.read {
        local[0x3f] &= 0x7f;
    }
    let entry = match stored.folder {
        0 => 284,
        1 => 285,
        _ => 287,
    };
    let path = native_path(api, &local, entry)?;
    let mut body = read_file(&path, length)?;
    if !stored.read && bytes[0x3f] & 0x80 != 0 {
        let mut read = *bytes;
        read[0x3f] &= 0x7f;
        // The filename is the native encoded header. Renaming changes its read
        // bit while preserving the exact body and recipient/attachment metadata.
        move_file(&path, &native_path(api, &read, entry)?)?;
    }
    stored.read = true;
    let Some(end) = body.iter().position(|b| *b == 0) else { return Ok(None) };
    if end + 1 > 8192 || !body[..end].contains(&7) {
        return Ok(None);
    }
    body.truncate(end + 1);
    Ok(Some(body))
}

fn local_headers(api: &Api, folder: u8) -> Result<Vec<StoredHeader>, i32> {
    let mut directory = [0u8; 512];
    let kind = match folder { 0 => 7, 1 => 8, _ => 6 };
    api.call(126, &[kind, directory.as_mut_ptr() as usize, 512, 0]);
    let end = directory
        .iter()
        .position(|b| *b == 0)
        .filter(|n| *n > 0)
        .ok_or(-5120)?;
    let directory = std::path::PathBuf::from(crate::game::ansi_to_string(&directory[..end]));
    let mut headers = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(|_| -10240)? {
        let entry = entry.map_err(|_| -10240)?;
        if !entry.file_type().map_err(|_| -10240)?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|s| s.is_ascii() && s.len() == 96) else {
            continue;
        };
        let name = std::ffi::CString::new(name).map_err(|_| -5120)?;
        let mut header = [0u64; 12];
        // The installed 291 wrapper takes output first, source filename second.
        // Older reverse-engineering notes had these parameters reversed.
        if api.call(291, &[header.as_mut_ptr() as usize, name.as_ptr() as usize]) != 0 {
            continue;
        }
        let bytes = &bytes_of(&header)[..HEADER_LEN];
        headers.push(StoredHeader {
            header: bytes.try_into().unwrap(),
            folder,
            read: bytes[0x3f] & 0x80 == 0,
        });
        if headers.len() >= 100_000 {
            return Err(-5120);
        }
    }
    Ok(headers)
}

/// app 10056b40 list errors -2/-12/-18/-256: state 5, then state 2 next frame.
fn transient_list_error(code: i32) -> bool {
    matches!(code, -2 | -12 | -18 | -256)
}

/// app 1005245d: the same listed message as an already queued download.
fn same_message(a: &[u8], b: &[u8]) -> bool {
    a[0x30..0x38] == b[0x30..0x38] && a[..0x10] == b[..0x10] && a[0x3c..0x3e] == b[0x3c..0x3e]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fetched {
    /// Downloaded, saved and acknowledged.
    Saved,
    /// Acknowledged without download (app 10051bbc).
    Discarded,
    Failed(i32),
}

/// app 10056fe7: one listed message. States 0-3 download (326/327), 4 saves,
/// 5-8 acknowledge (322/323). One state action per frame.
struct Download {
    /// The 0x50 record; +0x4c points at `body`. Boxed: the core keeps the
    /// record and body addresses while a ticket is pending.
    record: [u64; RECORD_LEN / 8],
    body: Zeroizing<Vec<u8>>,
    path: [u8; 512],
    state: u8,
    ticket: i32,
    frames: u32,
    failures: u32,
    error: i32,
    /// The body is stored; only then may it be acknowledged.
    saved: bool,
    outcome: Option<Fetched>,
}
impl Download {
    fn new(api: &Api, listed: &[u8]) -> Option<Box<Self>> {
        let length = u32_at(listed, 0x38) as usize;
        if length > MAX_BODY {
            return None;
        }
        let header: [u8; HEADER_LEN] = listed[..HEADER_LEN].try_into().unwrap();
        // 284(record) names the inbox file (app 1005702a). Without a storable
        // path nothing is downloaded or acknowledged; the server keeps it.
        let path = native_path(api, &header, 284).ok()?;
        let mut job = Box::new(Self {
            record: [0; RECORD_LEN / 8],
            body: Zeroizing::new(vec![0; length.max(1)]),
            path,
            state: if discarded_unread(api, &header) { 5 } else { 0 },
            ticket: -1,
            frames: 0,
            failures: 0,
            error: 0,
            saved: false,
            outcome: None,
        });
        bytes_of_mut(&mut job.record)[..HEADER_LEN].copy_from_slice(&header);
        job.record[9] = (job.body.as_ptr() as u64) << 32;
        Some(job)
    }
    fn header(&self) -> &[u8] {
        &bytes_of(&self.record)[..HEADER_LEN]
    }
    fn pending(&self) -> Option<i32> {
        matches!(self.state, 1 | 6).then_some(self.ticket)
    }
    fn retry(&mut self, api: &Api, reissue: u8, wait: u8) {
        // No new attempt once Play is requested; the server keeps the message.
        if crate::native_auth::play_requested() {
            self.outcome = Some(Fetched::Failed(self.error));
            return;
        }
        if self.error == -256 {
            self.state = reissue;
            return;
        }
        self.failures += 1;
        if self.failures < MAIL_ATTEMPTS && api.one(1107, self.error as usize) != 0 {
            (self.frames, self.state) = (0, wait);
        } else {
            self.outcome = Some(Fetched::Failed(self.error));
        }
    }
    fn step(&mut self, api: &Api) {
        let record = self.record.as_mut_ptr() as usize;
        match self.state {
            0 => {
                let ticket = api.one(326, record);
                if ticket < 0 {
                    (self.error, self.state) = (ticket, 2);
                } else {
                    (self.ticket, self.frames, self.state) = (ticket, 0, 1);
                }
            }
            1 | 6 => {
                self.frames += 1;
                let polled = api.one(if self.state == 1 { 327 } else { 323 }, self.ticket as usize);
                let failed = if self.state == 1 { 2 } else { 7 };
                if polled > 0 {
                    if self.state == 1 {
                        self.state = 4;
                    } else {
                        self.outcome = Some(if self.saved { Fetched::Saved } else { Fetched::Discarded });
                    }
                } else if polled < 0 {
                    (self.error, self.state) = (polled, failed);
                } else if self.frames > mail_frames() {
                    cancel(api, self.ticket);
                    (self.error, self.state) = (native_jobs::TIMED_OUT, failed);
                }
            }
            2 => self.retry(api, 0, 3),
            7 => self.retry(api, 5, 8),
            3 | 8 if crate::native_auth::play_requested() => self.outcome = Some(Fetched::Failed(self.error)),
            3 | 8 => {
                self.frames += 1;
                if self.frames >= mail_frames() {
                    self.state = if self.state == 3 { 0 } else { 5 };
                }
            }
            4 => {
                // The Viewer ignores its save result (10056816). Crystal never
                // acknowledges a message it has not stored completely.
                let length = (u32_at(self.header(), 0x38) as usize).min(self.body.len());
                if let Err(code) = write_file(&self.path, &self.body[..length]) {
                    self.outcome = Some(Fetched::Failed(code));
                    return;
                }
                (self.saved, self.failures, self.state) = (true, 0, 5);
            }
            _ => {
                let ticket = api.one(322, record);
                if ticket < 0 {
                    (self.error, self.state) = (ticket, 7);
                } else {
                    (self.ticket, self.frames, self.state) = (ticket, 0, 6);
                }
            }
        }
    }
}

impl Service {
    /// The Viewer's social entry: after sign-in and after every return from a
    /// game, once the list loads have finished. Runs the stock automatic mail
    /// work (list, download, save, acknowledge). `frame` is one owner frame
    /// and returns false when the owner is stopping.
    pub fn social_entry(&mut self, api: &Api, frame: &mut dyn FnMut() -> bool) {
        self.begin_social_entry(api);
        self.mail_cycle(api, frame);
    }

    /// At initial sign-in only, the list can poll faster: its pending/retry
    /// limits use wall time. Keep downloads and acknowledgements on the
    /// ordinary frame so their 3600-frame deadlines retain their duration.
    /// Returns the owner frame the list finished on: the Viewer's mail →
    /// presence pause counts frames from the list response, not from the end
    /// of the transfers.
    pub fn social_entry_with_list_frame(&mut self, api: &Api, list_interval: Duration, frame: &mut dyn FnMut(Duration) -> bool) -> u64 {
        self.begin_social_entry(api);
        let listed = self.list(api, &mut || frame(list_interval));
        let at = crate::native_auth::owner_frames();
        if let Some(listed) = listed { self.download_listed(api, &listed, &mut || frame(native_jobs::FRAME)); }
        at
    }

    fn begin_social_entry(&mut self, api: &Api) {
        // app 10051043 lists on every main-menu entry: the flag that requests
        // it is set once at Viewer start and never cleared, and the live stock
        // capture lists after sign-in and after the game alike (pol-spec 05 §3).
        // 1019fef3..1019ff14 still clears an arrival no receiver took.
        if api.zero(1304) != 0 {
            api.call(1305, &[0]); // void
        }
        // A check request leaves the 30 s retry wait at once (10056b83).
        self.list_retry = None;
    }

    /// Called once per idle owner frame, between games only.
    /// A due retry is a new attempt, so none starts once Play is requested;
    /// the wait is kept for the next main-menu entry, which lists anyway.
    pub fn idle_frame(&mut self, api: &Api, frame: &mut dyn FnMut() -> bool) {
        if crate::native_auth::play_requested() { return; }
        if self.list_retry.is_some_and(|at| Instant::now() >= at) {
            self.list_retry = None;
            self.mail_cycle(api, frame);
        }
    }

}

impl PushDelivery {
    /// Store queued pushes, and with `receipts` also answer them and send due
    /// receipts. Owners call it without receipts when the member session is
    /// already gone (sign-out after loss), so nothing pushed is lost locally.
    pub fn flush_pushed(&mut self, api: &Api, receipts: bool) {
        let dropped = mail().drops.swap(0, Ordering::AcqRel);
        if dropped != 0 {
            log!("native POL dropped {dropped} pushed message(s) the receiver could not copy");
        }
        let mut pushed = std::mem::take(&mut *mail().pushed.lock().unwrap_or_else(|e| e.into_inner()));
        pushed.append(&mut self.unsaved);
        for message in pushed {
            // app 10056b24: queue the receipt, then save the message.
            if receipts && !message.answered {
                self.receipts.push(receipt(api, &message.header));
            }
            if !storable(&message.header, &message.body) {
                log!("native POL pushed message kind is not stored by the Viewer; skipped");
                continue;
            }
            match native_path(api, &message.header, 284).and_then(|path| write_file(&path, &message.body)) {
                Ok(()) => events::send("{\"type\":\"native_mail_changed\"}\n".into()),
                // Stock receipts even an unsaved message; keep it and retry the save.
                Err(code) if message.attempts < SAVE_ATTEMPTS => {
                    log!("native POL could not save a pushed message ({code}); retrying");
                    self.unsaved.push(Pushed { answered: receipts || message.answered, attempts: message.attempts + 1, ..message });
                }
                Err(code) => log!("error: native POL could not save a pushed message ({code})"),
            }
        }
        if receipts {
            // Send job state 7 (app 10055fea): 333 every frame until accepted.
            self.receipts.retain_mut(|buffer| api.one(333, buffer.as_mut_ptr() as usize) < 0);
        }
    }

    /// The Viewer instance ends at the game handoff and its receipt queue with
    /// it: a receipt still unaccepted then is never sent after the game.
    /// Returns how many were dropped.
    pub fn abandon_receipts(&mut self) -> usize {
        let dropped = self.receipts.len();
        self.receipts.clear();
        dropped
    }
}

impl Service {

    fn mail_cycle(&mut self, api: &Api, frame: &mut dyn FnMut() -> bool) {
        let Some(listed) = self.list(api, frame) else { return };
        self.download_listed(api, &listed, frame);
    }

    fn download_listed(&mut self, api: &Api, listed: &[u8], frame: &mut dyn FnMut() -> bool) {
        // Transfers are new requests: none start once Play is requested (the
        // listed messages stay on the server for the next main-menu entry).
        if crate::native_auth::play_requested() {
            log!("native POL mail transfers not started: Play requested");
            return;
        }
        // app 1005641d: every new record (header +0x3e bit 15) not already queued.
        let mut jobs: Vec<Box<Download>> = Vec::new();
        for record in listed.as_chunks::<RECORD_LEN>().0.iter().filter(|r| u16_at(&r[..], 0x3e) & 0x8000 != 0) {
            if jobs.iter().any(|job| same_message(job.header(), record)) {
                continue;
            }
            jobs.extend(Download::new(api, record));
        }
        if jobs.is_empty() {
            return;
        }
        loop {
            let mut active = false;
            for job in jobs.iter_mut().filter(|job| job.outcome.is_none()) {
                job.step(api);
                active |= job.outcome.is_none();
            }
            if !active {
                break;
            }
            if !frame() {
                for ticket in jobs.iter().filter(|job| job.outcome.is_none()).filter_map(|job| job.pending()) {
                    cancel(api, ticket);
                }
                break;
            }
        }
        for job in &jobs {
            if let Some(Fetched::Failed(code)) = job.outcome {
                log!("native POL mail transfer stopped ({code}); the server keeps the message");
            }
        }
        if jobs.iter().any(|job| job.outcome == Some(Fetched::Saved)) {
            events::send("{\"type\":\"native_mail_changed\"}\n".into());
        }
    }

    /// app 10056b40 states 2, 3 and 5. Returns the listed records, or None
    /// when nothing was listed (a retry is then scheduled as the Viewer does).
    fn list(&mut self, api: &Api, frame: &mut dyn FnMut() -> bool) -> Option<Zeroizing<Vec<u8>>> {
        // 07d §4.7: one start/end pair over the reissues; its end line on return.
        let mut request: Option<native_jobs::RequestLog> = None;
        'issue: loop {
            // 1001cf90: the Viewer issues nothing without a connected member,
            // and a closing or cancelled owner starts no list only to cancel it.
            if api.zero(838) != 3 || crate::native_auth::owner_stopping(api) {
                return None;
            }
            let mut buffer = Zeroizing::new(vec![0u64; LIST_MAX * RECORD_LEN / 8]);
            let request = request.get_or_insert_with(|| native_jobs::RequestLog::start("mail-list"));
            request.attempts += 1;
            let ticket = api.call(329, &[buffer.as_mut_ptr() as usize, LIST_MAX]);
            request.result = ticket;
            if ticket < 0 {
                if transient_list_error(ticket) {
                    // A re-list is a new attempt: none once Play is requested.
                    if !frame() || !frame() || crate::native_auth::play_requested() {
                        return None;
                    }
                    continue 'issue;
                }
                // 10056e1d: a second failure in a row waits for the next check.
                if self.list_errors == 0 {
                    self.list_errors = 1;
                    self.list_retry = Some(Instant::now() + LIST_RETRY);
                }
                return None;
            }
            self.list_errors = 0;
            let started = Instant::now();
            loop {
                if !frame() {
                    cancel(api, ticket);
                    request.result = native_jobs::ABORTED;
                    return None;
                }
                let polled = api.one(330, ticket as usize);
                (request.polls, request.result) = (request.polls + 1, polled);
                if polled > 0 {
                    // Core 10027ac0 clears bit 15 of every record past the count.
                    return Some(Zeroizing::new(bytes_of(&buffer).to_vec()));
                }
                if polled < 0 {
                    if transient_list_error(polled) {
                        if !frame() || crate::native_auth::play_requested() {
                            return None;
                        }
                        continue 'issue;
                    }
                    // 10056c38: -5434 waits for the next check; others retry in 30 s.
                    if polled != -5434 {
                        self.list_errors += 1;
                        self.list_retry = Some(Instant::now() + LIST_RETRY);
                    }
                    return None;
                }
                if started.elapsed() > LIST_PENDING {
                    // 10056d41: cancel after 60 s, then the 30 s retry wait.
                    cancel(api, ticket);
                    request.result = native_jobs::TIMED_OUT;
                    self.list_retry = Some(Instant::now() + LIST_RETRY);
                    return None;
                }
            }
        }
    }

    /// Requests from the launcher; `frame` is the owner's idle frame.
    pub fn execute_with(&mut self, api: &Api, request: NativeSocialRequest, frame: &mut dyn FnMut() -> bool) {
        if events::pipe_closed() || api.zero(838) != 3 {
            result(&request, native_jobs::ABORTED, None);
            return;
        }
        result(&request, 0, None);
        match &request {
            NativeSocialRequest::Inbox { .. } | NativeSocialRequest::Check { .. } => {
                let (folder, page) = if let NativeSocialRequest::Inbox { folder, page, .. } = &request { (*folder, *page) } else {
                    // The Viewer's check (1005166e): leave any retry wait and run
                    // the list job now; new mail is downloaded, saved and
                    // acknowledged before the Inbox is shown.
                    self.list_retry = None;
                    self.mail_cycle(api, frame);
                    (0, 0)
                };
                // Otherwise local only: new mail is downloaded automatically at
                // the social entry, on arrival and on a check, as by the Viewer.
                let mut records = match local_headers(api, folder) {
                    Ok(h) => h,
                    Err(code) => {
                        result(&request, code, None);
                        return;
                    }
                };
                records.sort_by_key(|r| std::cmp::Reverse(u32_at(&r.header, 0x34)));
                let page =
                    page.min((records.len().div_ceil(HEADER_COUNT) as u32).saturating_sub(1));
                let offset = page as usize * HEADER_COUNT;
                let unread = records.iter().filter(|r| r.header[0x3f] & 0x80 != 0).count() as u32;
                let mut payload = vec![0; 12 + HEADER_COUNT * HEADER_LEN];
                payload[..4].copy_from_slice(&(records.len() as u32).to_le_bytes());
                payload[4..8].copy_from_slice(&page.to_le_bytes());
                payload[8..12].copy_from_slice(&unread.to_le_bytes());
                for (out, record) in payload[12..]
                    .as_chunks_mut::<HEADER_LEN>().0.iter_mut()
                    .zip(records.iter().skip(offset).take(HEADER_COUNT))
                {
                    out.copy_from_slice(&record.header);
                }
                self.headers = records;
                result(&request, 1, Some(&payload));
            }
            NativeSocialRequest::Read { header, .. } => {
                let stored = self.headers.iter_mut().find(|r| &r.header == header);
                match stored.ok_or(-5120).and_then(|r| read_mail(api, r)) {
                    Ok(Some(body)) => result(&request, 1, Some(&body)),
                    // Stored and now read, but not a text message this view shows.
                    Ok(None) => result(&request, 2, None),
                    Err(code) => result(&request, code, None),
                }
            }
            NativeSocialRequest::Send { alternate, slot, uid, subject, message, .. } => {
                match send_mail(api, *alternate, *slot, *uid, subject, message, frame) {
                    Ok((code, warning)) => result(
                        &request,
                        code,
                        warning.map(|n| n.to_string()).as_deref().map(str::as_bytes),
                    ),
                    Err(code) => result(&request, code, None),
                }
            }
            NativeSocialRequest::Remove { alternate, slot, uid, .. } => {
                result(&request, remove_entry(api, *alternate, *slot, *uid, frame), None);
            }
        }
    }
}

/// app 1003e080: remove marks the entry locally (188 for list A, 186 for the
/// block list B, i.e. unblock), then the per-frame updater 100422b3 commits
/// with Put Friend List 1034/1035 (flag 0). 1034 holds the version guard and
/// may fetch instead (core 100255e0). No Get follows: the Viewer redraws from
/// the cache (1003fb56).
fn remove_entry(api: &Api, alternate: bool, slot: u16, uid: u64, frame: &mut dyn FnMut() -> bool) -> i32 {
    let catalog = CATALOG_ACCESS.lock().unwrap_or_else(|e| e.into_inner());
    let marked = friend(api, alternate, slot, uid).map(|_| api.one(if alternate { 186 } else { 188 }, slot as usize));
    drop(catalog);
    match marked {
        Err(code) => code,
        Ok(code) if code < 0 => code,
        Ok(_) => native_jobs::run(
            api,
            native_jobs::Request { name: "friend-remove", start: &mut || api.zero(1034), poll: 1035, policy: native_jobs::Policy::Normal },
            frame,
        ),
    }
}

#[cfg(test)]
pub unsafe fn smoke_builders(table: &[usize]) {
    let api = Api::test_table(table);
    let subject = cp932("Native subject that is long", 127).unwrap();
    let text = cp932("Native text\nSecond line", 4095).unwrap();
    let mut name = [0u8; 16];
    name[..6].copy_from_slice(b"Sender");
    let m = Message { own: 0x1122334455667788, to: 0x8877665544332211, name, sender_handle: 5, recipient_handle: 9 };
    let (mut buffer, length, truncated) = pack_message(&api, &m, &subject, &text).unwrap();
    assert!(truncated);
    let bytes = bytes_of(&buffer);
    assert_eq!(&bytes[..8], &0x1122334455667788u64.to_le_bytes());
    assert_eq!(&bytes[8..16], &0x8877665544332211u64.to_le_bytes());
    assert_eq!(&bytes[16..23], b"Sender\0");
    assert_eq!(&bytes[0x20..0x30], b"Native subject \0");
    assert_eq!(u32_at(bytes, 0x38) as usize, length);
    assert_eq!((bytes[0x3c], bytes[0x3d]), (5, 9));
    assert_eq!(u16_at(bytes, 0x3e) & 0xbfff, 0x8000, "kind 0, no attachment, new");
    assert_eq!(u16_at(bytes, 0x40), 1000);
    let expected = b"Native subject that is long\x07Native text\nSecond line\0";
    assert_eq!(&bytes[BODY_AT..BODY_AT + expected.len()], expected);
    viewer_fixup(&mut bytes_of_mut(&mut buffer)[..HEADER_LEN], truncated, false);
    let bytes = bytes_of(&buffer);
    assert_eq!(u16_at(bytes, 0x42) & 0x3f00, 0x1000);
    // Feed the installed core's real single-argument Viewer dispatch a
    // synthetic encoded notification. This does not pump, connect, send,
    // acknowledge, write a file or expose a real message.
    assert_eq!(table[334] - table[331], 0xe0);
    let header: [u8; HEADER_LEN] = bytes[..HEADER_LEN].try_into().unwrap();
    let path = native_path(&api, &header, 284).unwrap();
    let end = path.iter().position(|b| *b == 0).unwrap();
    let filename = path[..end].rsplit(|b| *b == b'\\' || *b == b'/').next().unwrap();
    assert_eq!(filename.len(), 96);
    let mut wire = Zeroizing::new(Vec::with_capacity(96 + length));
    wire.extend_from_slice(filename);
    wire.extend_from_slice(&bytes[BODY_AT..BODY_AT + length]);
    let notifications = MailNotifications::start(&api);
    api.f::<unsafe extern "C" fn(i32)>(1032, 1)(1);
    api.f::<unsafe extern "C" fn(usize)>(334, 1)(wire.as_ptr() as usize);
    assert!(notifications.take_changed(), "native Viewer message callback was not dispatched");
    {
        let pushed = mail().pushed.lock().unwrap();
        assert_eq!(pushed.len(), 1, "the Viewer record carries its body at +0x4c");
        assert_eq!(&pushed[0].header[..0x30], &header[..0x30]);
        assert_eq!(&pushed[0].body[..], &bytes[BODY_AT..BODY_AT + length]);
    }
    drop(notifications);
    discard_pushed();
    api.f::<unsafe extern "C" fn(i32)>(1032, 1)(0);
    assert_ne!(api.zero(838), 3);
}

#[cfg(test)]
pub unsafe fn smoke_read_file(table: &[usize], path: &[u8], expected: &[u8]) {
    unsafe extern "C" fn no_pump() -> i32 {
        0
    }
    let mut table = table.to_vec();
    table[818] = no_pump as *const () as usize;
    let api = Api::test_table(&table);
    assert_eq!(&*read_file(path, expected.len()).unwrap(), expected);
    smoke_storage(&api);
}

#[cfg(test)]
unsafe fn smoke_storage(api: &Api) {
    let subject = cp932("Offline archive", 127).unwrap();
    let text = cp932("Preserved body", 4095).unwrap();
    let mut name = [0u8; 16];
    name[..6].copy_from_slice(b"Sender");
    let m = Message { own: 0x1122334455667788, to: 0x8877665544332211, name, sender_handle: 1, recipient_handle: 2 };
    let (buffer, length, _) = pack_message(api, &m, &subject, &text).unwrap();
    let bytes = bytes_of(&buffer);
    let mut header: [u8; HEADER_LEN] = bytes[..HEADER_LEN].try_into().unwrap();
    header[0x3f] |= 0x80;
    let original = header;
    let path = native_path(api, &header, 284).unwrap();
    assert_eq!(header, original, "native path construction must preserve IDs");
    write_file(&path, &bytes[BODY_AT..BODY_AT + length]).unwrap();
    let mut listed = local_headers(api, 0).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].header, header);
    let body = read_mail(api, &mut listed[0]).unwrap().unwrap();
    assert_eq!(&*body, b"Offline archive\x07Preserved body\0");
    let listed = local_headers(api, 0).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].read);
    let read_path = native_path(api, &listed[0].header, 284).unwrap();
    assert_eq!(&*read_file(&read_path, length).unwrap(), &bytes[BODY_AT..BODY_AT + length]);
    let outbox = native_path(api, &header, 286).unwrap();
    let sent = native_path(api, &header, 287).unwrap();
    write_file(&outbox, &bytes[BODY_AT..BODY_AT + length]).unwrap();
    move_file(&outbox, &sent).unwrap();
    assert_eq!(local_headers(api, 2).unwrap().len(), 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, path::PathBuf, sync::atomic::AtomicUsize};

    /// Fake core state; tests hold SERIAL because the push queue is global.
    static SERIAL: Mutex<()> = Mutex::new(());
    #[derive(Default)]
    struct Core {
        calls: Vec<(usize, u64)>,
        listed: Vec<[u8; RECORD_LEN]>,
        list_starts: Vec<i32>,
        pending: HashMap<i32, usize>,
        dir: PathBuf,
        unwritable: bool,
        member: bool,
        friends: Vec<[u8; 0xb0]>,
        blocked: Vec<[u8; 0xb0]>,
        flagged: i32,
        built: Vec<u64>,
        built_at: usize,
        strings: Vec<Vec<u8>>,
        packed: Vec<usize>,
        sent: Vec<Vec<u8>>,
        receipts: Vec<Vec<u8>>,
        receipt_results: Vec<i32>,
        put_polls: Vec<i32>,
        pending_push: Option<[u8; RECORD_LEN]>,
    }
    static CORE: Mutex<Option<Core>> = Mutex::new(None);
    fn with<R>(f: impl FnOnce(&mut Core) -> R) -> R {
        f(CORE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().unwrap())
    }
    fn note(slot: usize, info: u64) {
        with(|c| c.calls.push((slot, info)))
    }
    fn calls() -> Vec<(usize, u64)> {
        with(|c| c.calls.clone())
    }
    unsafe fn raw<'a>(at: usize, length: usize) -> &'a mut [u8] {
        std::slice::from_raw_parts_mut(at as *mut u8, length)
    }
    unsafe fn text(at: usize) -> Vec<u8> {
        std::ffi::CStr::from_ptr(at as *const std::ffi::c_char).to_bytes().to_vec()
    }
    fn id_of(header: &[u8]) -> u64 {
        u64::from(u32_at(header, 0x30))
    }
    unsafe fn write_path(prefix: &str, header: usize, out: usize) {
        let id = id_of(raw(header, HEADER_LEN));
        let (dir, unwritable) = with(|c| (c.dir.clone(), c.unwritable));
        let path = if unwritable { dir.join("missing").join(format!("{prefix}-{id}.bin")) } else { dir.join(format!("{prefix}-{id}.bin")) };
        let path = path.to_str().unwrap().as_bytes();
        let out = raw(out, 512);
        out[..path.len()].copy_from_slice(path);
        out[path.len()] = 0;
    }

    unsafe extern "C" fn connected() -> i32 { 3 }
    unsafe extern "C" fn list(buffer: usize, max: u32) -> i32 {
        note(329, u64::from(max));
        let (start, listed) = with(|c| (if c.list_starts.is_empty() { 7 } else { c.list_starts.remove(0) }, c.listed.clone()));
        if start >= 0 {
            for (index, record) in listed.iter().enumerate() {
                raw(buffer + index * RECORD_LEN, RECORD_LEN).copy_from_slice(record);
            }
        }
        start
    }
    unsafe extern "C" fn list_poll(ticket: usize) -> i32 { note(330, ticket as u64); 1 }
    unsafe extern "C" fn download(record: usize) -> i32 {
        let id = id_of(raw(record, HEADER_LEN));
        note(326, id);
        with(|c| c.pending.insert(100 + id as i32, record));
        100 + id as i32
    }
    unsafe extern "C" fn download_poll(ticket: usize) -> i32 {
        let record = raw(with(|c| c.pending[&(ticket as i32)]), RECORD_LEN);
        raw(u32_at(record, 0x4c) as usize, 9).copy_from_slice(b"Hi\x07There\0");
        note(327, id_of(record));
        1
    }
    unsafe extern "C" fn inbox_path(header: usize, out: usize) { write_path("in", header, out) }
    /// 126: the folder the Inbox lists (the fixture's own directory).
    unsafe extern "C" fn folder_path(_kind: i32, out: usize, size: i32, _: i32) -> i32 {
        let dir = with(|c| c.dir.clone());
        let bytes = dir.to_str().unwrap().as_bytes();
        assert!(bytes.len() < size as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len());
        *((out + bytes.len()) as *mut u8) = 0;
        bytes.len() as i32
    }
    unsafe extern "C" fn outbox_path(header: usize, out: usize) { write_path("out", header, out) }
    unsafe extern "C" fn sent_path(header: usize, out: usize) { write_path("sent", header, out) }
    unsafe extern "C" fn acknowledge(record: usize) -> i32 {
        let id = id_of(raw(record, HEADER_LEN));
        let stored = with(|c| c.dir.join(format!("in-{id}.bin"))).exists();
        note(322, id);
        note(1322, u64::from(stored));
        200 + id as i32
    }
    unsafe extern "C" fn acknowledge_poll(ticket: usize) -> i32 { note(323, ticket as u64 - 200); 1 }
    unsafe extern "C" fn not_retryable(_: usize) -> i32 { 0 }
    unsafe extern "C" fn cancel_ticket(ticket: usize) { note(328, ticket as u64) }
    fn valid_id() -> [u8; 8] {
        let mut id = *b"?BCD1234";
        id[0] = (id[1..].iter().map(|b| u32::from(*b)).sum::<u32>() % 26) as u8 + b'A';
        id
    }
    unsafe extern "C" fn member_of(_: u32, _: u32, out: usize) {
        let id = if with(|c| c.member) { valid_id() } else { *b"abcd1234" };
        raw(out, 8).copy_from_slice(&id);
    }
    unsafe extern "C" fn list_a(index: usize, out: usize) -> i32 {
        let record = with(|c| c.friends.get(index).copied()).unwrap_or([0; 0xb0]);
        raw(out, 0xb0).copy_from_slice(&record);
        0
    }
    unsafe extern "C" fn list_b(index: usize, out: usize) -> i32 {
        let record = with(|c| c.blocked.get(index).copied()).unwrap_or([0; 0xb0]);
        raw(out, 0xb0).copy_from_slice(&record);
        0
    }
    unsafe extern "C" fn no_groups() -> i32 { 0 }
    unsafe extern "C" fn arrival_flag() -> i32 { with(|c| c.flagged) }
    unsafe extern "C" fn clear_arrival(value: i32) { note(1305, value as u64) }
    unsafe extern "C" fn current_handle() -> i32 { 5 }
    unsafe extern "C" fn handle_record(handle: usize, out: usize) -> i32 {
        let out = raw(out, 0x28);
        out.fill(0);
        out[0] = 1;
        out[8..11].copy_from_slice(format!("Me{handle}").as_bytes());
        0
    }
    unsafe extern "C" fn own_id() -> u64 { 0x0000_0001_0000_0002 }
    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn build(header: usize, kind: u32, own_lo: u32, own_hi: u32, to_lo: u32, to_hi: u32, name: usize, subject: usize, sender: u32, recipient: u32, application: u32, flags7: u32, length: u32) -> i32 {
        with(|c| {
            c.built = [kind, own_lo, own_hi, to_lo, to_hi, sender, recipient, application, flags7, length].map(u64::from).to_vec();
            c.built_at = header;
            c.strings = vec![text(name), text(subject)];
        });
        // Core 1001a760's fields; 0x3e bits 12-13 and 0x42 bits 8-11/13 are
        // set here only to prove the Viewer fix-up clears them.
        let h = raw(header, HEADER_LEN);
        h[..4].copy_from_slice(&own_lo.to_le_bytes());
        h[4..8].copy_from_slice(&own_hi.to_le_bytes());
        h[8..12].copy_from_slice(&to_lo.to_le_bytes());
        h[12..16].copy_from_slice(&to_hi.to_le_bytes());
        h[0x38..0x3c].copy_from_slice(&length.to_le_bytes());
        h[0x3c] = sender as u8;
        h[0x3d] = recipient as u8;
        h[0x3e..0x40].copy_from_slice(&((0xb000 | (kind << 7) | flags7) as u16).to_le_bytes());
        h[0x40..0x42].copy_from_slice(&(application as u16).to_le_bytes());
        h[0x42..0x44].copy_from_slice(&0x2ffeu16.to_le_bytes());
        length as i32
    }
    unsafe extern "C" fn pack(subject: usize, message: usize, attach_length: u32, attach: usize, body: usize) -> i32 {
        let (subject, message) = (text(subject), text(message));
        with(|c| {
            c.packed = vec![attach_length as usize, attach, body];
            c.strings.extend([subject.clone(), message.clone()]);
        });
        let mut out = subject;
        out.push(7);
        out.extend(message);
        out.push(0);
        raw(body, out.len()).copy_from_slice(&out);
        out.len() as i32
    }
    unsafe extern "C" fn send(buffer: usize) -> i32 {
        note(324, 0);
        with(|c| c.sent.push(raw(buffer, BODY_AT + 26).to_vec()));
        9
    }
    unsafe extern "C" fn send_poll(_: usize) -> i32 { 1 }
    unsafe extern "C" fn receipt_sink(buffer: usize) -> i32 {
        with(|c| {
            c.receipts.push(raw(buffer, BODY_AT + 2).to_vec());
            if c.receipt_results.is_empty() { 0 } else { c.receipt_results.remove(0) }
        })
    }
    unsafe extern "C" fn remove_a(index: usize) -> i32 { note(188, index as u64); 0 }
    unsafe extern "C" fn remove_b(index: usize) -> i32 { note(186, index as u64); 0 }
    unsafe extern "C" fn put_friends() -> i32 { note(1034, 0); 4 }
    unsafe extern "C" fn put_friends_poll(ticket: usize) -> i32 {
        note(1035, ticket as u64);
        with(|c| if c.put_polls.is_empty() { 1 } else { c.put_polls.remove(0) })
    }
    unsafe extern "C" fn get_friends() -> i32 { note(163, 0); 0 }
    unsafe extern "C" fn pump() -> i32 {
        note(818, 0);
        if let Some(record) = with(|c| c.pending_push.take()) {
            mail_arrived(record.as_ptr() as usize);
        }
        0
    }
    unsafe extern "C" fn no_error() -> i32 { 0 }

    fn fake(core: Core) -> (Api, PathBuf) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-social-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        *CORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Core { dir: dir.clone(), ..core });
        let mut table = vec![0usize; 1560];
        for (slot, function) in [
            (838, connected as *const ()), (126, folder_path as *const ()), (329, list as *const ()), (330, list_poll as *const ()),
            (326, download as *const ()), (327, download_poll as *const ()), (284, inbox_path as *const ()),
            (286, outbox_path as *const ()), (287, sent_path as *const ()), (322, acknowledge as *const ()),
            (323, acknowledge_poll as *const ()), (1107, not_retryable as *const ()), (328, cancel_ticket as *const ()),
            (337, cancel_ticket as *const ()), (316, member_of as *const ()), (167, list_a as *const ()),
            (169, list_b as *const ()), (385, no_groups as *const ()), (1304, arrival_flag as *const ()),
            (1305, clear_arrival as *const ()), (190, current_handle as *const ()), (173, handle_record as *const ()),
            (342, own_id as *const ()), (270, build as *const ()), (272, pack as *const ()), (324, send as *const ()),
            (325, send_poll as *const ()), (333, receipt_sink as *const ()), (188, remove_a as *const ()),
            (186, remove_b as *const ()), (1034, put_friends as *const ()), (1035, put_friends_poll as *const ()),
            (163, get_friends as *const ()),
            (818, pump as *const ()), (1103, no_error as *const ()),
        ] {
            table[slot] = function as usize;
        }
        (Api::test_table(&table), dir)
    }
    fn listed(id: u32, new: bool, from: u64, handle: u8) -> [u8; RECORD_LEN] {
        let mut record = [0u8; RECORD_LEN];
        record[..8].copy_from_slice(&from.to_le_bytes());
        record[8..16].copy_from_slice(&0x9999u64.to_le_bytes());
        record[0x30..0x34].copy_from_slice(&id.to_le_bytes());
        record[0x34..0x38].copy_from_slice(&(1000 + id).to_le_bytes());
        record[0x38..0x3c].copy_from_slice(&9u32.to_le_bytes());
        record[0x3c] = handle;
        record[0x3d] = 1;
        record[0x3e..0x40].copy_from_slice(&(if new { 0x8000u16 } else { 0 }).to_le_bytes());
        record
    }
    fn entry(uid: u64, handle: u8, flags: u64) -> [u8; 0xb0] {
        let mut record = [0u8; 0xb0];
        record[..8].copy_from_slice(&uid.to_le_bytes());
        record[8..16].copy_from_slice(&flags.to_le_bytes());
        record[0x98] = 1;
        record[0x9c..0xa0].copy_from_slice(&(u32::from(handle) << 13).to_le_bytes());
        record[0xa0..0xa5].copy_from_slice(b"Alice");
        record
    }
    fn position(calls: &[(usize, u64)], call: (usize, u64)) -> usize {
        calls.iter().position(|c| *c == call).unwrap_or_else(|| panic!("missing {call:?}"))
    }

    #[test]
    fn listing_reads_200_records_at_0x50_and_saves_each_new_message_before_acknowledging_it() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core {
            listed: vec![listed(1, true, 0x41, 1), listed(2, false, 0x42, 1), listed(3, true, 0x43, 1), listed(4, true, 0x44, 1)],
            ..Default::default()
        });
        Service::default().social_entry(&api, &mut || true);
        let calls = calls();
        assert!(calls.contains(&(329, 200)));
        let downloads: Vec<_> = calls.iter().filter(|c| c.0 == 326).map(|c| c.1).collect();
        assert_eq!(downloads, [1, 3, 4], "0x50 stride; only header +0x3e bit 15");
        for id in [1, 3, 4] {
            assert!(position(&calls, (326, id)) < position(&calls, (327, id)));
            assert!(position(&calls, (327, id)) < position(&calls, (322, id)));
            assert!(position(&calls, (322, id)) < position(&calls, (323, id)));
            assert_eq!(std::fs::read(dir.join(format!("in-{id}.bin"))).unwrap(), b"Hi\x07There\0");
        }
        assert!(calls.iter().filter(|c| c.0 == 1322).all(|c| c.1 == 1), "saved before every 322");
        assert!(!calls.iter().any(|c| c.1 == 2 && matches!(c.0, 322 | 326)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_list_cadence_does_not_accelerate_mail_download_or_acknowledgement_frames() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { listed: vec![listed(5, true, 0x45, 1)], ..Default::default() });
        let mut first_transfer = None;
        let list_done = Service::default().social_entry_with_list_frame(&api, Duration::from_millis(1),
            &mut |interval| {
                crate::native_auth::count_owner_frame_for_test();
                note(if interval == Duration::from_millis(1) { 4000 } else { 4001 }, interval.as_millis() as u64);
                if interval != Duration::from_millis(1) { first_transfer.get_or_insert_with(crate::native_auth::owner_frames); }
                true
            });
        // The mail → presence pause counts frames from the list response (07c §5.4), not after the transfers.
        assert!(list_done < first_transfer.unwrap());
        let calls = calls();
        assert_eq!(calls.iter().filter(|c| c.0 == 4000).count(), 1, "only the list uses the fast callback");
        assert_eq!(calls.iter().filter(|c| c.0 == 4001).count(), 4, "download, save and acknowledgement retain ordinary frames");
        assert!(position(&calls, (4000, 1)) < position(&calls, (330, 7)));
        assert!(position(&calls, (330, 7)) < position(&calls, (326, 5)));
        assert!(position(&calls, (326, 5)) < position(&calls, (4001, 20)));
        assert!(position(&calls, (327, 5)) < position(&calls, (322, 5)));
        assert!(calls.iter().filter(|c| c.0 == 1322).all(|c| c.1 == 1), "stored before acknowledgement");
        assert_eq!(std::fs::read(dir.join("in-5.bin")).unwrap(), b"Hi\x07There\0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn play_during_the_mail_list_polls_it_to_its_end_and_starts_no_transfer_or_cancel() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { listed: vec![listed(5, true, 0x45, 1)], ..Default::default() });
        // Play arrives on the first frame, while the list ticket is in Poll (07c §6 S1).
        let mut play = None;
        Service::default().social_entry(&api, &mut || { play.get_or_insert_with(crate::native_auth::TestPlay::requested); true });
        assert!(play.is_some());
        drop(play);
        let calls = calls();
        assert!(position(&calls, (329, 200)) < position(&calls, (330, 7)), "polled to its end");
        assert!(!calls.iter().any(|c| matches!(c.0, 328 | 326 | 322)), "no cancel and no new transfer: {calls:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_user_check_lists_downloads_and_acknowledges_new_mail_but_an_inbox_reload_does_not() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { listed: vec![listed(5, true, 0x45, 1)], ..Default::default() });
        let mut service = Service::default();
        // Showing the Inbox only reads stored mail: no server request.
        service.execute_with(&api, NativeSocialRequest::Inbox { id: "1".repeat(32), folder: 0, page: 0 }, &mut || true);
        assert!(!calls().iter().any(|c| matches!(c.0, 329 | 326 | 322)));
        // The Viewer's check runs the list job and stores the new message first.
        service.execute_with(&api, NativeSocialRequest::Check { id: "2".repeat(32) }, &mut || true);
        let calls = calls();
        assert!(calls.contains(&(329, 200)));
        assert!(position(&calls, (326, 5)) < position(&calls, (322, 5)));
        assert_eq!(std::fs::read(dir.join("in-5.bin")).unwrap(), b"Hi\x07There\0");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_message_that_cannot_be_stored_is_never_acknowledged() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { listed: vec![listed(1, true, 0x41, 1)], unwritable: true, ..Default::default() });
        Service::default().social_entry(&api, &mut || true);
        let calls = calls();
        assert!(calls.contains(&(327, 1)));
        assert!(!calls.iter().any(|c| matches!(c.0, 322 | 323)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_mail_from_a_stranger_is_acknowledged_without_download() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { listed: vec![listed(1, true, 0x41, 2)], member: true, ..Default::default() });
        Service::default().social_entry(&api, &mut || true);
        let calls = calls();
        assert!(calls.contains(&(322, 1)) && calls.contains(&(323, 1)));
        assert!(!calls.iter().any(|c| matches!(c.0, 326 | 327)));
        assert!(!dir.join("in-1.bin").exists());
        // The same sender as a friend on that handle is downloaded.
        with(|c| { c.calls.clear(); c.friends = vec![entry(0x41, 2, 0)]; });
        Service::default().social_entry(&api, &mut || true);
        assert!(calls_has(326, 1));
        // A request (kind 1) from a stranger is downloaded too.
        let mut request = listed(5, true, 0x45, 2);
        request[0x3e..0x40].copy_from_slice(&(0x8000u16 | (1 << 7)).to_le_bytes());
        with(|c| { c.calls.clear(); c.friends.clear(); c.listed = vec![request]; });
        Service::default().social_entry(&api, &mut || true);
        assert!(calls_has(326, 5));
        std::fs::remove_dir_all(dir).unwrap();
    }
    fn calls_has(slot: usize, info: u64) -> bool {
        calls().contains(&(slot, info))
    }

    #[test]
    fn every_main_menu_entry_lists_mail_and_clears_an_unclaimed_arrival_first() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core::default());
        let mut service = Service::default();
        service.social_entry(&api, &mut || true);
        assert!(calls_has(329, 200), "the sign-in entry lists");
        with(|c| c.calls.clear());
        // Live stock capture: the entry after a game lists too, flagged or not.
        service.social_entry(&api, &mut || true);
        assert!(calls_has(329, 200), "the entry after a game lists");
        assert!(!calls_has(1305, 0), "nothing to clear without an arrival");
        with(|c| { c.calls.clear(); c.flagged = 1 });
        service.social_entry(&api, &mut || true);
        let calls = calls();
        assert!(position(&calls, (1305, 0)) < position(&calls, (329, 200)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_failures_follow_the_viewer_retry_rules() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { list_starts: vec![-515, -515, -256, 7], ..Default::default() });
        let mut service = Service::default();
        assert!(service.list(&api, &mut || true).is_none());
        assert!(service.list_retry.is_some(), "first failure retries after 30 s");
        service.list_retry = None;
        assert!(service.list(&api, &mut || true).is_none());
        assert!(service.list_retry.is_none(), "a second failure waits for the next check");
        assert!(service.list(&api, &mut || true).is_some(), "-256 re-issues next frame");
        assert_eq!(service.list_errors, 0);
        assert_eq!(calls().iter().filter(|c| c.0 == 329).count(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_push_is_saved_and_receipted_while_another_request_is_pending() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let body = b"Hi\x07There\0".to_vec();
        let mut record = listed(7, true, 0xaaaa, 3);
        record[0x4c..0x50].copy_from_slice(&(body.as_ptr() as u32).to_le_bytes());
        let (api, dir) = fake(Core {
            friends: vec![entry(0x77, 0, 0)],
            put_polls: vec![0, 0, 1],
            receipt_results: vec![-2, 0],
            pending_push: Some(record),
            ..Default::default()
        });
        set_listening(true);
        let mut delivery = PushDelivery::default();
        let mut frames = 0;
        let result = remove_entry(&api, false, 0, 0x77, &mut || {
            frames += 1;
            let running = crate::native_auth::owner_frame(&api, &mut delivery);
            assert_eq!(std::fs::read(dir.join("in-7.bin")).unwrap(), body,
                "incoming storage must not wait for the pending friend commit");
            assert_eq!(with(|c| c.receipts.len()), frames.min(2),
                "a rejected receipt retries on the next owner frame");
            running
        });
        assert_eq!(result, 1);
        assert!(frames >= 3);
        assert!(position(&calls(), (818, 0)) < position(&calls(), (1035, 4)));
        assert!(delivery.receipts.is_empty());
        assert_eq!(mail().drops.load(Ordering::Acquire), 0);
        set_listening(false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn normal_send_is_kind_0_through_270_and_272_with_the_viewer_fixup() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let viewer = (1000u64 << 33) | (1 << 13) | (1 << 11) | (1 << 12);
        let (api, dir) = fake(Core { friends: vec![entry(0x2222, 7, viewer)], ..Default::default() });
        let sent = send_mail(&api, false, 0, 0x2222, "A long subject line", "Hello", &mut || true);
        assert_eq!(sent, Ok((1, None)), "fixture {}: {}", dir.display(), std::io::Error::last_os_error());
        let (built, built_at, strings, packed, header) = with(|c| (c.built.clone(), c.built_at, c.strings.clone(), c.packed.clone(), c.sent[0].clone()));
        // kind, own lo/hi, to lo/hi, sender handle (190), recipient handle
        // (+0x9c bits 13..18), application, attachment type, body length.
        assert_eq!(built, [0, 2, 1, 0x2222, 0, 5, 7, 1000, 0, 26]);
        assert_eq!(strings, [b"Me5".to_vec(), b"A long subject ".to_vec(), b"A long subject line".to_vec(), b"Hello".to_vec()]);
        assert_eq!(packed, [0, 0, built_at + BODY_AT]);
        assert_eq!(u16_at(&header, 0x42), 0x10fe, "bits 8-11/13 cleared, bit 12 = subject cut");
        assert_eq!(u16_at(&header, 0x3e), 0xc000, "bits 12-13 cleared, immediate delivery");
        assert_eq!(&header[BODY_AT..BODY_AT + 26], b"A long subject line\x07Hello\0");
        assert!(dir.join("sent-0.bin").exists() && !dir.join("out-0.bin").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn after_play_a_failed_mail_send_starts_no_second_324() {
        use std::sync::atomic::{AtomicI32, AtomicU32};
        static STARTS: AtomicU32 = AtomicU32::new(0);
        static RESULT: AtomicI32 = AtomicI32::new(0);
        unsafe extern "C" fn start(_: usize) -> i32 { STARTS.fetch_add(1, Ordering::SeqCst); RESULT.load(Ordering::SeqCst) }
        unsafe extern "C" fn retryable(_: usize) -> i32 { 1 }
        let mut table = vec![0usize; 1560];
        table[324] = start as *const () as usize;
        table[1107] = retryable as *const () as usize;
        let api = Api::test_table(&table);
        let _play = crate::native_auth::TestPlay::requested();
        // A retryable failure and a full request table both stand once Play is requested.
        for code in [-2, -256] {
            STARTS.store(0, Ordering::SeqCst);
            RESULT.store(code, Ordering::SeqCst);
            let mut buffer = [0u64; 8];
            assert_eq!(send_job(&api, &mut buffer, &mut || true), code);
            assert_eq!(STARTS.load(Ordering::SeqCst), 1, "result {code}: one 324 only");
        }
    }

    #[test]
    fn receipts_still_unaccepted_at_the_game_handoff_are_never_sent_later() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { receipt_results: vec![-256], ..Default::default() });
        let body = b"Hi\x07There\0".to_vec();
        let mut record = [0u64; RECORD_LEN / 8];
        {
            let bytes = bytes_of_mut(&mut record);
            bytes[0x10..0x15].copy_from_slice(b"Alice");
            bytes[0x30] = 7;
            bytes[0x38] = 9;
            bytes[0x3e..0x40].copy_from_slice(&0x8000u16.to_le_bytes());
            bytes[0x40..0x42].copy_from_slice(&1000u16.to_le_bytes());
            bytes[0x4c..0x50].copy_from_slice(&(body.as_ptr() as u32).to_le_bytes());
        }
        set_listening(true);
        unsafe { mail_arrived(record.as_ptr() as usize) };
        let mut delivery = PushDelivery::default();
        delivery.flush_pushed(&api, true); // 333 refused (-256): kept
        assert_eq!(with(|c| c.receipts.len()), 1);
        assert_eq!(delivery.abandon_receipts(), 1);
        delivery.flush_pushed(&api, true); // the restarted Viewer after the game
        assert_eq!(with(|c| c.receipts.len()), 1, "no 333 for a pre-game receipt");
        set_listening(false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_block_list_is_never_mailed() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { blocked: vec![entry(0x88, 1, 0)], ..Default::default() });
        assert_eq!(send_mail(&api, true, 0, 0x88, "s", "m", &mut || true), Err(BLOCKED));
        assert!(with(|c| c.built.is_empty() && c.sent.is_empty()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn immediate_delivery_follows_the_viewer_cache_presence_rules() {
        let viewer = (1000u64 << 33) | (1 << 13) | (1 << 11) | (1 << 12);
        assert!(immediate_delivery(&entry(1, 0, viewer), 0x180));
        assert!(!immediate_delivery(&entry(1, 0, viewer), 0x181));
        assert!(!immediate_delivery(&entry(1, 0, viewer & !(1 << 12)), 10));
        assert!(!immediate_delivery(&entry(1, 0, viewer & !(1 << 11)), 10), "mode 1 needs online");
        assert!(immediate_delivery(&entry(1, 0, (1001u64 << 33) | (2 << 13) | (1 << 12)), 10));
        assert!(!immediate_delivery(&entry(1, 0, (1000u64 << 33) | (4 << 13) | (1 << 11) | (1 << 12)), 10));
        let game = (1u64 << 33) | (1 << 13) | (1 << 11) | (1 << 12);
        assert!(!immediate_delivery(&entry(1, 0, game), 10));
        let mut linked = entry(1, 0, game | (1 << 16) | (2 << 17));
        linked[0x18 + 32] = 1;
        linked[0x1a + 32..0x1c + 32].copy_from_slice(&1000u16.to_le_bytes());
        assert!(immediate_delivery(&linked, 10));
    }

    #[test]
    fn pushed_messages_are_saved_and_answered_with_a_333_receipt() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core { receipt_results: vec![-2, 0], ..Default::default() });
        set_listening(true);
        let body = b"Hi\x07There\0".to_vec();
        let mut record = [0u64; RECORD_LEN / 8];
        {
            let bytes = bytes_of_mut(&mut record);
            bytes[..8].copy_from_slice(&0xaaaau64.to_le_bytes());
            bytes[8..16].copy_from_slice(&0xbbbbu64.to_le_bytes());
            bytes[0x10..0x15].copy_from_slice(b"Alice");
            bytes[0x20..0x27].copy_from_slice(b"Subject");
            bytes[0x30] = 7;
            bytes[0x38] = 9;
            bytes[0x3c] = 3;
            bytes[0x3d] = 4;
            bytes[0x3e..0x40].copy_from_slice(&0x8000u16.to_le_bytes());
            bytes[0x40..0x42].copy_from_slice(&1000u16.to_le_bytes());
            bytes[0x4c..0x50].copy_from_slice(&(body.as_ptr() as u32).to_le_bytes());
        }
        unsafe { mail_arrived(record.as_ptr() as usize) };
        // A receipt for one of our own messages is not answered or stored.
        let mut own = record;
        bytes_of_mut(&mut own)[0x3e..0x40].copy_from_slice(&0x9000u16.to_le_bytes());
        unsafe { mail_arrived(own.as_ptr() as usize) };
        assert_eq!(mail().pushed.lock().unwrap().len(), 1);
        // Leaving the Viewer keeps what already arrived for the next idle frame.
        set_listening(false);
        unsafe { mail_arrived(record.as_ptr() as usize) };
        assert_eq!(mail().pushed.lock().unwrap().len(), 1);
        let mut delivery = PushDelivery::default();
        delivery.flush_pushed(&api, true);
        assert_eq!(std::fs::read(dir.join("in-7.bin")).unwrap(), body);
        let receipt = with(|c| c.receipts[0].clone());
        assert_eq!(u64_at(&receipt, 0), 0xbbbb);
        assert_eq!(u64_at(&receipt, 8), 0xaaaa);
        assert_eq!(&receipt[0x10..0x14], b"Me4\0", "my handle's name");
        assert_eq!(&receipt[0x20..0x27], b"Subject");
        assert_eq!((receipt[0x30], u32_at(&receipt, 0x38)), (7, 2));
        assert_eq!((receipt[0x3c], receipt[0x3d]), (4, 3));
        assert_eq!(u16_at(&receipt, 0x3e), 0xd000);
        assert_eq!(u16_at(&receipt, 0x40), 1000);
        assert_eq!(&receipt[BODY_AT..BODY_AT + 2], b"\x07\0");
        delivery.flush_pushed(&api, true); // -2 then accepted
        delivery.flush_pushed(&api, true);
        assert_eq!(with(|c| c.receipts.len()), 2);
        set_listening(false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_saves_retry_without_a_second_receipt_and_sign_out_stores_without_answering() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (api, dir) = fake(Core::default());
        let body = b"Hi\x07There\0".to_vec();
        let mut record = [0u64; RECORD_LEN / 8];
        {
            let bytes = bytes_of_mut(&mut record);
            bytes[0x10..0x15].copy_from_slice(b"Alice");
            bytes[0x30] = 7;
            bytes[0x38] = 9;
            bytes[0x3e..0x40].copy_from_slice(&0x8000u16.to_le_bytes());
            bytes[0x40..0x42].copy_from_slice(&1000u16.to_le_bytes());
            bytes[0x4c..0x50].copy_from_slice(&(body.as_ptr() as u32).to_le_bytes());
        }
        set_listening(true);
        unsafe { mail_arrived(record.as_ptr() as usize) };
        // A directory where the message file goes makes the local save fail.
        std::fs::create_dir_all(dir.join("in-7.bin")).unwrap();
        let mut delivery = PushDelivery::default();
        delivery.flush_pushed(&api, true);
        assert_eq!(with(|c| c.receipts.len()), 1, "stock receipts before saving");
        assert_eq!(delivery.unsaved.len(), 1, "kept for another save attempt");
        std::fs::remove_dir_all(dir.join("in-7.bin")).unwrap();
        delivery.flush_pushed(&api, true);
        assert_eq!(std::fs::read(dir.join("in-7.bin")).unwrap(), body);
        assert!(delivery.unsaved.is_empty());
        assert_eq!(with(|c| c.receipts.len()), 1, "a retried save never answers twice");

        // Signing out after a loss stores the last push without answering it.
        std::fs::remove_file(dir.join("in-7.bin")).unwrap();
        unsafe { mail_arrived(record.as_ptr() as usize) };
        delivery.flush_pushed(&api, false);
        assert_eq!(std::fs::read(dir.join("in-7.bin")).unwrap(), body);
        assert_eq!(with(|c| c.receipts.len()), 1);
        // A push the receiver cannot copy is counted for the owner's log.
        bytes_of_mut(&mut record)[0x4c..0x50].copy_from_slice(&0u32.to_le_bytes());
        unsafe { mail_arrived(record.as_ptr() as usize) };
        assert_eq!(mail().drops.load(Ordering::Acquire), 1);
        delivery.flush_pushed(&api, false);
        assert_eq!(mail().drops.load(Ordering::Acquire), 0);
        set_listening(false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removal_marks_locally_and_commits_with_put_friend_list_only() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let friends = vec![entry(0x70, 0, 0), entry(0x71, 0, 0), entry(0x72, 0, 0), entry(0x77, 0, 0)];
        let (api, dir) = fake(Core { friends, blocked: vec![entry(0x88, 1, 0)], put_polls: vec![0, 0, 1], ..Default::default() });
        assert_eq!(remove_entry(&api, false, 3, 0x77, &mut || true), 1);
        assert_eq!(calls(), [(188, 3), (1034, 0), (1035, 4), (1035, 4), (1035, 4)]);
        with(|c| c.calls.clear());
        assert_eq!(remove_entry(&api, true, 0, 0x88, &mut || true), 1, "unblock");
        assert_eq!(calls(), [(186, 0), (1034, 0), (1035, 4)]);
        with(|c| c.calls.clear());
        assert_eq!(remove_entry(&api, false, 3, 0x78, &mut || true), -7189);
        assert!(calls().is_empty(), "no edit, commit or Get Friend List (163)");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subjects_are_cut_at_character_boundaries() {
        assert_eq!(subject15(b"exactly fifteen"), (&b"exactly fifteen"[..], false));
        assert_eq!(subject15(b"sixteen bytes!!!"), (&b"sixteen bytes!!"[..], true));
        let wide = [0x82u8, 0xa0].repeat(8);
        assert_eq!(subject15(&wide), (&wide[..14], true));
        assert_eq!(subject15(b"fourteen bytes\r\n"), (&b"fourteen bytes"[..], true));
    }

    #[test]
    fn member_ids_need_the_viewer_checksum() {
        assert!(member_id(&valid_id()));
        let mut wrong = valid_id();
        wrong[0] = if wrong[0] == b'Z' { b'A' } else { wrong[0] + 1 };
        assert!(!member_id(&wrong));
        assert!(!member_id(b"abcd1234"));
    }

    static RECEIVER: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn set_receiver(receiver: usize) {
        RECEIVER.store(receiver, Ordering::SeqCst);
    }
    #[test]
    fn idle_notifications_coalesce_and_detach_before_game_handoff() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut table = vec![0; 1560];
        table[331] = set_receiver as *const () as usize;
        let receiver = MailNotifications::start(&Api::test_table(&table));
        let pointer = RECEIVER.load(Ordering::SeqCst);
        assert_ne!(pointer, 0);
        let callback: unsafe extern "C" fn(usize) = unsafe { std::mem::transmute(pointer) };
        let record = [0u64; RECORD_LEN / 8];
        unsafe { callback(record.as_ptr() as usize); callback(record.as_ptr() as usize); }
        assert!(receiver.take_changed());
        assert!(!receiver.take_changed());
        assert!(mail().pushed.lock().unwrap().is_empty(), "no body: nothing to store");
        drop(receiver);
        assert_eq!(RECEIVER.load(Ordering::SeqCst), 0);
        unsafe { callback(record.as_ptr() as usize); }
        assert!(!mail().changed.load(Ordering::Acquire));
    }
    #[test]
    fn read_headers_are_owned_by_this_native_inbox_and_expire_with_it() {
        let mut service = Service::default();
        let mut first = [0u8; HEADER_COUNT * HEADER_LEN];
        first[16..22].copy_from_slice(b"Alice\0");
        let header: [u8; HEADER_LEN] = first[..HEADER_LEN].try_into().unwrap();
        assert!(!service.contains(&header));
        service.remember(&first);
        assert!(service.contains(&header));
        let mut forged = header;
        forged[0x38] = 255;
        assert!(!service.contains(&forged));
        service.remember(&[0; HEADER_COUNT * HEADER_LEN]);
        assert!(!service.contains(&header));
    }
    #[test]
    fn encoding_refuses_substitutions_controls_and_native_byte_overflow() {
        assert_eq!(&*cp932("あ", 2).unwrap(), &[0x82, 0xa0, 0]);
        assert!(cp932("あ", 1).is_err());
        assert!(cp932("🙂", 127).is_err());
        assert!(cp932("a\x07b", 127).is_err());
        assert!(cp932("a\0b", 127).is_err());
        assert_eq!(&*cp932("", 127).unwrap(), &[0]);
    }
}
