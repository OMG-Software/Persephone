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
//!   the decryption settings refuse; ADR-0005) or `undecryptable`.
//! * Membership changes and changes to the room's name, topic, picture,
//!   encryption, access, history visibility, power levels, addresses, server
//!   ACL and version are `state` items (`state_change`): who joined or was
//!   removed, and who changed what, matters most in an encrypted room. A
//!   change of picture alone, a redacted state event, and other state are not
//!   shown. Reactions and other non-message events are not shown.
//! * An edit (`m.replace`) of a text message is an `edit` item only when its
//!   sender sent the edited message, compared by full user ID: anyone can
//!   send an edit naming someone else's message. The edited message's sender
//!   comes from the same page, from up to `MAX_KNOWN_SENDERS` remembered, or
//!   from the homeserver for up to `MAX_EDIT_LOOKUPS` edits a page; an edit
//!   that cannot be checked is left out. An edit is never shown as a message
//!   of its own.
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
        EventId, OwnedEventId, OwnedUserId, RoomId, UInt,
        events::{
            AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
            SyncStateEvent,
            room::{
                member::MembershipChange as Change,
                message::{
                    MessageType, OriginalSyncRoomMessageEvent, Relation,
                    sanitize::remove_plain_reply_fallback,
                },
            },
        },
    },
};
use persephone_contracts::{
    ContactStatus, MembershipChange, NoticeKind, RoomEncryption, RoomList, RoomSummary,
    StateChange, TimelineItem, TimelinePage, UnixMillis,
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

/// Most senders of editable messages remembered, across all rooms.
pub const MAX_KNOWN_SENDERS: usize = 512;
/// Most edited messages a page reads from the homeserver to check an edit's
/// sender; an edit beyond them is left out of that page.
pub const MAX_EDIT_LOOKUPS: usize = 8;

#[derive(Default)]
pub struct MatrixViews {
    cursors: Mutex<Cursors>,
    /// Oldest first: (room, event, sender) of messages an edit may replace,
    /// so an edit's sender can be checked without asking the homeserver.
    senders: Mutex<VecDeque<(String, OwnedEventId, OwnedUserId)>>,
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

/// The event's `origin_server_ts`. Read from the raw event, so an encrypted
/// one has its time too. `UInt` is at most 2^53 - 1, which `UnixMillis`
/// holds.
fn server_time(event: &TimelineEvent) -> Option<UnixMillis> {
    event
        .raw()
        .get_field::<UInt>("origin_server_ts")
        .ok()
        .flatten()
        .and_then(|millis| UnixMillis::new(millis.into()))
}

/// The event as an original (not redacted) `m.room.message`.
fn room_message(event: &TimelineEvent) -> Option<OriginalSyncRoomMessageEvent> {
    match event.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(message),
        ))) => Some(message),
        _ => None,
    }
}

/// The text of a text, notice or emote message.
fn text_body(msgtype: &MessageType) -> Option<&str> {
    match msgtype {
        MessageType::Text(text) => Some(&text.body),
        MessageType::Notice(notice) => Some(&notice.body),
        MessageType::Emote(emote) => Some(&emote.body),
        _ => None,
    }
}

/// The sender of the event when an edit may replace it: an original text
/// message that is not an edit itself.
fn editable_sender(event: &TimelineEvent) -> Option<OwnedUserId> {
    let message = room_message(event)?;
    let editable = text_body(&message.content.msgtype).is_some()
        && !matches!(message.content.relates_to, Some(Relation::Replacement(_)));
    editable.then_some(message.sender)
}

/// What an edit (`m.replace`) claims, before its sender is checked against
/// the edited message's.
struct EditClaim {
    replaces: OwnedEventId,
    sender: OwnedUserId,
    /// The projected new text.
    body: String,
}

/// The claim of an edit whose new content is text, or `None` for any other
/// event.
fn edit_claim(event: &TimelineEvent) -> Option<EditClaim> {
    let message = room_message(event)?;
    let Some(Relation::Replacement(replacement)) = &message.content.relates_to else {
        return None;
    };
    let body = text_body(&replacement.new_content.msgtype)?;
    Some(EditClaim {
        replaces: replacement.event_id.clone(),
        sender: message.sender.clone(),
        body: project_body(body),
    })
}

