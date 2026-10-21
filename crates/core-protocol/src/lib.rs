//! Messages between the keyless UI process and the key-holding core
//! (ADR-0004).
//!
//! Each call is one control frame from the UI and one control frame back,
//! using the bounded frame protocol from `persephone-media-protocol`. Control
//! frames are at most 128 KiB, and the receiver rejects an oversized or
//! malformed header before allocating the body. This crate deliberately does
//! not depend on the core, so the UI's dependency graph contains no SDK, store
//! or key code.

use persephone_media_protocol::{
    FrameError, FrameHeader, FrameKind, ResourceBudgets, read_frame, write_frame,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

/// A command the UI may ask the core to run. Unknown commands and unknown
/// fields fail to decode.
///
/// Every variant is a struct variant, even without arguments: serde does not
/// apply `deny_unknown_fields` to unit variants of an internally tagged enum,
/// so `AppStatus` as a unit variant silently accepted extra fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    AppStatus {},
    RoomList {},
    /// A page of a room's history, older than `cursor` (or the newest page).
    TimelinePage {
        room_id: String,
        /// Required, though it may be null: serde would otherwise treat a
        /// missing field as `None`.
        #[serde(deserialize_with = "Option::deserialize")]
        cursor: Option<String>,
    },
    /// Start sending [`Event::RoomChanged`] for this room.
    Subscribe {
        room_id: String,
    },
    Unsubscribe {
        room_id: String,
    },
    /// Close the protected session: refuse new work, wait for admitted work,
    /// then close the stores. Replies with the resulting store state.
    LockSession {},
    /// Reopen the protected session with the key from the OS vault. Replies
    /// with the resulting store state.
    UnlockSession {},
    /// Sign in to this homeserver. The core asks for the username and password
    /// itself, in a native prompt; nothing secret crosses this channel. Replies
    /// at once with the account status; the outcome arrives as
    /// [`Event::SessionChanged`].
    SignIn {
        homeserver: String,
    },
}

/// Longest homeserver URL either side accepts in a call.
pub const MAX_HOMESERVER_URL_BYTES: usize = 2048;

/// Most rooms the UI may subscribe to at once.
pub const MAX_SUBSCRIPTIONS: usize = 16;

/// A notification from the core. Events carry no message content: the UI
/// fetches content through bounded calls. A burst of changes to one room
/// coalesces into one pending event, so the core's queue is bounded by the
/// number of subscriptions.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    RoomChanged {
        room_id: String,
    },
    /// The session locked, unlocked or changed state: drop everything shown
    /// from it and ask for `app_status`. Sent before the lock waits for
    /// admitted work, so the UI clears first.
    SessionChanged {},
    /// Rooms were added, removed or changed: fetch the room list again.
    /// Coalesced like room events.
    RoomListChanged {},
}

/// Every frame from the core to the UI: a reply to a call, or an event.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreMessage {
    Reply(Reply),
    Event(Event),
}

/// Longest room ID either side accepts in a call (Matrix limits IDs to 255 bytes).
pub const MAX_ROOM_ID_BYTES: usize = 255;
/// Longest opaque timeline cursor either side accepts in a call.
pub const MAX_CURSOR_BYTES: usize = 64;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u32,
    pub call: Call,
}

/// Errors the core reports to the UI. Codes only; no text from the core's
/// internals crosses the boundary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unavailable,
    Internal,
    /// An argument failed validation.
    InvalidArgument,
    /// The named room or cursor does not exist.
    NotFound,
    /// A limit such as [`MAX_SUBSCRIPTIONS`] would be exceeded.
    LimitExceeded,
    /// The session is locked (or locking); protected views are unavailable.
    Locked,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Ok(serde_json::Value),
    Error(ErrorCode),
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub id: u32,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub enum ReceiveError {
    /// The peer closed the stream, at or within a frame.
    Closed,
    /// The frame header failed validation; its body was not read.
    Frame(FrameError),
    /// The frame was not a control frame.
    UnexpectedKind,
    /// The body was not a valid message.
    Malformed,
}

/// Writes one message as a control frame. Fails without writing anything if
/// the encoded message exceeds the control frame limit.
pub fn send<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    let header = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, header, &body)
}

