//! The PlayOnline portrait: the sign-in probe and the change the Viewer's
//! handle editor makes. Where the handle record keeps the portrait is
//! UNVERIFIED (`wire::handle_portrait`); until `wire::PORTRAIT_FIELD_CONFIRMED`
//! a change is refused before any core call, so nothing reaches the server.
use crate::{
    events, log,
    native_auth::Api,
    native_jobs::{self, Policy, Request},
    wire,
};
use std::fmt::Write;

/// The answer to a change while the field is unconfirmed (no core call made).
pub(crate) const NOT_CONFIRMED: i32 = -5120;
/// No selected handle, or its record is not in use.
const NO_HANDLE: i32 = -7186;
const HANDLES: i32 = 64;

/// The selected handle (190) and its record (173), on the owner thread.
fn active_handle(api: &Api) -> Option<(usize, [u8; 0x28])> {
    let index = api.zero(190);
    if !(0..HANDLES).contains(&index) {
        return None;
    }
    let mut words = [0u64; 5];
    let code = api.call(173, &[index as usize, words.as_mut_ptr() as usize]);
    let mut record = [0u8; 0x28];
    for (out, word) in record.chunks_exact_mut(8).zip(words) {
        out.copy_from_slice(&word.to_le_bytes());
    }
    (code >= 0 && record[0] & 1 != 0).then_some((index as usize, record))
}

/// Writes `record` over handle `index` in polcore's local list (174).
fn edit(api: &Api, index: usize, record: &[u8; 0x28]) -> i32 {
    let mut words = [0u64; 5];
    for (word, bytes) in words.iter_mut().zip(record.chunks_exact(8)) {
        *word = u64::from_le_bytes(bytes.try_into().unwrap());
    }
    api.call(174, &[index, words.as_mut_ptr() as usize])
}

fn send_own(portrait: u16) {
    events::send(format!("{{\"type\":\"native_own_portrait\",\"portrait\":{portrait}}}\n"));
}

fn qword(record: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(record[at..at + 8].try_into().unwrap())
}

/// At sign-in, once the handle list has loaded: both candidate fields of the
/// selected handle, as numbers only (the name at +8..+0x18 is never logged),
/// then the own-portrait line for the launcher.
pub(crate) fn probe_own(api: &Api) {
    let Some((index, record)) = active_handle(api) else {
        log!("portrait probe: no active handle");
        return;
    };
    let (field, low) = wire::handle_portrait_candidates(&record);
    log!(
        "portrait probe: handle={index} field13={field} dword1c={low} q0=0x{:016x} q18=0x{:016x}",
        qword(&record, 0),
        qword(&record, 0x18)
    );
    send_own(wire::handle_portrait(&record));
}

/// The first friend snapshot of a session: `slot=+0x9c&0x1fff/raw +0x9c` for
/// every entry in use (slots 200.. are list B). Numbers only.
pub(crate) fn probe_friends(blob: &[u8]) -> String {
    let mut line = String::from("portrait probe: friends");
    for (slot, record) in blob[8..].chunks_exact(wire::FRIEND_RECORD_LEN).enumerate() {
        if record[0x98] & 1 == 0 {
            continue;
        }
        let raw = u32::from_le_bytes(record[0x9c..0xa0].try_into().unwrap());
        let _ = write!(line, " {slot}={}/0x{raw:x}", wire::friend_portrait(record));
    }
    line
}

/// Change the selected handle's portrait as the Viewer's handle editor does:
/// copy its record, replace only the portrait bits, edit (174), then Put
/// Handle List (1036/1037). Sent only when the user picks a new portrait.
pub(crate) fn set_own(api: &Api, portrait: u16, frame: &mut dyn FnMut() -> bool) -> i32 {
    set_own_with(api, portrait, wire::PORTRAIT_FIELD_CONFIRMED, frame)
}

