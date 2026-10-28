//! Sends one outbox message, and keeps the user's consent to send in rooms
//! that are not encrypted (ADR-0005).
//!
//! * The room's encryption is read from the homeserver if not known. A room
//!   that is not encrypted needs the user's consent first; without it nothing
//!   is sent. In an encrypted room the SDK encrypts or fails before any
//!   request: it never sends plain text there.
//! * In an encrypted room, nothing is sent while a member's identity changed
//!   and the user has not accepted it (ADR-0007; `identities.rs`), or when
//!   that cannot be told. The check runs just before the send: an identity
//!   that changes in between still gets this one message's key.
//! * A failure is a code. A transport failure (the connection dropped or timed
//!   out) is `unknown`, because the request may have reached the homeserver;
//!   retrying with the same transaction ID is then safe. The SDK is built
//!   without retries (`account.rs`), so nothing is retried behind the user.
//! * Consent is a list of room IDs in an encrypted crypto-store custom value,
//!   at most [`MAX_CONSENTED_ROOMS`].
//! * A reply carries `m.in_reply_to` and no quoted fallback (the
//!   specification dropped it in v1.13), so the quoted text is not copied
//!   out of an encrypted room's history into a new event.
//! * An edit is sent only after the core reads the edited event from the
//!   homeserver and finds it is this account's own text message, itself not
//!   an edit; otherwise it fails as `rejected` and nothing is sent. It
//!   carries the new text in `m.new_content`, and a `* ` fallback body.
//! * A reaction goes through the same checks as a message, then is sent at
//!   once (not through the outbox): it has no text to keep, and the user
//!   can react again if it fails.
//! * An image goes through the same checks, then the SDK's attachment send:
//!   the SDK uploads it encrypted in an encrypted room (and fails rather than
//!   upload it in plain), and sends the event with the image's size, MIME
//!   type and a generic file name, without a thumbnail. A retry uploads the
//!   file again but sends the event with the same transaction ID.

use crate::files::FileUpload;
use crate::identities;
use crate::media::ImageUpload;
use crate::outbox::{Relation, SendOutcome};
use crate::store::Stores;
use matrix_sdk::{
    Client, HttpError, Room, RoomState,
    attachment::{AttachmentConfig, AttachmentInfo, BaseImageInfo},
    room::Receipts,
    ruma::{
        EventId, OwnedTransactionId, RoomId, UInt,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            reaction::ReactionEventContent,
            relation::{Annotation, InReplyTo, Reply},
            room::message::{self, MessageType, ReplacementMetadata, RoomMessageEventContent},
        },
    },
};
use matrix_sdk_crypto::{OlmError, SessionRecipientCollectionError, store::CryptoStore};
use persephone_contracts::SendFailure;
use persephone_core_protocol::ErrorCode;

/// Crypto-store custom value holding the rooms the user allowed plain text in.
pub const CONSENT_KEY: &str = "persephone.plaintext_rooms.v1";
/// Most rooms kept in the consent list.
pub const MAX_CONSENTED_ROOMS: usize = 200;
const MAX_CONSENT_BYTES: usize = 64 * 1024;

/// The joined room to send in, once the checks every message passes have
/// passed; otherwise how the send ended.
async fn checked_room(
    client: &Client,
    room_id: &str,
    consented: bool,
) -> Result<Room, SendOutcome> {
    let Ok(room_id) = RoomId::parse(room_id) else {
        return Err(SendOutcome::Failed(SendFailure::Failed));
    };
    let Some(room) = client
        .get_room(&room_id)
        .filter(|room| room.state() == RoomState::Joined)
    else {
        return Err(SendOutcome::Failed(SendFailure::NotJoined));
    };
    let encrypted = match room.latest_encryption_state().await {
        // Never treated as "not encrypted": consent must not send plain text
        // into a room whose encryption is not known.
        Ok(state) if state.is_unknown() => return Err(SendOutcome::Failed(SendFailure::Failed)),
        Ok(state) => state.is_encrypted(),
        Err(error) => return Err(classify(&error)),
    };
    if !encrypted && !consented {
        return Err(SendOutcome::Failed(SendFailure::ConfirmationRequired));
    }
    if encrypted {
        match identities::changed_members(&room).await {
            Some(changed) if changed.is_empty() => {}
            Some(_) => return Err(SendOutcome::Failed(SendFailure::IdentityChanged)),
            None => return Err(SendOutcome::Failed(SendFailure::Failed)),
        }
    }
    Ok(room)
}

