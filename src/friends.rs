//! Launcher-side views of the genuine POL catalog. No passwords or session keys.

use crate::i18n::{t, tf};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Friend {
    pub name: String,
    pub alternate: bool,
    pub slot: u16,
    #[serde(default)]
    pub uid: Option<u64>,
    #[serde(default)]
    pub online: Option<bool>,
    #[serde(default)]
    pub location: Option<String>,
    /// The character part of `location`, when the game published one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    /// The friend's portrait ID (`wire::friend_portrait`); show it with `portraits::image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portrait: Option<u16>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub entries: Vec<Friend>,
    pub fetched_at: u64,
    pub error: Option<String>,
    #[serde(default)]
    pub game_running: Option<bool>,
    #[serde(default)]
    pub session_error: Option<String>,
    #[serde(default)]
    pub own_mode: Option<i32>,
    /// The selected handle's portrait ID, from `native_own_portrait`.
    #[serde(default)]
    pub own_portrait: Option<u16>,
    #[serde(default)]
    pub social: crate::social::State,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Friends").field("count", &self.entries.len()).field("error", &self.error.is_some()).finish()
    }
}

/// Snapshot copied by POL's original, locked getters. A catalog entry without
/// a received status timestamp does not establish that someone is offline.
pub(crate) fn native_snapshot(blob:&[u8])->Result<Snapshot,String> {
    use crate::wire::{FRIENDS_LEN as BLOB_LEN,FRIENDS_MAGIC as MAGIC,FRIEND_RECORD_LEN as RECORD_LEN};
    if blob.len()!=BLOB_LEN || &blob[..8]!=MAGIC { return Err("Invalid native POL friend snapshot.".into()); }
    let mut entries=Vec::new();
    for (slot,rec) in blob[8..].as_chunks::<RECORD_LEN>().0.iter().enumerate() {
        if rec[0x98]&1==0 { continue; }
        let alternate=slot>=200;
        let flags=u64::from_le_bytes(rec[8..16].try_into().unwrap());
        let timestamp=u64::from_le_bytes(rec[0x10..0x18].try_into().unwrap());
        let online=(!alternate && timestamp!=0).then_some(flags&(1<<11)!=0);
        let class=(flags>>33)&0x3ff;
        let location=if online==Some(true) {
            match class { 1=>Some("FINAL FANTASY XI".into()),1000=>Some("PlayOnline".into()),_=>None }
        } else { None };
        entries.push(Friend { name:c_name(&rec[0xa0..]),alternate,slot:(if alternate { slot-200 } else { slot }) as u16,uid:Some(u64::from_le_bytes(rec[..8].try_into().unwrap())),online,location,character:None,portrait:Some(crate::wire::friend_portrait(rec)) });
    }
    sort_entries(&mut entries);
    Ok(Snapshot { entries,fetched_at:now(), ..Snapshot::default() })
}

use crate::wire::{LOCATIONS_LEN as LOCATION_BLOB_LEN, LOCATION_RECORD_LEN};
pub(crate) fn valid_locations(blob: &[u8]) -> bool {
    blob.len() == LOCATION_BLOB_LEN && &blob[..8] == crate::wire::LOCATIONS_MAGIC
}