fn set_own_with(api: &Api, portrait: u16, confirmed: bool, frame: &mut dyn FnMut() -> bool) -> i32 {
    if !confirmed {
        log!("native POL portrait change refused: the portrait field is not confirmed");
        return NOT_CONFIRMED;
    }
    if portrait > wire::PORTRAIT_MASK {
        return NOT_CONFIRMED;
    }
    let Some((index, record)) = active_handle(api) else { return NO_HANDLE };
    if wire::handle_portrait(&record) == portrait {
        return 1;
    }
    let mut edited = record;
    wire::set_handle_portrait(&mut edited, portrait);
    let code = edit(api, index, &edited);
    if code < 0 {
        log!("native POL portrait edit result {code}");
        return code;
    }
    let code = native_jobs::run(
        api,
        Request { name: "portrait", start: &mut || api.zero(1036), poll: 1037, policy: Policy::Normal },
        frame,
    );
    log!("native POL portrait change result {code}");
    if native_jobs::succeeded(code) {
        // Read the record back (173): the change counts only if it now holds the chosen portrait.
        // The launcher shows what it holds either way.
        let now = active_handle(api).map(|(_, now)| wire::handle_portrait(&now));
        if let Some(now) = now {
            send_own(now);
        }
        if now != Some(portrait) {
            log!("native POL portrait read back {now:?}, not the chosen portrait");
            return wire::PORTRAIT_NOT_SAVED;
        }
    } else {
        // The server did not take it: put the record back as read, so a later Put Handle List
        // from another path never uploads the unsent portrait.
        let restored = edit(api, index, &record);
        if restored < 0 {
            log!("native POL portrait restore result {restored}");
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Core {
        handle: [u8; 0x28],
        edited: Option<(usize, [u8; 0x28])>,
        calls: Vec<usize>,
        /// What Put Handle List's poll (1037) answers.
        poll: i32,
        /// The record the core holds once Put Handle List is done, when not the edited one.
        after_put: Option<[u8; 0x28]>,
    }
    thread_local! { static CORE: RefCell<Core> = const { RefCell::new(Core { handle: [0; 0x28], edited: None, calls: Vec::new(), poll: 1, after_put: None }) }; }

    unsafe extern "C" fn selected() -> i32 { CORE.with(|c| c.borrow_mut().calls.push(190)); 3 }
    unsafe extern "C" fn read(index: usize, out: usize) -> i32 {
        CORE.with(|c| {
            let mut c = c.borrow_mut();
            c.calls.push(173);
            assert_eq!(index, 3);
            std::ptr::copy_nonoverlapping(c.handle.as_ptr(), out as *mut u8, 0x28);
        });
        0
    }
    unsafe extern "C" fn edit(index: usize, record: usize) -> i32 {
        CORE.with(|c| {
            let mut c = c.borrow_mut();
            c.calls.push(174);
            let mut copy = [0u8; 0x28];
            std::ptr::copy_nonoverlapping(record as *const u8, copy.as_mut_ptr(), 0x28);
            c.handle = copy;
            c.edited = Some((index, copy));
        });
        0
    }
    unsafe extern "C" fn put() -> i32 { CORE.with(|c| c.borrow_mut().calls.push(1036)); 1 }
    unsafe extern "C" fn put_poll(_: usize) -> i32 {
        CORE.with(|c| {
            let mut c = c.borrow_mut();
            c.calls.push(1037);
            if let Some(record) = c.after_put { c.handle = record; }
            c.poll
        })
    }
    unsafe extern "C" fn no_error() -> i32 { 0 }
    unsafe extern "C" fn member() -> i32 { 3 }

    fn fixture() -> Api {
        let mut handle = [0u8; 0x28];
        handle[0] = 1;
        handle[1..4].copy_from_slice(&[0x5a, 0xa5, 0x0f]);
        handle[8..13].copy_from_slice(b"Owner");
        handle[0x18..0x20].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        handle[0x20..0x28].fill(0x03);
        wire::set_handle_portrait(&mut handle, 42);
        handle[5] |= 0xe0; // bits above the 13-bit field
        CORE.with(|c| *c.borrow_mut() = Core { handle, edited: None, calls: Vec::new(), poll: 1, after_put: None });
        let mut table = vec![0usize; 1560];
        table[190] = selected as *const () as usize;
        table[173] = read as *const () as usize;
        table[174] = edit as *const () as usize;
        table[1036] = put as *const () as usize;
        table[1037] = put_poll as *const () as usize;
        table[1103] = no_error as *const () as usize;
        table[1107] = no_error as *const () as usize; // not retryable
        table[838] = member as *const () as usize;
        Api::test_table(&table)
    }

    #[test]
    fn nothing_reaches_the_core_while_the_field_is_unconfirmed() {
        let api = fixture();
        assert!(!wire::PORTRAIT_FIELD_CONFIRMED, "flip only after the live test");
        assert_eq!(set_own(&api, 7, &mut || true), NOT_CONFIRMED);
        assert_eq!(set_own_with(&api, 7, false, &mut || true), NOT_CONFIRMED);
        CORE.with(|c| {
            let c = c.borrow();
            assert!(c.calls.is_empty(), "no read, edit or Put Handle List: {:?}", c.calls);
            assert!(c.edited.is_none());
        });
    }

    #[test]
    fn a_confirmed_change_copies_the_record_and_replaces_only_the_portrait() {
        let api = fixture();
        let before = CORE.with(|c| c.borrow().handle);
        assert_eq!(set_own_with(&api, 300, true, &mut || true), 1);
        CORE.with(|c| {
            let c = c.borrow();
            let (index, edited) = c.edited.unwrap();
            assert_eq!(index, 3);
            assert_eq!(wire::handle_portrait(&edited), 300);
            let mut expected = before;
            wire::set_handle_portrait(&mut expected, 300);
            assert_eq!(edited, expected, "every other byte is the record as read");
            assert_eq!(c.calls, [190, 173, 174, 1036, 1037, 190, 173]);
        });
    }

    #[test]
    fn a_failed_put_handle_list_puts_the_record_back() {
        let api = fixture();
        let before = CORE.with(|c| c.borrow().handle);
        CORE.with(|c| c.borrow_mut().poll = -7);
        assert_eq!(set_own_with(&api, 300, true, &mut || true), -7);
        CORE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.handle, before, "the local list holds the record as read");
            assert_eq!(c.calls, [190, 173, 174, 1036, 1037, 174]);
        });
    }

    #[test]
    fn a_change_the_record_does_not_hold_afterwards_is_not_saved() {
        let api = fixture();
        let before = CORE.with(|c| c.borrow().handle);
        // Put Handle List succeeds, but the record read back still holds the old portrait.
        CORE.with(|c| c.borrow_mut().after_put = Some(before));
        assert_eq!(set_own_with(&api, 300, true, &mut || true), wire::PORTRAIT_NOT_SAVED);
        CORE.with(|c| {
            let c = c.borrow();
            assert_eq!(c.calls, [190, 173, 174, 1036, 1037, 190, 173], "read back once, nothing else written");
            assert_eq!(wire::handle_portrait(&c.handle), 42, "the record is left as the core holds it");
        });
    }

    #[test]
    fn the_same_portrait_or_an_out_of_range_id_sends_nothing() {
        let api = fixture();
        assert_eq!(set_own_with(&api, 42, true, &mut || true), 1);
        assert_eq!(set_own_with(&api, 0x2000, true, &mut || true), NOT_CONFIRMED);
        CORE.with(|c| assert!(!c.borrow().calls.contains(&174)));
    }

    #[test]
    fn the_friend_probe_logs_numbers_only() {
        let mut blob = vec![0u8; wire::FRIENDS_LEN];
        blob[..8].copy_from_slice(wire::FRIENDS_MAGIC);
        let at = |slot: usize| 8 + slot * wire::FRIEND_RECORD_LEN;
        for (slot, value) in [(0usize, (2u32 << 13) | 15), (201, 9)] {
            let record = &mut blob[at(slot)..at(slot) + wire::FRIEND_RECORD_LEN];
            record[0x98] = 1;
            record[0x9c..0xa0].copy_from_slice(&value.to_le_bytes());
            record[0xa0..0xa5].copy_from_slice(b"Alice");
        }
        assert_eq!(probe_friends(&blob), "portrait probe: friends 0=15/0x400f 201=9/0x9");
    }
}
