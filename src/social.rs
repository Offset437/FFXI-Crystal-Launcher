//! Account-scoped launcher controls for the installed Viewer's native services.
use crate::i18n::{t, tf};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// A social error, notice or system message for display in the current
/// language. `State` stores these in English (it is cached on disk and shared
/// with the session worker), so the text is translated only where it is shown.
/// Anything unrecognised (a server or OS message) is returned as it is.
pub(crate) fn localize(text: &str) -> String {
    localized(text).unwrap_or_else(|| text.to_owned())
}

/// `localize` for a text it recognises; `None` for anything else.
fn localized(text: &str) -> Option<String> {
    match text {
        "PlayOnline could not encode this message. Shorten it or remove unsupported characters (-5120)." => {
            return Some(t("PlayOnline could not encode this message. Shorten it or remove unsupported characters (-5120).").into());
        }
        "Message sent by PlayOnline." => return Some(t("Message sent by PlayOnline.").into()),
        "Unblocked by PlayOnline." => return Some(t("Unblocked by PlayOnline.").into()),
        "Removed from your PlayOnline friends." => return Some(t("Removed from your PlayOnline friends.").into()),
        "PlayOnline returned no message headers." => return Some(t("PlayOnline returned no message headers.").into()),
        "Invalid native inbox page." => return Some(t("Invalid native inbox page.").into()),
        "Incomplete native inbox page." => return Some(t("Incomplete native inbox page.").into()),
        "This inbox request has ended." => return Some(t("This inbox request has ended.").into()),
        "This message request has ended." => return Some(t("This message request has ended.").into()),
        "This message is no longer in the inbox." => return Some(t("This message is no longer in the inbox.").into()),
        "PlayOnline returned no message text." => return Some(t("PlayOnline returned no message text.").into()),
        "PlayOnline returned an invalid message." => return Some(t("PlayOnline returned an invalid message.").into()),
        "PlayOnline returned an unsupported message format." => {
            return Some(t("PlayOnline returned an unsupported message format.").into());
        }
        "Invalid native storage result." => return Some(t("Invalid native storage result.").into()),
        "Invalid native PlayOnline message header." => return Some(t("Invalid native PlayOnline message header.").into()),
        "Could not encode the PlayOnline request." => return Some(t("Could not encode the PlayOnline request.").into()),
        _ => {}
    }
    if let Some(code) = text
        .strip_prefix("Message sent by PlayOnline; the local Sent copy could not be stored (")
        .and_then(|rest| rest.strip_suffix("). Do not send it again."))
    {
        return Some(tf(
            "Message sent by PlayOnline; the local Sent copy could not be stored ({code}). Do not send it again.",
            &[("code", &code)],
        ));
    }
    if let Some(kind) = text
        .strip_prefix("This PlayOnline message (type ")
        .and_then(|rest| rest.strip_suffix(") can't be shown in Crystal. It is saved with your messages."))
    {
        return Some(tf(
            "This PlayOnline message (type {kind}) can't be shown in Crystal. It is saved with your messages.",
            &[("kind", &kind)],
        ));
    }
    if let Some((action, code)) = text
        .strip_prefix("PlayOnline ")
        .and_then(|rest| rest.strip_suffix(")."))
        .and_then(|rest| rest.split_once(" failed ("))
    {
        // One sentence per action, so the action is worded in the language too.
        let args: [(&str, &dyn std::fmt::Display); 1] = [("code", &code)];
        match action {
            "inbox" => return Some(tf("PlayOnline could not load the inbox ({code}).", &args)),
            "check" => return Some(tf("PlayOnline could not check for new messages ({code}).", &args)),
            "read" => return Some(tf("PlayOnline could not open the message ({code}).", &args)),
            "send" => return Some(tf("PlayOnline could not send the message ({code}).", &args)),
            // Also an unblock (list B): the stored text does not say which.
            "remove" => return Some(tf("PlayOnline could not remove this entry ({code}).", &args)),
            _ => {}
        }
    }
    None
}

