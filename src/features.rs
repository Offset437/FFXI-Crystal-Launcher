//! Launcher features that sit beside sign-in and Play: window layouts, client hotkeys, crash
//! recovery, USER-data backups, desktop notifications, per-client performance, the launch
//! profile picker, and play time with optional Discord presence.
//!
//! Every module here is toolkit-agnostic: plain types, functions and events. The interface binds
//! to them; nothing here draws. Their saved choices live in one file (`store`), apart from the
//! window's settings.json, so either file can be read by any older or newer Crystal.
//!
//! Nothing here sends anything over the network except Discord's local IPC pipe (off by
//! default). No module polls: work happens when the interface reports a game start or end,
//! when a WinEvent or hotkey arrives on the one desktop thread (started only while something
//! needs it), or when a session's published file changes.

pub(crate) mod backups;
pub(crate) mod desktop;
pub(crate) mod discord;
pub(crate) mod game_window;
pub(crate) mod hotkeys;
pub(crate) mod keyhook;
pub(crate) mod layouts;
pub(crate) mod notify;
pub(crate) mod order;
pub(crate) mod perf;
pub(crate) mod playtime;
pub(crate) mod profiles;
pub(crate) mod recovery;
pub(crate) mod runtime;
pub(crate) mod store;
pub(crate) mod ui;
mod wiring;

pub(crate) use runtime::{Event, Runtime};
pub(crate) use store::FeatureSettings;

/// The client a feature remembers things for: the account, and the character slot its automatic
/// character login picks (absent: the account's character select). Crystal does not read game
/// memory, so the slot is the finest identity it knows before the game runs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClientKey {
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
}

impl ClientKey {
    pub(crate) fn new(account: impl Into<String>, slot: Option<u8>) -> Self {
        Self { account: account.into(), slot: slot.filter(|s| (1..=16).contains(s)) }
    }

    /// The text form used as a map key in the saved file: `<account>` or `<account>#<slot>`.
    pub(crate) fn id(&self) -> String {
        match self.slot {
            Some(slot) => format!("{}#{slot}", self.account),
            None => self.account.clone(),
        }
    }

    pub(crate) fn parse(id: &str) -> Option<Self> {
        let (account, slot) = match id.rsplit_once('#') {
            Some((account, slot)) => (account, Some(slot.parse::<u8>().ok().filter(|s| (1..=16).contains(s))?)),
            None => (id, None),
        };
        (!account.is_empty()).then(|| Self { account: account.to_owned(), slot })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_keys_round_trip_through_their_text_form() {
        let plain = ClientKey::new("abc", None);
        let slotted = ClientKey::new("abc", Some(3));
        assert_eq!(ClientKey::parse(&plain.id()), Some(plain));
        assert_eq!(ClientKey::parse(&slotted.id()), Some(slotted.clone()));
        assert_eq!(slotted.id(), "abc#3");
        // A slot outside 1..=16 is no slot.
        assert_eq!(ClientKey::new("abc", Some(0)).slot, None);
        assert_eq!(ClientKey::parse("abc#99"), None);
        assert_eq!(ClientKey::parse(""), None);
    }
}
