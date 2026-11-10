//! Chat registration over a caller-owned connection.
//!
//! Ordering follows installed polcore 100140e0/10015e80 and the pinned
//! upstream transport reference. No sockets, account lookup or system RNG.
//! Account-specific suffix material and native uninitialized address padding
//! remain explicit inputs. Mock registration is not retail interoperability.

use crate::{
    chat::{self, Greeting, GreetingChecksum, RegistrationMetadata, ServerClock},
    crypto::{PolBlowfish, StreamReset},
    identity::{self, NickSuffix},
    rsa::{KeyPair, RsaError},
    Error,
};
use std::io::{self, BufRead, BufReader, Read, Write};
use zeroize::Zeroizing;

pub struct Inputs<'a> {
    pub account_id: u64,
    pub pol_secret: &'a [u8],
    pub suffix: &'a NickSuffix,
    pub user_mode: u8,
    pub address_tail: [u8; 12],
    pub initial_metadata: RegistrationMetadata,
    /// Caller-owned memory limit. This is not a claimed server restriction.
    pub maximum_line_bytes: usize,
}

#[derive(Debug)]
pub enum HandshakeError {
    Io(io::ErrorKind),
    Chat(chat::ChatError),
    Protocol(Error),
    Rsa(RsaError),
    LineLimit,
    UnexpectedReply,
    Service(u16),
    ServerError,
    Redirect,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "chat channel error: {kind:?}"),
            Self::Chat(_) => f.write_str("invalid chat reply"),
            Self::Protocol(e) => e.fmt(f),
            Self::Rsa(_) => f.write_str("chat key negotiation failed"),
            Self::LineLimit => f.write_str("chat line exceeds caller memory limit"),
            Self::UnexpectedReply => f.write_str("unexpected chat negotiation reply"),
            Self::Service(code) => write!(f, "chat service status {code}"),
            Self::ServerError => f.write_str("chat server ended registration"),
            Self::Redirect => f.write_str("chat greeting requests another connection"),
        }
    }
}
impl std::error::Error for HandshakeError {}
impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}
impl From<chat::ChatError> for HandshakeError {
    fn from(e: chat::ChatError) -> Self {
        Self::Chat(e)
    }
}
impl From<Error> for HandshakeError {
    fn from(e: Error) -> Self {
        Self::Protocol(e)
    }
}
impl From<RsaError> for HandshakeError {
    fn from(e: RsaError) -> Self {
        Self::Rsa(e)
    }
}

/// Keeps the registered connection alive under its caller's ownership. The
/// greeting clock is immutable; credential time uses the separate clock anchor.
/// No Debug implementation exposes the negotiated key or account metadata.
pub struct Registered<C> {
    channel: BufReader<C>,
    cipher: PolBlowfish,
    pub greeting: Greeting,
    pub clock: ServerClock,
    pub metadata: RegistrationMetadata,
    pub metadata_received: bool,
    pub address: [u8; 20],
    pub session_key: Zeroizing<[u8; 8]>,
    pub iv: (u32, u32),
    maximum_line_bytes: usize,
}

impl<C: Read + Write> Registered<C> {
    /// Caller-owned connection access for managed shutdown and heartbeat clones.
    pub fn channel_ref(&self) -> &C {
        self.channel.get_ref()
    }

    /// Read and verify the next registered message; buffered bytes are retained
    /// across registration. Caller owns timeout/cancellation/heartbeat policy.
    pub fn read_message(&mut self) -> Result<Zeroizing<Vec<u8>>, HandshakeError> {
        let raw = read_line(&mut self.channel, self.maximum_line_bytes)?;
        let plain = Zeroizing::new(self.cipher.stream(&raw, StreamReset::Boundary));
        identity::verified_line_body(&plain)?;
        Ok(plain)
    }

    pub fn send_command(&mut self, body: &[u8]) -> Result<(), HandshakeError> {
        let framed = identity::frame_line(body)?;
        let encrypted = Zeroizing::new(self.cipher.stream(&framed, StreamReset::Boundary));
        self.channel.get_mut().write_all(&encrypted)?;
        self.channel.get_mut().flush()?;
        Ok(())
    }
}

/// Run greeting -> USER -> key300 -> encrypted NICK -> registration replies.
/// `key` must be a freshly generated keypair supplied by the caller. The
/// function preserves metadata received before422; absent metadata retains
/// explicit initial state, matching native's separate metadata buffer.
pub fn register<C: Read + Write>(
    channel: C,
    key: KeyPair,
    inputs: Inputs<'_>,
    monotonic_ms: impl FnMut() -> u32,
) -> Result<Registered<C>, HandshakeError> {
    register_reconnecting(
        channel,
        key,
        inputs,
        |_| Err(HandshakeError::Redirect),
        monotonic_ms,
    )
}

