//! The profile passphrase (ADR-0006).
//!
//! Argon2id derives a key-encryption key from the passphrase, and
//! XChaCha20-Poly1305 seals the store key under it. The sealed form is what
//! the OS vault wraps (`vault.rs`), so a process running as the user can no
//! longer open the profile without the passphrase: it has to guess, at the
//! KDF's cost per guess.
//!
//! Sealed layout (`SEALED_BYTES`): format byte, Argon2id memory (KiB), passes
//! and lanes (each `u32`, little-endian), 16-byte salt, 24-byte nonce, then
//! the sealed key and its tag. Everything before the sealed key is the
//! AEAD's associated data, so a changed parameter fails the check.

use crate::credentials::CredentialSource;
use crate::vault::{KEY_BYTES, StoreKey};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{AeadInOut, Key, KeyInit, XChaCha20Poly1305, XNonce, aead::Tag};
use std::{sync::Arc, time::Duration};
use zeroize::Zeroizing;

/// Shortest new passphrase accepted. There are no composition rules.
pub const MIN_PASSPHRASE_CHARS: usize = 10;
/// Wrong entries allowed in one unlock before `secret_rejected`.
pub const MAX_ATTEMPTS: u32 = 5;
/// Most memory a stored parameter set may ask for: 1 GiB.
pub const MAX_MEMORY_KIB: u32 = 1 << 20;
const MAX_PASSES: u32 = 16;
const MAX_LANES: u32 = 16;

const FORMAT: u8 = 1;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;
const HEADER_BYTES: usize = 1 + 3 * 4 + SALT_BYTES + NONCE_BYTES;
pub const SEALED_BYTES: usize = HEADER_BYTES + KEY_BYTES + TAG_BYTES;

/// Argon2id cost parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    memory_kib: u32,
    passes: u32,
    lanes: u32,
}

impl KdfParams {
    /// RFC 9106's second recommended set: 64 MiB, 3 passes, 4 lanes. The core
    /// seals with these.
    pub const DEFAULT: Self = Self {
        memory_kib: 64 * 1024,
        passes: 3,
        lanes: 4,
    };

    /// A parameter set within Argon2's limits and this core's bounds, which
    /// cap the memory a stored set can make the core allocate.
    pub fn new(memory_kib: u32, passes: u32, lanes: u32) -> Option<Self> {
        let valid = (1..=MAX_LANES).contains(&lanes)
            && (1..=MAX_PASSES).contains(&passes)
            && memory_kib >= 8 * lanes
            && memory_kib <= MAX_MEMORY_KIB;
        valid.then_some(Self {
            memory_kib,
            passes,
            lanes,
        })
    }
}

/// How the core asks for the passphrase and checks it.
#[derive(Clone)]
pub struct Unlock {
    pub source: Arc<dyn CredentialSource>,
    /// The parameters new sealed keys get. Existing ones keep their own.
    pub kdf: KdfParams,
    /// The wait before the second entry; it doubles for each later one.
    pub backoff: Duration,
}

impl Unlock {
    /// The release settings: default parameters and a one-second backoff.
    pub fn new(source: Arc<dyn CredentialSource>) -> Self {
        Self {
            source,
            kdf: KdfParams::DEFAULT,
            backoff: Duration::from_secs(1),
        }
    }

