//! DPAPI, the Windows vault around the sealed store key ([`crate::vault`]).

use crate::vault::{KeyProtector, VaultError};
use std::ptr::{null, null_mut};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};
use zeroize::Zeroizing;

/// DPAPI for the current user, with application-specific entropy so a blob
/// wrapped by another program (or for another purpose) does not unwrap here.
pub struct Dpapi {
    entropy: &'static [u8],
}

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

impl Default for Dpapi {
    fn default() -> Self {
        Self::new()
    }
}

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
