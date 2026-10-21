//! Helpers shared by the core's integration tests.

use persephone_core::vault::{KEY_BYTES, KeyProtector, StoreKey, VaultError};
use zeroize::Zeroizing;

/// Reversible but keyed by a tag, so a blob from another "context" fails like
/// DPAPI's would. Test only; not protection.
pub struct Tagged(pub u8);

impl KeyProtector for Tagged {
    fn protect(&self, key: &StoreKey) -> Result<Vec<u8>, VaultError> {
        let mut blob = vec![self.0];
        blob.extend(key.iter().map(|b| b ^ 0xA5));
        Ok(blob)
    }

    fn unprotect(&self, blob: &[u8]) -> Result<StoreKey, VaultError> {
        match blob.split_first() {
            Some((&tag, rest)) if tag == self.0 && rest.len() == KEY_BYTES => {
                let mut key = Zeroizing::new([0u8; KEY_BYTES]);
                for (out, b) in key.iter_mut().zip(rest) {
                    *out = b ^ 0xA5;
                }
                Ok(key)
            }
            _ => Err(VaultError::Failed),
        }
    }
}