/// Follow the primary-chat greeting redirect exactly once, before sending USER
/// or NICK. Native preserves its RSA key, closes the old channel, discards its
/// queued bytes/challenge, and accepts a fresh greeting with retryflag2.
/// The connector owns the actual socket and timeout policy; no credential
/// rejection or post-USER failure is automatically retried here.
pub fn register_reconnecting<C: Read + Write>(
    channel: C,
    key: KeyPair,
    inputs: Inputs<'_>,
    reconnect: impl FnOnce(chat::Redirect) -> Result<C, HandshakeError>,
    mut monotonic_ms: impl FnMut() -> u32,
) -> Result<Registered<C>, HandshakeError> {
    // Validate before consuming a channel or sending an account request.
    if inputs.pol_secret.len() > 15 || inputs.pol_secret.contains(&0) {
        return Err(Error::InvalidFrame.into());
    }
    if inputs.maximum_line_bytes < 6 {
        return Err(HandshakeError::LineLimit);
    }
    let mut channel = BufReader::new(channel);
    let raw_greeting = read_line(&mut channel, inputs.maximum_line_bytes)?;
    let mut greeting = chat::parse_greeting(&raw_greeting, GreetingChecksum::NativeInitial)?;
    if let Some(target) = greeting.redirect() {
        // Drop the initial socket and any read-ahead before opening its target.
        drop(channel);
        channel = BufReader::new(reconnect(target)?);
        let raw_greeting = read_line(&mut channel, inputs.maximum_line_bytes)?;
        greeting = chat::parse_greeting(&raw_greeting, GreetingChecksum::NativeInitial)?;
    }
    let clock = ServerClock::new(greeting.server_clock, monotonic_ms());
    let address = greeting.address_with_tail(inputs.address_tail);
    let user = identity::frame_line(
        format!("USER x {} * :{}", inputs.user_mode & 15, key.realname()).as_bytes(),
    )?;
    channel.get_mut().write_all(&user)?;
    channel.get_mut().flush()?;

    let raw_key = read_line(&mut channel, inputs.maximum_line_bytes)?;
    let key_body = plaintext_body(&raw_key)?;
    let (command, arguments) = command_tail(key_body)?;
    if command != b"300" {
        return Err(numeric(command)
            .map(HandshakeError::Service)
            .unwrap_or(HandshakeError::UnexpectedReply));
    }
    let (_, payload) = command_tail(arguments)?;
    let session_key = Zeroizing::new(key.recover_session_key(payload)?);
    let iv = key.stream_iv();
    let mut cipher = PolBlowfish::new(&session_key[..], iv);
    let nick = identity::build_nick(
        inputs.account_id,
        &greeting.challenge,
        inputs.pol_secret,
        inputs.suffix,
    )?;
    let encrypted = Zeroizing::new(cipher.stream(&nick, StreamReset::Boundary));
    channel.get_mut().write_all(&encrypted)?;
    channel.get_mut().flush()?;

    let mut metadata = inputs.initial_metadata;
    let mut metadata_received = false;
    loop {
        let raw = read_line(&mut channel, inputs.maximum_line_bytes)?;
        let plain = Zeroizing::new(cipher.stream(&raw, StreamReset::Boundary));
        let body = identity::verified_line_body(&plain)?;
        let (command, _) = command_tail(body)?;
        if command == b"ERROR" {
            return Err(HandshakeError::ServerError);
        }
        match numeric(command) {
            Some(300) => {
                let prior = metadata.converted[12..16].try_into().unwrap();
                metadata = chat::parse_registration_metadata(&plain, prior)?;
                metadata_received = true;
            }
            Some(422) => break,
            Some(status) if status >= 400 => return Err(HandshakeError::Service(status)),
            _ => (),
        }
    }
    Ok(Registered {
        channel,
        cipher,
        greeting,
        clock,
        metadata,
        metadata_received,
        address,
        session_key,
        iv,
        maximum_line_bytes: inputs.maximum_line_bytes,
    })
}

