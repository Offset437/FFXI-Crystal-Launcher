//! Windows system entropy for RSA key generation; no clock/PRNG fallback.
//!
//! ABI verified against Windows SDK10.0.26100.0 shared/bcrypt.h:
//! lines34..44 (WINAPI=__stdcall; signed LONG NTSTATUS; success>=0),
//! line452 (BCRYPT_ALG_HANDLE=PVOID), lines1616..1625 (flag2 and signature).
//! Windows ULONG/LONG are32 bits on both supported x86 and x64 ABIs.

use crate::rsa::{RandomBytes, RsaError};
use zeroize::Zeroize;

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRandom;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyError {
    LengthOverflow,
    OsStatus(i32),
}
impl std::fmt::Display for EntropyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LengthOverflow => f.write_str("entropy request exceeds BCrypt ULONG length"),
            Self::OsStatus(status) => write!(
                f,
                "BCryptGenRandom failed with NTSTATUS 0x{:08x}",
                *status as u32
            ),
        }
    }
}
impl std::error::Error for EntropyError {}

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(
        algorithm: *mut std::ffi::c_void,
        buffer: *mut u8,
        buffer_length: u32,
        flags: u32,
    ) -> i32;
}

impl SystemRandom {
    /// Empty requests succeed without touching an OS API. Any failed nonempty
    /// request wipes the complete output slice.
    pub fn fill_bytes(&mut self, output: &mut [u8]) -> Result<(), EntropyError> {
        if output.is_empty() {
            return Ok(());
        }
        fill_using(output, |bytes, length| {
            const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
            // SAFETY: bytes is writable for exactly length checked below.
            // The system-preferred flag requires no provider handle; this
            // synchronous call does not retain the slice pointer.
            unsafe {
                BCryptGenRandom(
                    std::ptr::null_mut(),
                    bytes.as_mut_ptr(),
                    length,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            }
        })
    }
}
impl RandomBytes for SystemRandom {
    fn fill(&mut self, output: &mut [u8]) -> crate::rsa::Result<()> {
        self.fill_bytes(output).map_err(|_| RsaError::RandomSource)
    }
}

fn checked_length(length: usize) -> Result<u32, EntropyError> {
    u32::try_from(length).map_err(|_| EntropyError::LengthOverflow)
}

fn fill_using(
    output: &mut [u8],
    call: impl FnOnce(&mut [u8], u32) -> i32,
) -> Result<(), EntropyError> {
    if output.is_empty() {
        return Ok(());
    }
    let length = match checked_length(output.len()) {
        Ok(length) => length,
        Err(error) => {
            output.zeroize();
            return Err(error);
        }
    };
    let status = call(output, length);
    if status < 0 {
        output.zeroize();
        return Err(EntropyError::OsStatus(status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_system_provider_accepts_a_64_byte_request() {
        let mut output = zeroize::Zeroizing::new([0; 64]);
        RandomBytes::fill(&mut SystemRandom, &mut output[..]).unwrap();
        // This checks API availability/ABI and successful completion, not a
        // probabilistic claim about the distribution of one sample.
    }

    #[test]
    fn a_partial_provider_failure_wipes_the_entire_slice() {
        let mut output = [0xa5; 64];
        let status = 0xc0000001u32 as i32;
        assert_eq!(
            fill_using(&mut output, |bytes, length| {
                assert_eq!(length, 64);
                bytes[..7].fill(0x5a);
                status
            }),
            Err(EntropyError::OsStatus(status))
        );
        assert_eq!(output, [0; 64]);
    }

    #[test]
    fn empty_requests_skip_the_provider_and_status_success_is_signed() {
        fill_using(&mut [], |_, _| panic!("empty request called provider")).unwrap();
        SystemRandom.fill_bytes(&mut []).unwrap();
        let mut output = [0; 4];
        fill_using(&mut output, |bytes, length| {
            assert_eq!(length, 4);
            bytes.copy_from_slice(&[1, 2, 3, 4]);
            1 // A nonnegative NTSTATUS is success, not just STATUS_SUCCESS=0.
        })
        .unwrap();
        assert_eq!(output, [1, 2, 3, 4]);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn oversized_lengths_fail_without_allocating_a_giant_buffer() {
        assert_eq!(checked_length(u32::MAX as usize), Ok(u32::MAX));
        assert_eq!(
            checked_length(u32::MAX as usize + 1),
            Err(EntropyError::LengthOverflow)
        );
    }
}
