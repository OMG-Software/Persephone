//! This account's settings, kept in its encrypted profile.
//!
//! The settings are one JSON record in the crypto store, like the plaintext
//! consent list, so each account has its own and signing out wipes them with
//! the rest of the profile. No setting is kept by the web view or outside the
//! profile.
//!
//! The stored record is read leniently: a setting it does not have takes its
//! default, and one this build does not know is ignored, so a record written
//! by an older or newer build still reads. A record that is too large or not
//! JSON reads as the defaults; the next save replaces it.

use crate::store::Stores;
use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::AccountSettings;
use serde::{Deserialize, Serialize};

pub const SETTINGS_KEY: &str = "persephone.account_settings.v1";
const MAX_SETTINGS_BYTES: usize = 4096;

/// The stored form. Every field has a default, and unknown fields are
/// ignored; the wire form ([`AccountSettings`]) stays strict.
#[derive(Deserialize, Serialize)]
#[serde(default)]
struct Stored {
    enter_sends: bool,
}

impl Default for Stored {
    fn default() -> Self {
        Self::from(AccountSettings::default())
    }
}

impl From<AccountSettings> for Stored {
    fn from(settings: AccountSettings) -> Self {
        Self {
            enter_sends: settings.enter_sends,
        }
    }
}

impl From<Stored> for AccountSettings {
    fn from(stored: Stored) -> Self {
        Self {
            enter_sends: stored.enter_sends,
        }
    }
}

fn decode(bytes: &[u8]) -> AccountSettings {
    if bytes.len() > MAX_SETTINGS_BYTES {
        return AccountSettings::default();
    }
    serde_json::from_slice::<Stored>(bytes)
        .unwrap_or_default()
        .into()
}

/// The settings, or `None` when the store cannot be read. A record never
/// written gives the defaults.
pub async fn load(stores: &Stores) -> Option<AccountSettings> {
    match stores.crypto().get_custom_value(SETTINGS_KEY).await {
        Ok(Some(bytes)) => Some(decode(&bytes)),
        Ok(None) => Some(AccountSettings::default()),
        Err(_) => None,
    }
}

/// Replaces the settings: `true` once they are written.
pub async fn save(stores: &Stores, settings: AccountSettings) -> bool {
    let Ok(bytes) = serde_json::to_vec(&Stored::from(settings)) else {
        return false;
    };
    stores
        .crypto()
        .set_custom_value(SETTINGS_KEY, bytes)
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_saved_record_then_it_reads_back() {
        let off = AccountSettings { enter_sends: false };
        let bytes = serde_json::to_vec(&Stored::from(off)).unwrap();
        assert_eq!(decode(&bytes), off);
    }

    #[test]
    fn given_a_record_from_another_build_then_known_settings_read_and_others_default() {
        assert_eq!(
            decode(br#"{"enter_sends":false,"theme":"dark"}"#),
            AccountSettings { enter_sends: false }
        );
        assert_eq!(decode(b"{}"), AccountSettings::default());
    }

    #[test]
    fn given_a_damaged_or_oversized_record_then_the_defaults_are_used() {
        for bytes in [&b"not json"[..], br#"{"enter_sends":"no"}"#, br#"[1]"#] {
            assert_eq!(decode(bytes), AccountSettings::default());
        }
        let mut large = br#"{"enter_sends":false,"pad":""#.to_vec();
        large.resize(MAX_SETTINGS_BYTES + 1, b'x');
        large.extend_from_slice(br#""}"#);
        assert_eq!(decode(&large), AccountSettings::default());
    }
}