/// The visible form of one event, or `None` for events not shown. An edit
/// is never shown as a message; `MatrixViews::timeline_page` checks it.
fn project(event: &TimelineEvent) -> Option<TimelineItem> {
    let event_id = event.event_id()?.to_string();
    let sender = project_name(event.sender()?.as_str());
    let origin_server_ts = server_time(event);
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
    let message = match event.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(message),
        ))) => message,
        Ok(AnySyncTimelineEvent::State(state)) => {
            return state_change(&state).map(|change| TimelineItem::State {
                event_id,
                sender,
                origin_server_ts,
                change,
            });
        }
        _ => return None,
    };
    // A reply's quoted fallback is the sender's copy of another event: it
    // can say anything, so it is dropped and the page quotes the event
    // itself from history it loaded.
    let reply_to = match &message.content.relates_to {
        Some(Relation::Reply(reply)) => Some(reply.in_reply_to.event_id.to_string()),
        Some(Relation::Replacement(_)) => return None,
        _ => None,
    };
    let text = |body: &str| {
        project_body(if reply_to.is_some() {
            remove_plain_reply_fallback(body)
        } else {
            body
        })
    };
    Some(match &message.content.msgtype {
        MessageType::Text(text_content) => TimelineItem::Message {
            event_id,
            sender,
            body: text(&text_content.body),
            origin_server_ts,
            reply_to,
        },
        MessageType::Notice(notice) => TimelineItem::Message {
            event_id,
            sender,
            body: text(&notice.body),
            origin_server_ts,
            reply_to,
        },
        MessageType::Emote(emote) => TimelineItem::Message {
            event_id,
            sender,
            body: text(&emote.body),
            origin_server_ts,
            reply_to,
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

/// A name or topic as shown: projected, and `None` when empty.
fn shown_text(raw: &str) -> Option<String> {
    let shown = project_name(raw);
    (!shown.trim().is_empty()).then_some(shown)
}

/// What a state event changed, or `None` for a change not shown.
fn state_change(event: &AnySyncStateEvent) -> Option<StateChange> {
    use crate::room_settings::{history_visibility, join_rule};
    Some(match event {
        AnySyncStateEvent::RoomMember(SyncStateEvent::Original(member)) => {
            let target = project_name(member.state_key.as_str());
            let membership = match member.membership_change() {
                Change::Joined | Change::InvitationAccepted => MembershipChange::Joined,
                Change::Left => MembershipChange::Left,
                Change::Invited | Change::KnockAccepted => MembershipChange::Invited,
                Change::InvitationRejected => MembershipChange::InviteRejected,
                Change::InvitationRevoked => MembershipChange::InviteWithdrawn,
                Change::Kicked => MembershipChange::Kicked,
                Change::Banned | Change::KickedAndBanned => MembershipChange::Banned,
                Change::Unbanned => MembershipChange::Unbanned,
                Change::Knocked => MembershipChange::Knocked,
                Change::KnockRetracted => MembershipChange::KnockWithdrawn,
                Change::KnockDenied => MembershipChange::KnockDenied,
                Change::ProfileChanged {
                    displayname_change: Some(change),
                    ..
                } => {
                    return Some(StateChange::DisplayName {
                        target,
                        name: change.new.and_then(shown_text),
                    });
                }
                // A new picture alone, no change, or one ruma cannot name.
                _ => return None,
            };
            StateChange::Member { target, membership }
        }
        AnySyncStateEvent::RoomName(SyncStateEvent::Original(name)) => StateChange::Name {
            name: shown_text(&name.content.name),
        },
        AnySyncStateEvent::RoomTopic(SyncStateEvent::Original(topic)) => StateChange::Topic {
            topic: shown_text(&topic.content.topic),
        },
        AnySyncStateEvent::RoomAvatar(SyncStateEvent::Original(_)) => StateChange::Avatar {},
        AnySyncStateEvent::RoomEncryption(SyncStateEvent::Original(_)) => {
            StateChange::Encryption {}
        }
        AnySyncStateEvent::RoomJoinRules(SyncStateEvent::Original(rules)) => {
            StateChange::JoinRule {
                rule: join_rule(Some(rules.content.join_rule.clone())),
            }
        }
        AnySyncStateEvent::RoomHistoryVisibility(SyncStateEvent::Original(history)) => {
            StateChange::HistoryVisibility {
                visibility: history_visibility(Some(history.content.history_visibility.clone())),
            }
        }
        AnySyncStateEvent::RoomPowerLevels(SyncStateEvent::Original(_)) => {
            StateChange::PowerLevels {}
        }
        AnySyncStateEvent::RoomCanonicalAlias(SyncStateEvent::Original(_)) => {
            StateChange::Addresses {}
        }
        AnySyncStateEvent::RoomCreate(SyncStateEvent::Original(_)) => StateChange::Created {},
        AnySyncStateEvent::RoomServerAcl(SyncStateEvent::Original(_)) => StateChange::ServerAcl {},
        AnySyncStateEvent::RoomTombstone(SyncStateEvent::Original(_)) => StateChange::Upgraded {},
        _ => return None,
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

    fn senders(&self) -> MutexGuard<'_, VecDeque<(String, OwnedEventId, OwnedUserId)>> {
        self.senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remembers the sender of an event an edit may replace.
    fn remember_sender(&self, room: &str, event: &TimelineEvent) {
        let (Some(event_id), Some(sender)) = (event.event_id(), editable_sender(event)) else {
            return;
        };
        let mut senders = self.senders();
        if senders
            .iter()
            .any(|(r, id, _)| r == room && *id == event_id)
        {
            return;
        }
        senders.push_back((room.to_owned(), event_id.to_owned(), sender));
        while senders.len() > MAX_KNOWN_SENDERS {
            senders.pop_front();
        }
    }

    fn known_sender(&self, room: &str, event_id: &EventId) -> Option<OwnedUserId> {
        self.senders()
            .iter()
            .find(|(r, id, _)| r == room && id == event_id)
            .map(|(_, _, sender)| sender.clone())
    }

    /// The edit as a timeline item when its sender sent the edited message,
    /// which must be a text message that is not an edit itself. The edited
    /// message's sender comes from `chunk`, from those remembered, or, while
    /// `lookups` allows, from the homeserver; otherwise the edit is left out.
    async fn checked_edit(
        &self,
        joined: &matrix_sdk::Room,
        room: &str,
        chunk: &[TimelineEvent],
        edit: &TimelineEvent,
        claim: EditClaim,
        lookups: &mut usize,
    ) -> Option<TimelineItem> {
        let event_id = edit.event_id()?.to_string();
        let in_chunk = chunk
            .iter()
            .find(|event| event.event_id() == Some(&*claim.replaces));
        let original_sender = match in_chunk {
            Some(event) => editable_sender(event),
            None => match self.known_sender(room, &claim.replaces) {
                Some(sender) => Some(sender),
                None if *lookups < MAX_EDIT_LOOKUPS => {
                    *lookups += 1;
                    let original = joined.event(&claim.replaces, None).await.ok()?;
                    self.remember_sender(room, &original);
                    editable_sender(&original)
                }
                None => None,
            },
        };
        (original_sender? == claim.sender).then(|| TimelineItem::Edit {
            event_id,
            sender: project_name(claim.sender.as_str()),
            origin_server_ts: server_time(edit),
            replaces: claim.replaces.to_string(),
            body: claim.body,
        })
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
            let mut items = Vec::new();
            let mut lookups = 0;
            for event in messages.chunk.iter().rev() {
                self.remember_sender(room, event);
                let item = match edit_claim(event) {
                    Some(claim) => {
                        self.checked_edit(
                            &joined,
                            room,
                            &messages.chunk,
                            event,
                            claim,
                            &mut lookups,
                        )
                        .await
                    }
                    None => project(event),
                };
                items.extend(item);
            }
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

    /// GIVEN a reply with a quoted fallback, which the sender writes and can
    /// make say anything WHEN projected THEN the item names the event it
    /// replies to and its body is only the reply's own text; text that only
    /// looks like a quote, outside a reply, is kept.
    #[test]
    fn given_a_reply_then_the_fallback_is_dropped_and_the_event_named() {
        let reply = project(&plain_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "> <@bob:example.org> I never said this\n> second line\n\nmy answer",
            "m.relates_to": { "m.in_reply_to": { "event_id": "$asked" } },
        })))
        .unwrap();
        let TimelineItem::Message { body, reply_to, .. } = reply else {
            panic!("a message: {reply:?}");
        };
        assert_eq!(body, "my answer");
        assert_eq!(reply_to.as_deref(), Some("$asked"));

        let quote = project(&plain_event(serde_json::json!({
            "msgtype": "m.text",
            "body": "> <@bob:example.org> quoted\n\nmine",
        })))
        .unwrap();
        let TimelineItem::Message { body, reply_to, .. } = quote else {
            panic!("a message");
        };
        assert_eq!(body, "> <@bob:example.org> quoted\n\nmine");
        assert_eq!(reply_to, None);
    }

    fn state_event(
        kind: &str,
        sender: &str,
        state_key: &str,
        content: serde_json::Value,
        prev_content: Option<serde_json::Value>,
    ) -> TimelineEvent {
        let mut event = serde_json::json!({
            "type": kind,
            "event_id": "$state",
            "sender": sender,
            "state_key": state_key,
            "origin_server_ts": 1_767_258_000_000u64,
            "content": content,
        });
        if let Some(prev) = prev_content {
            event["unsigned"] = serde_json::json!({ "prev_content": prev });
        }
        TimelineEvent::from_plaintext(
            matrix_sdk::ruma::serde::Raw::from_json_string(event.to_string()).unwrap(),
        )
    }

    fn change_of(event: &TimelineEvent) -> Option<StateChange> {
        match project(event) {
            Some(TimelineItem::State { change, .. }) => Some(change),
            None => None,
            Some(other) => panic!("a state item or nothing: {other:?}"),
        }
    }

    fn member(
        sender: &str,
        target: &str,
        membership: &str,
        prev: Option<&str>,
    ) -> Option<StateChange> {
        change_of(&state_event(
            "m.room.member",
            sender,
            target,
            serde_json::json!({ "membership": membership }),
            prev.map(|prev| serde_json::json!({ "membership": prev })),
        ))
    }

    /// GIVEN membership events WHEN projected THEN each names the member
    /// and what happened to them, telling a member leaving from one removed
    /// by someone else.
    #[test]
    fn given_membership_events_then_each_becomes_a_member_change() {
        const A: &str = "@alice:example.org";
        const B: &str = "@bob:example.org";
        let change = |membership| {
            Some(StateChange::Member {
                target: B.into(),
                membership,
            })
        };
        for (sender, membership, prev, expected) in [
            (B, "join", None, MembershipChange::Joined),
            (B, "join", Some("invite"), MembershipChange::Joined),
            (B, "leave", Some("join"), MembershipChange::Left),
            (A, "leave", Some("join"), MembershipChange::Kicked),
            (A, "ban", Some("join"), MembershipChange::Banned),
            (A, "leave", Some("ban"), MembershipChange::Unbanned),
            (A, "invite", None, MembershipChange::Invited),
            (B, "leave", Some("invite"), MembershipChange::InviteRejected),
            (
                A,
                "leave",
                Some("invite"),
                MembershipChange::InviteWithdrawn,
            ),
            (B, "knock", None, MembershipChange::Knocked),
            (B, "leave", Some("knock"), MembershipChange::KnockWithdrawn),
        ] {
            assert_eq!(
                member(sender, B, membership, prev),
                change(expected),
                "{sender} {prev:?} -> {membership}"
            );
        }
        // No change at all is not shown.
        assert_eq!(member(B, B, "join", Some("join")), None);
    }

    /// GIVEN a member changing their display name, to something hostile or
    /// to nothing, or only their picture WHEN projected THEN the name is
    /// inert bounded text, a removed name is `None`, and a picture change
    /// alone is not shown.
    #[test]
    fn given_profile_changes_then_names_are_projected_and_pictures_alone_hidden() {
        const B: &str = "@bob:example.org";
        let profile = |content: serde_json::Value, prev: serde_json::Value| {
            change_of(&state_event("m.room.member", B, B, content, Some(prev)))
        };
        let hostile = format!("Admin\u{202E}\n{}", "x".repeat(500));
        let Some(StateChange::DisplayName { target, name }) = profile(
            serde_json::json!({ "membership": "join", "displayname": hostile }),
            serde_json::json!({ "membership": "join", "displayname": "Bob" }),
        ) else {
            panic!("a display name change");
        };
        assert_eq!(target, B);
        let name = name.unwrap();
        assert!(
            !name.contains('\u{202E}') && !name.contains('\n'),
            "{name:?}"
        );
        assert!(name.chars().count() <= crate::views::MAX_DISPLAY_NAME_CHARS + 1);
        assert_eq!(
            profile(
                serde_json::json!({ "membership": "join" }),
                serde_json::json!({ "membership": "join", "displayname": "Bob" }),
            ),
            Some(StateChange::DisplayName {
                target: B.into(),
                name: None
            })
        );
        assert_eq!(
            profile(
                serde_json::json!({ "membership": "join", "avatar_url": "mxc://s/new" }),
                serde_json::json!({ "membership": "join", "avatar_url": "mxc://s/old" }),
            ),
            None
        );
    }

    /// GIVEN changes to the room's state WHEN projected THEN each security-
    /// relevant one is shown, names and topics are projected and empty ones
    /// are `None`, and an unknown state type is not shown.
    #[test]
    fn given_room_state_events_then_known_changes_are_shown() {
        const A: &str = "@alice:example.org";
        let room = |kind: &str, content: serde_json::Value| {
            change_of(&state_event(kind, A, "", content, None))
        };
        assert_eq!(
            room(
                "m.room.name",
                serde_json::json!({ "name": "Plans\u{202E}" })
            ),
            Some(StateChange::Name {
                name: Some("Plans\u{FFFD}".into())
            })
        );
        assert_eq!(
            room("m.room.name", serde_json::json!({ "name": "" })),
            Some(StateChange::Name { name: None })
        );
        assert_eq!(
            room(
                "m.room.topic",
                serde_json::json!({ "topic": "line one\nline two" })
            ),
            Some(StateChange::Topic {
                topic: Some("line one line two".into())
            })
        );
        assert_eq!(
            room(
                "m.room.encryption",
                serde_json::json!({ "algorithm": "m.megolm.v1.aes-sha2" })
            ),
            Some(StateChange::Encryption {})
        );
        assert_eq!(
            room(
                "m.room.join_rules",
                serde_json::json!({ "join_rule": "public" })
            ),
            Some(StateChange::JoinRule {
                rule: persephone_contracts::JoinRule::Public
            })
        );
        assert_eq!(
            room(
                "m.room.history_visibility",
                serde_json::json!({ "history_visibility": "world_readable" })
            ),
            Some(StateChange::HistoryVisibility {
                visibility: persephone_contracts::HistoryVisibility::WorldReadable
            })
        );
        assert_eq!(
            room(
                "m.room.power_levels",
                serde_json::json!({ "users": { A: 100 } })
            ),
            Some(StateChange::PowerLevels {})
        );
        assert_eq!(
            room(
                "m.room.server_acl",
                serde_json::json!({ "allow": ["*"], "deny": [] })
            ),
            Some(StateChange::ServerAcl {})
        );
        assert_eq!(
            room("org.example.custom", serde_json::json!({ "x": 1 })),
            None
        );
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
