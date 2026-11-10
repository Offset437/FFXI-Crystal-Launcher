//! First-time PlayOnline handle setup through the installed core. A public
//! handle name must be supplied by the player; existing links are preserved.
use crate::{
    native_auth::Api,
    native_jobs::{self, Policy, Request},
};
use zeroize::Zeroizing;

const SLOTS: usize = 64;
const PER_HANDLE: usize = 8;
const SETUP_REQUIRED: &str = "This PlayOnline account has no handle yet. In the account editor, enter a public PlayOnline handle name, then sign in again.";

#[derive(Clone)]
struct Handle {
    index: usize,
    name: Vec<u8>,
    used: usize,
}

fn handles(api: &Api) -> Result<Vec<Handle>, String> {
    let mut handles = Vec::new();
    for index in 0..SLOTS {
        let mut record = Zeroizing::new([0u64; 5]);
        let code = api.call(173, &[index, record.as_mut_ptr() as usize]);
        if code < 0 {
            return Err(format!("Could not read PlayOnline handles ({code})."));
        }
        let bytes = unsafe { std::slice::from_raw_parts(record.as_ptr().cast::<u8>(), 0x28) };
        if bytes[0] & 1 == 0 {
            continue;
        }
        let end = bytes[8..24].iter().position(|&b| b == 0).unwrap_or(16);
        handles.push(Handle {
            index,
            name: bytes[8..8 + end].to_vec(),
            used: bytes[32..40].iter().filter(|&&b| b & 1 != 0).count(),
        });
    }
    Ok(handles)
}