/// Display only the fields the verified game cache published for this snapshot.
/// The loader already applied FFXiMain's rules (101eaec0): a world label alone
/// for another world, otherwise the name when shown and the area when not in
/// my zone. A disconnected/unknown/offline record, or a block-list entry,
/// cannot acquire a location from extras.
pub(crate) fn apply_native_locations(snapshot: &mut Snapshot, blob: &[u8]) -> Result<(), String> {
    if !valid_locations(blob) { return Err("Invalid game friend locations.".into()); }
    for friend in &mut snapshot.entries {
        if friend.online != Some(true) || friend.alternate { continue; }
        let index = friend.slot as usize;
        if index >= crate::wire::FRIEND_SLOTS { continue; }
        let record = &blob[8 + index * LOCATION_RECORD_LEN..][..LOCATION_RECORD_LEN];
        // Stricter than `c_name`: a field without its NUL is not shown.
        let text = |field: &[u8]| -> Option<String> {
            let end = field.iter().position(|b| *b == 0)?;
            let value = decode_name(&field[..end]);
            (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
        };
        let character = text(&record[..16]);
        let fields: Vec<_> = character.iter().cloned().chain([&record[16..80], &record[80..96]].into_iter().filter_map(text)).collect();
        friend.character = character;
        if !fields.is_empty() { friend.location = Some(fields.join(" · ")); }
    }
    Ok(())
}

pub(crate) fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

/// Friends by name, then list A before list B, then slot.
fn sort_entries(entries: &mut [Friend]) {
    entries.sort_by_cached_key(|f| (f.name.to_lowercase(), f.alternate, f.slot));
}

/// A NUL-terminated CP932 field (the whole field when it has no NUL).
pub(crate) fn c_name(field: &[u8]) -> String {
    decode_name(&field[..field.iter().position(|b| *b == 0).unwrap_or(field.len())])
}

pub(crate) fn decode_name(bytes: &[u8]) -> String {
    if bytes.is_ascii() { return String::from_utf8_lossy(bytes).into_owned(); }
    crate::win::codepage::decode(crate::win::codepage::SHIFT_JIS, bytes, false)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// What the Friends page's right side shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pane {
    /// A conversation with the friend in this catalog slot (list A).
    Friend { slot: u16, name: String },
    /// Every stored message by folder (Inbox, Sent, Archived), from anyone.
    Mailbox,
    /// The block list (list B).
    Blocked,
}

/// One stored PlayOnline message as the conversation knows it: its header,
/// from the newest Inbox or Sent page.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// The header a `Read` request names.
    pub raw: String,
    /// The message itself, whatever its read flag (`social::message_key`).
    pub key: String,
    /// 0 Inbox, 2 Sent.
    pub folder: u8,
    /// The other side's uid: the sender (Inbox) or the recipient (Sent).
    pub peer: Option<u64>,
    pub sender: String,
    pub subject: String,
    pub unread: bool,
    pub stamp: u32,
}
impl Entry {
    fn of(folder: u8, header: &crate::social::Header) -> Self {
        Self {
            raw: header.raw.clone(),
            key: crate::social::message_key(&header.raw),
            folder,
            peer: if folder == 2 { header.recipient } else { header.sender_uid() },
            sender: header.sender.clone(),
            subject: header.subject.clone(),
            unread: folder == 0 && header.unread,
            stamp: header.stamp(),
        }
    }
    pub fn mine(&self) -> bool {
        self.folder == 2
    }
}

/// A message's text, once read. `message` may be the stored (English) notice
/// for a type Crystal can't show; `social::localize` words it.
#[derive(Clone, Debug)]
pub(crate) struct Body {
    pub subject: String,
    pub message: String,
}
impl Body {
    pub fn unsupported(&self) -> bool {
        self.message.starts_with("This PlayOnline message (type ")
    }
}

/// The launcher's memory of stored mail, for conversations: the newest page
/// of Inbox and Sent, and the bodies read so far. In memory only, per account
/// and session; the messages themselves stay in PlayOnline's own folders.
#[derive(Default)]
pub(crate) struct Mail {
    /// Page 0 of Inbox [0] and Sent [2] as last listed (Archived [1] unused).
    pub listed: [Option<Vec<Entry>>; 3],
    pub bodies: std::collections::HashMap<String, Body>,
    /// Messages a read failed for; not asked for again this session.
    pub failed: std::collections::HashSet<String>,
    /// Messages that were unread when listed: the "new" divider.
    pub fresh: std::collections::HashSet<String>,
    /// The message the read in flight is for.
    pub reading: Option<String>,
    /// A listing failed or timed out: stop asking until the user acts.
    pub stalled: bool,
}

/// A message on its way, shown at the end of the conversation until the Sent
/// folder lists it.
pub(crate) struct Outgoing {
    pub uid: u64,
    pub subject: String,
    pub message: String,
    pub sent: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub account: Option<String>,
    pub snapshot: Option<Snapshot>,
    pub connected: bool,
    pub loading: bool,
    pub query: String,
    pub error: String,
    pub pane: Option<Pane>,
    /// Online [0] / Offline [1] / Waiting [2] sections folded away.
    pub collapsed: [bool; 3],
    /// The status options (groups, Auto-Away) are open under the status.
    pub status_options: bool,
    pub composer: Option<Composer>,
    pub remove: Option<Friend>,
    pub pending: Option<(String, std::time::Instant)>,
    /// "Also set for my groups" is unchecked (it starts checked).
    pub skip_groups: bool,
    pub mail: Mail,
    pub outgoing: Option<Outgoing>,
}

/// One "Auto-away time" choice (Viewer setting 4/7): minutes, 0 = off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AutoAway(pub u8);
impl std::fmt::Display for AutoAway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 { f.write_str(t("Auto-Away off")) } else { f.write_str(&tf("Auto-Away after {minutes} min", &[("minutes", &self.0)])) }
    }
}
/// Friends (list A) first, then the block list (list B, core 100236a0).
pub(crate) fn split_lists<'a>(entries: &'a [Friend], query: &str) -> (Vec<&'a Friend>, Vec<&'a Friend>) {
    let query = query.to_lowercase();
    entries.iter().filter(|f| f.name.to_lowercase().contains(&query)).partition(|f| !f.alternate)
}

pub(crate) struct Composer {
    pub recipient: Friend,
    pub subject: String,
    pub text: String,
    /// The optional subject line is open.
    pub show_subject: bool,
}
impl Composer {
    pub fn new(recipient: Friend) -> Self {
        Self { recipient, subject: String::new(), text: String::new(), show_subject: false }
    }
}

