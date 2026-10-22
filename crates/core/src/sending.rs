//! Sends one outbox message, and keeps the user's consent to send in rooms
//! that are not encrypted (ADR-0005).
//!
//! * The room's encryption is read from the homeserver if not known. A room
//!   that is not encrypted needs the user's consent first; without it nothing
//!   is sent. In an encrypted room the SDK encrypts or fails before any
//!   request: it never sends plain text there.
//! * A failure is a code. A transport failure (the connection dropped or timed
//!   out) is `unknown`, because the request may have reached the homeserver;
//!   retrying with the same transaction ID is then safe. The SDK is built
//!   without retries (`account.rs`), so nothing is retried behind the user.
//! * Consent is a list of room IDs in an encrypted crypto-store custom value,
//!   at most [`MAX_CONSENTED_ROOMS`].

use crate::outbox::SendOutcome;
use crate::store::Stores;
use matrix_sdk::{
    Client, HttpError, RoomState,
    ruma::{OwnedTransactionId, RoomId, events::room::message::RoomMessageEventContent},
};
use matrix_sdk_crypto::{OlmError, SessionRecipientCollectionError, store::CryptoStore};
use persephone_contracts::SendFailure;

/// Crypto-store custom value holding the rooms the user allowed plain text in.
pub const CONSENT_KEY: &str = "persephone.plaintext_rooms.v1";
/// Most rooms kept in the consent list.
pub const MAX_CONSENTED_ROOMS: usize = 200;
const MAX_CONSENT_BYTES: usize = 64 * 1024;

/// Sends `body` as a text message with `transaction_id`.
pub async fn send(
    client: &Client,
    room_id: &str,
    body: &str,
    transaction_id: OwnedTransactionId,
    consented: bool,
) -> SendOutcome {
    let Ok(room_id) = RoomId::parse(room_id) else {
        return SendOutcome::Failed(SendFailure::Failed);
    };
    let Some(room) = client
        .get_room(&room_id)
        .filter(|room| room.state() == RoomState::Joined)
    else {
        return SendOutcome::Failed(SendFailure::NotJoined);
    };
    let encrypted = match room.latest_encryption_state().await {
        // Never treated as "not encrypted": consent must not send plain text
        // into a room whose encryption is not known.
        Ok(state) if state.is_unknown() => return SendOutcome::Failed(SendFailure::Failed),
        Ok(state) => state.is_encrypted(),
        Err(error) => return classify(&error),
    };
    if !encrypted && !consented {
        return SendOutcome::Failed(SendFailure::ConfirmationRequired);
    }
    match room
        .send(RoomMessageEventContent::text_plain(body))
        .with_transaction_id(transaction_id)
        .await
    {
        Ok(result) => SendOutcome::Sent(result.response.event_id.to_string()),
        Err(error) => classify(&error),
    }
}

/// What a send error means for the user.
pub fn classify(error: &matrix_sdk::Error) -> SendOutcome {
    use SessionRecipientCollectionError as Recipients;
    SendOutcome::Failed(match error {
        matrix_sdk::Error::Http(http) => match **http {
            HttpError::Reqwest(_) => return SendOutcome::Unknown,
            HttpError::Api(_) => SendFailure::Rejected,
            _ => SendFailure::Failed,
        },
        matrix_sdk::Error::OlmError(olm) => match &**olm {
            OlmError::SessionRecipientCollectionError(
                Recipients::CrossSigningNotSetup | Recipients::SendingFromUnverifiedDevice,
            ) => SendFailure::DeviceNotVerified,
            OlmError::SessionRecipientCollectionError(
                Recipients::VerifiedUserChangedIdentity(_)
                | Recipients::VerifiedUserHasUnsignedDevice(_),
            ) => SendFailure::IdentityChanged,
            _ => SendFailure::Failed,
        },
        matrix_sdk::Error::WrongRoomState(_) => SendFailure::NotJoined,
        _ => SendFailure::Failed,
    })
}

async fn consented_rooms(stores: &Stores) -> Vec<String> {
    match stores.crypto().get_custom_value(CONSENT_KEY).await {
        Ok(Some(bytes)) if bytes.len() <= MAX_CONSENT_BYTES => {
            serde_json::from_slice(&bytes).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// Whether the user allowed plain text in this room.
pub async fn consented(stores: &Stores, room_id: &str) -> bool {
    consented_rooms(stores)
        .await
        .iter()
        .any(|room| room == room_id)
}

/// Records consent for a room: `true` once it is recorded, `false` when the
/// list is full or the store could not be written.
pub async fn allow_plaintext(stores: &Stores, room_id: &str) -> bool {
    let mut rooms = consented_rooms(stores).await;
    if rooms.iter().any(|room| room == room_id) {
        return true;
    }
    if rooms.len() >= MAX_CONSENTED_ROOMS {
        return false;
    }
    rooms.push(room_id.to_owned());
    let Ok(bytes) = serde_json::to_vec(&rooms) else {
        return false;
    };
    stores
        .crypto()
        .set_custom_value(CONSENT_KEY, bytes)
        .await
        .is_ok()
}
