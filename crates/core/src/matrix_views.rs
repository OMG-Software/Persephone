//! Room list and timeline pages from the signed-in Matrix client, projected
//! with the same limits as the synthetic views (`views.rs`).
//!
//! * Names and senders go through `views::project_name`; message bodies
//!   through `views::project_body`.
//! * Text messages (text, notice, emote) are shown as messages. Other
//!   `m.room.message` types become an `unsupported` notice and events this
//!   device cannot decrypt an `undecryptable` notice, so neither disappears.
//!   State events, reactions and other non-message events are not shown.
//! * A page holds at most `MAX_PAGE_ITEMS` items and `MAX_PAGE_BYTES`
//!   serialized. The SDK cannot resume inside a fetched chunk, so a chunk that
//!   would exceed the byte budget is fetched again with half the limit, down to
//!   one event, instead of being cut.
//! * The homeserver's pagination tokens stay in the core. The UI gets short
//!   cursors (`m1.<n>`) for at most `MAX_CURSORS` tokens; an evicted or unknown
//!   cursor is `not_found`.

use crate::views::{MAX_PAGE_BYTES, MAX_PAGE_ITEMS, MAX_ROOMS, project_body, project_name};
use matrix_sdk::{
    Client, RoomState,
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    room::MessagesOptions,
    ruma::{
        RoomId, UInt,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            room::message::MessageType,
        },
    },
};
use persephone_contracts::{NoticeKind, RoomList, RoomSummary, TimelineItem, TimelinePage};
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
    if matches!(event.kind, TimelineEventKind::UnableToDecrypt { .. }) {
        return Some(TimelineItem::Notice {
            event_id,
            sender,
            notice: NoticeKind::Undecryptable,
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
        },
        MessageType::Notice(notice) => TimelineItem::Message {
            event_id,
            sender,
            body: project_body(&notice.body),
        },
        MessageType::Emote(emote) => TimelineItem::Message {
            event_id,
            sender,
            body: project_body(&emote.body),
        },
        _ => TimelineItem::Notice {
            event_id,
            sender,
            notice: NoticeKind::Unsupported,
        },
    })
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

    pub fn room_list(client: &Client) -> RoomList {
        let mut rooms = client.joined_rooms();
        rooms.sort_by(|a, b| a.room_id().cmp(b.room_id()));
        let truncated = rooms.len() > MAX_ROOMS;
        RoomList {
            rooms: rooms
                .into_iter()
                .take(MAX_ROOMS)
                .map(|room| RoomSummary {
                    room_id: room.room_id().to_string(),
                    display_name: room
                        .cached_display_name()
                        .map(|name| project_name(&name.to_string())),
                    unread_count: u32::try_from(room.num_unread_messages()).unwrap_or(u32::MAX),
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