/// The content of a text message with `body`, related to an event by
/// `relation`.
pub fn text_content(body: &str, relation: Option<&Relation>) -> RoomMessageEventContent {
    let mut content = RoomMessageEventContent::text_plain(body);
    match relation {
        None => content,
        Some(Relation::Reply(event_id)) => {
            content.relates_to = Some(message::Relation::Reply(Reply::new(InReplyTo::new(
                event_id.clone(),
            ))));
            content
        }
        Some(Relation::Replace(event_id)) => {
            content.make_replacement(ReplacementMetadata::new(event_id.clone(), None))
        }
    }
}

/// Whether `event_id` in `room` is one of this account's own text
/// messages, itself not an edit: the only events an edit may replace.
async fn own_text_message(client: &Client, room: &Room, event_id: &EventId) -> bool {
    let Some(own) = client.user_id() else {
        return false;
    };
    let Ok(event) = room.event(event_id, None).await else {
        return false;
    };
    if event.sender().as_deref() != Some(own) {
        return false;
    }
    matches!(
        event.raw().deserialize(),
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(message),
        ))) if matches!(message.content.msgtype, MessageType::Text(_))
            && !matches!(message.content.relates_to, Some(message::Relation::Replacement(_)))
    )
}

/// Sends `body` as a text message with `transaction_id`.
pub async fn send(
    client: &Client,
    room_id: &str,
    body: &str,
    relation: Option<&Relation>,
    transaction_id: OwnedTransactionId,
    consented: bool,
) -> SendOutcome {
    let room = match checked_room(client, room_id, consented).await {
        Ok(room) => room,
        Err(outcome) => return outcome,
    };
    if let Some(Relation::Replace(event_id)) = relation
        && !own_text_message(client, &room, event_id).await
    {
        return SendOutcome::Failed(SendFailure::Rejected);
    }
    match room
        .send(text_content(body, relation))
        .with_transaction_id(transaction_id)
        .await
    {
        Ok(result) => SendOutcome::Sent(result.response.event_id.to_string()),
        Err(error) => classify(&error),
    }
}

/// Reacts to `event_id` in `room_id` with `key`, after the checks every
/// message passes.
pub async fn react(
    client: &Client,
    room_id: &str,
    event_id: &EventId,
    key: &str,
    consented: bool,
) -> SendOutcome {
    let room = match checked_room(client, room_id, consented).await {
        Ok(room) => room,
        Err(outcome) => return outcome,
    };
    let content = ReactionEventContent::new(Annotation::new(event_id.to_owned(), key.to_owned()));
    match room.send(content).await {
        Ok(result) => SendOutcome::Sent(result.response.event_id.to_string()),
        Err(error) => classify(&error),
    }
}

/// Deletes (redacts) `event_id` in the joined room `room_id`, for everyone.
/// The homeserver decides whether this account may: `rejected` when it
/// refuses, `unavailable` when it cannot be reached.
pub async fn delete(client: &Client, room_id: &str, event_id: &EventId) -> Result<(), ErrorCode> {
    joined(client, room_id)?
        .redact(event_id, None, None)
        .await
        .map(|_| ())
        .map_err(|error| refusal(error.as_client_api_error().is_some()))
}

/// The joined room `room_id`, or `not_found`.
fn joined(client: &Client, room_id: &str) -> Result<Room, ErrorCode> {
    RoomId::parse(room_id)
        .ok()
        .and_then(|room_id| client.get_room(&room_id))
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)
}

/// What a failed request means for the page: `rejected` when the homeserver
/// answered it, `unavailable` when it may not have been reached.
fn refusal(answered: bool) -> ErrorCode {
    if answered {
        ErrorCode::Rejected
    } else {
        ErrorCode::Unavailable
    }
}

