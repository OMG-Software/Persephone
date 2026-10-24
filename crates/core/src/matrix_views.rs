//! Room list and timeline pages from the signed-in Matrix client, projected
//! with the same limits as the synthetic views (`views.rs`).
//!
//! * Names and senders go through `views::project_name`; message bodies
//!   through `views::project_body`. A direct-message room names its contact
//!   (`contacts.rs`), and is named after them when it has no name.
//! * Each item carries the event's `origin_server_ts`: the time the sender's
//!   homeserver claims, unchecked. An event without a usable one has none.
//! * Text messages (text, notice, emote) are shown as messages, and images
//!   as image items without fetching anything (`media.rs`). Other
//!   `m.room.message` types become an `unsupported` notice, so nothing
//!   disappears. An encrypted event that cannot be shown becomes a notice
//!   saying why: `withheld` (the sender withheld the key from this device),
//!   `untrusted_sender` (the sending device is not signed by its owner, which
//!   the decryption settings refuse; ADR-0005) or `undecryptable`. State
//!   events, reactions and other non-message events are not shown.
//! * A page holds at most `MAX_PAGE_ITEMS` items and `MAX_PAGE_BYTES`
//!   serialized. The SDK cannot resume inside a fetched chunk, so a chunk that
//!   would exceed the byte budget is fetched again with half the limit, down to
//!   one event, instead of being cut.
//! * The homeserver's pagination tokens stay in the core. The UI gets short
//!   cursors (`m1.<n>`) for at most `MAX_CURSORS` tokens; an evicted or unknown
//!   cursor is `not_found`.

use crate::contacts;
use crate::views::{MAX_PAGE_BYTES, MAX_PAGE_ITEMS, MAX_ROOMS, project_body, project_name};
use matrix_sdk::{
    Client, EncryptionState, RoomState,
    deserialized_responses::{TimelineEvent, TimelineEventKind, UnableToDecryptReason},
    room::MessagesOptions,
    ruma::{
        RoomId, UInt,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            room::message::MessageType,
        },
    },
};
use persephone_contracts::{
    ContactStatus, NoticeKind, RoomEncryption, RoomList, RoomSummary, TimelineItem, TimelinePage,
    UnixMillis,
};
use persephone_core_protocol::{ErrorCode, MAX_CURSOR_BYTES, MAX_ROOM_ID_BYTES};
use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
};

/// Most pagination tokens kept at once, across all rooms.
pub const MAX_CURSORS: usize = 64;
const CURSOR_PREFIX: &str = "m1.";

#[derive(Default)]
struct Cursors {
    next: u64,
    /// Oldest first: (cursor number, room, homeserver token).
    entries: VecDeque<(u64, String, String)>,
}

#[derive(Default)]
pub struct MatrixViews {
    cursors: Mutex<Cursors>,
}

fn room_id(raw: &str) -> Result<&RoomId, ErrorCode> {
    if raw.len() > MAX_ROOM_ID_BYTES || !raw.starts_with('!') {
        return Err(ErrorCode::InvalidArgument);
    }
    <&RoomId>::try_from(raw).map_err(|_| ErrorCode::InvalidArgument)
}

fn joined_room(client: &Client, raw: &str) -> Result<matrix_sdk::Room, ErrorCode> {
    let id = room_id(raw)?;
    client
        .get_room(id)
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)
}

