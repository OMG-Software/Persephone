//! The deafened to-device event (ADR-0071).
//!
//! A member tells the other members of the call that it is deafened, so the
//! call window can show a muted headset next to the muted microphone. The
//! event is sent Olm-encrypted, to the same devices as the frame key and
//! with the same checks (ADR-0005). The state is the sender's own word. The
//! page shows it as a hint only.

use super::membership::participant_identity;
use matrix_sdk::ruma::{DeviceId, RoomId, UserId};
use serde_json::{Value, json};

/// The to-device event type.
pub const EVENT_TYPE: &str = "io.github.omg-software.persephone.call.deafened";

/// Longest content examined, in bytes of its JSON text. The content is
/// small; a larger one is refused before it is read.
pub const MAX_CONTENT_BYTES: usize = 1024;

/// Content that says whether this device is `deafened`. `seq` is a counter
/// that the sender raises on each change in one call.
pub fn outgoing_content(
    room_id: &RoomId,
    own_identity: &str,
    own_device_id: &DeviceId,
    deafened: bool,
    seq: u64,
) -> Value {
    json!({
        "room_id": room_id.as_str(),
        "member_id": own_identity,
        "member": { "id": own_identity, "claimed_device_id": own_device_id.as_str() },
        "deafened": deafened,
        "seq": seq,
    })
}

/// The counter at the start of a call: the milliseconds since the Unix
/// epoch at `now_ms`. A new call's counter is then above any counter of an
/// earlier call of this device, so a receiver that kept an old note takes
/// the new call's messages. It shows members roughly when the state
/// changed, which they can already see from the call. Each change adds 1.
pub fn first_seq(now_ms: u64) -> u64 {
    now_ms
}

/// A checked deafened message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingDeafen {
    /// The sender's device, as keys name it: `{user}:{device}`.
    pub identity: String,
    pub deafened: bool,
    pub seq: u64,
}

/// Why an incoming message was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    TooLarge,
    NotAnObject,
    MissingRoom,
    WrongRoom,
    MissingClaimedDevice,
    ClaimedDeviceMismatch,
    MissingIdentity,
    IdentityMismatch,
    BadDeafened,
    BadSeq,
}

/// Parse and check the content of an incoming deafened event.
///
/// This checks only the content, with the checks of
/// `encryption_keys::parse_incoming`. The caller must also check that the
/// event arrived Olm-encrypted from a device signed by its owner, and that
/// the device is in the call. `sender` and `sender_device` come from the
/// encrypted envelope, so a sender can speak only for its own device.
pub fn parse_incoming(
    content: &Value,
    expected_room: &RoomId,
    sender: &UserId,
    sender_device: &DeviceId,
) -> Result<IncomingDeafen, Refusal> {
    let object = content.as_object().ok_or(Refusal::NotAnObject)?;
    if content.to_string().len() > MAX_CONTENT_BYTES {
        return Err(Refusal::TooLarge);
    }
    match object.get("room_id").and_then(Value::as_str) {
        None => return Err(Refusal::MissingRoom),
        Some(room) if room != expected_room.as_str() => return Err(Refusal::WrongRoom),
        Some(_) => {}
    }
    let member = object.get("member").and_then(Value::as_object);
    let claimed = member
        .and_then(|m| m.get("claimed_device_id"))
        .and_then(Value::as_str)
        .ok_or(Refusal::MissingClaimedDevice)?;
    if claimed != sender_device.as_str() {
        return Err(Refusal::ClaimedDeviceMismatch);
    }
    let identity = participant_identity(sender, sender_device);
    let mut named = false;
    for claimed_identity in [object.get("member_id"), member.and_then(|m| m.get("id"))]
        .into_iter()
        .flatten()
    {
        if claimed_identity.as_str() != Some(identity.as_str()) {
            return Err(Refusal::IdentityMismatch);
        }
        named = true;
    }
    if !named {
        return Err(Refusal::MissingIdentity);
    }
    let deafened = object
        .get("deafened")
        .and_then(Value::as_bool)
        .ok_or(Refusal::BadDeafened)?;
    let seq = object
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or(Refusal::BadSeq)?;
    Ok(IncomingDeafen {
        identity,
        deafened,
        seq,
    })
}

/// What this device knows of one sender's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub deafened: bool,
    pub seq: u64,
}