fn read_line<C: Read>(
    channel: &mut BufReader<C>,
    maximum: usize,
) -> Result<Zeroizing<Vec<u8>>, HandshakeError> {
    let mut line = Zeroizing::new(Vec::new());
    loop {
        let available = channel.fill_buf()?;
        if available.is_empty() {
            return Err(HandshakeError::Io(io::ErrorKind::UnexpectedEof));
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if count > maximum.saturating_sub(line.len()) {
            return Err(HandshakeError::LineLimit);
        }
        line.extend_from_slice(&available[..count]);
        channel.consume(count);
        if line.last() == Some(&b'\n') {
            return Ok(line);
        }
    }
}

fn plaintext_body(raw: &[u8]) -> Result<&[u8], HandshakeError> {
    let line = raw
        .strip_suffix(b"\r\n")
        .ok_or(HandshakeError::UnexpectedReply)?;
    if line.len() < 4 || line.iter().any(|b| !(32..=126).contains(b)) {
        return Err(HandshakeError::UnexpectedReply);
    }
    // Before cipher-on native does not verify the line checksum.
    Ok(&line[..line.len() - 4])
}

fn command_tail(mut body: &[u8]) -> Result<(&[u8], &[u8]), HandshakeError> {
    fn skip(body: &[u8]) -> Result<&[u8], HandshakeError> {
        let index = body
            .iter()
            .position(|b| *b == b' ')
            .ok_or(HandshakeError::UnexpectedReply)?;
        let tail = &body[index..];
        let next = tail
            .iter()
            .position(|b| *b != b' ')
            .ok_or(HandshakeError::UnexpectedReply)?;
        Ok(&tail[next..])
    }
    if body.starts_with(b":") {
        body = skip(body)?;
    }
    if body.is_empty() {
        return Err(HandshakeError::UnexpectedReply);
    }
    let end = body.iter().position(|b| *b == b' ').unwrap_or(body.len());
    let command = &body[..end];
    let tail = &body[end..];
    let next = tail.iter().position(|b| *b != b' ').unwrap_or(tail.len());
    Ok((command, &tail[next..]))
}

fn numeric(command: &[u8]) -> Option<u16> {
    if command.len() != 3 || !command.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        command
            .iter()
            .fold(0u16, |n, b| n * 10 + u16::from(b - b'0')),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rsa::RandomBytes;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    struct FixtureRandom(usize);
    impl RandomBytes for FixtureRandom {
        fn fill(&mut self, out: &mut [u8]) -> crate::rsa::Result<()> {
            out.fill(0);
            match self.0 {
                0 => {
                    out[0] = 45;
                    out[15] = 0x80;
                }
                25 => {
                    out[0] = 233;
                    out[15] = 0x81;
                }
                1..=24 | 26..=49 => out[0] = 2,
                _ => return Err(RsaError::RandomSource),
            }
            self.0 += 1;
            Ok(())
        }
    }
    struct Channel {
        input: Cursor<Vec<u8>>,
        output: Arc<Mutex<Vec<u8>>>,
        fragment: usize,
    }
    impl Read for Channel {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(self.fragment);
            self.input.read(&mut out[..n])
        }
    }
    impl Write for Channel {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    const MODULUS: &str = "6qATTTTTTTTTTTTTTTTTKsjTTTTTTTTTTTTTTTTTAIT";
    const CIPHERTEXT: &str = "KS5hhD7nJNwTKrtLtAtzLL71N__IBiN6eOhDmizgZcj";
    const KEY: [u8; 8] = [1, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
    fn framed_greeting(header: [u8; 24]) -> Vec<u8> {
        let alphabet = b"N43OVHBJ1Y2C0WSXED5QFILRZMUTAPGK";
        let mut encoded = Vec::new();
        let mut accumulator = 0u32;
        let mut bits = 0;
        for b in header {
            accumulator = (accumulator << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                encoded.push(alphabet[((accumulator >> bits) & 31) as usize]);
            }
        }
        if bits != 0 {
            encoded.push(alphabet[((accumulator << (5 - bits)) & 31) as usize]);
        }
        let mut greeting = b"001 recipient ".to_vec();
        greeting.extend(encoded);
        identity::frame_line(&greeting).unwrap().to_vec()
    }
    fn setup(fragment: usize, corrupted: bool) -> (Channel, KeyPair, NickSuffix) {
        let key = KeyPair::generate(&mut FixtureRandom(0)).unwrap();
        assert_eq!(key.realname(), MODULUS);
        let mut header = [0; 24];
        header[..4].copy_from_slice(&123456u32.to_be_bytes());
        header[4..8].copy_from_slice(&[127, 0, 0, 1]);
        header[20..22].copy_from_slice(&51240u16.to_be_bytes());
        let mut input = framed_greeting(header);
        input.extend_from_slice(
            &identity::frame_line(format!("300 recipient {CIPHERTEXT}").as_bytes()).unwrap(),
        );
        let mut server = PolBlowfish::new(&KEY, key.stream_iv());
        let metadata =
            identity::frame_line(b"300 recipient NNNEVNZVNFONS3NY41HEZO1S4A143VEQ3E2D0HZ").unwrap();
        input.extend(server.stream(&metadata, StreamReset::Boundary));
        let mut ready = identity::frame_line(b"422 recipient").unwrap();
        if corrupted {
            ready[1] ^= 1;
        }
        input.extend(server.stream(&ready, StreamReset::Boundary));
        let pending = identity::frame_line(b"300 recipient pending").unwrap();
        input.extend(server.stream(&pending, StreamReset::Boundary));
        let suffix = NickSuffix::from_material(2, &[0; 8], &[0; 82]).unwrap();
        (
            Channel {
                input: Cursor::new(input),
                output: Arc::new(Mutex::new(Vec::new())),
                fragment,
            },
            key,
            suffix,
        )
    }
    fn inputs(suffix: &NickSuffix) -> Inputs<'_> {
        Inputs {
            account_id: 74565,
            pol_secret: b"synthetic",
            suffix,
            user_mode: 0,
            address_tail: [0xa5; 12],
            initial_metadata: RegistrationMetadata { converted: [0; 24] },
            maximum_line_bytes: 4096,
        }
    }
    #[test]
    fn fragmented_handshake_preserves_clock_metadata_cipher_and_pending_input() {
        for fragment in [3, 4096] {
            let (channel, key, suffix) = setup(fragment, false);
            let output = channel.output.clone();
            let mut session = register(channel, key, inputs(&suffix), || 5000).unwrap();
            assert_eq!(*session.session_key, KEY);
            assert_eq!(session.clock.seconds_at(6000), 123457);
            assert!(session.metadata_received);
            assert_eq!(session.metadata.host_index(), 2);
            assert_eq!(&session.address[8..], &[0xa5; 12]);
            let user = identity::frame_line(format!("USER x 0 * :{MODULUS}").as_bytes()).unwrap();
            let output = output.lock().unwrap();
            assert_eq!(&output[..user.len()], &user[..]);
            let mut receiver = PolBlowfish::new(&KEY, session.iv);
            let nick = receiver.stream(&output[user.len()..], StreamReset::Boundary);
            assert_eq!(
                nick,
                *identity::build_nick(74565, &session.greeting.challenge, b"synthetic", &suffix)
                    .unwrap()
            );
            drop(output);
            let pending = session.read_message().unwrap();
            assert_eq!(
                identity::verified_line_body(&pending).unwrap(),
                b"300 recipient pending"
            );
        }
    }

    #[test]
    fn follows_one_redirect_discards_old_buffers_and_keeps_rsa_and_fresh_challenge() {
        for (wire_port, expected_port) in [(0u16, 51240u16), (54321, 54321)] {
            let (mut final_channel, key, suffix) = setup(3, false);
            let mut first_header = [0; 24];
            first_header[..4].copy_from_slice(&99u32.to_be_bytes());
            first_header[4..8].copy_from_slice(&[192, 0, 2, 100]);
            first_header[8..12].copy_from_slice(&[203, 0, 113, 7]);
            first_header[12..14].copy_from_slice(&wire_port.to_be_bytes());
            first_header[20..22].copy_from_slice(&1111u16.to_be_bytes());
            let mut first_bytes = framed_greeting(first_header);
            first_bytes.extend_from_slice(b"stale initial socket bytes\r\n");
            let first_output = Arc::new(Mutex::new(Vec::new()));
            let initial = Channel {
                input: Cursor::new(first_bytes),
                output: first_output.clone(),
                fragment: 4096,
            };
            let old_line_end = final_channel
                .input
                .get_ref()
                .iter()
                .position(|b| *b == b'\n')
                .unwrap()
                + 1;
            let mut last_header = [0; 24];
            last_header[..4].copy_from_slice(&123456u32.to_be_bytes());
            last_header[4..8].copy_from_slice(&[127, 0, 0, 1]);
            last_header[8..12].copy_from_slice(&[203, 0, 113, 8]); // Must not redirect again.
            last_header[12..14].copy_from_slice(&1234u16.to_be_bytes());
            last_header[20..22].copy_from_slice(&51240u16.to_be_bytes());
            final_channel
                .input
                .get_mut()
                .splice(..old_line_end, framed_greeting(last_header));
            let final_output = final_channel.output.clone();
            let mut values = inputs(&suffix);
            values.user_mode = 8;
            let mut session = register_reconnecting(
                initial,
                key,
                values,
                |target| {
                    assert_eq!(
                        target,
                        chat::Redirect {
                            ipv4: [203, 0, 113, 7],
                            port: expected_port
                        }
                    );
                    assert_eq!(Arc::strong_count(&first_output), 1); // Old channel was dropped.
                    assert!(first_output.lock().unwrap().is_empty()); // No credentials sent there.
                    Ok(final_channel)
                },
                || 5000,
            )
            .unwrap();
            assert_eq!(session.greeting.header, last_header);
            assert_eq!(session.clock.seconds_at(6000), 123457);
            assert_eq!(session.address[..8], [1, 0, 40, 200, 1, 0, 0, 127]);
            let out = final_output.lock().unwrap();
            let user = identity::frame_line(format!("USER x 8 * :{MODULUS}").as_bytes()).unwrap();
            assert_eq!(&out[..user.len()], &user[..]);
            let mut receiver = PolBlowfish::new(&KEY, session.iv);
            assert_eq!(
                receiver.stream(&out[user.len()..], StreamReset::Boundary),
                *identity::build_nick(74565, &session.greeting.challenge, b"synthetic", &suffix)
                    .unwrap()
            );
            drop(out);
            assert_eq!(
                identity::verified_line_body(&session.read_message().unwrap()).unwrap(),
                b"300 recipient pending"
            );
        }
    }

    #[test]
    fn failed_redirect_connection_never_sends_credentials_or_registers() {
        let (mut channel, key, suffix) = setup(4096, false);
        let output = channel.output.clone();
        let mut header = [0; 24];
        header[8..12].copy_from_slice(&[203, 0, 113, 7]);
        channel.input = Cursor::new(framed_greeting(header));
        let result = register_reconnecting(
            channel,
            key,
            inputs(&suffix),
            |_| Err(HandshakeError::Io(io::ErrorKind::ConnectionRefused)),
            || 0,
        );
        assert!(matches!(
            result,
            Err(HandshakeError::Io(io::ErrorKind::ConnectionRefused))
        ));
        assert!(output.lock().unwrap().is_empty());
    }

    #[test]
    fn direct_registration_does_not_call_redirect_connector() {
        let (channel, key, suffix) = setup(3, false);
        assert!(register_reconnecting(
            channel,
            key,
            inputs(&suffix),
            |_| { panic!("direct greeting must not reconnect") },
            || 0
        )
        .is_ok());
    }
    #[test]
    fn corrupt_registration_cannot_return_a_registered_session() {
        let (channel, key, suffix) = setup(3, true);
        assert!(matches!(
            register(channel, key, inputs(&suffix), || 0),
            Err(HandshakeError::Protocol(Error::Checksum))
        ));
    }
    #[test]
    fn terminal_server_error_cannot_be_hidden_by_a_later_success_code() {
        let (mut channel, key, suffix) = setup(3, false);
        let insert_at = channel
            .input
            .get_ref()
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .nth(2)
            .unwrap()
            .0
            + 1;
        let mut server = PolBlowfish::new(&KEY, key.stream_iv());
        let error = identity::frame_line(b"ERROR :registration ended").unwrap();
        let encrypted = server.stream(&error, StreamReset::Boundary);
        channel
            .input
            .get_mut()
            .splice(insert_at..insert_at, encrypted);
        assert!(matches!(
            register(channel, key, inputs(&suffix), || 0),
            Err(HandshakeError::ServerError)
        ));
    }
    #[test]
    fn invalid_credentials_and_oversized_lines_stop_before_nick() {
        let (channel, key, suffix) = setup(3, false);
        let output = channel.output.clone();
        let mut values = inputs(&suffix);
        values.pol_secret = b"a\0b";
        assert!(matches!(
            register(channel, key, values, || 0),
            Err(HandshakeError::Protocol(Error::InvalidFrame))
        ));
        assert!(output.lock().unwrap().is_empty());
        let (channel, key, suffix) = setup(3, false);
        let output = channel.output.clone();
        let mut values = inputs(&suffix);
        values.maximum_line_bytes = 10;
        assert!(matches!(
            register(channel, key, values, || 0),
            Err(HandshakeError::LineLimit)
        ));
        assert!(output.lock().unwrap().is_empty());
    }
}