/// The most a PlayOnline subject and text may hold, in CP932 bytes (the
/// loader's `cp932(subject, 127)`; text below 0xfff with its NUL).
pub(crate) const SUBJECT_MAX: usize = 127;
pub(crate) const TEXT_MAX: usize = 4094;

/// `text`'s length in CP932 bytes, or `None` when PlayOnline can't encode it
/// (a character with no CP932 form).
pub(crate) fn cp932_len(text: &str) -> Option<usize> {
    if text.is_empty() {
        return Some(0);
    }
    use windows_sys::Win32::Globalization::{WideCharToMultiByte, WC_NO_BEST_FIT_CHARS};
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut substituted = 0;
    let n = unsafe {
        WideCharToMultiByte(932, WC_NO_BEST_FIT_CHARS, wide.as_ptr(), wide.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), &mut substituted)
    };
    if n <= 0 || substituted != 0 {
        return None;
    }
    Some(n as usize)
}

/// A header stamp as a local time, when it reads as a plausible Unix time
/// (2000 to tomorrow). The Viewer sorts by this field; that it is a time is
/// not verified, so anything else shows no time.
pub(crate) fn message_time(stamp: u32) -> Option<chrono::DateTime<chrono::Local>> {
    let stamp = i64::from(stamp);
    (946_684_800..=now() as i64 + 86_400).contains(&stamp)
        .then(|| chrono::DateTime::from_timestamp(stamp, 0))
        .flatten()
        .map(|t| t.with_timezone(&chrono::Local))
}

/// Whether `entry` is part of the conversation with `friend`: by uid, or by
/// handle name for an Inbox header without a sender uid.
fn in_conversation(friend: &Friend, entry: &Entry) -> bool {
    match (friend.uid, entry.peer) {
        (Some(a), Some(b)) => a == b,
        _ => entry.folder == 0 && entry.sender == friend.name,
    }
}

impl State {
    /// The friend whose conversation is open (list A only).
    pub fn friend(&self) -> Option<&Friend> {
        let Some(Pane::Friend { slot, name }) = &self.pane else { return None };
        self.snapshot.as_ref()?.entries.iter().find(|f| !f.alternate && f.slot == *slot && &f.name == name)
    }

    /// Open `pane`. The "new" divider belongs to the conversation that was
    /// open, so the messages read there stop being new.
    pub fn select(&mut self, pane: Option<Pane>) {
        if self.pane == pane {
            return;
        }
        let unread: std::collections::HashSet<&String> =
            self.mail.listed[0].iter().flatten().filter(|e| e.unread).map(|e| &e.key).collect();
        self.mail.fresh.retain(|key| unread.contains(key));
        self.remove = None;
        self.mail.stalled = false;
        self.outgoing = None;
        self.pane = pane;
        self.composer = self.friend().filter(|f| f.uid.is_some()).cloned().map(Composer::new);
    }

    /// The conversation with `friend`, oldest first.
    pub fn thread(&self, friend: &Friend) -> Vec<&Entry> {
        let mut items: Vec<&Entry> = [0, 2]
            .into_iter()
            .filter_map(|folder| self.mail.listed[folder].as_ref())
            .flatten()
            .filter(|e| in_conversation(friend, e))
            .collect();
        items.sort_by_key(|e| (e.stamp, e.folder));
        items
    }

    /// Unread Inbox messages from `friend` on the newest page.
    pub fn unread_from(&self, friend: &Friend) -> usize {
        self.mail.listed[0].iter().flatten().filter(|e| e.unread && in_conversation(friend, e)).count()
    }

    /// Whether the open conversation still has messages to fetch.
    pub fn syncing(&self) -> bool {
        self.friend().is_some() && !self.mail.stalled && (self.pending.is_some() || self.next_sync().is_some())
    }

    /// The next request that fills the open conversation: list Inbox (new
    /// mail, badges), then read this friend's Inbox messages, then list and
    /// read Sent. One at a time, and only while PlayOnline is idle; the
    /// Mailbox page drives its own listings.
    pub fn next_sync(&self) -> Option<crate::social::Request> {
        use crate::social::Request;
        let snapshot = self.snapshot.as_ref()?;
        let social = &snapshot.social;
        if !self.connected
            || snapshot.game_running != Some(false)
            || !social.ready
            || social.busy
            || self.pending.is_some()
            || self.mail.stalled
            // A Remove awaiting confirmation goes first.
            || self.remove.is_some()
            || self.pane == Some(Pane::Mailbox)
        {
            return None;
        }
        let list = |folder| Some(Request::Inbox { folder, page: 0 });
        // No unread count yet: a fresh session (sign-in, back from FFXI) whose
        // new mail arrived before the launcher could hear about it.
        if self.mail.listed[0].is_none() || social.inbox_changed || social.inbox_unread.is_none() {
            return list(0);
        }
        let friend = self.friend().filter(|f| f.uid.is_some())?;
        for folder in [0u8, 2] {
            let Some(entries) = &self.mail.listed[folder as usize] else { return list(folder) };
            let wanted = entries.iter().find(|e| {
                in_conversation(friend, e) && !self.mail.bodies.contains_key(&e.key) && !self.mail.failed.contains(&e.key)
            });
            if let Some(entry) = wanted {
                // A read names a header of the folder PlayOnline listed last.
                let listed = social.folder == folder && social.page == 0 && social.headers.iter().any(|h| h.raw == entry.raw);
                return if listed { Some(Request::Read { header: entry.raw.clone() }) } else { list(folder) };
            }
        }
        None
    }

