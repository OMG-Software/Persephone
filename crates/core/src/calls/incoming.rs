//! Incoming frame keys while a call runs (ADR-0005, ADR-0043).
//!
//! A to-device handler, registered only while the call runs, summarises each
//! `io.element.call.encryption_keys` event and forwards it through a bounded
//! queue. [`accept`] makes the decision over those summaries, so it needs no
//! SDK. A key is accepted only when every check holds; otherwise it is
//! dropped. The checks are the same as for room keys: the event came Olm
//! encrypted, from a device its owner's cross-signing identity signed, whose
//! identity did not change without approval.

use super::deafen::{self, IncomingDeafen};
use super::encryption_keys::{self, IncomingKey};
use super::keys::{InsertRefusal, RemoteKeys};
use matrix_sdk::{
    Client,
    event_handler::EventHandlerHandle,
    ruma::{OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, events::AnyToDeviceEvent, serde::Raw},
};
use matrix_sdk_base::deserialized_responses::{AlgorithmInfo, EncryptionInfo};
use serde_json::Value;
use std::time::Instant;
use tokio::sync::mpsc;

/// Events waiting for the call task. A full queue drops new events.
pub const QUEUE: usize = 64;

/// Longest raw to-device event examined, in bytes.
pub const MAX_EVENT_BYTES: usize = 8 * 1024;

/// What the event's encryption said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeSummary {
    /// The algorithm was Olm.
    pub olm: bool,
    /// The sender that the decryption authenticated equals the event's
    /// `sender`.
    pub sender_matches: bool,
    pub sender_device: Option<OwnedDeviceId>,
    /// The sender's Curve25519 key, base64, from the Olm envelope.
    pub curve25519: String,
}

/// What this account knows of the sender's device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceSummary {
    pub cross_signed_by_owner: bool,
    pub curve25519: Option<String>,
}

/// Which call to-device event a candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A frame key (ADR-0043).
    Key,
    /// A deafened message (ADR-0071).
    Deafened,
}

/// An event with everything [`accept`] needs besides the call's own state.
#[derive(Clone)]
pub struct Candidate {
    pub kind: Kind,
    pub sender: OwnedUserId,
    pub content: Value,
    /// `None` when the event arrived unencrypted.
    pub envelope: Option<EnvelopeSummary>,
    /// `None` when the device is unknown.
    pub device: Option<DeviceSummary>,
    /// Whether the sender's identity changed without approval. `None` when
    /// the SDK could not say.
    pub identity_changed: Option<bool>,
    pub received: Instant,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

/// Why a key was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    NotEncrypted,
    NotOlm,
    SenderMismatch,
    NoSenderDevice,
    UnknownDevice,
    NotCrossSigned,
    KeyMismatch,
    /// The sender's identity changed, or the SDK could not say.
    IdentityUnsafe,
    Content(encryption_keys::Refusal),
    DeafenContent(deafen::Refusal),
    /// The sender's device is not a current call member. The sender may
    /// have been faster than the membership event; see [`Refusal::is_early`].
    NotMember,
    Store(InsertRefusal),
}

impl Refusal {
    /// A refusal that a later membership change can cure.
    pub fn is_early(self) -> bool {
        self == Self::NotMember
    }
}

/// The sender's device, when the event's encryption checks hold: Olm, an
/// authenticated sender, a known device that its owner signed, an unchanged
/// identity.
fn sender_device(candidate: &Candidate) -> Result<&OwnedDeviceId, Refusal> {
    let envelope = candidate.envelope.as_ref().ok_or(Refusal::NotEncrypted)?;
    if !envelope.olm {
        return Err(Refusal::NotOlm);
    }
    if !envelope.sender_matches {
        return Err(Refusal::SenderMismatch);
    }
    let sender_device = envelope
        .sender_device
        .as_ref()
        .ok_or(Refusal::NoSenderDevice)?;
    let device = candidate.device.as_ref().ok_or(Refusal::UnknownDevice)?;
    if !device.cross_signed_by_owner {
        return Err(Refusal::NotCrossSigned);
    }
    if device.curve25519.as_deref() != Some(envelope.curve25519.as_str()) {
        return Err(Refusal::KeyMismatch);
    }
    if candidate.identity_changed != Some(false) {
        return Err(Refusal::IdentityUnsafe);
    }
    Ok(sender_device)
}

