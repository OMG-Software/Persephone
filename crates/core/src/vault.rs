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

/// The platform's vault, or a protector that refuses everything.
pub fn platform() -> std::sync::Arc<dyn KeyProtector> {
    #[cfg(windows)]
    return std::sync::Arc::new(Dpapi::new());
    #[cfg(not(windows))]
    return std::sync::Arc::new(Unsupported);
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

/// DPAPI for the current user, with application-specific entropy so a blob
/// wrapped by another program (or for another purpose) does not unwrap here.
#[cfg(windows)]
pub struct Dpapi {
    entropy: &'static [u8],
}

#[cfg(windows)]
impl Dpapi {
    pub const ENTROPY: &'static [u8] = b"Persephone local store key v1";

    pub const fn new() -> Self {
        Self::with_entropy(Self::ENTROPY)
    }

    /// For tests: different entropy stands in for a blob from another context.
    pub const fn with_entropy(entropy: &'static [u8]) -> Self {
        Self { entropy }
    }
}

#[cfg(windows)]
impl Default for Dpapi {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
mod dpapi {
    use super::{Dpapi, KeyProtector, VaultError};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    use zeroize::Zeroizing;

    fn blob(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB, VaultError> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len()).map_err(|_| VaultError::Failed)?,
            // DPAPI does not write through input blobs.
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    /// Owns a DPAPI output buffer; zeroes and frees it on drop.
    struct Output(CRYPT_INTEGER_BLOB);

    impl Output {
        fn bytes(&self) -> &[u8] {
            if self.0.pbData.is_null() {
                return &[];
            }
            // SAFETY: DPAPI returned `cbData` bytes at `pbData`.
            unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // SAFETY: the buffer is DPAPI's LocalAlloc allocation of
                // `cbData` bytes, owned only here.
                unsafe {
                    std::ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                    LocalFree(self.0.pbData.cast());
                }
            }
        }
    }

    fn empty() -> Output {
        Output(CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        })
    }

    impl KeyProtector for Dpapi {
        fn protect(&self, data: &[u8]) -> Result<Vec<u8>, VaultError> {
            let input = blob(data)?;
            let entropy = blob(self.entropy)?;
            let mut output = empty();
            // SAFETY: all blobs point to live buffers for the call; UI is
            // forbidden, so DPAPI never prompts.
            let ok = unsafe {
                CryptProtectData(
                    &input,
                    null(),
                    &entropy,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            };
            if ok == 0 {
                return Err(VaultError::Failed);
            }
            Ok(output.bytes().to_vec())
        }

        fn unprotect(&self, wrapped: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
            let input = blob(wrapped)?;
            let entropy = blob(self.entropy)?;
            let mut output = empty();
            // SAFETY: as above; DPAPI checks the blob's integrity.
            let ok = unsafe {
                CryptUnprotectData(
                    &input,
                    null_mut(),
                    &entropy,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            };
            if ok == 0 {
                return Err(VaultError::Failed);
            }
            Ok(Zeroizing::new(output.bytes().to_vec()))
        }
    }
}