    /// The wait before entry `attempt` (counted from 0): nothing before the
    /// first, then `backoff`, doubling.
    pub fn wait_before(&self, attempt: u32) -> Duration {
        match attempt {
            0 => Duration::ZERO,
            n => self.backoff.saturating_mul(1 << (n - 1).min(16)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealError {
    /// Not a sealed key in a format and parameter range this core accepts.
    Malformed,
    /// The passphrase does not open it (or it was changed).
    Wrong,
    /// Memory or randomness was not available.
    Failed,
}

fn derive(
    passphrase: &str,
    salt: &[u8],
    params: KdfParams,
) -> Result<Zeroizing<[u8; KEY_BYTES]>, SealError> {
    let params = Params::new(
        params.memory_kib,
        params.passes,
        params.lanes,
        Some(KEY_BYTES),
    )
    .map_err(|_| SealError::Malformed)?;
    let blocks = params.block_count();
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    // The working memory is the core's own, so it is zeroed when dropped;
    // argon2's own allocation is freed without clearing.
    let mut memory: Zeroizing<Vec<Block>> = Zeroizing::new(Vec::new());
    memory
        .try_reserve_exact(blocks)
        .map_err(|_| SealError::Failed)?;
    memory.resize(blocks, Block::new());
    let mut kek = Zeroizing::new([0u8; KEY_BYTES]);
    argon
        .hash_password_into_with_memory(
            passphrase.as_bytes(),
            salt,
            kek.as_mut(),
            memory.as_mut_slice(),
        )
        .map_err(|_| SealError::Failed)?;
    Ok(kek)
}

fn header(params: KdfParams, salt: &[u8; SALT_BYTES], nonce: &[u8; NONCE_BYTES]) -> Vec<u8> {
    let mut header = Vec::with_capacity(SEALED_BYTES);
    header.push(FORMAT);
    for value in [params.memory_kib, params.passes, params.lanes] {
        header.extend_from_slice(&value.to_le_bytes());
    }
    header.extend_from_slice(salt);
    header.extend_from_slice(nonce);
    header
}

/// Seals `key` under `passphrase` with a fresh salt and nonce.
pub fn seal(key: &StoreKey, passphrase: &str, params: KdfParams) -> Result<Vec<u8>, SealError> {
    let mut salt = [0u8; SALT_BYTES];
    let mut nonce = [0u8; NONCE_BYTES];
    getrandom::fill(&mut salt).map_err(|_| SealError::Failed)?;
    getrandom::fill(&mut nonce).map_err(|_| SealError::Failed)?;
    let kek = derive(passphrase, &salt, params)?;
    let mut sealed = header(params, &salt, &nonce);
    let mut buffer = Zeroizing::new(**key);
    let tag = XChaCha20Poly1305::new(Key::cast_from_core(&kek))
        .encrypt_inout_detached(&XNonce::from(nonce), &sealed, buffer.as_mut_slice().into())
        .map_err(|_| SealError::Failed)?;
    sealed.extend_from_slice(buffer.as_ref());
    sealed.extend_from_slice(&tag);
    Ok(sealed)
}

/// Opens a sealed key. The parameters are checked against this core's bounds
/// before any memory is allocated for them.
pub fn open(sealed: &[u8], passphrase: &str) -> Result<StoreKey, SealError> {
    if sealed.len() != SEALED_BYTES || sealed[0] != FORMAT {
        return Err(SealError::Malformed);
    }
    let word = |at: usize| u32::from_le_bytes(sealed[at..at + 4].try_into().expect("4 bytes"));
    let params = KdfParams::new(word(1), word(5), word(9)).ok_or(SealError::Malformed)?;
    let salt = &sealed[13..13 + SALT_BYTES];
    let nonce: [u8; NONCE_BYTES] = sealed[13 + SALT_BYTES..HEADER_BYTES]
        .try_into()
        .expect("nonce length");
    let tag = Tag::<XChaCha20Poly1305>::try_from(&sealed[HEADER_BYTES + KEY_BYTES..])
        .map_err(|_| SealError::Malformed)?;
    let kek = derive(passphrase, salt, params)?;
    let mut key = Zeroizing::new([0u8; KEY_BYTES]);
    key.copy_from_slice(&sealed[HEADER_BYTES..HEADER_BYTES + KEY_BYTES]);
    XChaCha20Poly1305::new(Key::cast_from_core(&kek))
        .decrypt_inout_detached(
            &XNonce::from(nonce),
            &sealed[..HEADER_BYTES],
            key.as_mut_slice().into(),
            &tag,
        )
        .map_err(|_| SealError::Wrong)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: KdfParams = KdfParams {
        memory_kib: 64,
        passes: 1,
        lanes: 1,
    };

    fn key() -> StoreKey {
        Zeroizing::new([7u8; KEY_BYTES])
    }

    /// GIVEN a sealed key WHEN it is opened with the same passphrase THEN the
    /// key comes back; WHEN with another passphrase THEN it is refused.
    #[test]
    fn given_a_sealed_key_then_only_its_passphrase_opens_it() {
        let sealed = seal(&key(), "correct horse", CHEAP).unwrap();
        assert_eq!(sealed.len(), SEALED_BYTES);
        assert_eq!(*open(&sealed, "correct horse").unwrap(), *key());
        assert_eq!(open(&sealed, "correct hors").err(), Some(SealError::Wrong));
        assert_eq!(open(&sealed, "").err(), Some(SealError::Wrong));
        let again = seal(&key(), "correct horse", CHEAP).unwrap();
        assert_ne!(sealed, again, "a fresh salt and nonce each time");
    }

    /// GIVEN a sealed key WHEN any byte is changed THEN it does not open:
    /// the header is authenticated, and a damaged format or parameter set
    /// is refused before any memory is allocated for it.
    #[test]
    fn given_a_changed_byte_then_the_sealed_key_is_refused() {
        let sealed = seal(&key(), "correct horse", CHEAP).unwrap();
        for at in 0..SEALED_BYTES {
            let mut changed = sealed.clone();
            changed[at] ^= 0x01;
            assert!(open(&changed, "correct horse").is_err(), "byte {at}");
        }
        assert_eq!(
            open(&sealed[..SEALED_BYTES - 1], "correct horse").err(),
            Some(SealError::Malformed)
        );
        let mut huge = sealed.clone();
        huge[1..5].copy_from_slice(&(MAX_MEMORY_KIB + 1).to_le_bytes());
        assert_eq!(
            open(&huge, "correct horse").err(),
            Some(SealError::Malformed)
        );
    }

    /// GIVEN parameter sets WHEN checked THEN only those within Argon2's
    /// limits and the memory cap are accepted; the default is within them.
    #[test]
    fn given_parameter_sets_then_only_bounded_ones_are_accepted() {
        assert_eq!(KdfParams::new(64 * 1024, 3, 4), Some(KdfParams::DEFAULT));
        assert!(KdfParams::new(MAX_MEMORY_KIB, 16, 16).is_some());
        for (memory, passes, lanes) in [
            (MAX_MEMORY_KIB + 1, 3, 4),
            (31, 1, 4),
            (64, 0, 1),
            (64, 17, 1),
            (64, 1, 0),
            (1024, 1, 17),
        ] {
            assert_eq!(KdfParams::new(memory, passes, lanes), None);
        }
    }

    /// GIVEN the release settings WHEN entries fail THEN the waits before
    /// them are 0, 1, 2, 4 and 8 seconds.
    #[test]
    fn given_wrong_entries_then_the_wait_doubles() {
        let unlock = Unlock::new(Arc::new(crate::credentials::Unsupported));
        let waits: Vec<u64> = (0..MAX_ATTEMPTS)
            .map(|n| unlock.wait_before(n).as_secs())
            .collect();
        assert_eq!(waits, [0, 1, 2, 4, 8]);
    }
}