/// Reads one message. The frame header is validated and the budget reserved
/// before the body is allocated.
pub fn receive<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<T, ReceiveError> {
    let frame = read_frame(reader, budgets).map_err(|error| match error {
        FrameError::Io(_) => ReceiveError::Closed,
        other => ReceiveError::Frame(other),
    })?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReceiveError::UnexpectedKind);
    }
    serde_json::from_slice(frame.body()).map_err(|_| ReceiveError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: FrameKind, body: &[u8]) -> Vec<u8> {
        let mut wire = FrameHeader {
            kind,
            length: body.len() as u64,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(body);
        wire
    }

    #[test]
    fn given_request_and_reply_when_sent_then_they_round_trip() {
        let mut wire = Vec::new();
        let request = Request {
            id: 7,
            call: Call::AppStatus {},
        };
        send(&mut wire, &request).unwrap();
        assert_eq!(
            String::from_utf8(wire[17..].to_vec()).unwrap(),
            r#"{"id":7,"call":{"command":"app_status"}}"#
        );
        let reply = Reply {
            id: 7,
            outcome: Outcome::Error(ErrorCode::Unavailable),
        };
        send(&mut wire, &reply).unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        assert_eq!(
            receive::<_, Request>(&mut reader, &budgets).unwrap(),
            request
        );
        assert_eq!(receive::<_, Reply>(&mut reader, &budgets).unwrap(), reply);
        assert!(matches!(
            receive::<_, Reply>(&mut reader, &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    #[test]
    fn given_core_messages_when_sent_then_replies_and_events_are_distinguished() {
        let mut wire = Vec::new();
        let reply = CoreMessage::Reply(Reply {
            id: 3,
            outcome: Outcome::Error(ErrorCode::LimitExceeded),
        });
        let event = CoreMessage::Event(Event::RoomChanged {
            room_id: "!r:example.invalid".into(),
        });
        let session = CoreMessage::Event(Event::SessionChanged {});
        let list = CoreMessage::Event(Event::RoomListChanged {});
        send(&mut wire, &reply).unwrap();
        send(&mut wire, &event).unwrap();
        send(&mut wire, &session).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"session_changed":{}}}"#),
            "wire form of the session event"
        );
        send(&mut wire, &list).unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        for expected in [reply, event, session, list] {
            assert_eq!(
                receive::<_, CoreMessage>(&mut reader, &budgets).unwrap(),
                expected
            );
        }
        for body in [
            &br#"{"event":{"room_changed":{"room_id":"!r","body":"x"}}}"#[..],
            br#"{"event":{"message_added":{"room_id":"!r"}}}"#,
            br#"{"reply":{"id":1,"outcome":{"ok":null}},"event":null}"#,
            br#"{"event":{"session_changed":{"state":"locked"}}}"#,
            br#"{"event":"session_changed"}"#,
            br#"{"event":{"room_list_changed":{"rooms":[]}}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, CoreMessage>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn given_unknown_command_field_or_kind_then_receive_refuses() {
        let budgets = ResourceBudgets::default();
        for body in [
            &br#"{"id":1,"call":{"command":"export_keys"}}"#[..],
            br#"{"id":1,"call":{"command":"app_status"},"extra":true}"#,
            br#"{"id":1,"call":{"command":"app_status","arg":1}}"#,
            br#"{"id":-1,"call":{"command":"app_status"}}"#,
            br#"{"id":1,"call":{"command":"room_list","limit":5}}"#,
            br#"{"id":1,"call":{"command":"timeline_page","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"timeline_page","room_id":"!r:s","cursor":null,"x":1}}"#,
            br#"{"id":1,"call":{"command":"lock_session","force":true}}"#,
            br#"{"id":1,"call":{"command":"unlock_session","key":"00"}}"#,
            br#"{"id":1,"call":{"command":"sign_in"}}"#,
            br#"{"id":1,"call":{"command":"sign_in","homeserver":"https://h","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"sign_in","homeserver":null}}"#,
            b"not json",
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let mut reader = Cursor::new(frame(FrameKind::EncodedImage, b"{}"));
        assert!(matches!(
            receive::<_, Request>(&mut reader, &budgets),
            Err(ReceiveError::UnexpectedKind)
        ));
    }

    /// A header announcing more than the control limit is refused before the
    /// body is read; the stream position proves nothing past the header was
    /// consumed.
    #[test]
    fn given_oversized_control_header_then_body_is_never_read() {
        let mut wire = FrameHeader {
            kind: FrameKind::Control,
            length: 64 * 1024 * 1024,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(&[b'x'; 4096]);
        let mut reader = Cursor::new(wire);
        let result = receive::<_, Request>(&mut reader, &ResourceBudgets::default());
        assert!(matches!(
            result,
            Err(ReceiveError::Frame(FrameError::TooLarge))
        ));
        assert_eq!(reader.position(), 17);
    }

    #[test]
    fn given_message_over_control_limit_then_send_writes_nothing() {
        let mut wire = Vec::new();
        let huge = serde_json::Value::String("x".repeat(200 * 1024));
        assert!(send(&mut wire, &huge).is_err());
        assert!(wire.is_empty());
    }
}
