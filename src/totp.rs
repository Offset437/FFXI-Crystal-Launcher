//! Authenticator codes for accounts with a saved key: RFC 6238 TOTP (HMAC-SHA1, 30-second
//! steps, six digits) from the base32 key Square Enix's authenticator setup shows.
//!
//! A code is made only at the moment a sign-in hands its credentials to the game host
//! (`pol_less::native_auth`), in the place a typed code would go, so PlayOnline receives exactly
//! what a person typing it would send. Codes and keys are never logged. A code is never used
//! twice: each account's data folder keeps the last time step it submitted
//! (`authenticator-step`, a step number, not a code), shared by every Crystal process.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

use crate::i18n::t;

/// One code's window.
pub(crate) const STEP_MS: u64 = 30_000;
/// A code with less than this left in its window could expire before PlayOnline checks it, so
/// the sign-in waits for the next window instead. The code leaves with the session line; the
/// loader sends it at member login, after Connect and the W2U check: 0.7–4.1 s later (mean
/// 1.2 s) over 71 sign-ins in the loader log of 2026-10-22, so 8 s is twice the worst seen.
pub(crate) const MIN_LEFT_MS: u64 = 8_000;
/// 80 bits, the shortest key authenticator setups hand out.
const MIN_KEY_CHARS: usize = 16;
const MAX_KEY_CHARS: usize = 128;
const DIGITS: u32 = 6;
/// The per-account record of the last submitted time step.
const LEDGER: &str = "authenticator-step";

/// Why a typed key was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyError {
    Characters,
    Length,
}

impl KeyError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Characters => t("The authenticator key can only contain the letters A–Z and the digits 2–7."),
            Self::Length => t("The authenticator key should be 16 to 128 characters long."),
        }
    }
}

/// The key as Crystal stores it: A–Z and 2–7 only. Spaces, lowercase and trailing `=` padding
/// (as setup pages and other tools show it) are accepted and removed; anything else is refused.
pub(crate) fn normalize(input: &str) -> Result<Zeroizing<String>, KeyError> {
    let mut key = Zeroizing::new(String::with_capacity(input.len()));
    let mut padding = false;
    for c in input.chars() {
        match c {
            c if c.is_ascii_whitespace() => {}
            '=' => padding = true,
            // Padding only ends a key.
            _ if padding => return Err(KeyError::Characters),
            'a'..='z' | 'A'..='Z' | '2'..='7' => key.push(c.to_ascii_uppercase()),
            _ => return Err(KeyError::Characters),
        }
    }
    if !(MIN_KEY_CHARS..=MAX_KEY_CHARS).contains(&key.len()) {
        return Err(KeyError::Length);
    }
    Ok(key)
}

/// Whether `key` is a stored key (`normalize`'s output).
pub(crate) fn is_stored_form(key: &str) -> bool {
    normalize(key).is_ok_and(|normalized| *normalized == key)
}

/// RFC 4648 base32 of a stored key; bits after the last whole byte are dropped, as
/// authenticator apps do.
fn decode(key: &str) -> Option<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(key.len() * 5 / 8));
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in key.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    (!bytes.is_empty()).then_some(bytes)
}