/// The visible form of one event, or `None` for events not shown.
fn project(event: &TimelineEvent) -> Option<TimelineItem> {
    let event_id = event.event_id()?.to_string();
    let sender = project_name(event.sender()?.as_str());
    // Read from the raw event, so an encrypted one has its time too. `UInt`
    // is at most 2^53 - 1, which `UnixMillis` holds.
    let origin_server_ts = event
        .raw()
        .get_field::<UInt>("origin_server_ts")
        .ok()
        .flatten()
        .and_then(|millis| UnixMillis::new(millis.into()));
    if let TimelineEventKind::UnableToDecrypt { utd_info, .. } = &event.kind {
        return Some(TimelineItem::Notice {
            event_id,
            sender,
            notice: undecryptable_notice(&utd_info.reason),
            origin_server_ts,
        });
    }
    // An event the SDK could not try to decrypt arrives as plain text that is
    // still encrypted; it is never shown as anything else.
    if event
        .raw()
        .get_field::<String>("type")
        .ok()
        .flatten()
        .is_some_and(|kind| kind == "m.room.encrypted")
    {
        return Some(TimelineItem::Notice {
            event_id,
            sender,
            notice: NoticeKind::Undecryptable,
            origin_server_ts,
        });
    }
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(message),
    ))) = event.raw().deserialize()
    else {
        return None;
    };
    Some(match &message.content.msgtype {
        MessageType::Text(text) => TimelineItem::Message {
            event_id,
            sender,
            body: project_body(&text.body),
            origin_server_ts,
        },
        MessageType::Notice(notice) => TimelineItem::Message {
            event_id,
            sender,
            body: project_body(&notice.body),
            origin_server_ts,
        },
        MessageType::Emote(emote) => TimelineItem::Message {
            event_id,
            sender,
            body: project_body(&emote.body),
            origin_server_ts,
        },
        MessageType::Image(image) => {
            crate::media::timeline_item(event_id, sender, origin_server_ts, image)
        }
        _ => TimelineItem::Notice {
            event_id,
            sender,
            notice: NoticeKind::Unsupported,
            origin_server_ts,
        },
    })
}

/// Why an encrypted event is not shown.
fn undecryptable_notice(reason: &UnableToDecryptReason) -> NoticeKind {
    match reason {
        UnableToDecryptReason::SenderIdentityNotTrusted(_) => NoticeKind::UntrustedSender,
        UnableToDecryptReason::MissingMegolmSession {
            withheld_code: Some(_),
        } => NoticeKind::Withheld,
        _ => NoticeKind::Undecryptable,
    }
}

impl MatrixViews {
    fn cursors(&self) -> MutexGuard<'_, Cursors> {
        self.cursors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stores a homeserver token and returns the cursor the UI sees.
    fn remember(&self, room: &str, token: String) -> String {
        let mut cursors = self.cursors();
        let number = cursors.next;
        cursors.next += 1;
        cursors.entries.push_back((number, room.to_owned(), token));
        while cursors.entries.len() > MAX_CURSORS {
            cursors.entries.pop_front();
        }
        format!("{CURSOR_PREFIX}{number}")
    }

    fn token(&self, room: &str, cursor: &str) -> Result<String, ErrorCode> {
        if cursor.len() > MAX_CURSOR_BYTES {
            return Err(ErrorCode::InvalidArgument);
        }
        let digits = cursor
            .strip_prefix(CURSOR_PREFIX)
            .ok_or(ErrorCode::InvalidArgument)?;
        if digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let number: u64 = digits.parse().map_err(|_| ErrorCode::InvalidArgument)?;
        self.cursors()
            .entries
            .iter()
            .find(|(n, r, _)| *n == number && r == room)
            .map(|(_, _, token)| token.clone())
            .ok_or(ErrorCode::NotFound)
    }

    pub fn has_room(client: &Client, room: &str) -> bool {
        joined_room(client, room).is_ok()
    }

    /// The other person in a joined direct-message room; `not_found` for any
    /// other room.
    pub async fn contact_status(client: &Client, room: &str) -> Result<ContactStatus, ErrorCode> {
        let joined = joined_room(client, room)?;
        contacts::status(client, &joined)
            .await
            .ok_or(ErrorCode::NotFound)
    }

