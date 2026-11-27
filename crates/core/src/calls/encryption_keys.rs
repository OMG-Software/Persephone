//! The `io.element.call.encryption_keys` to-device event (ADR-0043).
//!
//! It carries a member's frame key. It is sent Olm-encrypted and only to
//! devices signed by their owner's cross-signing identity (ADR-0005).

use super::MAX_CALL_MEMBERS;
use super::keys::FrameKey;
use super::membership::participant_identity;
use matrix_sdk::ruma::{
    DeviceId, OwnedDeviceId, OwnedUserId, RoomId, UserId,
    events::AnyToDeviceEventContent,
    serde::{Base64, Raw, base64::Standard},
};
use matrix_sdk_base::crypto::CollectStrategy;
use serde_json::{Value, json};
use zeroize::Zeroizing;

/// The to-device event type.
pub const EVENT_TYPE: &str = "io.element.call.encryption_keys";

/// Content for the key `key` of this device.
pub fn outgoing_content(
    room_id: &RoomId,
    own_identity: &str,
    own_device_id: &DeviceId,
    key: &FrameKey,
) -> Value {
    let encoded = Base64::<Standard, _>::new(key.bytes.as_slice()).encode();
    json!({
        "room_id": room_id.as_str(),
        "member_id": own_identity,
        "media_key": { "index": key.index, "key": encoded },
        "keys": { "index": key.index, "key": encoded },
        "member": { "id": own_identity, "claimed_device_id": own_device_id.as_str() },
    })
}

/// A key received from another member.
pub struct IncomingKey {
    pub identity: String,
    pub index: u8,
    pub bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for IncomingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingKey")
            .field("identity", &self.identity)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

/// Why an incoming key was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotAnObject,
    MissingRoom,
    WrongRoom,
    NoKeyObject,
    BadIndex,
    BadKeyEncoding,
    BadKeyLength,
    MissingClaimedDevice,
    ClaimedDeviceMismatch,
    MissingIdentity,
    IdentityMismatch,
}

/// Parse and check the content of an incoming key event.
///
/// This checks only the content. The caller must also check that the event
/// arrived Olm-encrypted from a device signed by its owner's cross-signing
/// identity, and that the sender's device is in the call (ADR-0005). The
/// content is the sender's word: `sender` and `sender_device` come from the
/// encrypted envelope, and a sender can set keys only for its own identity.
pub fn parse_incoming(
    content: &Value,
    expected_room: &RoomId,
    sender: &UserId,
    sender_device: &DeviceId,
) -> Result<IncomingKey, Refusal> {
    let content = content.as_object().ok_or(Refusal::NotAnObject)?;
    match content.get("room_id").and_then(Value::as_str) {
        None => return Err(Refusal::MissingRoom),
        Some(room) if room != expected_room.as_str() => return Err(Refusal::WrongRoom),
        Some(_) => {}
    }

    let member = content.get("member").and_then(Value::as_object);
    let claimed = member
        .and_then(|m| m.get("claimed_device_id"))
        .and_then(Value::as_str)
        .ok_or(Refusal::MissingClaimedDevice)?;
    if claimed != sender_device.as_str() {
        return Err(Refusal::ClaimedDeviceMismatch);
    }

    let expected_identity = participant_identity(sender, sender_device);
    let mut named = false;
    for claimed_identity in [content.get("member_id"), member.and_then(|m| m.get("id"))]
        .into_iter()
        .flatten()
    {
        if claimed_identity.as_str() != Some(expected_identity.as_str()) {
            return Err(Refusal::IdentityMismatch);
        }
        named = true;
    }
    if !named {
        return Err(Refusal::MissingIdentity);
    }

    let key_object = content
        .get("media_key")
        .filter(|v| v.is_object())
        .or_else(|| content.get("keys").filter(|v| v.is_object()))
        .or_else(|| {
            content
                .get("keys")
                .and_then(Value::as_array)
                .filter(|a| a.len() == 1)
                .and_then(|a| a.first())
                .filter(|v| v.is_object())
        })
        .ok_or(Refusal::NoKeyObject)?;
    let index = key_object
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|i| u8::try_from(i).ok())
        .ok_or(Refusal::BadIndex)?;
    let encoded = key_object
        .get("key")
        .and_then(Value::as_str)
        .ok_or(Refusal::BadKeyEncoding)?;
    if encoded.len() > 4 * 64 {
        return Err(Refusal::BadKeyLength);
    }
    let bytes = Base64::<Standard>::parse(encoded)
        .map_err(|_| Refusal::BadKeyEncoding)?
        .into_inner();
    let bytes = Zeroizing::new(bytes);
    if !(16..=64).contains(&bytes.len()) {
        return Err(Refusal::BadKeyLength);
    }
    Ok(IncomingKey {
        identity: expected_identity,
        index,
        bytes,
    })
}

/// What [`send_key`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendReport {
    /// Devices that were sent the key.
    pub sent: usize,
    /// Devices not sent to: unknown, or not signed by their owner. They
    /// cannot hear this user; the page says so.
    pub skipped: Vec<(OwnedUserId, OwnedDeviceId)>,
    /// Devices the SDK could not encrypt for or reach.
    pub failed: Vec<(OwnedUserId, OwnedDeviceId)>,
}

