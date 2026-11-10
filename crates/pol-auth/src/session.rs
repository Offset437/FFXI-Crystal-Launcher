use crate::assembly;
use std::io::{self, Write};
use zeroize::Zeroizing;

pub struct SessionState {
    addr20: [u8; 20],
    clock: u32,
    chat_key: Zeroizing<[u8; 8]>,
    host: u8,
}

impl SessionState {
    /// `clock` is the original greeting word stored by native 10019b80.
    /// Credential packets use the advancing ServerClock separately.
    pub fn new(addr20: [u8; 20], clock: u32, chat_key: [u8; 8], host: u8) -> Self {
        Self {
            addr20,
            clock,
            chat_key: Zeroizing::new(chat_key),
            host,
        }
    }
}

#[derive(Clone)]
pub struct LobbySession {
    value: Zeroizing<[u8; 16]>,
    authcode: Zeroizing<[u8; 52]>,
}

impl LobbySession {
    /// Only use a community body returned by an authenticated server exchange.
    /// `reply` excludes the checksum from the native 32-byte community body.
    pub fn from_community(reply: &[u8; 28], state: &SessionState) -> Self {
        let inputs = assembly::AssemblyInputs {
            addr20: &state.addr20,
            clock: state.clock,
            chat_key: &state.chat_key,
            host: state.host,
        };
        Self {
            value: Zeroizing::new(assembly::sixteen_byte_value(reply)),
            authcode: Zeroizing::new(assembly::build_authcode(reply, &inputs)),
        }
    }

    pub fn write_handoff(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"XIRT001\n")?;
        writer.write_all(&*self.value)?;
        writer.write_all(&*self.authcode)
    }
}

impl std::fmt::Debug for LobbySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LobbySession([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_handoff_has_the_exact_existing_host_format() {
        let state = SessionState::new([0x20; 20], 123456, [1; 8], 7);
        let session = LobbySession::from_community(&[0x11; 28], &state);
        let mut handoff = Vec::new();
        session.write_handoff(&mut handoff).unwrap();
        assert_eq!(handoff.len(), 76);
        assert_eq!(&handoff[..8], b"XIRT001\n");
        assert_eq!(&handoff[8..24], &[0x11; 16]);
        assert_eq!(format!("{session:?}"), "LobbySession([redacted])");
    }
}