/// Decides whether to keep a received key. Stores it in `keys` only when
/// every check holds.
pub fn accept(
    candidate: &Candidate,
    room_id: &RoomId,
    is_member: &impl Fn(&str) -> bool,
    keys: &mut RemoteKeys,
) -> Result<IncomingKey, Refusal> {
    let sender_device = sender_device(candidate)?;
    let key = encryption_keys::parse_incoming(
        &candidate.content,
        room_id,
        &candidate.sender,
        sender_device,
    )
    .map_err(Refusal::Content)?;
    if !is_member(&key.identity) {
        return Err(Refusal::NotMember);
    }
    keys.insert(&key.identity, key.index, &key.bytes, is_member)
        .map_err(|refusal| match refusal {
            InsertRefusal::NotMember => Refusal::NotMember,
            other => Refusal::Store(other),
        })?;
    Ok(key)
}

/// Decides whether to take a received deafened message: the checks of
/// [`accept`], then the content, then the sender's device must be a current
/// member. The caller applies the counter (`deafen::take`).
pub fn accept_deafened(
    candidate: &Candidate,
    room_id: &RoomId,
    is_member: &impl Fn(&str) -> bool,
) -> Result<IncomingDeafen, Refusal> {
    let sender_device = sender_device(candidate)?;
    let message = deafen::parse_incoming(
        &candidate.content,
        room_id,
        &candidate.sender,
        sender_device,
    )
    .map_err(Refusal::DeafenContent)?;
    if !is_member(&message.identity) {
        return Err(Refusal::NotMember);
    }
    Ok(message)
}

/// The wire shape of a to-device event, as far as the handler needs it.
#[derive(serde::Deserialize)]
struct Loose {
    sender: OwnedUserId,
    #[serde(rename = "type")]
    event_type: String,
    content: Value,
}