/// Marks the joined room `room_id` read up to `event_id`: its fully-read
/// marker and a private read receipt, which other people do not see, and a
/// public read receipt only when `public` (ADR-0023).
pub async fn mark_read(
    client: &Client,
    room_id: &str,
    event_id: &EventId,
    public: bool,
) -> Result<(), ErrorCode> {
    let receipts = Receipts::new()
        .fully_read_marker(event_id.to_owned())
        .private_read_receipt(event_id.to_owned())
        .public_read_receipt(public.then(|| event_id.to_owned()));
    joined(client, room_id)?
        .send_multiple_receipts(receipts)
        .await
        .map_err(|error| refusal(error.as_client_api_error().is_some()))
}

/// Tells the joined room `room_id` this account started or stopped typing.
/// The caller checks the account's setting first (ADR-0023).
pub async fn typing(client: &Client, room_id: &str, typing: bool) -> Result<(), ErrorCode> {
    joined(client, room_id)?
        .typing_notice(typing)
        .await
        .map_err(|error| refusal(error.as_client_api_error().is_some()))
}

/// Parses a MIME type into whatever type the SDK's attachment send names,
/// without this crate depending on that type's crate directly.
fn parse_mime<T: std::str::FromStr>(text: &str) -> Option<T> {
    text.parse().ok()
}

/// Sends a stripped image with `transaction_id`, after the same checks as
/// [`send`]. The width and height are the image's as shown; no thumbnail,
/// caption or original file name is sent.
pub async fn send_image(
    client: &Client,
    room_id: &str,
    image: &ImageUpload,
    transaction_id: OwnedTransactionId,
    consented: bool,
) -> SendOutcome {
    let room = match checked_room(client, room_id, consented).await {
        Ok(room) => room,
        Err(outcome) => return outcome,
    };
    // The SDK takes its own copy; it is reserved like the upload.
    let Ok((data, _reservation)) = image.copy_for_send() else {
        return SendOutcome::Failed(SendFailure::Failed);
    };
    let Some(mime) = parse_mime(image.mime_type()) else {
        return SendOutcome::Failed(SendFailure::Failed);
    };
    let (width, height) = image.size();
    let info = AttachmentInfo::Image(BaseImageInfo {
        width: Some(UInt::from(width)),
        height: Some(UInt::from(height)),
        size: UInt::try_from(data.len() as u64).ok(),
        blurhash: None,
        is_animated: Some(false),
    });
    let config = AttachmentConfig::new().txn_id(transaction_id).info(info);
    match room
        .send_attachment(image.file_name(), &mime, data, config)
        .await
    {
        Ok(response) => SendOutcome::Sent(response.event_id.to_string()),
        Err(error) => classify(&error),
    }
}

/// Sends a picked file with `transaction_id`, after the same checks as
/// [`send`], as `application/octet-stream` with its name (ADR-0029).
pub async fn send_file(
    client: &Client,
    room_id: &str,
    file: &FileUpload,
    transaction_id: OwnedTransactionId,
    consented: bool,
) -> SendOutcome {
    let room = match checked_room(client, room_id, consented).await {
        Ok(room) => room,
        Err(outcome) => return outcome,
    };
    let Some(mime) = parse_mime("application/octet-stream") else {
        return SendOutcome::Failed(SendFailure::Failed);
    };
    let config = AttachmentConfig::new().txn_id(transaction_id);
    match room
        .send_attachment(file.name(), &mime, file.bytes().to_vec(), config)
        .await
    {
        Ok(response) => SendOutcome::Sent(response.event_id.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::OwnedEventId;

    /// GIVEN a reply WHEN its content is built THEN it names the event in
    /// `m.in_reply_to` and the body is only the user's text, with no quoted
    /// fallback.
    #[test]
    fn given_a_reply_then_the_content_relates_to_it_without_a_fallback() {
        let event_id = OwnedEventId::try_from("$original:example.org").unwrap();
        let content =
            serde_json::to_value(text_content("yes", Some(&Relation::Reply(event_id)))).unwrap();
        assert_eq!(content["body"], "yes");
        assert_eq!(
            content["m.relates_to"]["m.in_reply_to"]["event_id"],
            "$original:example.org"
        );
        assert!(content.get("formatted_body").is_none());

        let plain = serde_json::to_value(text_content("hi", None)).unwrap();
        assert!(plain.get("m.relates_to").is_none());
    }
}