/// RFC 4226 HOTP of `counter` with `digits` digits (RFC 6238 uses the time step as counter).
fn hotp(secret: &[u8], counter: u64, digits: u32) -> Zeroizing<String> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = ring::hmac::sign(&key, &counter.to_be_bytes());
    let mac = tag.as_ref();
    let offset = usize::from(mac[mac.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([mac[offset] & 0x7f, mac[offset + 1], mac[offset + 2], mac[offset + 3]]);
    Zeroizing::new(format!("{:0width$}", binary % 10u32.pow(digits), width = digits as usize))
}

/// Which time step a code submitted at `now_ms` (since the Unix epoch) uses, and how long to
/// wait before submitting it: the current window's, unless less than MIN_LEFT_MS of it remains
/// (then the next one's), and never `last`, the step this account submitted before (then the
/// one after it). A `last` more than one step ahead is a clock that went back since, not a
/// code that could be reused: it is ignored rather than waited for.
pub(crate) fn plan(now_ms: u64, last: Option<u64>) -> (u64, Duration) {
    let mut step = now_ms / STEP_MS;
    if (step + 1) * STEP_MS - now_ms < MIN_LEFT_MS {
        step += 1;
    }
    if let Some(last) = last {
        if step <= last && last <= step + 1 {
            step = last + 1;
        }
    }
    (step, Duration::from_millis((step * STEP_MS).saturating_sub(now_ms)))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// An account's authenticator, held only for the one sign-in that uses it. No Debug: it
/// holds the secret.
pub(crate) struct Authenticator {
    secret: Zeroizing<Vec<u8>>,
    ledger: PathBuf,
}

impl Authenticator {
    /// The authenticator for `key` (stored form), recording its codes in the account's data
    /// folder `directory`. `None` for an account without a key.
    pub(crate) fn new(key: &str, directory: &Path) -> Option<Self> {
        if key.is_empty() {
            return None;
        }
        Some(Self { secret: decode(key)?, ledger: directory.join(LEDGER) })
    }

    /// A fresh code, made now: it may first wait (at most about a minute) for a window in which
    /// it is neither about to expire nor already used. `Sent::record` once it has left.
    pub(crate) fn code(&self) -> (Zeroizing<String>, Sent) {
        self.code_with(now_ms, std::thread::sleep)
    }

    fn code_with(&self, mut now: impl FnMut() -> u64, mut sleep: impl FnMut(Duration)) -> (Zeroizing<String>, Sent) {
        let (step, wait) = plan(now(), read_ledger(&self.ledger));
        if !wait.is_zero() {
            sleep(wait);
            // A sleep that ended early still waits for the step to begin.
            for _ in 0..3 {
                let left = (step * STEP_MS).saturating_sub(now());
                if left == 0 { break; }
                sleep(Duration::from_millis(left));
            }
        }
        (hotp(&self.secret, step, DIGITS), Sent { ledger: self.ledger.clone(), step })
    }
}

/// A made code's time step, recorded once the code has left for the game host: a sign-in that
/// fails after that never reuses it, and one whose send failed keeps the window.
pub(crate) struct Sent {
    ledger: PathBuf,
    step: u64,
}

impl Sent {
    pub(crate) fn record(self) {
        write_ledger(&self.ledger, self.step);
    }
}

/// The code of stored key `key` for time step `step`, and the step last recorded in `directory`.
#[cfg(test)]
pub(crate) fn code_at(key: &str, step: u64) -> Zeroizing<String> {
    hotp(&decode(key).unwrap(), step, DIGITS)
}
#[cfg(test)]
pub(crate) fn recorded_step(directory: &Path) -> Option<u64> {
    read_ledger(&directory.join(LEDGER))
}

fn read_ledger(path: &Path) -> Option<u64> {
    let text = fs::read(path).ok().filter(|bytes| bytes.len() <= 32)?;
    std::str::from_utf8(&text).ok()?.trim().parse().ok()
}

/// Best effort: a folder that cannot be written still signs in (the account's session record
/// lives in the same folder, so this does not happen in practice).
fn write_ledger(path: &Path, step: u64) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = crate::fsutil::write_atomic(path, step.to_string().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// RFC 6238 appendix B, SHA-1 rows: the ASCII secret "12345678901234567890", 8 digits.
    #[test]
    fn rfc_6238_sha1_vectors() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59u64, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
            (20_000_000_000, "65353130"),
        ] {
            assert_eq!(hotp(secret, time / 30, 8).as_str(), expected, "T = {time}");
            // Six digits are the same code's last six.
            assert_eq!(hotp(secret, time / 30, 6).as_str(), &expected[2..], "T = {time}");
        }
        // RFC 4226 appendix D, counters 0 and 9, through the base32 path a user's key takes.
        let key = normalize("gezd gnbv gy3t qojq gezd gnbv gy3t qojq").unwrap();
        let decoded = decode(&key).unwrap();
        assert_eq!(&decoded[..], secret);
        assert_eq!(hotp(&decoded, 0, 6).as_str(), "755224");
        assert_eq!(hotp(&decoded, 9, 6).as_str(), "520489");
    }

    #[test]
    fn keys_accept_spaces_lowercase_and_padding_and_refuse_anything_else() {
        assert_eq!(normalize("JBSW Y3DP EHPK 3PXP").unwrap().as_str(), "JBSWY3DPEHPK3PXP");
        assert_eq!(normalize("jbswy3dpehpk3pxp").unwrap().as_str(), "JBSWY3DPEHPK3PXP");
        assert_eq!(normalize(" jbsw\ty3dp ehpk 3pxp====\n").unwrap().as_str(), "JBSWY3DPEHPK3PXP");
        assert_eq!(normalize("JBSWY3DPEHPK3PXP======").unwrap().as_str(), "JBSWY3DPEHPK3PXP");
        for bad in ["JBSWY3DPEHPK3PX1", "JBSWY3DPEHPK3PX0", "JBSWY3DPEHPK3PX8", "JBSW-Y3DP-EHPK-3PXP",
            "JBSWY3DP=EHPK3PXP", "JBSWY3DPEHPK3PXP=A", "JBSWY3DPÉHPK3PXP"] {
            assert_eq!(normalize(bad).unwrap_err(), KeyError::Characters, "{bad}");
        }
        for bad in ["", "   ", "====", "JBSWY3DPEHPK3PX", &"A".repeat(129)] {
            assert_eq!(normalize(bad).unwrap_err(), KeyError::Length, "{bad:?}");
        }
        assert!(normalize(&"A".repeat(128)).is_ok());
        assert!(is_stored_form("JBSWY3DPEHPK3PXP"));
        assert!(!is_stored_form("jbswy3dpehpk3pxp") && !is_stored_form("JBSW Y3DP EHPK 3PXP") && !is_stored_form(""));
        // The decoder drops the bits after the last whole byte and refuses lowercase input.
        assert_eq!(decode("MZXW6YTB").unwrap().as_slice(), b"fooba");
        assert_eq!(decode("MZXW6YTBOI").unwrap().as_slice(), b"foobar");
        assert_eq!(decode("MZXW6YTBOJ").unwrap().as_slice(), b"foobar");
        assert!(decode("mzxw6ytboi").is_none());
        for message in [KeyError::Characters.message(), KeyError::Length.message()] { assert!(!message.is_empty()); }
    }

    #[test]
    fn a_code_about_to_expire_waits_for_the_next_window() {
        let window = 1_000 * STEP_MS;
        // Plenty of time left: this window's code, now.
        assert_eq!(plan(window, None), (1_000, Duration::ZERO));
        assert_eq!(plan(window + STEP_MS - MIN_LEFT_MS, None), (1_000, Duration::ZERO), "exactly MIN_LEFT_MS left is enough");
        // Less than that left: the next window's code, once it begins.
        assert_eq!(plan(window + STEP_MS - MIN_LEFT_MS + 1, None), (1_001, Duration::from_millis(MIN_LEFT_MS - 1)));
        assert_eq!(plan(window + STEP_MS - 1, None), (1_001, Duration::from_millis(1)));
    }

    #[test]
    fn a_submitted_code_is_never_used_again() {
        let window = 1_000 * STEP_MS;
        // The same window again: wait for the next one.
        assert_eq!(plan(window + 5_000, Some(1_000)), (1_001, Duration::from_millis(25_000)));
        // The previous submission took the next window early (the MIN_LEFT_MS rule): the one after it.
        assert_eq!(plan(window + 29_000, Some(1_001)), (1_002, Duration::from_millis(31_000)));
        // Older submissions do not hold anything up.
        assert_eq!(plan(window + 5_000, Some(999)), (1_000, Duration::ZERO));
        // A clock that went back hours is not waited for.
        assert_eq!(plan(window + 5_000, Some(5_000)), (1_000, Duration::ZERO));

        // Through the ledger, as two sign-ins in a row (two processes share the file).
        let root = std::env::temp_dir().join(format!("crystal-totp-{}", crate::backend::unique_id().unwrap()));
        let authenticator = Authenticator::new("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", &root).unwrap();
        let clock = Cell::new(window + 5_000);
        let slept = Cell::new(Duration::ZERO);
        let sleep = |d: Duration| { slept.set(slept.get() + d); clock.set(clock.get() + d.as_millis() as u64); };
        let (unsent, _) = authenticator.code_with(|| clock.get(), sleep);
        assert_eq!(read_ledger(&root.join(LEDGER)), None, "a code that never left keeps its window");
        let (first, sent) = authenticator.code_with(|| clock.get(), sleep);
        assert_eq!(first.as_str(), unsent.as_str());
        sent.record();
        assert_eq!(slept.get(), Duration::ZERO);
        assert_eq!(read_ledger(&root.join(LEDGER)), Some(1_000));
        let again = Authenticator::new("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", &root).unwrap();
        let (second, sent) = again.code_with(|| clock.get(), sleep);
        sent.record();
        assert_eq!(slept.get(), Duration::from_millis(25_000), "waited for the next window");
        assert_eq!(read_ledger(&root.join(LEDGER)), Some(1_001));
        assert_ne!(first.as_str(), second.as_str());
        assert_eq!(second.as_str(), hotp(&decode("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap(), 1_001, 6).as_str());
        // The ledger holds the step, not the code or the key.
        let ledger = fs::read_to_string(root.join(LEDGER)).unwrap();
        assert_eq!(ledger, "1001");
        assert!(Authenticator::new("", &root).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