fn unlinked_characters(api: &Api) -> Result<Vec<usize>, String> {
    let mut characters = Vec::new();
    for index in 0..SLOTS {
        let mut record = Zeroizing::new([0u64; 13]);
        let code = api.call(177, &[index, record.as_mut_ptr() as usize]);
        if code < 0 {
            return Err(format!(
                "Could not read PlayOnline character links ({code})."
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(record.as_ptr().cast::<u8>(), 0x68) };
        if bytes[0] & 1 != 0
            && u16::from_le_bytes([bytes[2], bytes[3]]) == 1
            && record[2] & 0x8000 == 0
        {
            characters.push(index);
        }
    }
    Ok(characters)
}

fn names(input: &str) -> Result<Vec<Zeroizing<Vec<u8>>>, String> {
    if input.chars().any(char::is_control) {
        return Err("PlayOnline handle names cannot contain control characters.".into());
    }
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut names: Vec<Zeroizing<Vec<u8>>> = Vec::new();
    for name in input.split(',') {
        let name = name.trim();
        if name.is_empty() || names.len() == 8 {
            return Err(
                "Enter up to eight public PlayOnline handle names, separated by commas.".into(),
            );
        }
        let encoded = crate::native_social::cp932(name, 15).map_err(|_| {
            "Each handle name must fit PlayOnline's 15-byte limit and use supported characters."
                .to_owned()
        })?;
        if names.iter().any(|n| **n == *encoded) {
            return Err("PlayOnline handle names must be distinct.".into());
        }
        names.push(encoded);
    }
    Ok(names)
}

/// A cp932 handle name without its trailing NUL.
fn bare(name: &[u8]) -> &[u8] { &name[..name.len() - 1] }

fn commit(
    api: &Api,
    name: &'static str,
    start: usize,
    poll: usize,
    frame: &mut dyn FnMut() -> bool,
) -> Result<(), String> {
    let code = native_jobs::run(
        api,
        Request {
            name,
            start: &mut || api.zero(start),
            poll,
            policy: Policy::Normal,
        },
        frame,
    );
    if crate::native_jobs::succeeded(code) {
        Ok(())
    } else {
        Err(format!(
            "PlayOnline handle setup could not be saved ({code}). Sign in again to retry."
        ))
    }
}

fn select_handle(
    api: &Api,
    handle: Option<&Handle>,
    frame: &mut dyn FnMut() -> bool,
) -> Result<(), String> {
    if api.zero(190) >= 0 {
        return Ok(());
    }
    let handle = handle.ok_or("No matching PlayOnline handle was found. Enter an existing handle name in the account editor and sign in again.")?;
    let code = crate::native_auth::status_request(
        api,
        "setup-select-handle",
        [handle.index as i32, -2, 1, -2, 0, -2],
        frame,
    );
    if crate::native_jobs::succeeded(code) {
        Ok(())
    } else {
        Err(format!(
            "PlayOnline could not select the new handle ({code})."
        ))
    }
}

/// An empty setting never changes an initialized account. Supplying names is
/// explicit permission to create missing named handles and link unregistered
/// FFXI characters to them. Reads and changes stay on the core's owner thread.
pub(crate) fn ensure(
    api: &Api,
    input: &str,
    frame: &mut dyn FnMut() -> bool,
) -> Result<(), String> {
    let existing = handles(api)?;
    if input.trim().is_empty() {
        return if existing.is_empty() {
            Err(SETUP_REQUIRED.into())
        } else {
            Ok(())
        };
    }
    let names = names(input)?;
    let unlinked = unlinked_characters(api)?;
    if !existing.is_empty() && unlinked.is_empty() {
        // A prior attempt may have saved the characters before its handle
        // commit or selection failed. The native guard sends only dirty data.
        commit(api, "setup-handle-links", 1036, 1037, frame)?;
        let target = names
            .iter()
            .find_map(|name| existing.iter().find(|h| h.name == bare(name)));
        return select_handle(api, target, frame);
    }

    // Check capacity before changing even the core's local cache. No existing
    // character is moved, and other games' links consume capacity normally.
    let capacity: usize = names
        .iter()
        .map(|name| {
            existing
                .iter()
                .find(|h| h.name == bare(name))
                .map_or(PER_HANDLE, |h| PER_HANDLE.saturating_sub(h.used))
        })
        .sum();
    if unlinked.len() > capacity {
        return Err(format!("These handle names have room for {capacity} characters, but {} need registration. Add another public handle name in the account editor (eight characters per handle).", unlinked.len()));
    }
    let needed = unlinked.len().max(usize::from(existing.is_empty()));
    let mut available = 0;
    let mut targets = Vec::new();
    let mut created = 0;
    for name in names {
        if available >= needed {
            break;
        }
        if let Some(handle) = existing.iter().find(|h| h.name == bare(&name)) {
            available += PER_HANDLE.saturating_sub(handle.used);
            targets.push(handle.clone());
        } else {
            // app's create-handle action: allocate a free native slot, kind 1.
            let index = api.call(197, &[usize::MAX, 1, name.as_ptr() as usize]);
            if !(0..SLOTS as i32).contains(&index) {
                return Err(format!("PlayOnline could not create the public handle ({index}). Check its name and retry."));
            }
            targets.push(Handle {
                index: index as usize,
                name: bare(&name).to_vec(),
                used: 0,
            });
            available += PER_HANDLE;
            created += 1;
        }
    }
    if created != 0 {
        commit(api, "setup-handles", 1036, 1037, frame)?;
    }
    let confirmed = handles(api)?;
    if targets.iter().any(|target| {
        !confirmed
            .iter()
            .any(|h| h.index == target.index && h.name == target.name)
    }) {
        return Err(
            "The PlayOnline handle list changed during setup. Sign in again to retry.".into(),
        );
    }

    let mut assigned = Vec::new();
    let mut character = unlinked.into_iter();
    for target in &targets {
        for _ in target.used..PER_HANDLE {
            let Some(index) = character.next() else { break };
            let code = api.call(183, &[index, target.index]);
            if code < 0 {
                return Err(format!("PlayOnline could not register a character to its handle ({code}). Sign in again to retry."));
            }
            assigned.push((index, target.index));
        }
    }
    if !assigned.is_empty() {
        commit(api, "setup-character-links", 1038, 1039, frame)?;
        // The Viewer's PutChrList completion queues PutHandleList as well.
        // Both sides of the registration must survive a fresh sign-in.
        commit(api, "setup-handle-links", 1036, 1037, frame)?;
        for &(index, handle) in &assigned {
            let mut record = Zeroizing::new([0u64; 13]);
            let code = api.call(177, &[index, record.as_mut_ptr() as usize]);
            if code < 0 || record[2] & 0x8000 == 0 || (record[2] >> 16) & 0x3f != handle as u64 {
                return Err("PlayOnline did not confirm the character registration. Sign in again to retry.".into());
            }
        }
    }
    select_handle(api, targets.first(), frame)?;
    crate::log!(
        "native POL handle setup completed: {created} new handle(s), {} character link(s)",
        assigned.len()
    );
    Ok(())
}

/// Exercises the real installed core's local record ABI in a fresh test
/// process. No member, connection, request driver, game, or saved file is used.
#[cfg(test)]
pub(crate) unsafe fn smoke_local(table: &[usize], base: usize) {
    let api = Api::test_table(table);
    assert_eq!(api.zero(838), 0, "offline core only");
    assert!(handles(&api).unwrap().is_empty());
    // The character cache below was located in the 1.18.15e core by hand;
    // production never writes or reads it.
    if !crate::pol_core::Layout::live(base).is_ok_and(|l| l.timestamp == 0x4E4E_5917) {
        eprintln!("handle record ABI: character-cache part runs on the 1.18.15e core only");
        return;
    }
    // These synthetic active characters exist only in this process's freshly
    // loaded DLL cache, never in a live client or any on-disk settings.
    let scratch = std::slice::from_raw_parts_mut((base + 0x403080) as *mut u8, 9 * 0x68);
    assert!(scratch.iter().all(|&b| b == 0));
    for row in scratch.as_chunks_mut::<0x68>().0 {
        row[0] = 1;
        row[2] = 1;
    }
    assert_eq!(
        unlinked_characters(&api).unwrap(),
        (0..9).collect::<Vec<_>>()
    );
    assert_eq!(
        api.call(197, &[usize::MAX, 1, c"CrystalABI".as_ptr() as usize]),
        0
    );
    let records = handles(&api).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, b"CrystalABI");
    for index in 0..8 {
        assert_eq!(api.call(183, &[index, 0]), 0);
    }
    assert!(
        api.call(183, &[8, 0]) < 0,
        "a ninth link must not overflow the handle"
    );
    assert_eq!(handles(&api).unwrap()[0].used, PER_HANDLE);
    assert_eq!(unlinked_characters(&api).unwrap(), [8]);
    assert_eq!(api.zero(838), 0, "no sign-in was started");
    scratch.fill(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Core {
        handles: Vec<[u8; 0x28]>,
        characters: Vec<[u8; 0x68]>,
        saved_handles: Vec<[u8; 0x28]>,
        saved_characters: Vec<[u8; 0x68]>,
        calls: Vec<(usize, Vec<usize>)>,
        selected: i32,
        fail_links: bool,
    }
    thread_local! { static CORE: RefCell<Core> = RefCell::new(Core::default()); }
    fn dispatch(slot: usize, args: &[usize]) -> i32 {
        CORE.with(|state| {
            let mut state = state.borrow_mut();
            if !matches!(slot, 173 | 177 | 838) {
                state.calls.push((slot, args.to_vec()));
            }
            match slot {
                173 | 177 => unsafe {
                    let record = if slot == 173 {
                        state.handles[args[0]].as_slice()
                    } else {
                        state.characters[args[0]].as_slice()
                    };
                    std::ptr::copy_nonoverlapping(
                        record.as_ptr(),
                        args[1] as *mut u8,
                        record.len(),
                    );
                    0
                },
                197 => unsafe {
                    let index = state.handles.iter().position(|h| h[0] & 1 == 0).unwrap();
                    let name = std::ffi::CStr::from_ptr(args[2] as *const i8).to_bytes();
                    let record = &mut state.handles[index];
                    record[0] = 1;
                    record[8..8 + name.len()].copy_from_slice(name);
                    index as i32
                },
                183 => {
                    let (character, handle) = (args[0], args[1]);
                    let available = state.handles[handle][32..40]
                        .iter()
                        .position(|&b| b & 1 == 0)
                        .unwrap();
                    state.handles[handle][32 + available] = 1 | ((character as u8) << 1);
                    let value = 0x8000u32 | ((handle as u32) << 16);
                    state.characters[character][16..20].copy_from_slice(&value.to_le_bytes());
                    0
                }
                1036 | 1038 | 376 => 1,
                1037 => {
                    state.saved_handles = state.handles.clone();
                    1
                }
                1039 if state.fail_links => -5120,
                1039 => {
                    state.saved_characters = state.characters.clone();
                    1
                }
                190 => state.selected,
                377 => {
                    let selected = state.calls.iter().rev().find(|(s, _)| *s == 376).unwrap().1[0];
                    state.selected = selected as i32;
                    1
                }
                838 => 3,
                1107 | 337 => 0,
                _ => panic!("unexpected native call {slot}"),
            }
        })
    }
    fn fixture(characters: usize) -> Api {
        CORE.with(|s| {
            let mut state = Core {
                handles: vec![[0; 0x28]; SLOTS],
                characters: vec![[0; 0x68]; SLOTS],
                selected: -1,
                ..Core::default()
            };
            for row in &mut state.characters[..characters] {
                row[0] = 1;
                row[2] = 1;
            }
            state.saved_handles = state.handles.clone();
            state.saved_characters = state.characters.clone();
            *s.borrow_mut() = state;
        });
        let mut table = vec![0; 1560];
        macro_rules! zero { ($($n:literal),*) => { $( { unsafe extern "C" fn f()->i32 { dispatch($n,&[]) } table[$n]=f as *const () as usize; } )* }; }
        macro_rules! one { ($($n:literal),*) => { $( { unsafe extern "C" fn f(a:usize)->i32 { dispatch($n,&[a]) } table[$n]=f as *const () as usize; } )* }; }
        macro_rules! two { ($($n:literal),*) => { $( { unsafe extern "C" fn f(a:usize,b:usize)->i32 { dispatch($n,&[a,b]) } table[$n]=f as *const () as usize; } )* }; }
        zero!(1036, 1038, 190, 838);
        one!(1037, 1039, 377, 1107, 337);
        two!(173, 177, 183);
        unsafe extern "C" fn create(a: usize, b: usize, c: usize) -> i32 {
            dispatch(197, &[a, b, c])
        }
        unsafe extern "C" fn status(
            a: usize,
            b: usize,
            c: usize,
            d: usize,
            e: usize,
            f: usize,
        ) -> i32 {
            dispatch(376, &[a, b, c, d, e, f])
        }
        // The owner's latched-error peek before each request (not recorded).
        unsafe extern "C" fn no_error() -> i32 { 0 }
        table[197] = create as *const () as usize;
        table[376] = status as *const () as usize;
        table[1103] = no_error as *const () as usize;
        Api::test_table(&table)
    }
    fn operations() -> Vec<usize> {
        CORE.with(|s| s.borrow().calls.iter().map(|(slot, _)| *slot).collect())
    }

    #[test]
    fn uninitialized_account_requests_a_player_chosen_name_before_mutation() {
        let api = fixture(3);
        assert!(ensure(&api, "", &mut || true)
            .unwrap_err()
            .contains("account editor"));
        assert!(operations().is_empty());
    }
    #[test]
    fn blank_setting_preserves_an_existing_account_even_with_unregistered_characters() {
        let api = fixture(3);
        api.call(197, &[usize::MAX, 1, c"Existing".as_ptr() as usize]);
        CORE.with(|s| s.borrow_mut().calls.clear());
        ensure(&api, "", &mut || true).unwrap();
        assert!(operations().is_empty());
        CORE.with(|s| assert!(s.borrow().characters.iter().all(|c| c[17] & 0x80 == 0)));
    }
    #[test]
    fn first_setup_commits_handles_before_character_links_and_selects_the_handle() {
        let api = fixture(3);
        ensure(&api, "NewHandle", &mut || true).unwrap();
        assert_eq!(
            operations(),
            [197, 1036, 1037, 183, 183, 183, 1038, 1039, 1036, 1037, 190, 376, 377]
        );
        CORE.with(|s| {
            let s = s.borrow();
            assert_eq!(s.selected, 0);
            assert_eq!(&s.saved_handles[0][8..17], b"NewHandle");
            assert!(s.saved_characters[..3].iter().all(|c| c[17] & 0x80 != 0));
            assert_eq!(
                s.saved_handles[0][32..40]
                    .iter()
                    .filter(|b| **b & 1 != 0)
                    .count(),
                3
            );
        });
    }
    #[test]
    fn sixteen_characters_are_distributed_without_overflowing_eight_links_per_handle() {
        let api = fixture(16);
        ensure(&api, "First, Second", &mut || true).unwrap();
        CORE.with(|s| {
            let s = s.borrow();
            assert_eq!(s.handles.iter().filter(|h| h[0] & 1 != 0).count(), 2);
            assert!(s.handles[..2]
                .iter()
                .all(|h| h[32..40].iter().all(|b| b & 1 != 0)));
            assert!(s.characters[..8].iter().all(|c| c[18] == 0));
            assert!(s.characters[8..16].iter().all(|c| c[18] == 1));
        });
    }
    #[test]
    fn insufficient_capacity_is_reported_before_any_creation_or_commit() {
        let api = fixture(9);
        assert!(ensure(&api, "First", &mut || true)
            .unwrap_err()
            .contains("room for 8"));
        assert!(operations().is_empty());
    }
    #[test]
    fn existing_links_and_other_game_links_keep_their_slots() {
        let api = fixture(9);
        api.call(197, &[usize::MAX, 1, c"Existing".as_ptr() as usize]);
        CORE.with(|s| {
            let mut s = s.borrow_mut();
            s.handles[0][32..40].fill(1);
            s.characters[0][17] = 0x80; // an existing link must not be moved
            s.characters[1][2] = 2; // another game is not registered by FFXI setup
            s.calls.clear();
        });
        ensure(&api, "Existing, Added", &mut || true).unwrap();
        CORE.with(|s| {
            let s = s.borrow();
            assert_eq!(s.characters[0][18], 0);
            assert_eq!(s.characters[1][17], 0);
            assert!(s.characters[2..9].iter().all(|c| c[18] == 1));
        });
    }
    #[test]
    fn failed_character_commit_can_resume_without_creating_duplicate_handles() {
        let api = fixture(3);
        CORE.with(|s| s.borrow_mut().fail_links = true);
        assert!(ensure(&api, "NewHandle", &mut || true).is_err());
        CORE.with(|s| {
            let mut s = s.borrow_mut();
            s.handles = s.saved_handles.clone();
            s.characters = s.saved_characters.clone();
            s.calls.clear();
            s.fail_links = false;
        });
        ensure(&api, "NewHandle", &mut || true).unwrap();
        assert!(!operations().contains(&197));
        assert_eq!(
            operations().iter().filter(|&&s| s == 1036).count(),
            1,
            "save links without recreating the handle"
        );
        CORE.with(|s| {
            assert!(s.borrow().saved_characters[..3]
                .iter()
                .all(|c| c[17] & 0x80 != 0))
        });
    }
    #[test]
    fn cancelling_handle_commit_does_not_register_characters() {
        let api = fixture(3);
        assert!(ensure(&api, "NewHandle", &mut || false).is_err());
        assert!(operations().contains(&337));
        assert!(!operations().contains(&183));
    }
    #[test]
    fn completed_character_registration_can_resume_an_interrupted_handle_selection() {
        let api = fixture(3);
        ensure(&api, "NewHandle", &mut || true).unwrap();
        CORE.with(|s| {
            let mut s = s.borrow_mut();
            s.selected = -1;
            s.calls.clear();
        });
        ensure(&api, "NewHandle", &mut || true).unwrap();
        assert_eq!(operations(), [1036, 1037, 190, 376, 377]);
        CORE.with(|s| assert_eq!(s.borrow().selected, 0));
    }

    #[test]
    fn repeating_completed_setup_never_creates_or_relinks_characters() {
        let api = fixture(3);
        ensure(&api, "NewHandle", &mut || true).unwrap();
        CORE.with(|s| s.borrow_mut().calls.clear());
        ensure(&api, "NewHandle", &mut || true).unwrap();
        assert_eq!(operations(), [1036, 1037, 190]);
        CORE.with(|s| {
            assert_eq!(
                s.borrow().handles.iter().filter(|h| h[0] & 1 != 0).count(),
                1
            )
        });
    }

    #[test]
    fn invalid_names_never_reach_the_native_mutators() {
        let api = fixture(3);
        for value in [
            "First,,Second",
            "First,First",
            "TooLongForPOL1234",
            "emoji\u{1f642}",
            "control\n",
        ] {
            assert!(ensure(&api, value, &mut || true).is_err(), "{value:?}");
        }
        assert!(operations().is_empty());
    }
}
