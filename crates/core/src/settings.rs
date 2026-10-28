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
//! JSON reads as the defaults; the next save replaces it. A text size out of
//! its bounds reads as its default, so a damaged record cannot make the
//! interface unusable.

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
    chat_text_percent: u16,
    interface_text_percent: u16,
    desktop_notifications: bool,
    send_typing: bool,
    send_read_receipts: bool,
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
            chat_text_percent: settings.chat_text_percent,
            interface_text_percent: settings.interface_text_percent,
            desktop_notifications: settings.desktop_notifications,
            send_typing: settings.send_typing,
            send_read_receipts: settings.send_read_receipts,
        }
    }
}

impl From<Stored> for AccountSettings {
    fn from(stored: Stored) -> Self {
        let defaults = AccountSettings::default();
        let mut settings = Self {
            enter_sends: stored.enter_sends,
            chat_text_percent: stored.chat_text_percent,
            interface_text_percent: stored.interface_text_percent,
            desktop_notifications: stored.desktop_notifications,
            send_typing: stored.send_typing,
            send_read_receipts: stored.send_read_receipts,
        };
        if !(AccountSettings {
            interface_text_percent: defaults.interface_text_percent,
            ..settings
        })
        .within_bounds()
        {
            settings.chat_text_percent = defaults.chat_text_percent;
        }
        if !(AccountSettings {
            chat_text_percent: defaults.chat_text_percent,
            ..settings
        })
        .within_bounds()
        {
            settings.interface_text_percent = defaults.interface_text_percent;
        }
        settings
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
        let changed = AccountSettings {
            enter_sends: false,
            chat_text_percent: 140,
            interface_text_percent: 90,
            desktop_notifications: true,
            send_typing: true,
            send_read_receipts: true,
        };
        let bytes = serde_json::to_vec(&Stored::from(changed)).unwrap();
        assert_eq!(decode(&bytes), changed);
    }

    #[test]
    fn given_a_record_from_another_build_then_known_settings_read_and_others_default() {
        // An older build's record has only Enter to send.
        assert_eq!(
            decode(br#"{"enter_sends":false,"theme":"dark"}"#),
            AccountSettings {
                enter_sends: false,
                ..AccountSettings::default()
            }
        );
        assert_eq!(decode(b"{}"), AccountSettings::default());
    }

    /// GIVEN a stored text size out of its bounds WHEN read THEN that size
    /// takes its default and the other settings are kept.
    #[test]
    fn given_an_out_of_bounds_text_size_then_it_reads_as_its_default() {
        let read = decode(
            br#"{"enter_sends":false,"chat_text_percent":5000,"interface_text_percent":120,"desktop_notifications":true}"#,
        );
        assert_eq!(read.chat_text_percent, 100);
        assert_eq!(read.interface_text_percent, 120);
        assert!(!read.enter_sends && read.desktop_notifications);
        let read = decode(br#"{"chat_text_percent":150,"interface_text_percent":10}"#);
        assert_eq!(
            (read.chat_text_percent, read.interface_text_percent),
            (150, 100)
        );
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