/// Takes `message` over `known` (the sender's last note). A `seq` that is not
/// higher than the last one is stale: it changes nothing. Returns the new
/// note, or `None` for a stale message.
pub fn take(known: Option<Note>, message: &IncomingDeafen) -> Option<Note> {
    if known.is_some_and(|note| message.seq <= note.seq) {
        return None;
    }
    Some(Note {
        deafened: message.deafened,
        seq: message.seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{device_id, room_id, user_id};

    const IDENTITY: &str = "@a:example.org:DEV";

    fn content(deafened: bool, seq: u64) -> Value {
        outgoing_content(
            room_id!("!r:example.org"),
            IDENTITY,
            device_id!("DEV"),
            deafened,
            seq,
        )
    }

    fn parse(content: &Value) -> Result<IncomingDeafen, Refusal> {
        parse_incoming(
            content,
            room_id!("!r:example.org"),
            user_id!("@a:example.org"),
            device_id!("DEV"),
        )
    }

    /// GIVEN our own outgoing content WHEN parsed as the sender THEN the state and counter come back.
    #[test]
    fn given_outgoing_content_then_it_parses_back() {
        let got = parse(&content(true, 7)).unwrap();
        assert_eq!(
            got,
            IncomingDeafen {
                identity: IDENTITY.to_owned(),
                deafened: true,
                seq: 7
            }
        );
        assert!(!parse(&content(false, 8)).unwrap().deafened);
    }

    /// GIVEN content for another room or none WHEN parsed THEN it is refused.
    #[test]
    fn given_wrong_or_missing_room_then_refused() {
        let mut c = content(true, 1);
        c["room_id"] = json!("!other:example.org");
        assert_eq!(parse(&c).unwrap_err(), Refusal::WrongRoom);
        c.as_object_mut().unwrap().remove("room_id");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingRoom);
    }

    /// GIVEN a member id of another user WHEN parsed THEN it is refused.
    #[test]
    fn given_spoofed_identity_then_refused() {
        let mut c = content(true, 1);
        c["member_id"] = json!("@victim:example.org:DEV");
        assert_eq!(parse(&c).unwrap_err(), Refusal::IdentityMismatch);
        let mut c = content(true, 1);
        c["member"]["id"] = json!("@victim:example.org:DEV");
        assert_eq!(parse(&c).unwrap_err(), Refusal::IdentityMismatch);
        let mut c = content(true, 1);
        c["member_id"] = json!(5);
        assert_eq!(parse(&c).unwrap_err(), Refusal::IdentityMismatch);
        let mut c = content(true, 1);
        c.as_object_mut().unwrap().remove("member_id");
        c["member"].as_object_mut().unwrap().remove("id");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingIdentity);
    }

    /// GIVEN a claimed device that is missing or is not the sender's WHEN parsed THEN it is refused.
    #[test]
    fn given_claimed_device_problem_then_refused() {
        let mut c = content(true, 1);
        c["member"]["claimed_device_id"] = json!("OTHER");
        assert_eq!(parse(&c).unwrap_err(), Refusal::ClaimedDeviceMismatch);
        c.as_object_mut().unwrap().remove("member");
        assert_eq!(parse(&c).unwrap_err(), Refusal::MissingClaimedDevice);
        // The envelope's device is the one that counts.
        let got = parse_incoming(
            &content(true, 1),
            room_id!("!r:example.org"),
            user_id!("@a:example.org"),
            device_id!("OTHER"),
        );
        assert_eq!(got.unwrap_err(), Refusal::ClaimedDeviceMismatch);
    }

    /// GIVEN wrong types or a non-object WHEN parsed THEN it is refused.
    #[test]
    fn given_wrong_types_then_refused() {
        let mut c = content(true, 1);
        c["deafened"] = json!("yes");
        assert_eq!(parse(&c).unwrap_err(), Refusal::BadDeafened);
        let mut c = content(true, 1);
        c.as_object_mut().unwrap().remove("deafened");
        assert_eq!(parse(&c).unwrap_err(), Refusal::BadDeafened);
        let mut c = content(true, 1);
        c["seq"] = json!(-1);
        assert_eq!(parse(&c).unwrap_err(), Refusal::BadSeq);
        c["seq"] = json!("1");
        assert_eq!(parse(&c).unwrap_err(), Refusal::BadSeq);
        assert_eq!(parse(&json!([1])).unwrap_err(), Refusal::NotAnObject);
    }

    /// GIVEN content over the size limit WHEN parsed THEN it is refused before it is read.
    #[test]
    fn given_oversized_content_then_refused() {
        let mut c = content(true, 1);
        c["padding"] = json!("x".repeat(MAX_CONTENT_BYTES));
        assert_eq!(parse(&c).unwrap_err(), Refusal::TooLarge);
    }

    /// GIVEN a call that starts later than an earlier one WHEN the first
    /// counters are made THEN the later one is higher, also after many
    /// changes of the earlier call, and a note of the earlier call is
    /// replaced.
    #[test]
    fn given_a_later_call_then_its_counter_is_above_the_earlier_one() {
        let earlier = first_seq(1_000_000) + 500;
        let later = first_seq(1_060_000);
        assert!(later > earlier);
        let old = Note {
            deafened: true,
            seq: earlier,
        };
        let message = IncomingDeafen {
            identity: IDENTITY.to_owned(),
            deafened: false,
            seq: later,
        };
        assert_eq!(take(Some(old), &message).map(|n| n.deafened), Some(false));
    }

    /// GIVEN a known counter WHEN a message with a lower or equal counter comes THEN it is stale.
    #[test]
    fn given_a_stale_counter_then_the_note_is_kept() {
        let message = |deafened, seq| IncomingDeafen {
            identity: IDENTITY.to_owned(),
            deafened,
            seq,
        };
        let first = take(None, &message(true, 3)).unwrap();
        assert_eq!(
            first,
            Note {
                deafened: true,
                seq: 3
            }
        );
        assert_eq!(take(Some(first), &message(false, 3)), None);
        assert_eq!(take(Some(first), &message(false, 2)), None);
        let next = take(Some(first), &message(false, 4)).unwrap();
        assert!(!next.deafened);
    }
}