/// Why [`send_key`] sent nothing.
#[derive(Debug)]
pub enum SendError {
    TooManyRecipients,
    Content,
    /// The room's identity changes could not be read; nothing is sent.
    IdentityState,
    Sdk(matrix_sdk::Error),
}

/// Send `content` Olm-encrypted to `recipients` in `room`, only to known
/// devices that their owner's cross-signing identity signed, and only to
/// users whose identity did not change without approval (ADR-0005,
/// ADR-0007). Other devices are listed in the report, never sent plaintext,
/// and never sent to without the checks. If the identity changes cannot be
/// read, nothing is sent.
pub async fn send_key(
    room: &matrix_sdk::Room,
    recipients: &[(OwnedUserId, OwnedDeviceId)],
    content: Value,
) -> Result<SendReport, SendError> {
    send_event(room, recipients, EVENT_TYPE, content).await
}

/// As [`send_key`], for another call to-device event type with the same
/// rules (ADR-0071).
pub async fn send_event(
    room: &matrix_sdk::Room,
    recipients: &[(OwnedUserId, OwnedDeviceId)],
    event_type: &str,
    content: Value,
) -> Result<SendReport, SendError> {
    if recipients.len() > MAX_CALL_MEMBERS {
        return Err(SendError::TooManyRecipients);
    }
    let changed = crate::identities::changed_members(room)
        .await
        .ok_or(SendError::IdentityState)?;
    let raw: Raw<AnyToDeviceEventContent> = Raw::new(&content)
        .map_err(|_| SendError::Content)?
        .cast_unchecked();
    let client = room.client();
    let mut report = SendReport::default();
    let mut devices = Vec::new();
    for (user, device_id) in recipients {
        if changed.iter().any(|change| change.user_id == *user) {
            report.skipped.push((user.clone(), device_id.clone()));
            continue;
        }
        match client.encryption().get_device(user, device_id).await {
            Ok(Some(device)) if device.is_cross_signed_by_owner() => devices.push(device),
            _ => report.skipped.push((user.clone(), device_id.clone())),
        }
    }
    if devices.is_empty() {
        return Ok(report);
    }
    let count = devices.len();
    report.failed = client
        .encryption()
        .encrypt_and_send_raw_to_device(
            devices.iter().collect(),
            event_type,
            raw,
            // The same strategy as room keys (account.rs): the SDK checks the
            // owner's signature again and withholds from unsigned devices.
            CollectStrategy::IdentityBasedStrategy,
        )
        .await
        .map_err(SendError::Sdk)?;
    report.sent = count.saturating_sub(report.failed.len());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::keys::OwnKeys;
    use matrix_sdk::ruma::{device_id, room_id, user_id};
    use std::time::Instant;

    const IDENTITY: &str = "@a:example.org:DEV";

    fn content() -> Value {
        let keys = OwnKeys::new(Instant::now()).unwrap();
        outgoing_content(
            room_id!("!r:example.org"),
            IDENTITY,
            device_id!("DEV"),
            keys.current(),
        )
    }

    fn parse(content: &Value) -> Result<IncomingKey, Refusal> {
        parse_incoming(
            content,
            room_id!("!r:example.org"),
            user_id!("@a:example.org"),
            device_id!("DEV"),
        )
    }

    /// GIVEN our own outgoing content WHEN parsed as the sender THEN the same key and index come back.
    #[test]
    fn given_outgoing_content_then_it_parses_to_the_same_key() {
        let keys = OwnKeys::new(Instant::now()).unwrap();
        let c = outgoing_content(
            room_id!("!r:example.org"),
            IDENTITY,
            device_id!("DEV"),
            keys.current(),
        );
        assert!(!c["media_key"]["key"].as_str().unwrap().contains('='));
        let got = parse(&c).unwrap();
        assert_eq!(got.identity, IDENTITY);
        assert_eq!(got.index, keys.current().index);
        assert_eq!(got.bytes.as_slice(), keys.current().bytes.as_slice());
    }

    /// GIVEN a key for another room or none WHEN parsed THEN it is refused.
    #[test]
    fn given_wrong_or_missing_room_then_refused() {
        let mut c = content();
        c["room_id"] = json!("!other:example.org");
        assert_eq!(parse(&c).unwrap_err(), Refusal::WrongRoom);
        c.as_object_mut().unwrap().remove("room_id");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingRoom);
    }

    /// GIVEN a member id of another user WHEN parsed THEN it is refused, so keys cannot be set for others.
    #[test]
    fn given_spoofed_member_id_then_refused() {
        let mut c = content();
        c["member_id"] = json!("@victim:example.org:DEV");
        assert_eq!(parse(&c).unwrap_err(), Refusal::IdentityMismatch);
        let mut c = content();
        c["member"]["id"] = json!("@victim:example.org:DEV");
        assert_eq!(parse(&c).unwrap_err(), Refusal::IdentityMismatch);
        let mut c = content();
        c["member_id"] = json!(5);
        assert_eq!(parse(&c).unwrap_err(), Refusal::IdentityMismatch);
    }

    /// GIVEN no identity at all WHEN parsed THEN it is refused.
    #[test]
    fn given_no_identity_then_refused() {
        let mut c = content();
        c.as_object_mut().unwrap().remove("member_id");
        c["member"].as_object_mut().unwrap().remove("id");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingIdentity);
    }

    /// GIVEN only one of the two identity fields WHEN it is correct THEN the key is accepted.
    #[test]
    fn given_one_correct_identity_field_then_accepted() {
        let mut c = content();
        c.as_object_mut().unwrap().remove("member_id");
        assert!(parse(&c).is_ok());
    }

    /// GIVEN a claimed device that is missing or not the sender's WHEN parsed THEN it is refused.
    #[test]
    fn given_claimed_device_problem_then_refused() {
        let mut c = content();
        c["member"]["claimed_device_id"] = json!("OTHER");
        assert_eq!(parse(&c).unwrap_err(), Refusal::ClaimedDeviceMismatch);
        c["member"]
            .as_object_mut()
            .unwrap()
            .remove("claimed_device_id");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingClaimedDevice);
        c.as_object_mut().unwrap().remove("member");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingClaimedDevice);
    }

    /// GIVEN a sender device that differs from the claim WHEN parsed THEN it is refused.
    #[test]
    fn given_other_sender_device_then_refused() {
        let r = parse_incoming(
            &content(),
            room_id!("!r:example.org"),
            user_id!("@a:example.org"),
            device_id!("EVIL"),
        );
        assert_eq!(r.unwrap_err(), Refusal::ClaimedDeviceMismatch);
    }

    /// GIVEN indices outside 0..=255 or not integers WHEN parsed THEN they are refused; 255 is accepted.
    #[test]
    fn given_bad_index_then_refused() {
        for bad in [json!(256), json!(-1), json!("1"), json!(1.5), json!(null)] {
            let mut c = content();
            c["media_key"]["index"] = bad.clone();
            assert_eq!(parse(&c).unwrap_err(), Refusal::BadIndex, "{bad}");
        }
        let mut c = content();
        c["media_key"]["index"] = json!(255);
        assert_eq!(parse(&c).unwrap().index, 255);
    }

    /// GIVEN bad base64 or lengths of 15 and 65 bytes WHEN parsed THEN they are refused; 16 and 64 are accepted.
    #[test]
    fn given_bad_key_then_refused() {
        let b64 = |n: usize| Base64::<Standard, _>::new(vec![7u8; n]).encode();
        for (key, want) in [
            ("!!!not base64!!!".to_owned(), Refusal::BadKeyEncoding),
            (b64(15), Refusal::BadKeyLength),
            (b64(65), Refusal::BadKeyLength),
            ("A".repeat(10_000), Refusal::BadKeyLength),
        ] {
            let mut c = content();
            c["media_key"]["key"] = json!(key);
            assert_eq!(parse(&c).unwrap_err(), want);
        }
        for n in [16, 64] {
            let mut c = content();
            c["media_key"]["key"] = json!(b64(n));
            assert_eq!(parse(&c).unwrap().bytes.len(), n);
        }
        let mut c = content();
        c["media_key"]["key"] = json!(5);
        assert_eq!(parse(&c).unwrap_err(), Refusal::BadKeyEncoding);
    }

    /// GIVEN padded base64 WHEN parsed THEN it is accepted.
    #[test]
    fn given_padded_base64_then_accepted() {
        let mut c = content();
        // 20 bytes encode with padding.
        c["media_key"]["key"] = json!("AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(parse(&c).unwrap().bytes.len(), 20);
    }

    /// GIVEN the legacy shapes WHEN parsed THEN `keys` as an object or a one-item array works, and nothing else does.
    #[test]
    fn given_legacy_shapes_then_fallbacks_work() {
        let keys = content()["keys"].clone();
        let mut c = content();
        c.as_object_mut().unwrap().remove("media_key");
        assert!(parse(&c).is_ok());
        c["keys"] = json!([keys.clone()]);
        assert!(parse(&c).is_ok());
        c["keys"] = json!([keys.clone(), keys]);
        assert_eq!(parse(&c).unwrap_err(), Refusal::NoKeyObject);
        c["keys"] = json!("x");
        assert_eq!(parse(&c).unwrap_err(), Refusal::NoKeyObject);
    }

    /// GIVEN content that is not an object WHEN parsed THEN it is refused.
    #[test]
    fn given_non_object_then_refused() {
        assert_eq!(parse(&json!([1])).unwrap_err(), Refusal::NotAnObject);
        assert_eq!(parse(&json!(null)).unwrap_err(), Refusal::NotAnObject);
    }

    /// GIVEN a key WHEN debug-printed THEN the bytes do not appear.
    #[test]
    fn given_incoming_debug_then_no_key_bytes() {
        let got = parse(&content()).unwrap();
        let text = format!("{got:?}");
        assert!(!text.contains("bytes"));
    }
}