    pub fn room_list(client: &Client) -> RoomList {
        let mut rooms = client.joined_rooms();
        rooms.sort_by(|a, b| a.room_id().cmp(b.room_id()));
        let truncated = rooms.len() > MAX_ROOMS;
        RoomList {
            rooms: rooms
                .into_iter()
                .take(MAX_ROOMS)
                .map(|room| {
                    let contact = contacts::contact_of(client, &room);
                    RoomSummary {
                        room_id: room.room_id().to_string(),
                        // A direct-message room without a name of its own is
                        // named after its contact, not the SDK's member count.
                        display_name: match (room.name(), &contact) {
                            (None, Some(contact)) => Some(contacts::shown(contact)),
                            _ => room
                                .cached_display_name()
                                .map(|name| project_name(&name.to_string())),
                        },
                        unread_count: u32::try_from(room.num_unread_messages()).unwrap_or(u32::MAX),
                        encryption: match room.encryption_state() {
                            EncryptionState::Encrypted => RoomEncryption::Encrypted,
                            EncryptionState::NotEncrypted => RoomEncryption::NotEncrypted,
                            EncryptionState::Unknown => RoomEncryption::Unknown,
                        },
                        contact: contact.as_ref().map(|contact| contacts::shown(contact)),
                    }
                })
                .collect(),
            truncated,
        }
    }