    /// Learn from a finished request of ours: a listing's headers, a read's
    /// body, a send's result.
    pub fn completed(&mut self, snapshot: &Snapshot) {
        let social = &snapshot.social;
        use crate::social::Action;
        match social.action {
            Some(Action::Inbox | Action::Check) => {
                if social.error.is_some() {
                    self.mail.stalled = true;
                } else if social.page == 0 && social.folder != 1 {
                    let folder = social.folder;
                    let entries: Vec<Entry> = social.headers.iter().map(|h| Entry::of(folder, h)).collect();
                    self.mail.fresh.extend(entries.iter().filter(|e| e.unread).map(|e| e.key.clone()));
                    self.mail.listed[folder as usize] = Some(entries);
                }
            }
            Some(Action::Read) => {
                if let Some(key) = self.mail.reading.take() {
                    match (&social.error, &social.opened) {
                        (None, Some(opened)) => {
                            self.mail.bodies.insert(key.clone(), Body { subject: opened.subject.clone(), message: opened.message.clone() });
                            for entry in self.mail.listed.iter_mut().flatten().flatten().filter(|e| e.key == key) {
                                entry.unread = false;
                            }
                        }
                        _ => {
                            self.mail.failed.insert(key);
                        }
                    }
                }
            }
            Some(Action::Send) => {
                if social.error.is_none() {
                    // Sent has a new message: list it again.
                    self.mail.listed[2] = None;
                    let clear_draft = self.outgoing.as_ref().zip(self.composer.as_ref())
                        .is_some_and(|(outgoing, composer)| {
                            composer.recipient.uid == Some(outgoing.uid)
                                && composer.text.trim_end_matches('\n') == outgoing.message
                                && (if composer.show_subject { composer.subject.as_str() } else { "" }) == outgoing.subject
                        });
                    if let Some(outgoing) = &mut self.outgoing {
                        outgoing.sent = true;
                    }
                    // A late result belongs only to the draft that was sent.
                    // Selection or queued edits may have replaced that draft.
                    if let Some(composer) = self.composer.as_mut().filter(|_| clear_draft) {
                        composer.text.clear();
                        composer.subject.clear();
                        composer.show_subject = false;
                    }
                } else {
                    self.outgoing = None;
                }
            }
            Some(Action::Remove) if social.error.is_none() => {
                // Close only the removed friend's conversation. An unblock, or
                // another pane opened meanwhile, stays open with its draft.
                let removed = social.pending_remove.is_some_and(|(slot, uid)| {
                    matches!(&self.pane, Some(Pane::Friend { slot: open, .. }) if *open == slot)
                        && self.friend().is_none_or(|f| f.uid == Some(uid))
                });
                if removed {
                    self.select(None);
                }
            }
            _ => {}
        }
    }

