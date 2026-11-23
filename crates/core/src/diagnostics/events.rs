//! Every Matrix event the SDK hands to the client, as metadata only
//! (ADR-0058): its kind, room, ID, type, sender, state key, server time and
//! size. Content is never read into a record.
//!
//! The handlers stay registered for the sync loop's life and return at once
//! while the log is off.

use super::{Level, Source, global};
use matrix_sdk::{
    Client, Room,
    deserialized_responses::EncryptionInfo,
    event_handler::EventHandlerHandle,
    ruma::{
        events::{
            AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
            AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent, AnySyncStateEvent,
            AnyToDeviceEvent, presence::PresenceEvent,
        },
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::Value;

/// The record's target.
pub const TARGET: &str = "persephone::event";

/// The fields read from an event. Unknown members, the content among them,
/// are skipped by the parser and never kept.
#[derive(Deserialize, Default)]
struct Meta {
    #[serde(rename = "type")]
    kind: Option<String>,
    event_id: Option<String>,
    sender: Option<String>,
    state_key: Option<String>,
    origin_server_ts: Option<u64>,
}

/// The record for one event, or `None` when the log is off.
fn fields(
    section: &str,
    room: Option<&Room>,
    json: &str,
    encryption: Option<&EncryptionInfo>,
) -> Vec<(&'static str, Value)> {
    let meta: Meta = serde_json::from_str(json).unwrap_or_default();
    let mut fields = vec![("section", Value::from(section))];
    if let Some(room) = room {
        fields.push(("room_id", Value::from(room.room_id().as_str())));
    }
    let optional = [
        ("type", meta.kind.map(Value::from)),
        ("event_id", meta.event_id.map(Value::from)),
        ("sender", meta.sender.map(Value::from)),
        ("state_key", meta.state_key.map(Value::from)),
        ("origin_server_ts", meta.origin_server_ts.map(Value::from)),
    ];
    fields.extend(
        optional
            .into_iter()
            .filter_map(|(name, value)| Some((name, value?))),
    );
    fields.push(("bytes", Value::from(json.len())));
    if let Some(info) = encryption {
        fields.push(("decrypted", Value::Bool(true)));
        fields.push((
            "sender_device",
            Value::from(info.sender_device.as_ref().map(|d| d.as_str())),
        ));
    }
    fields.push(("content", Value::from("[content]")));
    fields
}

fn note(section: &str, room: Option<&Room>, json: &str, encryption: Option<&EncryptionInfo>) {
    let log = global();
    if !log.is_on() {
        return;
    }
    log.record(
        Source::Core,
        Level::Debug,
        TARGET,
        "event",
        &fields(section, room, json, encryption),
    );
}

/// Registers one handler for each event kind. The caller removes them with
/// the client's `remove_event_handler`. A timeline state event reaches only
/// the state handler, a message-like event only the message handler, so each
/// event is written once.
pub fn register(client: &Client) -> Vec<EventHandlerHandle> {
    vec![
        client.add_event_handler(
            |raw: Raw<AnySyncMessageLikeEvent>, room: Room, info: Option<EncryptionInfo>| {
                note("timeline", Some(&room), raw.json().get(), info.as_ref());
                std::future::ready(())
            },
        ),
        client.add_event_handler(|raw: Raw<AnySyncStateEvent>, room: Room| {
            note("state", Some(&room), raw.json().get(), None);
            std::future::ready(())
        }),
        client.add_event_handler(|raw: Raw<AnyStrippedStateEvent>, room: Room| {
            note("invite_state", Some(&room), raw.json().get(), None);
            std::future::ready(())
        }),
        client.add_event_handler(|raw: Raw<AnySyncEphemeralRoomEvent>, room: Room| {
            note("ephemeral", Some(&room), raw.json().get(), None);
            std::future::ready(())
        }),
        client.add_event_handler(|raw: Raw<AnyRoomAccountDataEvent>, room: Room| {
            note("room_account_data", Some(&room), raw.json().get(), None);
            std::future::ready(())
        }),
        client.add_event_handler(|raw: Raw<AnyGlobalAccountDataEvent>| {
            note("account_data", None, raw.json().get(), None);
            std::future::ready(())
        }),
        client.add_event_handler(|raw: Raw<PresenceEvent>| {
            note("presence", None, raw.json().get(), None);
            std::future::ready(())
        }),
        client.add_event_handler(|raw: Raw<AnyToDeviceEvent>, info: Option<EncryptionInfo>| {
            note("to_device", None, raw.json().get(), info.as_ref());
            std::future::ready(())
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN an event with content WHEN its fields are made THEN they hold
    /// the metadata and the size, and no part of the content.
    #[test]
    fn given_event_then_fields_hold_metadata_only() {
        let json = r#"{"type":"m.room.message","event_id":"$e1","sender":"@a:b","origin_server_ts":7,"content":{"msgtype":"m.text","body":"secret words"}}"#;
        let fields = fields("timeline", None, json, None);
        let text = serde_json::to_string(&fields).unwrap();
        assert!(!text.contains("secret words"), "{text}");
        assert!(!text.contains("msgtype"), "{text}");
        let find = |name: &str| {
            fields
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(find("type"), Some(Value::from("m.room.message")));
        assert_eq!(find("event_id"), Some(Value::from("$e1")));
        assert_eq!(find("sender"), Some(Value::from("@a:b")));
        assert_eq!(find("origin_server_ts"), Some(Value::from(7)));
        assert_eq!(find("bytes"), Some(Value::from(json.len())));
        assert_eq!(find("content"), Some(Value::from("[content]")));
        assert_eq!(find("state_key"), None);
    }

    /// GIVEN an event that does not parse THEN only the section, size and the
    /// content marker are kept.
    #[test]
    fn given_bad_event_then_fields_are_minimal() {
        let fields = fields("to_device", None, "not json", None);
        let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["section", "bytes", "content"]);
    }
}