    /// One page of history older than `cursor`, or the newest page.
    pub async fn timeline_page(
        &self,
        client: &Client,
        room: &str,
        cursor: Option<&str>,
    ) -> Result<TimelinePage, ErrorCode> {
        let joined = joined_room(client, room)?;
        let from = cursor.map(|cursor| self.token(room, cursor)).transpose()?;
        let mut limit = MAX_PAGE_ITEMS;
        loop {
            let mut options = MessagesOptions::backward().from(from.as_deref());
            options.limit = UInt::try_from(limit as u64).unwrap_or(UInt::MIN);
            let messages = joined
                .messages(options)
                .await
                .map_err(|_| ErrorCode::Unavailable)?;
            // Backward pagination returns newest first; pages read oldest first.
            let mut items: Vec<TimelineItem> =
                messages.chunk.iter().rev().filter_map(project).collect();
            let bytes: usize = items
                .iter()
                .map(|item| serde_json::to_vec(item).map_or(usize::MAX, |v| v.len()))
                .fold(0usize, usize::saturating_add);
            if bytes > MAX_PAGE_BYTES && limit > 1 {
                limit /= 2;
                continue;
            }
            if bytes > MAX_PAGE_BYTES {
                // One event over the budget cannot happen with projected
                // bodies; drop it rather than exceed the frame.
                items.clear();
            }
            let next_cursor = messages
                .end
                .filter(|_| !messages.chunk.is_empty())
                .map(|token| self.remember(room, token));
            return Ok(TimelinePage {
                room_id: room.to_owned(),
                items,
                next_cursor,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::deserialized_responses::{VerificationLevel, WithheldCode};

    /// GIVEN each reason the SDK gives for not decrypting WHEN projected THEN
    /// an untrusted sender and a withheld key get their own notice, and every
    /// other reason (including a missing key that may still arrive) is
    /// `undecryptable`.
    #[test]
    fn given_undecryptable_reasons_then_each_maps_to_its_notice() {
        for level in [
            VerificationLevel::UnsignedDevice,
            VerificationLevel::VerificationViolation,
            VerificationLevel::UnverifiedIdentity,
        ] {
            assert_eq!(
                undecryptable_notice(&UnableToDecryptReason::SenderIdentityNotTrusted(level)),
                NoticeKind::UntrustedSender
            );
        }
        for code in [WithheldCode::Unverified, WithheldCode::Blacklisted] {
            assert_eq!(
                undecryptable_notice(&UnableToDecryptReason::MissingMegolmSession {
                    withheld_code: Some(code)
                }),
                NoticeKind::Withheld
            );
        }
        for reason in [
            UnableToDecryptReason::MissingMegolmSession {
                withheld_code: None,
            },
            UnableToDecryptReason::UnknownMegolmMessageIndex,
            UnableToDecryptReason::MegolmDecryptionFailure,
            UnableToDecryptReason::MismatchedIdentityKeys,
            UnableToDecryptReason::Unknown,
        ] {
            assert_eq!(undecryptable_notice(&reason), NoticeKind::Undecryptable);
        }
    }

    fn plain_event(content: serde_json::Value) -> TimelineEvent {
        let event = serde_json::json!({
            "type": "m.room.message",
            "event_id": "$image",
            "sender": "@alice:example.org",
            "origin_server_ts": 1_767_258_000_000u64,
            "content": content,
        });
        TimelineEvent::from_plaintext(
            matrix_sdk::ruma::serde::Raw::from_json_string(event.to_string()).unwrap(),
        )
    }

    fn image_item(content: serde_json::Value) -> TimelineItem {
        project(&plain_event(content)).unwrap()
    }

    /// GIVEN a plain and an encrypted `m.image` WHEN projected THEN each is an
    /// image item with its declared layout data, its file name, and whether
    /// the file is encrypted; nothing names the media URL.
    #[test]
    fn given_image_events_then_they_become_image_items() {
        let plain = image_item(serde_json::json!({
            "msgtype": "m.image",
            "body": "caption or name",
            "filename": "cat.png",
            "url": "mxc://example.org/abc",
            "info": { "w": 640, "h": 480, "size": 48213, "mimetype": "image/png" },
        }));
        let TimelineItem::Image {
            event_id,
            sender,
            origin_server_ts,
            name,
            size,
            width,
            height,
            encrypted,
        } = &plain
        else {
            panic!("an image item: {plain:?}");
        };
        assert_eq!(
            (event_id.as_str(), sender.as_str()),
            ("$image", "@alice:example.org")
        );
        assert_eq!(
            origin_server_ts.map(UnixMillis::get),
            Some(1_767_258_000_000)
        );
        assert_eq!(name, "cat.png");
        assert_eq!(
            (*size, *width, *height, *encrypted),
            (Some(48213), Some(640), Some(480), false)
        );
        assert!(!serde_json::to_string(&plain).unwrap().contains("mxc:"));

        let encrypted = image_item(serde_json::json!({
            "msgtype": "m.image",
            "body": "photo.jpg",
            "file": {
                "url": "mxc://example.org/enc",
                "key": {
                    "kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR",
                    "k": "b50ACIv6LMn9AfMCFD1POJI_UAFWIclxAN1kWrEO2X8", "ext": true,
                },
                "iv": "AK1wyzigZtQAAAABAAAAKK",
                "hashes": { "sha256": "/NogKqW5bz/m8xHgFiH5haFGjCNVmUIPLzfvOhHdrxY" },
                "v": "v2",
            },
        }));
        let TimelineItem::Image {
            name,
            size,
            width,
            encrypted,
            ..
        } = &encrypted
        else {
            panic!("an image item");
        };
        assert_eq!(name, "photo.jpg", "the body names it without a file name");
        assert_eq!((*size, *width, *encrypted), (None, None, true));
        assert!(
            !serde_json::to_string(&encrypted)
                .unwrap()
                .contains("b50ACIv6"),
            "no key"
        );
    }

    /// GIVEN hostile names and absurd declared sizes WHEN projected THEN the
    /// name is bounded inert text, and sizes out of range are dropped: width
    /// and height only together and within the decoder's limit, the size only
    /// above zero and within u32.
    #[test]
    fn given_hostile_image_metadata_then_it_is_bounded_and_dropped_when_absurd() {
        let content = |name: &str, info: serde_json::Value| {
            serde_json::json!({
                "msgtype": "m.image",
                "body": name,
                "url": "mxc://example.org/abc",
                "info": info,
            })
        };
        let hostile = format!("evil\u{202E}gnp.exe\n<img src=x>{}", "x".repeat(1000));
        let item = image_item(content(&hostile, serde_json::json!({})));
        let TimelineItem::Image { name, .. } = &item else {
            panic!("an image item");
        };
        assert!(
            !name.contains('\u{202E}') && !name.contains('\n'),
            "{name:?}"
        );
        assert!(name.chars().count() <= crate::views::MAX_DISPLAY_NAME_CHARS + 1);
        assert!(
            name.starts_with("evil\u{FFFD}gnp.exe <img src=x>"),
            "{name:?}"
        );

        for (info, expected) in [
            (serde_json::json!({ "w": 0, "h": 10 }), (None, None)),
            (serde_json::json!({ "w": 100000, "h": 10 }), (None, None)),
            (
                serde_json::json!({ "w": 8192, "h": 8192 }),
                (Some(8192), Some(8192)),
            ),
            (serde_json::json!({ "w": 10 }), (None, None)),
            (serde_json::json!({ "h": 10 }), (None, None)),
        ] {
            let TimelineItem::Image { width, height, .. } = image_item(content("n", info.clone()))
            else {
                panic!("an image item");
            };
            assert_eq!((width, height), expected, "{info}");
        }
        for (info, expected) in [
            (serde_json::json!({ "size": 0 }), None),
            (serde_json::json!({ "size": 4294967296u64 }), None),
            (serde_json::json!({ "size": 9007199254740991u64 }), None),
            (
                serde_json::json!({ "size": 21 * 1024 * 1024 }),
                Some(21 * 1024 * 1024),
            ),
        ] {
            let TimelineItem::Image { size, .. } = image_item(content("n", info.clone())) else {
                panic!("an image item");
            };
            assert_eq!(size, expected, "{info}");
        }
        // An image event without any usable source is not an image.
        let broken = plain_event(serde_json::json!({ "msgtype": "m.image", "body": "x" }));
        assert!(!matches!(
            project(&broken),
            Some(TimelineItem::Image { .. })
        ));
        assert!(crate::media::image_content(&broken).is_none());
    }

    #[test]
    fn given_many_tokens_then_only_the_newest_are_kept_and_cursors_are_room_bound() {
        let views = MatrixViews::default();
        let first = views.remember("!a:s", "token-0".into());
        let mut last = String::new();
        for n in 1..=MAX_CURSORS {
            last = views.remember("!a:s", format!("token-{n}"));
        }
        assert_eq!(
            views.token("!a:s", &first),
            Err(ErrorCode::NotFound),
            "evicted"
        );
        assert_eq!(
            views.token("!a:s", &last).as_deref(),
            Ok(&*format!("token-{MAX_CURSORS}"))
        );
        assert_eq!(
            views.token("!b:s", &last),
            Err(ErrorCode::NotFound),
            "other room"
        );
        assert_eq!(views.cursors().entries.len(), MAX_CURSORS);
    }

    #[test]
    fn given_malformed_cursors_then_they_are_invalid() {
        let views = MatrixViews::default();
        views.remember("!a:s", "token".into());
        for cursor in [
            "",
            "t1.0",
            "m1.",
            "m1.01",
            "m1.-1",
            "m1.1a",
            "m1.٣",
            &"m1.9".repeat(30),
        ] {
            assert_eq!(
                views.token("!a:s", cursor),
                Err(ErrorCode::InvalidArgument),
                "{cursor:?}"
            );
        }
        assert_eq!(views.token("!a:s", "m1.0").as_deref(), Ok("token"));
        assert_eq!(views.token("!a:s", "m1.99"), Err(ErrorCode::NotFound));
    }

    #[test]
    fn given_bad_room_ids_then_they_are_invalid() {
        for room in [
            "",
            "room",
            "#alias:s",
            &format!("!{}", "r".repeat(MAX_ROOM_ID_BYTES)),
        ] {
            assert_eq!(
                room_id(room).err(),
                Some(ErrorCode::InvalidArgument),
                "{room:?}"
            );
        }
        assert!(room_id("!r:example.org").is_ok());
    }
}