/// Registers the handler for call keys in the room `room_id`. Remove the
/// returned handle when the call ends. The handler only summarises: it
/// forwards with a reserved queue slot and drops the event when the queue is
/// full. It holds no client: the SDK gives it the client for each event, so
/// the registration does not keep the client alive after a lock.
pub fn register(
    client: &Client,
    room_id: OwnedRoomId,
    queue: mpsc::Sender<Candidate>,
) -> EventHandlerHandle {
    client.add_event_handler(
        move |raw: Raw<AnyToDeviceEvent>, info: Option<EncryptionInfo>, own: Client| {
            let room_id = room_id.clone();
            let queue = queue.clone();
            async move {
                if raw.json().get().len() > MAX_EVENT_BYTES {
                    return;
                }
                let Ok(event) = serde_json::from_str::<Loose>(raw.json().get()) else {
                    return;
                };
                let kind = match event.event_type.as_str() {
                    encryption_keys::EVENT_TYPE => Kind::Key,
                    deafen::EVENT_TYPE => Kind::Deafened,
                    _ => return,
                };
                // Queue space first, so a flood costs no lookups.
                let Ok(permit) = queue.try_reserve() else {
                    return;
                };
                let envelope = info.map(|info| {
                    let (olm, curve25519) = match info.algorithm_info {
                        AlgorithmInfo::OlmV1Curve25519AesSha2 {
                            curve25519_public_key_base64,
                        } => (true, curve25519_public_key_base64),
                        _ => (false, String::new()),
                    };
                    EnvelopeSummary {
                        olm,
                        sender_matches: info.sender == event.sender,
                        sender_device: info.sender_device,
                        curve25519,
                    }
                });
                let device = match envelope.as_ref().and_then(|e| e.sender_device.as_ref()) {
                    Some(device_id) => {
                        match own.encryption().get_device(&event.sender, device_id).await {
                            Ok(Some(device)) => Some(DeviceSummary {
                                cross_signed_by_owner: device.is_cross_signed_by_owner(),
                                curve25519: device.curve25519_key().map(|key| key.to_base64()),
                            }),
                            _ => None,
                        }
                    }
                    None => None,
                };
                let identity_changed = match own.get_room(&room_id) {
                    Some(room) => crate::identities::changed_members(&room)
                        .await
                        .map(|changed| changed.iter().any(|c| c.user_id == event.sender)),
                    None => None,
                };
                permit.send(Candidate {
                    kind,
                    sender: event.sender,
                    content: event.content,
                    envelope,
                    device,
                    identity_changed,
                    received: Instant::now(),
                });
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{owned_device_id, owned_user_id, room_id};
    use serde_json::json;

    const ROOM: &str = "!room:example.org";
    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn candidate() -> Candidate {
        let identity = "@bob:example.org:BOBDEV";
        Candidate {
            kind: Kind::Key,
            sender: owned_user_id!("@bob:example.org"),
            content: json!({
                "room_id": ROOM,
                "member_id": identity,
                "media_key": { "index": 3, "key": KEY },
                "member": { "id": identity, "claimed_device_id": "BOBDEV" },
            }),
            envelope: Some(EnvelopeSummary {
                olm: true,
                sender_matches: true,
                sender_device: Some(owned_device_id!("BOBDEV")),
                curve25519: "curve".to_owned(),
            }),
            device: Some(DeviceSummary {
                cross_signed_by_owner: true,
                curve25519: Some("curve".to_owned()),
            }),
            identity_changed: Some(false),
            received: Instant::now(),
        }
    }

    fn run(candidate: &Candidate, member: bool) -> (Result<IncomingKey, Refusal>, usize) {
        let mut keys = RemoteKeys::new();
        let result = accept(
            candidate,
            room_id!("!room:example.org"),
            &|_| member,
            &mut keys,
        );
        (result, keys.member_count())
    }

    fn refused(candidate: &Candidate, expected: Refusal) {
        let (result, stored) = run(candidate, true);
        assert_eq!(result.err(), Some(expected));
        assert_eq!(stored, 0, "a refused key must not be stored");
    }

    /// GIVEN an Olm event from a cross-signed, unchanged, member device WHEN
    /// it is decided THEN the key is accepted and stored.
    #[test]
    fn given_every_check_holds_then_the_key_is_stored() {
        let (result, stored) = run(&candidate(), true);
        let key = result.expect("accepted");
        assert_eq!(key.identity, "@bob:example.org:BOBDEV");
        assert_eq!(key.index, 3);
        assert_eq!(stored, 1);
    }

    fn deafened_candidate() -> Candidate {
        let identity = "@bob:example.org:BOBDEV";
        Candidate {
            kind: Kind::Deafened,
            content: json!({
                "room_id": ROOM,
                "member_id": identity,
                "member": { "id": identity, "claimed_device_id": "BOBDEV" },
                "deafened": true,
                "seq": 1,
            }),
            ..candidate()
        }
    }

    /// GIVEN a deafened message from a cross-signed, unchanged, member
    /// device WHEN decided THEN it is taken.
    #[test]
    fn given_every_check_holds_then_a_deafened_message_is_taken() {
        let got = accept_deafened(
            &deafened_candidate(),
            room_id!("!room:example.org"),
            &|_| true,
        )
        .expect("accepted");
        assert_eq!(got.identity, "@bob:example.org:BOBDEV");
        assert!(got.deafened);
    }

    /// GIVEN a deafened message that fails an encryption check, has a bad
    /// content or comes from a device that is no member WHEN decided THEN it
    /// is refused, and only the membership refusal can be cured later.
    #[test]
    fn given_a_deafened_message_that_fails_a_check_then_refused() {
        let room = room_id!("!room:example.org");
        let mut c = deafened_candidate();
        c.envelope = None;
        assert_eq!(
            accept_deafened(&c, room, &|_| true).err(),
            Some(Refusal::NotEncrypted)
        );
        let mut c = deafened_candidate();
        c.device.as_mut().unwrap().cross_signed_by_owner = false;
        assert_eq!(
            accept_deafened(&c, room, &|_| true).err(),
            Some(Refusal::NotCrossSigned)
        );
        let mut c = deafened_candidate();
        c.identity_changed = Some(true);
        assert_eq!(
            accept_deafened(&c, room, &|_| true).err(),
            Some(Refusal::IdentityUnsafe)
        );
        let mut c = deafened_candidate();
        c.content["member"]["claimed_device_id"] = json!("OTHER");
        assert_eq!(
            accept_deafened(&c, room, &|_| true).err(),
            Some(Refusal::DeafenContent(
                deafen::Refusal::ClaimedDeviceMismatch
            ))
        );
        let refusal = accept_deafened(&deafened_candidate(), room, &|_| false).err();
        assert_eq!(refusal, Some(Refusal::NotMember));
        assert!(refusal.unwrap().is_early());
    }

    /// GIVEN an event that arrived unencrypted WHEN decided THEN it is
    /// refused.
    #[test]
    fn given_no_encryption_then_refused() {
        let mut c = candidate();
        c.envelope = None;
        refused(&c, Refusal::NotEncrypted);
    }

    /// GIVEN an envelope that is not Olm WHEN decided THEN it is refused.
    #[test]
    fn given_a_non_olm_envelope_then_refused() {
        let mut c = candidate();
        c.envelope.as_mut().unwrap().olm = false;
        refused(&c, Refusal::NotOlm);
    }

    /// GIVEN an event whose `sender` differs from the sender the Olm
    /// decryption authenticated WHEN decided THEN it is refused.
    #[test]
    fn given_a_sender_that_the_envelope_did_not_authenticate_then_refused() {
        let mut c = candidate();
        c.envelope.as_mut().unwrap().sender_matches = false;
        refused(&c, Refusal::SenderMismatch);
    }

    /// GIVEN an envelope without a sender device WHEN decided THEN it is
    /// refused.
    #[test]
    fn given_no_sender_device_then_refused() {
        let mut c = candidate();
        c.envelope.as_mut().unwrap().sender_device = None;
        refused(&c, Refusal::NoSenderDevice);
    }

    /// GIVEN a sender device this account does not know WHEN decided THEN it
    /// is refused.
    #[test]
    fn given_an_unknown_device_then_refused() {
        let mut c = candidate();
        c.device = None;
        refused(&c, Refusal::UnknownDevice);
    }

    /// GIVEN a device its owner did not cross-sign WHEN decided THEN it is
    /// refused.
    #[test]
    fn given_a_device_not_cross_signed_then_refused() {
        let mut c = candidate();
        c.device.as_mut().unwrap().cross_signed_by_owner = false;
        refused(&c, Refusal::NotCrossSigned);
    }

    /// GIVEN an envelope key that differs from the device's key, or a device
    /// without one, WHEN decided THEN it is refused.
    #[test]
    fn given_a_curve25519_mismatch_then_refused() {
        let mut c = candidate();
        c.device.as_mut().unwrap().curve25519 = Some("other".to_owned());
        refused(&c, Refusal::KeyMismatch);
        c.device.as_mut().unwrap().curve25519 = None;
        refused(&c, Refusal::KeyMismatch);
    }

    /// GIVEN a sender whose identity changed, or an identity state that could
    /// not be read, WHEN decided THEN it is refused.
    #[test]
    fn given_a_changed_or_unknown_identity_then_refused() {
        let mut c = candidate();
        c.identity_changed = Some(true);
        refused(&c, Refusal::IdentityUnsafe);
        c.identity_changed = None;
        refused(&c, Refusal::IdentityUnsafe);
    }

    /// GIVEN content for another room or another device WHEN decided THEN the
    /// content check refuses it.
    #[test]
    fn given_bad_content_then_refused() {
        let mut c = candidate();
        c.content["room_id"] = json!("!other:example.org");
        refused(&c, Refusal::Content(encryption_keys::Refusal::WrongRoom));
        let mut c = candidate();
        c.content["member"]["claimed_device_id"] = json!("EVIL");
        refused(
            &c,
            Refusal::Content(encryption_keys::Refusal::ClaimedDeviceMismatch),
        );
    }

    /// GIVEN a sender that is not a current call member WHEN decided THEN it
    /// is refused as early, so a later membership change may cure it.
    #[test]
    fn given_a_non_member_then_refused_as_early() {
        let (result, stored) = run(&candidate(), false);
        let refusal = result.expect_err("refused");
        assert_eq!(refusal, Refusal::NotMember);
        assert!(refusal.is_early());
        assert_eq!(stored, 0);
        assert!(!Refusal::NotOlm.is_early());
    }
}