    /// The Send request for the composer's draft, if it can go.
    pub fn send_request(&self) -> Option<crate::social::Request> {
        let draft = self.composer.as_ref()?;
        let uid = draft.recipient.uid?;
        let message = draft.text.clone();
        let message = message.trim_end_matches('\n').to_owned();
        let subject = if draft.show_subject { draft.subject.clone() } else { String::new() };
        let fits = cp932_len(&message).is_some_and(|n| n <= TEXT_MAX) && cp932_len(&subject).is_some_and(|n| n <= SUBJECT_MAX);
        let request = crate::social::Request::Send { alternate: draft.recipient.alternate, slot: draft.recipient.slot, uid, subject, message };
        (fits && request.valid()).then_some(request)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn game_locations_follow_catalog_slots_and_never_restore_offline_presence() {
        let mut snapshot = Snapshot { entries: vec![
            Friend { name:"Alice".into(),alternate:false,slot:1,uid:None,online:Some(true),location:None,character:None,portrait:None },
            Friend { name:"Zed".into(),alternate:false,slot:0,uid:None,online:Some(false),location:None,character:None,portrait:None },
        ], ..Snapshot::default() };
        let mut blob=vec![0;LOCATION_BLOB_LEN]; blob[..8].copy_from_slice(b"XIPOLG\x01\0");
        blob[8..13].copy_from_slice(b"Other");
        blob[8+96..8+96+5].copy_from_slice(b"Alice");
        blob[8+96+16..8+96+30].copy_from_slice(b"Bastok Markets");
        apply_native_locations(&mut snapshot,&blob).unwrap();
        assert_eq!(snapshot.entries[0].location.as_deref(),Some("Alice · Bastok Markets"));
        assert_eq!(snapshot.entries[1].location,None);
        assert!(apply_native_locations(&mut snapshot,&blob[..blob.len()-1]).is_err());
    }

    #[test]
    fn native_friends_keep_unknown_status_until_a_received_update() {
        use crate::wire::{FRIENDS_LEN as BLOB_LEN,FRIENDS_MAGIC as MAGIC,FRIEND_RECORD_LEN as RECORD_LEN};
        let mut blob=vec![0;BLOB_LEN];blob[..8].copy_from_slice(MAGIC);
        let record=&mut blob[8..8+RECORD_LEN];record[0x98]=1;record[0xa0..0xa5].copy_from_slice(b"Alice");
        let snapshot=native_snapshot(&blob).unwrap();assert_eq!(snapshot.entries[0].online,None);
        assert_eq!(snapshot.entries[0].portrait,Some(0));
        blob[8+0x9c..8+0xa0].copy_from_slice(&((3u32<<13)|1234).to_le_bytes());
        assert_eq!(native_snapshot(&blob).unwrap().entries[0].portrait,Some(1234),"one accessor: +0x9c & 0x1fff");
        let record=&mut blob[8..8+RECORD_LEN];record[0x10..0x18].copy_from_slice(&123u64.to_le_bytes());
        record[8..16].copy_from_slice(&((1u64<<11)|(1u64<<33)).to_le_bytes());
        let snapshot=native_snapshot(&blob).unwrap();assert_eq!(snapshot.entries[0].online,Some(true));
        assert_eq!(snapshot.entries[0].location.as_deref(),Some("FINAL FANTASY XI"));
        let record=&mut blob[8..8+RECORD_LEN];record[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(native_snapshot(&blob).unwrap().entries[0].online,Some(false));
        assert!(native_snapshot(&blob[..blob.len()-1]).is_err());
    }
    #[test]
    fn utf8_names_are_not_fabricated_from_arbitrary_bytes() {
        assert_eq!(decode_name(b"Alice"), "Alice");
        assert_eq!(decode_name(&[0x82,0xa0]), "あ");
    }

    #[test]
    fn older_cached_friends_do_not_become_offline_by_default() {
        let friend: Friend=serde_json::from_str(r#"{"name":"Alice","alternate":false,"slot":1}"#).unwrap();
        assert_eq!(friend.online,None);
        assert_eq!(friend.location,None);
        let snapshot: Snapshot=serde_json::from_str(r#"{"entries":[],"fetched_at":1,"error":null,"refreshing":true}"#).unwrap();
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn the_block_list_is_shown_apart_and_never_gets_a_location() {
        let friend = |name: &str, alternate: bool| Friend { name: name.into(), alternate, slot: 0, uid: Some(1), online: Some(true), location: None, character: None, portrait: None };
        let entries = vec![friend("Alice", false), friend("Mallory", true), friend("Bob", false)];
        let (friends, blocked) = split_lists(&entries, "");
        assert_eq!(friends.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Alice", "Bob"]);
        assert_eq!(blocked.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Mallory"]);
        let (friends, blocked) = split_lists(&entries, "MAL");
        assert!(friends.is_empty() && blocked.len() == 1);
        let mut snapshot = Snapshot { entries: vec![friend("Mallory", true)], ..Snapshot::default() };
        let mut blob = vec![0; LOCATION_BLOB_LEN]; blob[..8].copy_from_slice(b"XIPOLG\x01\0");
        blob[8 + 200 * 96 + 80..8 + 200 * 96 + 87].copy_from_slice(b"Phoenix");
        apply_native_locations(&mut snapshot, &blob).unwrap();
        assert_eq!(snapshot.entries[0].location, None);
    }

    #[test]
    fn another_world_shows_only_its_world_label() {
        let mut snapshot = Snapshot { entries: vec![Friend { name:"Alice".into(),alternate:false,slot:0,uid:None,online:Some(true),location:None,character:None,portrait:None }], ..Snapshot::default() };
        let mut blob = vec![0; LOCATION_BLOB_LEN]; blob[..8].copy_from_slice(b"XIPOLG\x01\0");
        blob[8 + 80..8 + 87].copy_from_slice(b"Phoenix");
        apply_native_locations(&mut snapshot, &blob).unwrap();
        assert_eq!(snapshot.entries[0].location.as_deref(), Some("Phoenix"));
    }

    fn header(id: u32, from: u64, to: u64, sender: &str, unread: bool, stamp: u32) -> crate::social::Header {
        let mut bytes = [0u8; 72];
        bytes[..8].copy_from_slice(&from.to_le_bytes());
        bytes[8..16].copy_from_slice(&to.to_le_bytes());
        bytes[16..16 + sender.len()].copy_from_slice(sender.as_bytes());
        bytes[0x30..0x34].copy_from_slice(&id.to_le_bytes());
        bytes[0x34..0x38].copy_from_slice(&stamp.to_le_bytes());
        bytes[0x3e..0x40].copy_from_slice(&(if unread { 0x8000u16 } else { 0 }).to_le_bytes());
        let mut raw = Vec::new();
        crate::pol_less::pipe::base64_into(&mut raw, &bytes);
        crate::social::Header { raw: String::from_utf8(raw).unwrap(), sender: sender.into(), subject: String::new(), unread, kind: 0, recipient: Some(to) }
    }

    /// Signed in, FFXI closed, Alyria (uid 1) and Bob (uid 2) on list A.
    fn signed_in() -> State {
        let friend = |slot: u16, name: &str| Friend { name: name.into(), alternate: false, slot, uid: Some(slot as u64 + 1), online: Some(true), location: None, character: None, portrait: None };
        State {
            connected: true,
            snapshot: Some(Snapshot {
                entries: vec![friend(0, "Alyria"), friend(1, "Bob")],
                fetched_at: 1,
                game_running: Some(false),
                social: crate::social::State { ready: true, ..Default::default() },
                ..Snapshot::default()
            }),
            ..State::default()
        }
    }

    /// The worker finished `request` with these headers listed (or `opened` read).
    fn finish(state: &mut State, request: &crate::social::Request, headers: &[crate::social::Header], opened: Option<&str>) {
        use crate::social::Request;
        let mut snapshot = state.snapshot.clone().unwrap();
        let social = &mut snapshot.social;
        social.action = Some(request.action());
        social.error = None;
        match request {
            Request::Inbox { folder, page } => {
                social.folder = *folder; social.page = *page; social.headers = headers.to_vec();
                if *folder == 0 { social.inbox_unread = Some(headers.iter().filter(|h| h.unread).count() as u32); social.inbox_changed = false; }
            }
            Request::Remove { .. } => { social.begin("0".repeat(32), request); social.busy = false; }
            Request::Read { .. } => {
                state.mail.reading = match request { Request::Read { header } => Some(crate::social::message_key(header)), _ => None };
                social.opened = opened.map(|m| crate::social::Opened { sender: String::new(), subject: String::new(), message: m.into(), recipient: None });
            }
            _ => {}
        }
        state.snapshot = Some(snapshot.clone());
        state.completed(&snapshot);
    }

    #[test]
    fn a_conversation_lists_then_reads_one_message_at_a_time_and_stops_when_full() {
        use crate::social::Request;
        let mut state = signed_in();
        let inbox = [header(1, 1, 9, "Alyria", true, 1_700_000_300), header(2, 2, 9, "Bob", true, 1_700_000_200), header(3, 1, 9, "Alyria", false, 1_700_000_100)];
        let sent = [header(4, 9, 1, "Me", false, 1_700_000_200)];
        // Badges first: Inbox is listed even with no conversation open.
        let request = state.next_sync().unwrap();
        assert!(matches!(request, Request::Inbox { folder: 0, page: 0 }));
        finish(&mut state, &request, &inbox, None);
        assert_eq!(state.next_sync().map(|r| r.action()), None, "nothing to read with no conversation open");
        assert_eq!(state.unread_from(&state.snapshot.as_ref().unwrap().entries[1].clone()), 1);
        state.select(Some(Pane::Friend { slot: 0, name: "Alyria".into() }));
        assert!(state.composer.is_some());
        // Alyria's two Inbox messages, then Sent, then its one message to her.
        for (expected, body) in [(&inbox[0], "Hi"), (&inbox[2], "Older")] {
            let request = state.next_sync().unwrap();
            assert!(matches!(&request, Request::Read { header } if header == &expected.raw), "{request:?}");
            finish(&mut state, &request, &[], Some(body));
        }
        let request = state.next_sync().unwrap();
        assert!(matches!(request, Request::Inbox { folder: 2, page: 0 }));
        finish(&mut state, &request, &sent, None);
        let request = state.next_sync().unwrap();
        assert!(matches!(&request, Request::Read { header } if header == &sent[0].raw));
        finish(&mut state, &request, &[], Some("Hello"));
        assert!(state.next_sync().is_none());
        // Oldest first, both folders; the unread one was read and stays "new".
        let alyria = state.friend().unwrap().clone();
        let thread: Vec<_> = state.thread(&alyria).iter().map(|e| (e.folder, e.stamp)).collect();
        assert_eq!(thread, [(0, 1_700_000_100), (2, 1_700_000_200), (0, 1_700_000_300)]);
        assert_eq!(state.unread_from(&alyria), 0);
        assert!(state.mail.fresh.contains(&crate::social::message_key(&inbox[0].raw)));
        // Leaving the conversation retires its "new" mark; Bob's stays.
        state.select(None);
        assert!(!state.mail.fresh.contains(&crate::social::message_key(&inbox[0].raw)));
        assert!(state.mail.fresh.contains(&crate::social::message_key(&inbox[1].raw)));
    }

    #[test]
    fn a_read_names_only_a_header_of_the_folder_listed_last_and_failures_stop() {
        use crate::social::Request;
        let mut state = signed_in();
        let inbox = [header(1, 1, 9, "Alyria", false, 1)];
        let sent = [header(2, 9, 1, "Me", false, 2)];
        finish(&mut state, &Request::Inbox { folder: 0, page: 0 }, &inbox, None);
        finish(&mut state, &Request::Inbox { folder: 2, page: 0 }, &sent, None);
        state.select(Some(Pane::Friend { slot: 0, name: "Alyria".into() }));
        // Sent is listed now, so Alyria's Inbox message needs Inbox again first.
        assert!(matches!(state.next_sync(), Some(Request::Inbox { folder: 0, .. })));
        finish(&mut state, &Request::Inbox { folder: 0, page: 0 }, &inbox, None);
        assert!(matches!(state.next_sync(), Some(Request::Read { ref header }) if *header == inbox[0].raw));
        // A failed read is not asked again.
        state.mail.reading = Some(crate::social::message_key(&inbox[0].raw));
        let mut snapshot = state.snapshot.clone().unwrap();
        snapshot.social.action = Some(crate::social::Action::Read);
        snapshot.social.error = Some("PlayOnline read failed (-5326).".into());
        state.completed(&snapshot);
        assert!(state.mail.failed.contains(&crate::social::message_key(&inbox[0].raw)));
        assert!(!matches!(state.next_sync(), Some(Request::Read { ref header }) if *header == inbox[0].raw));
        // A failed listing stalls the conversation until the user acts.
        snapshot.social.action = Some(crate::social::Action::Inbox);
        state.completed(&snapshot);
        assert!(state.mail.stalled && state.next_sync().is_none());
        state.select(Some(Pane::Friend { slot: 1, name: "Bob".into() }));
        assert!(!state.mail.stalled);
    }

    #[test]
    fn nothing_is_requested_while_ffxi_runs_busy_or_in_the_mailbox() {
        let mut state = signed_in();
        state.snapshot.as_mut().unwrap().game_running = Some(true);
        assert!(state.next_sync().is_none());
        let mut state = signed_in();
        state.snapshot.as_mut().unwrap().social.busy = true;
        assert!(state.next_sync().is_none());
        let mut state = signed_in();
        state.pending = Some(("x".into(), std::time::Instant::now()));
        assert!(state.next_sync().is_none());
        let mut state = signed_in();
        state.select(Some(Pane::Mailbox));
        assert!(state.next_sync().is_none());
        let mut state = signed_in();
        state.select(Some(Pane::Blocked));
        assert!(state.composer.is_none());
    }

    #[test]
    fn a_send_needs_text_within_playonline_limits_and_success_clears_the_draft() {
        use crate::social::Request;
        let mut state = signed_in();
        state.select(Some(Pane::Friend { slot: 0, name: "Alyria".into() }));
        assert!(state.send_request().is_none(), "empty");
        state.composer.as_mut().unwrap().text = String::from("Hello\n");
        state.composer.as_mut().unwrap().subject = "ignored unless shown".into();
        match &state.send_request() {
            Some(Request::Send { uid: 1, slot: 0, alternate: false, subject, message }) => {
                assert_eq!((subject.as_str(), message.as_str()), ("", "Hello"));
            }
            other => panic!("{other:?}"),
        }
        state.composer.as_mut().unwrap().text = "あ".repeat(TEXT_MAX / 2 + 1);
        assert!(state.send_request().is_none(), "over the CP932 limit");
        assert_eq!(cp932_len("aあ｡"), Some(4));
        assert_eq!(cp932_len("😀"), None, "no CP932 form");
        state.composer.as_mut().unwrap().text = String::from("Hi");
        state.outgoing = Some(Outgoing { uid: 1, subject: String::new(), message: "Hi".into(), sent: false });
        state.mail.listed[2] = Some(Vec::new());
        finish(&mut state, &Request::Check {}, &[], None);
        let mut snapshot = state.snapshot.clone().unwrap();
        snapshot.social.action = Some(crate::social::Action::Send);
        state.completed(&snapshot);
        assert!(state.composer.as_ref().unwrap().text.trim().is_empty());
        assert!(state.outgoing.as_ref().unwrap().sent);
        assert!(state.mail.listed[2].is_none(), "Sent is listed again");
        // A failed send keeps the draft and drops the pending bubble.
        state.composer.as_mut().unwrap().text = String::from("Again");
        snapshot.social.error = Some("PlayOnline send failed (-1).".into());
        state.completed(&snapshot);
        assert_eq!(state.composer.as_ref().unwrap().text.trim(), "Again");
        assert!(state.outgoing.is_none());
    }

    #[test]
    fn a_send_result_keeps_a_replaced_or_changed_draft() {
        for change in ["recipient", "message", "subject", "selection"] {
            let mut state = signed_in();
            state.select(Some(Pane::Friend { slot: 0, name: "Alyria".into() }));
            state.outgoing = Some(Outgoing { uid: 1, subject: String::new(), message: "Sent text".into(), sent: false });
            let draft = state.composer.as_mut().unwrap();
            draft.text = String::from("Sent text");
            match change {
                "recipient" => draft.recipient.uid = Some(2),
                "message" => draft.text = String::from("New text"),
                "subject" => { draft.show_subject = true; draft.subject = "New subject".into(); }
                "selection" => state.outgoing = None,
                _ => unreachable!(),
            }
            let before = state.composer.as_ref().unwrap().text.clone();
            let subject = state.composer.as_ref().unwrap().subject.clone();
            let mut snapshot = state.snapshot.clone().unwrap();
            snapshot.social.action = Some(crate::social::Action::Send);
            state.completed(&snapshot);
            assert_eq!(state.composer.as_ref().unwrap().text.clone(), before, "{change}");
            assert_eq!(state.composer.as_ref().unwrap().subject, subject, "{change}");
        }
    }

    #[test]
    fn drafts_must_satisfy_the_native_send_contract() {
        let mut state = signed_in();
        state.select(Some(Pane::Friend { slot: 0, name: "Alyria".into() }));
        for text in ["hello\0there", "hello\u{7}there"] {
            state.composer.as_mut().unwrap().text = String::from(text);
            assert!(state.send_request().is_none(), "control characters are rejected before Send is enabled");
        }
        state.composer.as_mut().unwrap().text = String::from("Hello\nworld\t!");
        assert!(state.send_request().is_some());
        state.composer.as_mut().unwrap().recipient.uid = Some(0);
        assert!(state.send_request().is_none());
    }

    #[test]
    fn a_fresh_session_relists_the_inbox_for_mail_that_arrived_unheard() {
        use crate::social::Request;
        let mut state = signed_in();
        let inbox = [header(1, 1, 9, "Alyria", false, 1)];
        finish(&mut state, &Request::Inbox { folder: 0, page: 0 }, &inbox, None);
        state.select(Some(Pane::Friend { slot: 0, name: "Alyria".into() }));
        assert!(!matches!(state.next_sync(), Some(Request::Inbox { folder: 0, .. })));
        // Back from FFXI (or signed in again): the session starts a new social state.
        state.snapshot.as_mut().unwrap().social = crate::social::State { ready: true, ..Default::default() };
        assert!(matches!(state.next_sync(), Some(Request::Inbox { folder: 0, page: 0 })));
    }

    #[test]
    fn a_remove_closes_only_the_removed_friends_conversation() {
        use crate::social::Request;
        // Unblocking from the Blocked pane keeps the pane (and its notice).
        let mut state = signed_in();
        state.select(Some(Pane::Blocked));
        finish(&mut state, &Request::Remove { alternate: true, slot: 0, uid: 5 }, &[], None);
        assert_eq!(state.pane, Some(Pane::Blocked));
        // Moving on to Bob while Alyria's remove runs keeps Bob's draft.
        state.select(Some(Pane::Friend { slot: 1, name: "Bob".into() }));
        state.composer.as_mut().unwrap().text = String::from("draft");
        finish(&mut state, &Request::Remove { alternate: false, slot: 0, uid: 1 }, &[], None);
        assert_eq!(state.pane, Some(Pane::Friend { slot: 1, name: "Bob".into() }));
        assert_eq!(state.composer.as_ref().unwrap().text.trim(), "draft");
        // Removing the open friend closes the conversation.
        finish(&mut state, &Request::Remove { alternate: false, slot: 1, uid: 2 }, &[], None);
        assert_eq!(state.pane, None);
    }

    #[test]
    fn only_plausible_stamps_show_a_time() {
        assert!(message_time(1_700_000_000).is_some());
        assert!(message_time(42).is_none());
        assert!(message_time(u32::MAX).is_none());
    }

    #[test]
    fn auto_away_offers_exactly_the_viewer_choices() {
        assert_eq!(crate::pol_less::AUTO_AWAY_CHOICES, [0, 10, 20, 30, 40, 50, 60]);
        assert_eq!(AutoAway(0).to_string(), "Auto-Away off");
        assert_eq!(AutoAway(10).to_string(), "Auto-Away after 10 min");
    }
}
