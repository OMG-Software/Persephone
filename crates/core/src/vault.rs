//! The OS vault: the outer layer around the sealed store key.
//!
//! The local store key is 32 random bytes and is never written to disk in
//! the clear. The profile passphrase seals it (`passphrase.rs`, ADR-0006), and
//! on Windows DPAPI then wraps the sealed form for the current user. DPAPI
//! does not prompt; any process running as the same user can remove this
//! layer, so on its own it protects only a copied profile. Profiles from
//! before ADR-0006 hold a store key wrapped by DPAPI alone.

use zeroize::Zeroizing;

pub const KEY_BYTES: usize = 32;

/// The store key. Its memory is zeroed when dropped; copies made by the SDK or
/// the OS are outside this guarantee.
pub type StoreKey = Zeroizing<[u8; KEY_BYTES]>;

#[derive(Debug, PartialEq, Eq)]
pub enum VaultError {
    /// No vault is implemented for this platform.
    Unsupported,
    /// The vault refused or failed the operation. No detail is kept, so no
    /// error text can carry key material.
    Failed,
}

/// Wraps and unwraps key material: the sealed store key, or a store key
/// from before ADR-0006. Implemented by the OS vault, and by stand-ins in
/// tests. The caller checks the unwrapped length.
pub trait KeyProtector: Send + Sync {
    fn protect(&self, data: &[u8]) -> Result<Vec<u8>, VaultError>;
    fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError>;
}

/// A new random key from the OS generator.
pub fn generate_key() -> Result<StoreKey, VaultError> {
    let mut key = Zeroizing::new([0u8; KEY_BYTES]);
    getrandom::fill(key.as_mut()).map_err(|_| VaultError::Failed)?;
    Ok(key)
}

/// The platform's vault ([`crate::platform`]), or a protector that refuses
/// everything where none is implemented.
pub fn platform() -> std::sync::Arc<dyn KeyProtector> {
    crate::platform::os::key_protector()
}

/// Refuses every operation, so an unsupported platform stays locked.
pub struct Unsupported;

impl KeyProtector for Unsupported {
    fn protect(&self, _: &[u8]) -> Result<Vec<u8>, VaultError> {
        Err(VaultError::Unsupported)
    }

    fn unprotect(&self, _: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        Err(VaultError::Unsupported)
    }
}

/// DPAPI, under its earlier path.
#[cfg(windows)]
pub use crate::platform::windows::vault::Dpapi;
