//! New sessions of this account, shown until the user says whether they
//! signed them in (ADR-0026).
//!
//! * The devices the user has already been shown are kept as one JSON list
//!   of device IDs in the encrypted crypto store, at most
//!   [`MAX_KNOWN_DEVICES`] (the oldest dropped first), so each profile has
//!   its own and signing out wipes it.
//! * The account's devices come from this computer's crypto store, which the
//!   sync keeps up to date. A device there that is neither this one nor in
//!   the list is new.
//! * With no list yet (the first time, or after it could not be read), the
//!   current devices are recorded and none is new: a fresh sign-in is not an
//!   alert.
//! * Names are the homeserver's and the device's word, projected like names.

use crate::store::Stores;
use crate::views::project_name;
use matrix_sdk::{Client, ruma::OwnedDeviceId};
use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::{NewSession, NewSessions};
use persephone_core_protocol::ErrorCode;
use std::collections::BTreeSet;

pub const KNOWN_DEVICES_KEY: &str = "persephone.known_devices.v1";
/// Most device IDs kept as known.
pub const MAX_KNOWN_DEVICES: usize = 200;
/// Most new sessions one reply lists.
pub const MAX_NEW_SESSIONS: usize = 10;
const MAX_KNOWN_BYTES: usize = 64 * 1024;

/// The known device IDs, oldest first; `Ok(None)` only when none were ever
/// recorded. A record that cannot be read is `internal`, never a first run:
/// treating it as one would record every current device, a new session
/// among them, as known.
async fn known(stores: &Stores) -> Result<Option<Vec<String>>, ErrorCode> {
    let Some(bytes) = stores
        .crypto()
        .get_custom_value(KNOWN_DEVICES_KEY)
        .await
        .map_err(|_| ErrorCode::Internal)?
    else {
        return Ok(None);
    };
    decode(&bytes).map(Some)
}

fn decode(bytes: &[u8]) -> Result<Vec<String>, ErrorCode> {
    if bytes.len() > MAX_KNOWN_BYTES {
        return Err(ErrorCode::Internal);
    }
    serde_json::from_slice(bytes).map_err(|_| ErrorCode::Internal)
}

async fn save(stores: &Stores, mut devices: Vec<String>) -> Result<(), ErrorCode> {
    let excess = devices.len().saturating_sub(MAX_KNOWN_DEVICES);
    devices.drain(..excess);
    let bytes = serde_json::to_vec(&devices).map_err(|_| ErrorCode::Internal)?;
    stores
        .crypto()
        .set_custom_value(KNOWN_DEVICES_KEY, bytes)
        .await
        .map_err(|_| ErrorCode::Internal)
}

/// The account's devices other than this one, from the crypto store, with
/// their display names.
async fn other_devices(client: &Client) -> Result<Vec<(OwnedDeviceId, Option<String>)>, ErrorCode> {
    let own = client.user_id().ok_or(ErrorCode::NotFound)?;
    let this = client.device_id();
    let devices = client
        .encryption()
        .get_user_devices(own)
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    Ok(devices
        .devices()
        .filter(|device| Some(device.device_id()) != this)
        .map(|device| {
            (
                device.device_id().to_owned(),
                device.display_name().map(ToOwned::to_owned),
            )
        })
        .collect())
}

/// The sessions not yet shown to the user. The first time, records the
/// current ones and returns none.
pub async fn new_sessions(client: &Client, stores: &Stores) -> Result<NewSessions, ErrorCode> {
    let devices = other_devices(client).await?;
    let Some(known) = known(stores).await? else {
        save(
            stores,
            devices.iter().map(|(id, _)| id.to_string()).collect(),
        )
        .await?;
        return Ok(NewSessions {
            sessions: Vec::new(),
            truncated: false,
        });
    };
    Ok(unknown(&known, &devices))
}

/// The devices not in `known`, at most [`MAX_NEW_SESSIONS`], projected.
fn unknown(known: &[String], devices: &[(OwnedDeviceId, Option<String>)]) -> NewSessions {
    let known: BTreeSet<&str> = known.iter().map(String::as_str).collect();
    let mut fresh = devices
        .iter()
        .filter(|(id, _)| !known.contains(id.as_str()));
    let sessions: Vec<NewSession> = fresh
        .by_ref()
        .take(MAX_NEW_SESSIONS)
        .map(|(id, name)| NewSession {
            device_id: project_name(id.as_str()),
            display_name: name.as_deref().map(project_name),
        })
        .collect();
    NewSessions {
        sessions,
        truncated: fresh.next().is_some(),
    }
}

/// Records that the user has seen these sessions. Only IDs of the account's
/// current devices are added, matched as projected, so the page cannot fill
/// the list with anything else.
pub async fn acknowledge(
    client: &Client,
    stores: &Stores,
    device_ids: &[String],
) -> Result<(), ErrorCode> {
    let devices = other_devices(client).await?;
    let mut known = known(stores).await?.unwrap_or_default();
    for (id, _) in &devices {
        let id = id.to_string();
        if device_ids.contains(&project_name(&id)) && !known.contains(&id) {
            known.push(id);
        }
    }
    save(stores, known).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: Option<&str>) -> (OwnedDeviceId, Option<String>) {
        (id.into(), name.map(ToOwned::to_owned))
    }

    /// GIVEN damaged or oversized records WHEN decoded THEN each is an
    /// error, not an empty list that would make every device known.
    #[test]
    fn given_an_unreadable_record_then_it_is_an_error() {
        assert_eq!(
            decode(br#"["A","B"]"#),
            Ok(vec!["A".to_owned(), "B".to_owned()])
        );
        for bad in [&b"not json"[..], br#"{"A":1}"#, br#"[1]"#] {
            assert_eq!(decode(bad), Err(ErrorCode::Internal));
        }
        let mut large = b"[\"".to_vec();
        large.resize(MAX_KNOWN_BYTES + 1, b'x');
        assert_eq!(decode(&large), Err(ErrorCode::Internal));
    }

    /// GIVEN known and new devices WHEN compared THEN only the new are
    /// listed, with names projected, at most the limit, saying when cut.
    #[test]
    fn given_devices_then_only_unknown_ones_are_new() {
        let known = vec!["OLD".to_owned()];
        let found = unknown(
            &known,
            &[
                device("OLD", Some("Laptop")),
                device("NEW", Some("Phone\u{202E}")),
            ],
        );
        assert_eq!(
            found.sessions,
            [NewSession {
                device_id: "NEW".into(),
                display_name: Some("Phone\u{FFFD}".into()),
            }]
        );
        assert!(!found.truncated);

        let many: Vec<_> = (0..=MAX_NEW_SESSIONS)
            .map(|n| device(&format!("D{n}"), None))
            .collect();
        let found = unknown(&[], &many);
        assert_eq!(found.sessions.len(), MAX_NEW_SESSIONS);
        assert!(found.truncated);
    }
}