/// A social request's kind: the loader's `action` word and the cache's `State.action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Inbox,
    Check,
    Read,
    Send,
    Remove,
}
impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Check => "check",
            Self::Read => "read",
            Self::Send => "send",
            Self::Remove => "remove",
        }
    }
    pub fn parse(word: &str) -> Option<Self> {
        [Self::Inbox, Self::Check, Self::Read, Self::Send, Self::Remove].into_iter().find(|a| a.as_str() == word)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum Request {
    Inbox {
        #[serde(default)]
        folder: u8,
        #[serde(default)]
        page: u32,
    },
    /// The Viewer's Check: fetch, save and acknowledge new mail, then show Inbox page 1.
    Check {},
    Read {
        header: String,
    },
    Send {
        alternate: bool,
        slot: u16,
        uid: u64,
        subject: String,
        message: String,
    },
    Remove {
        alternate: bool,
        slot: u16,
        uid: u64,
    },
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Self::Send {
            subject, message, ..
        } = self
        {
            subject.zeroize();
            message.zeroize();
        }
    }
}
impl Request {
    pub fn action(&self) -> Action {
        match self {
            Self::Inbox { .. } => Action::Inbox,
            Self::Check {} => Action::Check,
            Self::Read { .. } => Action::Read,
            Self::Send { .. } => Action::Send,
            Self::Remove { .. } => Action::Remove,
        }
    }
    pub fn valid(&self) -> bool {
        match self {
            Self::Inbox { folder, page } => *folder <= 2 && *page <= 4096,
            Self::Check {} => true,
            Self::Read { header } => {
                header.len() == crate::b64::encoded_len(crate::wire::SOCIAL_HEADER_LEN)
                    && crate::pol_less::pipe::base64_decode(header).is_some_and(|b| b.len() == crate::wire::SOCIAL_HEADER_LEN)
            }
            // List B is the block list; the Viewer never mails it.
            Self::Send {
                alternate,
                slot,
                uid,
                subject,
                message,
            } => {
                !*alternate
                    && valid_friend(*alternate, *slot, *uid)
                    && subject.len() <= 512
                    && message.len() <= 16384
                    && !message.trim().is_empty()
                    && [subject, message].iter().all(|s| {
                        !s.chars()
                            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                    })
            }
            Self::Remove {
                alternate,
                slot,
                uid,
            } => valid_friend(*alternate, *slot, *uid),
        }
    }
}
fn valid_friend(alternate: bool, slot: u16, uid: u64) -> bool {
    uid != 0 && (slot as usize) < if alternate { 100 } else { 200 }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Header {
    pub raw: String,
    pub sender: String,
    pub subject: String,
    pub unread: bool,
    pub kind: u8,
    #[serde(default)]
    pub recipient: Option<u64>,
}
/// A stored message's identity whatever its flags (reading one changes its
/// header): the uids, the message id at +0x30 and the handles, the fields the
/// loader's `same_message` compares (app 1005245d).
pub(crate) fn message_key(raw: &str) -> String {
    match crate::pol_less::pipe::base64_decode(raw).filter(|b| b.len() == crate::wire::SOCIAL_HEADER_LEN) {
        Some(b) => b[..0x10].iter().chain(&b[0x30..0x38]).chain(&b[0x3c..0x3e]).map(|x| format!("{x:02x}")).collect(),
        None => raw.to_owned(),
    }
}

impl Header {
    fn bytes(&self) -> Option<Vec<u8>> {
        crate::pol_less::pipe::base64_decode(&self.raw).filter(|b| b.len() == crate::wire::SOCIAL_HEADER_LEN)
    }
    /// The sender's PlayOnline uid (+0x00; the recipient's is +0x08).
    pub fn sender_uid(&self) -> Option<u64> {
        self.bytes().map(|b| u64::from_le_bytes(b[..8].try_into().unwrap())).filter(|uid| *uid != 0)
    }
    /// +0x34, the field the Viewer's message list sorts by, newest first
    /// (loader native_social: `Reverse(u32_at(header, 0x34))`). Read as a
    /// Unix time only when it is a plausible one (`friends::message_time`).
    pub fn stamp(&self) -> u32 {
        self.bytes().map_or(0, |b| u32::from_le_bytes(b[0x34..0x38].try_into().unwrap()))
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub ready: bool,
    pub busy: bool,
    pub id: Option<String>,
    pub action: Option<Action>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub headers: Vec<Header>,
    pub opened: Option<Opened>,
    pub pending_header: Option<String>,
    pub folder: u8,
    pub page: u32,
    pub total: u32,
    #[serde(default)]
    pub inbox_unread: Option<u32>,
    #[serde(default)]
    pub inbox_changed: bool,
    pub pending_folder: Option<u8>,
    pub pending_page: Option<u32>,
    /// The pending remove is an unblock (list B).
    #[serde(default)]
    pub pending_unblock: bool,
    /// The list A entry (slot, uid) the pending remove is for.
    #[serde(default)]
    pub pending_remove: Option<(u16, u64)>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Opened {
    pub sender: String,
    pub subject: String,
    pub message: String,
    #[serde(default)]
    pub recipient: Option<u64>,
}

impl State {
    pub fn begin(&mut self, id: String, request: &Request) {
        self.busy = true;
        self.id = Some(id);
        self.action = Some(request.action());
        self.pending_header = if let Request::Read { header } = request {
            Some(header.clone())
        } else {
            None
        };
        self.error = None;
        self.notice = None;
        match request {
            Request::Inbox { folder, page } => {
                self.pending_folder = Some(*folder);
                self.pending_page = Some(*page);
            }
            Request::Check {} => {
                self.pending_folder = Some(0);
                self.pending_page = Some(0);
            }
            _ => {}
        }
        self.pending_unblock = matches!(request, Request::Remove { alternate: true, .. });
        self.pending_remove = match request {
            Request::Remove { alternate: false, slot, uid } => Some((*slot, *uid)),
            _ => None,
        };
    }
    pub fn apply(
        &mut self,
        id: &str,
        action: Action,
        code: i32,
        payload: Option<&[u8]>,
    ) -> Result<(), String> {
        if self.id.as_deref() != Some(id) || self.action != Some(action) {
            return Ok(());
        }
        self.busy = code == 0;
        if code <= 0 {
            self.error = (code < 0).then(|| if action == Action::Send && code == -5120 {
                "PlayOnline could not encode this message. Shorten it or remove unsupported characters (-5120).".into()
            } else { format!("PlayOnline {} failed ({code}).", action.as_str()) });
            return Ok(());
        }
        match action {
            Action::Inbox | Action::Check => {
                let payload = payload.ok_or("PlayOnline returned no message headers.")?;
                if payload.len() != 12 + 32 * 72 {
                    return Err("Invalid native inbox page.".into());
                }
                let total = u32::from_le_bytes(payload[..4].try_into().unwrap());
                let page = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                let unread = u32::from_le_bytes(payload[8..12].try_into().unwrap());
                let last_page = total.div_ceil(32).saturating_sub(1);
                if total > 100_000
                    || unread > total
                    || self.pending_page.map(|p| p.min(last_page)) != Some(page)
                {
                    return Err("Invalid native inbox page.".into());
                }
                let headers = headers(payload[12..].try_into().unwrap())?;
                if headers.len() != total.saturating_sub(page * 32).min(32) as usize {
                    return Err("Incomplete native inbox page.".into());
                }
                if self.page != page || self.pending_folder != Some(self.folder) {
                    self.opened = None;
                }
                self.headers = headers;
                self.page = page;
                self.total = total;
                self.folder = self.pending_folder.ok_or("This inbox request has ended.")?;
                if self.folder == 0 {
                    self.inbox_unread = Some(unread);
                    self.inbox_changed = false;
                }
            }
            Action::Read => {
                let raw = self
                    .pending_header
                    .as_ref()
                    .ok_or("This message request has ended.")?;
                let header = self
                    .headers
                    .iter_mut()
                    .find(|h| &h.raw == raw)
                    .ok_or("This message is no longer in the inbox.")?;
                // Code 2: stored and now read, but a system or attachment
                // format (friend/group requests and responses) this view lacks.
                let (subject, message) = if code == 2 {
                    (
                        header.subject.clone(),
                        format!("This PlayOnline message (type {}) can't be shown in Crystal. It is saved with your messages.", header.kind),
                    )
                } else {
                    let payload = payload.ok_or("PlayOnline returned no message text.")?;
                    let end = payload
                        .iter()
                        .position(|b| *b == 0)
                        .ok_or("PlayOnline returned an invalid message.")?;
                    let separator = payload[..end]
                        .iter()
                        .position(|b| *b == 7)
                        .ok_or("PlayOnline returned an unsupported message format.")?;
                    (
                        crate::friends::decode_name(&payload[..separator]),
                        crate::friends::decode_name(&payload[separator + 1..end]),
                    )
                };
                self.opened = Some(Opened {
                    sender: header.sender.clone(),
                    subject,
                    message,
                    recipient: header.recipient,
                });
                if header.unread && self.folder == 0 {
                    self.inbox_unread = self.inbox_unread.map(|n| n.saturating_sub(1));
                }
                header.unread = false;
            }
            Action::Send => {
                self.notice = Some(if let Some(warning) = payload {
                    let code = std::str::from_utf8(warning)
                        .ok()
                        .and_then(|s| s.parse::<i32>().ok())
                        .ok_or("Invalid native storage result.")?;
                    format!("Message sent by PlayOnline; the local Sent copy could not be stored ({code}). Do not send it again.")
                } else {
                    "Message sent by PlayOnline.".into()
                })
            }
            Action::Remove => self.notice = Some(if self.pending_unblock { "Unblocked by PlayOnline." } else { "Removed from your PlayOnline friends." }.into()),
        }
        self.pending_header = None;
        Ok(())
    }
    pub fn valid(&self) -> bool {
        self.folder <= 2
            && self.page <= 4096
            && self.total <= 100_000
            && self.inbox_unread.is_none_or(|n| n <= 100_000)
            && self.headers.len() <= 32
            && self
                .headers
                .iter()
                .all(|h| h.raw.len() == 96 && h.sender.len() <= 256 && h.subject.len() <= 256)
            && self
                .id
                .as_ref()
                .is_none_or(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && self.error.as_ref().is_none_or(|s| s.len() <= 4096)
            && self.notice.as_ref().is_none_or(|s| s.len() <= 4096)
            && self.pending_folder.is_none_or(|f| f <= 2)
            && self.pending_page.is_none_or(|p| p <= 4096)
            && self.pending_header.as_ref().is_none_or(|s| s.len() == 96)
            && self.pending_remove.is_none_or(|(slot, uid)| valid_friend(false, slot, uid))
            && self.opened.as_ref().is_none_or(|o| {
                o.sender.len() <= 256 && o.subject.len() <= 512 && o.message.len() <= 16384
            })
    }
}

fn headers(bytes: &[u8; 32 * 72]) -> Result<Vec<Header>, String> {
    let mut headers = Vec::new();
    for record in bytes.as_chunks::<72>().0 {
        if record.iter().all(|b| *b == 0) {
            continue;
        }
        if !record[16..32].contains(&0) || !record[32..48].contains(&0) {
            return Err("Invalid native PlayOnline message header.".into());
        }
        let name = crate::friends::c_name;
        let mut raw = Vec::new();
        crate::pol_less::pipe::base64_into(&mut raw, record);
        let flags = u16::from_le_bytes(record[0x3e..0x40].try_into().unwrap());
        headers.push(Header {
            raw: String::from_utf8(raw).unwrap(),
            sender: name(&record[16..32]),
            subject: name(&record[32..48]),
            unread: flags & 0x8000 != 0,
            kind: ((flags >> 7) & 31) as u8,
            recipient: Some(u64::from_le_bytes(record[8..16].try_into().unwrap()))
                .filter(|uid| *uid != 0),
        });
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "0123456789abcdef0123456789abcdef";
    fn inbox() -> Vec<u8> {
        let mut payload = vec![0; 12 + 32 * 72];
        payload[..4].copy_from_slice(&1u32.to_le_bytes());
        payload[8..12].copy_from_slice(&1u32.to_le_bytes());
        let bytes = &mut payload[12..];
        bytes[..8].copy_from_slice(&7u64.to_le_bytes());
        bytes[16..22].copy_from_slice(b"Alice\0");
        bytes[32..38].copy_from_slice(b"Hello\0");
        bytes[0x3e..0x40].copy_from_slice(&(0x8000u16 | (17 << 7)).to_le_bytes());
        payload
    }
    #[test]
    fn native_completion_is_correlated_and_only_a_successful_read_marks_read() {
        let mut state = State::default();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        state.apply(ID, Action::Send, 1, None).unwrap();
        assert!(state.busy);
        state
            .apply(
                "ffffffffffffffffffffffffffffffff",
                Action::Inbox,
                1,
                Some(&inbox()),
            )
            .unwrap();
        assert!(state.busy);
        state.apply(ID, Action::Inbox, 0, None).unwrap();
        assert!(state.busy);
        state.apply(ID, Action::Inbox, 1, Some(&inbox())).unwrap();
        assert!(!state.busy);
        assert_eq!(state.headers[0].sender, "Alice");
        assert!(state.headers[0].unread);
        let read = Request::Read {
            header: state.headers[0].raw.clone(),
        };
        state.begin(ID.into(), &read);
        state.apply(ID, Action::Read, -5326, None).unwrap();
        assert!(state.headers[0].unread);
        assert!(state.error.as_ref().unwrap().contains("-5326"));
        state.begin(ID.into(), &read);
        state
            .apply(ID, Action::Read, 1, Some(b"Title\x07Text\nLine\0"))
            .unwrap();
        assert!(!state.headers[0].unread);
        assert_eq!(state.opened.as_ref().unwrap().message, "Text\nLine");
    }
    #[test]
    fn malformed_results_keep_the_previous_inbox_and_reject_non_text_bodies() {
        let mut state = State::default();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        state.apply(ID, Action::Inbox, 1, Some(&inbox())).unwrap();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        assert!(state.apply(ID, Action::Inbox, 1, Some(&[0; 72])).is_err());
        assert_eq!(state.headers.len(), 1);
        let mut invalid = inbox();
        invalid[28..44].fill(b'A');
        assert!(headers(invalid[12..].try_into().unwrap()).is_err());
        let read = Request::Read {
            header: state.headers[0].raw.clone(),
        };
        state.begin(ID.into(), &read);
        assert!(state.apply(ID, Action::Read, 1, Some(b"no separator\0")).is_err());
        assert!(state.opened.is_none());
        assert!(state.headers[0].unread);
    }
    #[test]
    fn requests_bound_native_slots_and_preserve_u64_recipients_and_json_text() {
        let send = Request::Send {
            alternate: false,
            slot: 199,
            uid: u64::MAX,
            subject: "あ\"subject".into(),
            message: "line\nline".into(),
        };
        assert!(send.valid());
        let encoded = serde_json::to_vec(&send).unwrap();
        assert!(serde_json::from_slice::<Request>(&encoded).unwrap().valid());
        assert!(!Request::Remove {
            alternate: true,
            slot: 100,
            uid: 1
        }
        .valid());
        assert!(!Request::Remove {
            alternate: false,
            slot: 200,
            uid: 1
        }
        .valid());
        assert!(!Request::Remove {
            alternate: false,
            slot: 0,
            uid: 0
        }
        .valid());
        assert!(!Request::Send {
            alternate: false,
            slot: 0,
            uid: 1,
            subject: "a\x07b".into(),
            message: "message".into()
        }
        .valid());
    }
    #[test]
    fn unsupported_messages_open_as_read_and_the_block_list_is_never_mailed() {
        let mut state = State::default();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        state.apply(ID, Action::Inbox, 1, Some(&inbox())).unwrap();
        state.inbox_unread = Some(1);
        let read = Request::Read { header: state.headers[0].raw.clone() };
        state.begin(ID.into(), &read);
        state.apply(ID, Action::Read, 2, None).unwrap();
        assert!(!state.headers[0].unread);
        assert_eq!(state.inbox_unread, Some(0));
        assert!(state.opened.as_ref().unwrap().message.contains("type 17"));
        assert!(state.error.is_none());
        assert!(!Request::Send { alternate: true, slot: 0, uid: 1, subject: String::new(), message: "m".into() }.valid());
        assert!(Request::Remove { alternate: true, slot: 99, uid: 1 }.valid(), "unblock");
        state.begin(ID.into(), &Request::Remove { alternate: true, slot: 0, uid: 1 });
        state.apply(ID, Action::Remove, 1, None).unwrap();
        assert_eq!(state.notice.as_deref(), Some("Unblocked by PlayOnline."));
        state.begin(ID.into(), &Request::Remove { alternate: false, slot: 0, uid: 1 });
        state.apply(ID, Action::Remove, 1, None).unwrap();
        assert_eq!(state.notice.as_deref(), Some("Removed from your PlayOnline friends."));
    }

    /// Every error, notice and system text `State` stores (in English) is one
    /// `localize` words in the current language.
    #[test]
    fn every_stored_social_text_is_localized() {
        let mut stored: Vec<String> = Vec::new();
        let mut keep = |state: &State, result: Result<(), String>| {
            stored.extend(result.err());
            stored.extend(state.error.clone());
            stored.extend(state.notice.clone());
            stored.extend(state.opened.as_ref().map(|o| o.message.clone()).filter(|m| m.starts_with("This PlayOnline")));
        };
        let read = |state: &mut State| {
            let header = state.headers[0].raw.clone();
            state.begin(ID.into(), &Request::Read { header });
        };
        // Failure codes of every action, and the send encoding failure.
        for action in [Action::Inbox, Action::Check, Action::Read, Action::Send, Action::Remove] {
            let mut state = State::default();
            state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
            state.action = Some(action);
            let result = state.apply(ID, action, -5326, None);
            keep(&state, result);
        }
        let mut state = State::default();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        state.action = Some(Action::Send);
        let result = state.apply(ID, Action::Send, -5120, None);
        keep(&state, result);
        // Inbox results.
        for payload in [None, Some(vec![0; 10]), Some(inbox()[..12].iter().copied().chain([0; 32 * 72]).collect::<Vec<u8>>())] {
            let mut state = State::default();
            state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
            let result = state.apply(ID, Action::Inbox, 1, payload.as_deref());
            keep(&state, result);
        }
        let mut bad_header = inbox();
        bad_header[12 + 16..12 + 32].fill(b'A');
        let mut state = State::default();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        let result = state.apply(ID, Action::Inbox, 1, Some(&bad_header));
        keep(&state, result);
        // Read results.
        let mut state = State::default();
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
        state.apply(ID, Action::Inbox, 1, Some(&inbox())).unwrap();
        for (code, payload) in [(1, None), (1, Some(&b"no end"[..])), (1, Some(&b"no separator\0"[..])), (2, None)] {
            read(&mut state);
            let result = state.apply(ID, Action::Read, code, payload);
            keep(&state, result);
        }
        read(&mut state);
        state.headers.clear();
        let result = state.apply(ID, Action::Read, 1, None);
        keep(&state, result);
        // Send and remove results.
        for payload in [None, Some(&b"-3"[..]), Some(&b"x"[..])] {
            state.begin(ID.into(), &Request::Inbox { folder: 0, page: 0 });
            state.action = Some(Action::Send);
            let result = state.apply(ID, Action::Send, 1, payload);
            keep(&state, result);
        }
        for alternate in [true, false] {
            state.begin(ID.into(), &Request::Remove { alternate, slot: 0, uid: 1 });
            let result = state.apply(ID, Action::Remove, 1, None);
            keep(&state, result);
        }
        stored.push("Could not encode the PlayOnline request.".into());
        assert!(stored.len() >= 20, "{stored:#?}");
        for text in &stored {
            assert!(localized(text).is_some(), "not localized: {text:?}");
        }
        assert_eq!(localize("A server's own words"), "A server's own words");
    }

    #[test]
    fn a_check_shows_inbox_page_one_with_what_it_fetched() {
        assert_eq!(Request::Check {}.action(), Action::Check);
        assert_eq!(Action::parse("check"), Some(Action::Check));
        assert_eq!(Action::parse("unknown"), None);
        assert!(Request::Check {}.valid());
        let mut state = State::default();
        state.begin(ID.into(), &Request::Check {});
        assert_eq!((state.pending_folder, state.pending_page), (Some(0), Some(0)));
        state.apply(ID, Action::Check, 0, None).unwrap();
        assert!(state.busy);
        state.apply(ID, Action::Check, 1, Some(&inbox())).unwrap();
        assert!(!state.busy);
        assert_eq!(state.folder, 0);
        assert!(!state.headers.is_empty());
    }

    #[test]
    fn unread_count_covers_other_pages_and_sent_reads_do_not_change_it() {
        let mut state = State::default();
        let mut payload = inbox();
        payload[..4].copy_from_slice(&65u32.to_le_bytes());
        payload[4..8].copy_from_slice(&2u32.to_le_bytes());
        payload[8..12].copy_from_slice(&41u32.to_le_bytes());
        state.begin(ID.into(), &Request::Inbox { folder: 0, page: 2 });
        state.apply(ID, Action::Inbox, 1, Some(&payload)).unwrap();
        assert_eq!(state.inbox_unread, Some(41));
        assert_eq!(state.headers.len(), 1);
        let read = Request::Read {
            header: state.headers[0].raw.clone(),
        };
        state.begin(ID.into(), &read);
        state
            .apply(ID, Action::Read, 1, Some(b"Title\x07Body\0"))
            .unwrap();
        assert_eq!(state.inbox_unread, Some(40));
        state.begin(ID.into(), &read);
        state
            .apply(ID, Action::Read, 1, Some(b"Title\x07Body\0"))
            .unwrap();
        assert_eq!(state.inbox_unread, Some(40));
        state.begin(ID.into(), &Request::Inbox { folder: 2, page: 2 });
        state.apply(ID, Action::Inbox, 1, Some(&inbox())).unwrap(); // native clamp after folder shrink
        assert_eq!(state.page, 0);
        assert_eq!(state.inbox_unread, Some(40));
        let read = Request::Read {
            header: state.headers[0].raw.clone(),
        };
        state.begin(ID.into(), &read);
        state
            .apply(ID, Action::Read, 1, Some(b"Title\x07Body\0"))
            .unwrap();
        assert_eq!(state.inbox_unread, Some(40));
    }
}
