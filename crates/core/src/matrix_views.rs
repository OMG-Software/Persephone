//! Room list and timeline pages from the signed-in Matrix client, projected
//! with the same limits as the synthetic views (`views.rs`).
//!
//! * Names and senders go through `views::project_name`; message bodies
//!   through `views::project_body`. A direct-message room names its contact
//!   (`contacts.rs`), and is named after them when it has no name.
//! * Each item carries the event's `origin_server_ts`: the time the sender's
//!   homeserver claims, unchecked. An event without a usable one has none.
//! * Text messages (text, notice, emote, server notice) are shown as
//!   messages, images as image items and files as file items, without
//!   fetching anything (`media.rs`, `files.rs`). An encrypted event that
//!   cannot be shown becomes a notice saying why: `withheld` (the sender
//!   withheld the key from this device), `untrusted_sender` (the sending
//!   device is not signed by its owner, which the decryption settings refuse;
//!   ADR-0005) or `undecryptable`.
//! * Membership changes and changes to the room's name, topic, picture,
//!   encryption, access, history visibility, power levels, addresses, server
//!   ACL and version are `state` items (`state_change`): who joined or was
//!   removed, and who changed what, matters most in an encrypted room. A
//!   reaction is a `reaction` item naming the event it reacts to, with its
//!   key projected like a name.
//! * Every other event is an `other` item (`other_item`): its kind, such as
//!   a verification step, a call, a poll or a sticker, and its type projected
//!   like a name. Nothing from its content is sent, so nothing disappears and
//!   nothing new is trusted.
//! * Items the account's switches hide are left out before a page fills
//!   (`timeline_filter`). While a page has room, older chunks are fetched,
//!   at most `MAX_PAGE_FETCHES`, so hidden events do not crowd out messages.
//! * An edit (`m.replace`) of a text message is an `edit` item only when its
//!   sender sent the edited message, compared by full user ID: anyone can
//!   send an edit naming someone else's message. The edited message's sender
//!   comes from the same page, from up to `MAX_KNOWN_SENDERS` remembered, or
//!   from the homeserver for up to `MAX_EDIT_LOOKUPS` edits a page; an edit
//!   that cannot be checked is left out. An edit is never shown as a message
//!   of its own.
//! * A deleted (redacted) message is a `deleted` notice, and a deletion is a
//!   `redaction` item naming the deleted event, so the page can mark a
//!   message it already shows.
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
        EventId, OwnedEventId, OwnedUserId, RoomId, UInt, UserId,
        api::client::filter::LazyLoadOptions,
        events::{
            AnyStateEvent, AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
            SyncMessageLikeEvent, SyncStateEvent,
            room::{
                guest_access::GuestAccess,
                member::{MembershipChange as Change, MembershipState},
                message::{
                    MessageType, OriginalSyncRoomMessageEvent, Relation,
                    sanitize::remove_plain_reply_fallback,
                },
                redaction::SyncRoomRedactionEvent,
            },
        },
        serde::Raw,
    },
};
use persephone_contracts::{
    ContactStatus, MembershipChange, NoticeKind, OtherEvent, RoomEncryption, RoomList, RoomSummary,
    SenderName, StateChange, TimelineEvents, TimelineItem, TimelinePage, UnixMillis,
};
use persephone_core_protocol::{ErrorCode, MAX_CURSOR_BYTES, MAX_ROOM_ID_BYTES};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
/// Most chunks one timeline page fetches while switched-off events leave it
/// with room: at most this many requests and `MAX_PAGE_ITEMS` times this
/// many events a page, however many events the switches hide.
pub const MAX_PAGE_FETCHES: usize = 5;

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

/// Require a valid room ID and locally known joined membership before a room
/// operation. A known invited or left room does not grant this authority.
pub(crate) fn joined_room(client: &Client, raw: &str) -> Result<matrix_sdk::Room, ErrorCode> {
    let id = room_id(raw)?;
    client
        .get_room(id)
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)
}

/// Ignore message content, while retaining room state and deletions. A
/// moderator's redaction must still remove another sender's earlier text.
pub(crate) fn ignored_content(
    event: &TimelineEvent,
    ignored: &std::collections::BTreeSet<OwnedUserId>,
) -> bool {
    event
        .sender()
        .is_some_and(|sender| ignored.contains(&sender))
        && event
            .raw()
            .get_field::<serde_json::Value>("state_key")
            .ok()
            .flatten()
            .is_none()
        && event
            .raw()
            .get_field::<String>("type")
            .ok()
            .flatten()
            .as_deref()
            != Some("m.room.redaction")
}

/// The event's `origin_server_ts`. Read from the raw event, so an encrypted
/// one has its time too. `UInt` is at most 2^53 - 1, which `UnixMillis`
/// holds.
pub(crate) fn server_time(event: &TimelineEvent) -> Option<UnixMillis> {
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
/// message (`m.text`, not a notice or emote) that is not an edit itself, the
/// same rule `sending::own_text_message` applies before this account sends
/// an edit.
fn editable_sender(event: &TimelineEvent) -> Option<OwnedUserId> {
    let message = room_message(event)?;
    let editable = matches!(message.content.msgtype, MessageType::Text(_))
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

/// Whether the event is a state event: it has a `state_key`.
fn is_state(event: &TimelineEvent) -> bool {
    event
        .raw()
        .get_field::<serde_json::Value>("state_key")
        .ok()
        .flatten()
        .is_some()
}

/// Whether the event was deleted: the homeserver keeps the deletion in
/// `unsigned`.
fn redacted(event: &TimelineEvent) -> bool {
    event
        .raw()
        .get_field::<serde_json::Value>("unsigned")
        .ok()
        .flatten()
        .is_some_and(|unsigned| unsigned.get("redacted_because").is_some())
}

/// Whether the event is a message, plain or encrypted, that was deleted. A
/// deleted sticker, poll or other message-like event keeps its kind
/// (`other_item`).
fn deleted_message(event: &TimelineEvent) -> bool {
    let is_message = event
        .raw()
        .get_field::<String>("type")
        .ok()
        .flatten()
        .is_some_and(|kind| matches!(kind.as_str(), "m.room.message" | "m.room.encrypted"));
    is_message && !is_state(event) && redacted(event)
}

/// The kind of a message-like event that has no other form, by its type.
fn event_kind(kind: &str) -> OtherEvent {
    match kind {
        "m.key.verification.request" => OtherEvent::VerificationRequest,
        "m.key.verification.ready" => OtherEvent::VerificationReady,
        "m.key.verification.start" => OtherEvent::VerificationStart,
        "m.key.verification.accept" | "m.key.verification.key" | "m.key.verification.mac" => {
            OtherEvent::VerificationStep
        }
        "m.key.verification.cancel" => OtherEvent::VerificationCancelled,
        "m.key.verification.done" => OtherEvent::VerificationDone,
        "m.call.invite"
        | "m.call.notify"
        | "org.matrix.msc4075.call.notify"
        | "m.rtc.notification"
        | "org.matrix.msc4075.rtc.notification" => OtherEvent::CallStarted,
        "m.call.answer" => OtherEvent::CallAnswered,
        "m.call.reject" | "m.rtc.decline" | "org.matrix.msc4310.rtc.decline" => {
            OtherEvent::CallDeclined
        }
        "m.call.hangup" => OtherEvent::CallEnded,
        "m.call.candidates"
        | "m.call.negotiate"
        | "m.call.replaces"
        | "m.call.select_answer"
        | "m.call.sdp_stream_metadata_changed"
        | "org.matrix.call.sdp_stream_metadata_changed"
        | "m.call.asserted_identity"
        | "org.matrix.call.asserted_identity"
        | "io.element.call.encryption_keys" => OtherEvent::CallSignalling,
        "m.poll.start" | "org.matrix.msc3381.poll.start" => OtherEvent::PollStarted,
        "m.poll.response" | "org.matrix.msc3381.poll.response" => OtherEvent::PollVote,
        "m.poll.end" | "org.matrix.msc3381.poll.end" => OtherEvent::PollEnded,
        "m.sticker" => OtherEvent::Sticker,
        "m.location" | "m.beacon" | "org.matrix.msc3672.beacon" => OtherEvent::Location,
        "m.room.message" => OtherEvent::Message,
        _ => OtherEvent::Event,
    }
}

/// Whether call member content has a membership: a non-empty
/// `memberships` list, or, in the older form, any content.
fn in_call(content: Option<&serde_json::Value>) -> bool {
    content
        .and_then(serde_json::Value::as_object)
        .is_some_and(|content| match content.get("memberships") {
            Some(memberships) => memberships.as_array().is_some_and(|list| !list.is_empty()),
            None => !content.is_empty(),
        })
}

/// The kind of a state event that is not a shown room change, by its type.
/// A call member event is a join when it adds a membership and a leave when
/// it removes the last one. One that keeps the sender in the call, or out
/// of it, as the previous content (`prev`) shows, is an update. Without the
/// previous content, the current content decides.
fn state_kind(
    kind: &str,
    content: Option<&serde_json::Value>,
    prev: Option<&serde_json::Value>,
) -> OtherEvent {
    match kind {
        "m.call.member" | "org.matrix.msc3401.call.member" => {
            let now = in_call(content);
            match (prev.map(|prev| in_call(Some(prev))), now) {
                (Some(before), now) if before == now => OtherEvent::CallUpdated,
                (_, true) => OtherEvent::CallJoined,
                (_, false) => OtherEvent::CallLeft,
            }
        }
        "m.beacon_info" | "org.matrix.msc3672.beacon_info" => {
            // A live share ends with the same event, marked not live.
            let live = content
                .and_then(|content| content.get("live"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            if live {
                OtherEvent::Location
            } else {
                OtherEvent::LocationStopped
            }
        }
        "m.space.child" => OtherEvent::SpaceChild,
        "m.space.parent" => OtherEvent::SpaceParent,
        "im.vector.modular.widgets" | "m.widget" | "io.element.widgets.layout" => {
            OtherEvent::Widget
        }
        // A pending invite is revoked, or spent, by sending the event again
        // with empty content: only one that still names an invite is one.
        "m.room.third_party_invite" => {
            if content
                .and_then(serde_json::Value::as_object)
                .is_some_and(|content| !content.is_empty())
            {
                OtherEvent::ThirdPartyInvite
            } else {
                OtherEvent::State
            }
        }
        _ => OtherEvent::State,
    }
}

/// An event with no other form, as an `other` item: its kind and its type,
/// read from the raw event so that a type this build cannot parse still
/// shows. A reaction or deletion that could not be read stays out: neither
/// is a line of its own. A deleted message-like event keeps its kind, from
/// its type, and is marked `deleted`, so the page applies both its kind's
/// switch and the deleted messages switch. A deleted state event's remaining
/// content says nothing, so it keeps its kind only where the line claims no
/// more than a change (spaces and widgets); otherwise it is generic `state`
/// (an emptied call member event is no proof that its sender left the call).
fn other_item(
    event: &TimelineEvent,
    event_id: String,
    sender: String,
    origin_server_ts: Option<UnixMillis>,
) -> Option<TimelineItem> {
    let raw = event.raw();
    let kind = raw.get_field::<String>("type").ok().flatten()?;
    let state = is_state(event);
    if !state && matches!(kind.as_str(), "m.reaction" | "m.room.redaction") {
        return None;
    }
    let deleted = !state && redacted(event);
    let content = raw.get_field::<serde_json::Value>("content").ok().flatten();
    let (event_kind, event_type) = if state {
        let prev = raw
            .get_field::<serde_json::Value>("unsigned")
            .ok()
            .flatten()
            .and_then(|unsigned| unsigned.get("prev_content").cloned());
        let found = state_kind(&kind, content.as_ref(), prev.as_ref());
        let neutral = matches!(
            found,
            OtherEvent::SpaceChild | OtherEvent::SpaceParent | OtherEvent::Widget
        );
        if redacted(event) && !neutral {
            (OtherEvent::State, kind.as_str())
        } else {
            (found, kind.as_str())
        }
    } else {
        let msgtype = content
            .as_ref()
            .and_then(|content| content.get("msgtype"))
            .and_then(serde_json::Value::as_str);
        // A message names its `msgtype`. One with no readable `msgtype` is a
        // generic event, named by its type.
        match (event_kind(&kind), msgtype) {
            (OtherEvent::Message, Some(msgtype)) => (OtherEvent::Message, msgtype),
            (OtherEvent::Message, None) => (OtherEvent::Event, kind.as_str()),
            (other, _) => (other, kind.as_str()),
        }
    };
    Some(TimelineItem::Other {
        event_id,
        sender,
        origin_server_ts,
        event: event_kind,
        event_type: project_name(event_type),
        state,
        deleted,
    })
}

/// The visible form of one event, or `None` for events not shown. An edit
/// is never shown as a message; `MatrixViews::timeline_page` checks it.
fn project(event: &TimelineEvent) -> Option<TimelineItem> {
    let event_id = event.event_id()?.to_string();
    let sender = project_name(event.sender()?.as_str());
    let origin_server_ts = server_time(event);
    if deleted_message(event) {
        return Some(TimelineItem::Notice {
            event_id,
            sender,
            notice: NoticeKind::Deleted,
            origin_server_ts,
        });
    }
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
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(
            SyncMessageLikeEvent::Original(reaction),
        ))) => {
            let key = project_name(&reaction.content.relates_to.key);
            if key.trim().is_empty() {
                return None;
            }
            return Some(TimelineItem::Reaction {
                event_id,
                sender,
                origin_server_ts,
                reacts_to: reaction.content.relates_to.event_id.to_string(),
                key,
            });
        }
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
            SyncRoomRedactionEvent::Original(redaction),
        ))) => {
            // Room version 11 moved `redacts` into the content.
            let redacts = redaction
                .content
                .redacts
                .as_ref()
                .or(redaction.redacts.as_ref())?;
            return Some(TimelineItem::Redaction {
                event_id,
                sender,
                origin_server_ts,
                redacts: redacts.to_string(),
            });
        }
        Ok(AnySyncTimelineEvent::State(state)) => {
            return match state_change(&state) {
                Some(change) => Some(TimelineItem::State {
                    event_id,
                    sender,
                    origin_server_ts,
                    change,
                }),
                None if member_unchanged(&state) => None,
                None => other_item(event, event_id, sender, origin_server_ts),
            };
        }
        _ => return other_item(event, event_id, sender, origin_server_ts),
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
        MessageType::ServerNotice(notice) => TimelineItem::Message {
            event_id,
            sender,
            body: text(&notice.body),
            origin_server_ts,
            reply_to,
        },
        MessageType::Image(image) => {
            crate::media::timeline_item(event_id, sender, origin_server_ts, image)
        }
        MessageType::File(_) | MessageType::Audio(_) | MessageType::Video(_) => {
            match crate::files::file_parts(&message.content.msgtype) {
                Some(file) => {
                    crate::files::timeline_item(event_id, sender, origin_server_ts, &file)
                }
                None => other_message(event_id, sender, origin_server_ts, &message.content.msgtype),
            }
        }
        msgtype => other_message(event_id, sender, origin_server_ts, msgtype),
    })
}

/// A message this build does not show as text, an image or a file, as an
/// `other` item named by its `msgtype`. Its body is never sent.
fn other_message(
    event_id: String,
    sender: String,
    origin_server_ts: Option<UnixMillis>,
    msgtype: &MessageType,
) -> TimelineItem {
    let event = match msgtype {
        MessageType::VerificationRequest(_) => OtherEvent::VerificationRequest,
        MessageType::Location(_) => OtherEvent::Location,
        _ => OtherEvent::Message,
    };
    TimelineItem::Other {
        event_id,
        sender,
        origin_server_ts,
        event,
        event_type: project_name(msgtype.msgtype()),
        state: false,
        deleted: false,
    }
}

/// A name or topic as shown: projected, and `None` when empty.
fn shown_text(raw: &str) -> Option<String> {
    let shown = project_name(raw);
    (!shown.trim().is_empty()).then_some(shown)
}

/// Most member events of one answer's `state` read for names. A server
/// sends one per person a page names, at most a sender and a member-change
/// target per item.
const MAX_NAME_EVENTS: usize = MAX_PAGE_ITEMS * 2;

/// The user IDs `items` show: each sender, and the member a change names.
fn named_users(items: &[TimelineItem]) -> BTreeSet<&str> {
    let mut users = BTreeSet::new();
    for item in items {
        let sender = match item {
            TimelineItem::Message { sender, .. }
            | TimelineItem::Notice { sender, .. }
            | TimelineItem::Image { sender, .. }
            | TimelineItem::File { sender, .. }
            | TimelineItem::Edit { sender, .. }
            | TimelineItem::Reaction { sender, .. }
            | TimelineItem::Redaction { sender, .. }
            | TimelineItem::Other { sender, .. } => sender,
            TimelineItem::State { sender, change, .. } => {
                if let StateChange::Member { target, .. }
                | StateChange::DisplayName { target, .. }
                | StateChange::ProfilePicture { target } = change
                {
                    users.insert(target.as_str());
                }
                sender
            }
            TimelineItem::Gap { .. } => continue,
        };
        users.insert(sender.as_str());
    }
    users
}

/// The display names of the people `items` show, for
/// [`TimelinePage::names`]: from `state`, the member events that a
/// lazy-loading `/messages` or `/context` answer carries, or else from the
/// store; nothing is fetched. A person whose member event was found has an
/// entry: their name, or `None` when that event shows none. A person whose
/// member event was not found, or could not be read, has no entry: their
/// name is unknown, which is not the same as having none.
pub(crate) async fn page_names(
    room: &matrix_sdk::Room,
    items: &[TimelineItem],
    state: &[Raw<AnyStateEvent>],
) -> Vec<SenderName> {
    let wanted = named_users(items);
    let mut names: BTreeMap<&str, Option<String>> = BTreeMap::new();
    for raw in state.iter().take(MAX_NAME_EVENTS) {
        if raw.get_field::<String>("type").ok().flatten().as_deref() != Some("m.room.member") {
            continue;
        }
        let Some(user) = raw.get_field::<String>("state_key").ok().flatten() else {
            continue;
        };
        let Some(&user) = wanted.get(user.as_str()) else {
            continue;
        };
        // An event whose content cannot be read says nothing about the name.
        let Some(content) = raw.get_field::<serde_json::Value>("content").ok().flatten() else {
            continue;
        };
        let name = content
            .get("displayname")
            .and_then(serde_json::Value::as_str)
            .and_then(shown_text);
        names.entry(user).or_insert(name);
    }
    for &user in &wanted {
        if names.contains_key(user) {
            continue;
        }
        let Ok(id) = UserId::parse(user) else {
            continue;
        };
        // A member the store does not have, or a failed read, leaves the
        // name unknown.
        if let Ok(Some(member)) = room.get_member_no_sync(&id).await {
            names.insert(user, member.display_name().and_then(shown_text));
        }
    }
    names
        .into_iter()
        .map(|(user_id, display_name)| SenderName {
            user_id: user_id.to_owned(),
            display_name,
        })
        .collect()
}

/// Whether a member event changes nothing: a membership sent again with the
/// same content. It is not a line of its own, because nothing happened.
fn member_unchanged(event: &AnySyncStateEvent) -> bool {
    matches!(
        event,
        AnySyncStateEvent::RoomMember(SyncStateEvent::Original(member))
            if matches!(member.membership_change(), Change::None)
    )
}

/// What a state event changed, or `None` for a change not shown.
fn state_change(event: &AnySyncStateEvent) -> Option<StateChange> {
    use crate::room_settings::{history_visibility, join_rule};
    Some(match event {
        AnySyncStateEvent::RoomMember(SyncStateEvent::Original(member)) => {
            let target = project_name(member.state_key.as_str());
            let membership = match member.membership_change() {
                // Without the earlier membership (`unsigned.prev_content`,
                // which a server leaves out where the history is hidden)
                // ruma takes it as "left", so a leave reads as no change.
                // The member left, or someone else removed them.
                Change::None
                    if member.content.membership == MembershipState::Leave
                        && member.unsigned.prev_content.is_none() =>
                {
                    if member.sender == member.state_key {
                        MembershipChange::Left
                    } else {
                        MembershipChange::Kicked
                    }
                }
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
                Change::ProfileChanged {
                    avatar_url_change: Some(_),
                    ..
                } => return Some(StateChange::ProfilePicture { target }),
                // No change, or one ruma cannot name.
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
        AnySyncStateEvent::RoomPinnedEvents(SyncStateEvent::Original(_)) => {
            StateChange::PinnedMessages {}
        }
        AnySyncStateEvent::RoomGuestAccess(SyncStateEvent::Original(guests)) => {
            StateChange::GuestAccess {
                allowed: guests.content.guest_access == GuestAccess::CanJoin,
            }
        }
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
    pub(crate) fn remember(&self, room: &str, token: String) -> String {
        let mut cursors = self.cursors();
        let number = cursors.next;
        cursors.next += 1;
        cursors.entries.push_back((number, room.to_owned(), token));
        while cursors.entries.len() > MAX_CURSORS {
            cursors.entries.pop_front();
        }
        format!("{CURSOR_PREFIX}{number}")
    }

    /// Resolve only canonical, retained cursors for the requested room; never accept server tokens.
    pub(crate) fn token(&self, room: &str, cursor: &str) -> Result<String, ErrorCode> {
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

    /// The other person in a joined direct-message room, or `None`.
    pub fn contact(client: &Client, room: &str) -> Option<matrix_sdk::ruma::OwnedUserId> {
        contacts::contact_of(client, &joined_room(client, room).ok()?)
    }

    /// `call_rooms` says which rooms have another device in their call
    /// (ADR-0052); `presence` holds the contacts' presence (ADR-0057).
    pub fn room_list(
        client: &Client,
        call_rooms: &crate::calls::rings::CallRooms,
        presence: &crate::presence::Book,
    ) -> RoomList {
        let now = crate::calls::controller::now_ms();
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
                        // Names the picture only; the page cannot fetch it.
                        picture_id: room.avatar_url().as_deref().map(crate::media::picture_id),
                        has_call: call_rooms.active(room.room_id(), now),
                        contact_presence: contact.as_deref().and_then(|user| presence.get(user)),
                    }
                })
                .collect(),
            truncated,
        }
    }

    /// Project a fetched chunk with the normal ignore and edit-identity rules.
    pub(crate) async fn project_chunk(
        &self,
        client: &Client,
        joined: &matrix_sdk::Room,
        chunk: &[TimelineEvent],
    ) -> Result<Vec<TimelineItem>, ErrorCode> {
        self.project_chunk_inner(client, joined, chunk, true).await
    }

    /// Attachments keep deletions but never resolve irrelevant text edits.
    pub(crate) async fn project_attachment_chunk(
        &self,
        client: &Client,
        joined: &matrix_sdk::Room,
        chunk: &[TimelineEvent],
    ) -> Result<Vec<TimelineItem>, ErrorCode> {
        self.project_chunk_inner(client, joined, chunk, false).await
    }

    /// Apply validated same-sender edits before a pinned original can leave core.
    pub(crate) async fn project_pin(
        &self,
        client: &Client,
        joined: &matrix_sdk::Room,
        original: TimelineEvent,
        edits: Vec<TimelineEvent>,
    ) -> Result<Vec<TimelineItem>, ErrorCode> {
        let id = original.event_id().ok_or(ErrorCode::Rejected)?.to_owned();
        // An undecryptable or unsupported replacement cannot establish current
        // text. Do not fall back to a potentially sensitive original preview.
        if edits.iter().any(|edit| {
            edit_claim(edit).is_none_or(|claim| claim.replaces != id)
                || !edit
                    .raw()
                    .get_field::<String>("room_id")
                    .is_ok_and(|room| room.is_none_or(|room| room == joined.room_id().as_str()))
        }) {
            // Invalid relation scope suppresses only this preview. Validate
            // the original and local ignore state before returning, so a bad
            // relation cannot mask a separate authority or store failure.
            // No preview can survive this branch, so do not fetch edit parents.
            self.project_chunk_inner(client, joined, std::slice::from_ref(&original), false)
                .await?;
            return Ok(Vec::new());
        }
        let mut chunk = vec![original];
        chunk.extend(edits);
        let items = self.project_chunk(client, joined, &chunk).await?;
        let latest = items
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Edit {
                    replaces,
                    body,
                    origin_server_ts,
                    event_id,
                    ..
                } if replaces == id.as_str() => Some((origin_server_ts, event_id, body)),
                _ => None,
            })
            .max_by_key(|(time, event_id, _)| (time.map(|time| time.get()), event_id.as_str()));
        let mut original = items
            .iter()
            .find(|item| match item {
                TimelineItem::Message { event_id, .. }
                | TimelineItem::Image { event_id, .. }
                | TimelineItem::File { event_id, .. }
                | TimelineItem::Notice { event_id, .. }
                | TimelineItem::Other { event_id, .. } => event_id == id.as_str(),
                _ => false,
            })
            .cloned();
        if let (Some(TimelineItem::Message { body, .. }), Some((_, _, replacement))) =
            (&mut original, latest)
        {
            body.clone_from(replacement);
        }
        Ok(original.into_iter().collect())
    }

    /// Reuse room/ignore validation, optionally resolving edit identities.
    async fn project_chunk_inner(
        &self,
        client: &Client,
        joined: &matrix_sdk::Room,
        chunk: &[TimelineEvent],
        resolve_edits: bool,
    ) -> Result<Vec<TimelineItem>, ErrorCode> {
        let ignored = crate::abuse::ignored_set(client).await?;
        let room = joined.room_id().as_str();
        let mut items = Vec::new();
        let mut lookups = 0;
        for event in chunk {
            if event
                .event_id()
                .is_some_and(|id| id.as_str().len() > persephone_core_protocol::MAX_EVENT_ID_BYTES)
                || event
                    .raw()
                    .get_field::<String>("room_id")
                    .map_err(|_| ErrorCode::Rejected)?
                    .is_some_and(|id| id != room)
            {
                return Err(ErrorCode::Rejected);
            }
            if ignored_content(event, &ignored) {
                continue;
            }
            self.remember_sender(room, event);
            let item = match edit_claim(event) {
                Some(claim) => {
                    if resolve_edits {
                        self.checked_edit(joined, room, chunk, event, claim, &mut lookups)
                            .await
                    } else {
                        None
                    }
                }
                None => project(event),
            };
            items.extend(item);
        }
        Ok(items)
    }

    /// One bounded page of history older than `cursor`, or the newest page.
    /// Refuse unreadable ignore state before fetching and filter ignored
    /// message content while preserving state events and redactions. Items
    /// the account's switches (`events`) hide are left out before the page
    /// fills, so the core fetches older chunks, at most `MAX_PAGE_FETCHES`,
    /// while the page has room left (`timeline_filter`).
    pub async fn timeline_page(
        &self,
        client: &Client,
        room: &str,
        cursor: Option<&str>,
        events: &TimelineEvents,
    ) -> Result<TimelinePage, ErrorCode> {
        let joined = joined_room(client, room)?;
        let ignored = crate::abuse::ignored_set(client).await?;
        let mut token = cursor.map(|cursor| self.token(room, cursor)).transpose()?;
        // Oldest first, as pages read; each chunk is older than the last.
        let mut items: Vec<TimelineItem> = Vec::new();
        let mut state = Vec::new();
        let mut names = Vec::new();
        let mut lookups = 0;
        'fetches: for fetch in 0..MAX_PAGE_FETCHES {
            let room_left = MAX_PAGE_ITEMS - items.len();
            // After the first chunk, no token is the start of the room.
            if room_left == 0 || (fetch > 0 && token.is_none()) {
                break;
            }
            let mut limit = room_left;
            loop {
                let chunk = self
                    .fetch_chunk(
                        &joined,
                        room,
                        token.as_deref(),
                        limit,
                        &ignored,
                        events,
                        &mut lookups,
                    )
                    .await?;
                // A homeserver that answers a token with the same token did
                // not move: its chunk would repeat this page's events.
                if fetch > 0 && chunk.end.is_some() && chunk.end == token {
                    break 'fetches;
                }
                let with: Vec<TimelineItem> = chunk.items.iter().chain(&items).cloned().collect();
                let with_state: Vec<_> = state.iter().chain(&chunk.state).cloned().collect();
                let with_names = page_names(&joined, &with, &with_state).await;
                if page_bytes(&with, &with_names) <= MAX_PAGE_BYTES {
                    items = with;
                    state = with_state;
                    names = with_names;
                    token = chunk.end;
                    break;
                }
                // The SDK cannot resume inside a chunk: fetch fewer events.
                if limit > 1 {
                    limit /= 2;
                    continue;
                }
                if fetch == 0 {
                    // One event over the budget cannot happen with projected
                    // bodies; drop it rather than exceed the frame.
                    items.clear();
                    names.clear();
                    token = chunk.end;
                }
                // A later chunk that does not fit starts the next page.
                break 'fetches;
            }
        }
        Ok(TimelinePage {
            room_id: room.to_owned(),
            items,
            next_cursor: token.map(|token| self.remember(room, token)),
            names,
        })
    }

    /// Up to `limit` events older than `from` (or the newest), projected and
    /// filtered by `events`, oldest first.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_chunk(
        &self,
        joined: &matrix_sdk::Room,
        room: &str,
        from: Option<&str>,
        limit: usize,
        ignored: &std::collections::BTreeSet<OwnedUserId>,
        events: &TimelineEvents,
        lookups: &mut usize,
    ) -> Result<Chunk, ErrorCode> {
        let mut options = MessagesOptions::backward().from(from);
        options.limit = UInt::try_from(limit as u64).unwrap_or(UInt::MIN);
        // The answer then carries the member events of the people the
        // page shows, for their display names.
        options.filter.lazy_load_options = LazyLoadOptions::Enabled {
            include_redundant_members: true,
        };
        let messages = joined
            .messages(options)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        // Backward pagination returns newest first; pages read oldest first.
        let mut items = Vec::new();
        for event in messages.chunk.iter().rev() {
            if ignored_content(event, ignored) {
                continue;
            }
            self.remember_sender(room, event);
            let item = match edit_claim(event) {
                Some(claim) => {
                    self.checked_edit(joined, room, &messages.chunk, event, claim, lookups)
                        .await
                }
                None => project(event),
            };
            items.extend(item.filter(|item| crate::timeline_filter::shown(item, events)));
        }
        // Only the members the kept items name: a page of many chunks keeps
        // its member events bounded by what it shows.
        let named = named_users(&items);
        let state = messages
            .state
            .into_iter()
            .filter(|raw| {
                raw.get_field::<String>("state_key")
                    .ok()
                    .flatten()
                    .is_some_and(|user| named.contains(user.as_str()))
            })
            .collect();
        let end = messages.end.filter(|_| !messages.chunk.is_empty());
        Ok(Chunk { items, state, end })
    }
}

/// One fetched chunk of history, projected and filtered, oldest first.
struct Chunk {
    items: Vec<TimelineItem>,
    /// The member events of the people the items name.
    state: Vec<Raw<AnyStateEvent>>,
    /// The token for older history; `None` at the start of the room.
    end: Option<String>,
}

/// The serialized size of a page's items and names.
fn page_bytes(items: &[TimelineItem], names: &[SenderName]) -> usize {
    items
        .iter()
        .map(|item| serde_json::to_vec(item).map_or(usize::MAX, |v| v.len()))
        .chain(
            names
                .iter()
                .map(|name| serde_json::to_vec(name).map_or(usize::MAX, |v| v.len())),
        )
        .fold(0usize, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::deserialized_responses::{VerificationLevel, WithheldCode};

    #[test]
    fn given_ignored_sender_then_message_content_is_hidden_but_state_and_deletions_remain() {
        let ignored = [matrix_sdk::ruma::user_id!("@alice:example.org").to_owned()].into();
        assert!(ignored_content(
            &plain_event(serde_json::json!({"msgtype":"m.text","body":"hidden"})),
            &ignored
        ));
        let state = state_event(
            "m.room.encryption",
            "@alice:example.org",
            "",
            serde_json::json!({"algorithm":"m.megolm.v1.aes-sha2"}),
            None,
        );
        assert!(!ignored_content(&state, &ignored));
        let redaction = raw_event(
            serde_json::json!({"type":"m.room.redaction","event_id":"$delete","sender":"@alice:example.org","origin_server_ts":1,"content":{"redacts":"$target"}}),
        );
        assert!(!ignored_content(&redaction, &ignored));
    }

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

    fn raw_event(event: serde_json::Value) -> TimelineEvent {
        TimelineEvent::from_plaintext(
            matrix_sdk::ruma::serde::Raw::from_json_string(event.to_string()).unwrap(),
        )
    }

    /// GIVEN a deleted message, plain or encrypted, and a deleted reaction
    /// WHEN projected THEN each message is a `deleted` notice from its
    /// sender, never an undecryptable one, and the reaction stays hidden.
    #[test]
    fn given_deleted_events_then_messages_show_as_deleted() {
        let unsigned = serde_json::json!({ "redacted_because": {
            "type": "m.room.redaction", "sender": "@mod:example.org",
            "event_id": "$why", "origin_server_ts": 2, "content": {}
        }});
        for kind in ["m.room.message", "m.room.encrypted"] {
            let item = project(&raw_event(serde_json::json!({
                "type": kind, "event_id": "$gone", "sender": "@alice:example.org",
                "origin_server_ts": 1, "content": {}, "unsigned": unsigned,
            })));
            let Some(TimelineItem::Notice { notice, sender, .. }) = item else {
                panic!("a notice: {item:?}");
            };
            assert_eq!(
                (notice, sender.as_str()),
                (NoticeKind::Deleted, "@alice:example.org")
            );
        }
        let reaction = project(&raw_event(serde_json::json!({
            "type": "m.reaction", "event_id": "$r", "sender": "@alice:example.org",
            "origin_server_ts": 1, "content": {}, "unsigned": unsigned,
        })));
        assert!(reaction.is_none(), "{reaction:?}");
    }

    /// GIVEN reactions with a normal, a hostile and an empty key WHEN
    /// projected THEN each names the event it reacts to with its key as
    /// bounded, inert text, and one with nothing to show is left out.
    #[test]
    fn given_reactions_then_keys_are_projected_and_empty_ones_dropped() {
        let reaction = |key: &str| {
            project(&raw_event(serde_json::json!({
                "type": "m.reaction", "event_id": "$r", "sender": "@bob:example.org",
                "origin_server_ts": 1,
                "content": { "m.relates_to": {
                    "rel_type": "m.annotation", "event_id": "$liked", "key": key
                }},
            })))
        };
        let Some(TimelineItem::Reaction { reacts_to, key, .. }) = reaction("👍") else {
            panic!("a reaction");
        };
        assert_eq!((reacts_to.as_str(), key.as_str()), ("$liked", "👍"));
        let hostile = format!("\u{202E}<b>{}", "x".repeat(300));
        let Some(TimelineItem::Reaction { key, .. }) = reaction(&hostile) else {
            panic!("a reaction");
        };
        assert!(!key.contains('\u{202E}'));
        assert!(key.chars().count() <= crate::views::MAX_DISPLAY_NAME_CHARS + 1);
        assert!(reaction("   ").is_none());
    }

    /// GIVEN a deletion, in the old form (`redacts` beside the content) and
    /// the room version 11 form (in the content) WHEN projected THEN it is a
    /// `redaction` item naming the deleted event.
    #[test]
    fn given_a_deletion_then_it_names_the_deleted_event() {
        for (top, content) in [
            (Some("$target"), serde_json::json!({})),
            (None, serde_json::json!({ "redacts": "$target" })),
        ] {
            let mut event = serde_json::json!({
                "type": "m.room.redaction", "event_id": "$del", "sender": "@mod:example.org",
                "origin_server_ts": 1, "content": content,
            });
            if let Some(top) = top {
                event["redacts"] = top.into();
            }
            let item = project(&raw_event(event));
            let Some(TimelineItem::Redaction {
                redacts, sender, ..
            }) = item
            else {
                panic!("a redaction: {item:?}");
            };
            assert_eq!(
                (redacts.as_str(), sender.as_str()),
                ("$target", "@mod:example.org")
            );
        }
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

    /// The room change an event shows, or `None` when it is no room change:
    /// nothing, or a generic `other` state item.
    fn change_of(event: &TimelineEvent) -> Option<StateChange> {
        match project(event) {
            Some(TimelineItem::State { change, .. }) => Some(change),
            None => None,
            Some(other) => panic!("a state item or nothing: {other:?}"),
        }
    }

    /// The kind and shown type of an `other` item, or `None` for anything else.
    fn other_of(event: &TimelineEvent) -> Option<(OtherEvent, String)> {
        match project(event) {
            Some(TimelineItem::Other {
                event, event_type, ..
            }) => Some((event, event_type)),
            _ => None,
        }
    }

    fn message_like(kind: &str, content: serde_json::Value) -> TimelineEvent {
        raw_event(serde_json::json!({
            "type": kind, "event_id": "$e", "sender": "@alice:example.org",
            "origin_server_ts": 1, "content": content,
        }))
    }

    /// GIVEN the message types this build shows no text, image or file for
    /// WHEN projected THEN each is an `other` item named by its `msgtype`, an
    /// in-room verification request and a location have their own kind, and
    /// no body is sent; a server notice is a message.
    #[test]
    fn given_other_message_types_then_each_is_named_and_its_body_is_not_sent() {
        let message = |content| other_of(&plain_event(content));
        assert_eq!(
            message(serde_json::json!({
                "msgtype": "m.key.verification.request", "body": "secret body",
                "to": "@bob:example.org", "from_device": "D",
                "methods": ["m.sas.v1"],
            })),
            Some((
                OtherEvent::VerificationRequest,
                "m.key.verification.request".into()
            ))
        );
        assert_eq!(
            message(serde_json::json!({
                "msgtype": "m.location", "body": "here", "geo_uri": "geo:51.5,0.1",
            })),
            Some((OtherEvent::Location, "m.location".into()))
        );
        let hostile = format!("org.example\u{202E}<b>{}", "x".repeat(300));
        let Some((OtherEvent::Message, shown)) =
            message(serde_json::json!({ "msgtype": hostile, "body": "secret body" }))
        else {
            panic!("an other message");
        };
        assert!(shown.starts_with("org.example") && !shown.contains('\u{202E}'));
        assert!(shown.chars().count() <= crate::views::MAX_DISPLAY_NAME_CHARS + 1);
        let item = project(&plain_event(serde_json::json!({
            "msgtype": "org.example.custom", "body": "secret body",
        })));
        assert!(
            !serde_json::to_string(&item)
                .unwrap()
                .contains("secret body"),
            "{item:?}"
        );
        let Some(TimelineItem::Message { body, .. }) = project(&plain_event(serde_json::json!({
            "msgtype": "m.server_notice", "body": "Server maintenance",
            "server_notice_type": "m.server_notice.usage_limit_reached",
        }))) else {
            panic!("a server notice is a message");
        };
        assert_eq!(body, "Server maintenance");
    }

    /// GIVEN message-like events of each known type and an unknown one WHEN
    /// projected THEN each is an `other` item of its kind with its type.
    #[test]
    fn given_other_message_like_events_then_each_has_its_kind() {
        for (kind, expected) in [
            ("m.key.verification.ready", OtherEvent::VerificationReady),
            ("m.key.verification.start", OtherEvent::VerificationStart),
            ("m.key.verification.key", OtherEvent::VerificationStep),
            ("m.key.verification.mac", OtherEvent::VerificationStep),
            (
                "m.key.verification.cancel",
                OtherEvent::VerificationCancelled,
            ),
            ("m.key.verification.done", OtherEvent::VerificationDone),
            ("m.call.invite", OtherEvent::CallStarted),
            (
                "org.matrix.msc4075.rtc.notification",
                OtherEvent::CallStarted,
            ),
            ("m.call.answer", OtherEvent::CallAnswered),
            ("m.call.reject", OtherEvent::CallDeclined),
            ("m.call.hangup", OtherEvent::CallEnded),
            ("m.call.candidates", OtherEvent::CallSignalling),
            ("m.call.select_answer", OtherEvent::CallSignalling),
            (
                "io.element.call.encryption_keys",
                OtherEvent::CallSignalling,
            ),
            ("org.matrix.msc3381.poll.start", OtherEvent::PollStarted),
            ("m.poll.response", OtherEvent::PollVote),
            ("m.poll.end", OtherEvent::PollEnded),
            ("m.sticker", OtherEvent::Sticker),
            ("m.location", OtherEvent::Location),
            ("m.beacon", OtherEvent::Location),
            ("org.example.custom", OtherEvent::Event),
        ] {
            assert_eq!(
                other_of(&message_like(kind, serde_json::json!({ "body": "x" }))),
                Some((expected, kind.into())),
                "{kind}"
            );
        }
        // GIVEN a message with no readable `msgtype` THEN it is a generic
        // event named by its type, not a message of type m.room.message.
        assert_eq!(
            other_of(&raw_event(serde_json::json!({
                "type": "m.room.message", "event_id": "$m", "sender": "@alice:example.org",
                "origin_server_ts": 1, "content": { "body": "hi" },
            }))),
            Some((OtherEvent::Event, "m.room.message".into()))
        );
    }

    /// A decrypted event, as the SDK gives one from an encrypted room.
    fn decrypted_event(event: serde_json::Value) -> TimelineEvent {
        use matrix_sdk::deserialized_responses::{
            AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, VerificationState,
        };
        TimelineEvent::from_decrypted(
            DecryptedRoomEvent {
                event: Raw::from_json_string(event.to_string()).unwrap(),
                encryption_info: std::sync::Arc::new(EncryptionInfo {
                    sender: matrix_sdk::ruma::user_id!("@alice:example.org").to_owned(),
                    sender_device: None,
                    forwarder: None,
                    algorithm_info: AlgorithmInfo::OlmV1Curve25519AesSha2 {
                        curve25519_public_key_base64: "key".into(),
                    },
                    verification_state: VerificationState::Verified,
                }),
                unsigned_encryption_info: None,
            },
            None,
        )
    }

    /// GIVEN well-formed in-room verification steps and a poll, decrypted
    /// from an encrypted room WHEN projected THEN each is read from its
    /// decrypted type, not `m.room.encrypted`, and has its own kind: a step
    /// after the request is an event type of its own, not a `msgtype`.
    #[test]
    fn given_decrypted_other_events_then_their_decrypted_type_is_used() {
        let relates = serde_json::json!({ "rel_type": "m.reference", "event_id": "$request" });
        for (kind, content, expected) in [
            (
                "m.key.verification.ready",
                serde_json::json!({
                    "from_device": "DEVICE", "methods": ["m.sas.v1"], "m.relates_to": relates,
                }),
                OtherEvent::VerificationReady,
            ),
            (
                "m.key.verification.done",
                serde_json::json!({ "m.relates_to": relates }),
                OtherEvent::VerificationDone,
            ),
            (
                "m.poll.start",
                serde_json::json!({
                    "m.text": [{ "body": "Lunch?" }],
                    "m.poll": { "question": { "m.text": [{ "body": "Lunch?" }] },
                        "answers": [{ "m.id": "a", "m.text": [{ "body": "Yes" }] },
                                    { "m.id": "b", "m.text": [{ "body": "No" }] }] },
                }),
                OtherEvent::PollStarted,
            ),
        ] {
            let event = decrypted_event(serde_json::json!({
                "type": kind, "event_id": "$e", "sender": "@alice:example.org",
                "room_id": "!room:example.org", "origin_server_ts": 1, "content": content,
            }));
            assert!(matches!(event.kind, TimelineEventKind::Decrypted(_)));
            assert_eq!(other_of(&event), Some((expected, kind.into())), "{kind}");
        }
    }

    /// GIVEN state events that are no shown room change WHEN projected THEN
    /// each is an `other` item of its kind; a call member event is a join
    /// with a membership and a leave without one.
    #[test]
    fn given_other_state_events_then_each_has_its_kind() {
        const A: &str = "@alice:example.org";
        let state = |kind: &str, content: serde_json::Value| {
            other_of(&state_event(kind, A, "key", content, None))
        };
        for (kind, content, expected) in [
            (
                "m.space.child",
                serde_json::json!({ "via": ["s"] }),
                OtherEvent::SpaceChild,
            ),
            (
                "m.space.parent",
                serde_json::json!({}),
                OtherEvent::SpaceParent,
            ),
            (
                "im.vector.modular.widgets",
                serde_json::json!({ "type": "x" }),
                OtherEvent::Widget,
            ),
            (
                "m.room.third_party_invite",
                serde_json::json!({ "display_name": "a…" }),
                OtherEvent::ThirdPartyInvite,
            ),
            // A revoked invite: the same event, emptied.
            (
                "m.room.third_party_invite",
                serde_json::json!({}),
                OtherEvent::State,
            ),
            ("m.beacon_info", serde_json::json!({}), OtherEvent::Location),
            (
                "m.beacon_info",
                serde_json::json!({ "live": true }),
                OtherEvent::Location,
            ),
            (
                "org.matrix.msc3672.beacon_info",
                serde_json::json!({ "live": false }),
                OtherEvent::LocationStopped,
            ),
            (
                "m.beacon_info",
                serde_json::json!({ "live": false }),
                OtherEvent::LocationStopped,
            ),
            (
                "m.call.member",
                serde_json::json!({ "application": "m.call", "call_id": "" }),
                OtherEvent::CallJoined,
            ),
            (
                "org.matrix.msc3401.call.member",
                serde_json::json!({ "memberships": [{ "call_id": "" }] }),
                OtherEvent::CallJoined,
            ),
            ("m.call.member", serde_json::json!({}), OtherEvent::CallLeft),
            (
                "org.matrix.msc3401.call.member",
                serde_json::json!({ "memberships": [] }),
                OtherEvent::CallLeft,
            ),
            (
                "org.example.custom",
                serde_json::json!({ "x": 1 }),
                OtherEvent::State,
            ),
        ] {
            assert_eq!(
                state(kind, content),
                Some((expected, kind.into())),
                "{kind}"
            );
        }
        // GIVEN the previous call member content THEN a join adds the first
        // membership, a leave removes the last, and anything else is an
        // update: a second device, or one of two leaving, is no join.
        let one = serde_json::json!({ "memberships": [{ "call_id": "", "device_id": "A" }] });
        let two = serde_json::json!({ "memberships": [
            { "call_id": "", "device_id": "A" }, { "call_id": "", "device_id": "B" }
        ] });
        let none = serde_json::json!({ "memberships": [] });
        for (content, prev, expected) in [
            (&one, &none, OtherEvent::CallJoined),
            (&none, &one, OtherEvent::CallLeft),
            (&two, &one, OtherEvent::CallUpdated),
            (&one, &two, OtherEvent::CallUpdated),
            (&none, &none, OtherEvent::CallUpdated),
        ] {
            assert_eq!(
                other_of(&state_event(
                    "org.matrix.msc3401.call.member",
                    A,
                    "_@alice:example.org_A",
                    content.clone(),
                    Some(prev.clone()),
                )),
                Some((expected, "org.matrix.msc3401.call.member".into())),
                "{content} after {prev}"
            );
        }
        // A member event that changes nothing is not a line: nothing
        // happened. A deleted one still shows as room state.
        assert!(
            project(&state_event(
                "m.room.member",
                A,
                A,
                serde_json::json!({ "membership": "join" }),
                Some(serde_json::json!({ "membership": "join" })),
            ))
            .is_none()
        );
    }

    /// GIVEN deleted events that are not messages WHEN projected THEN a
    /// message-like one keeps its kind and is marked deleted, a state one is
    /// an `other` state item, and an unreadable reaction or deletion stays
    /// out.
    #[test]
    fn given_deleted_or_unreadable_other_events_then_they_show_as_deleted_or_not_at_all() {
        let unsigned = serde_json::json!({ "redacted_because": {
            "type": "m.room.redaction", "sender": "@mod:example.org",
            "event_id": "$why", "origin_server_ts": 2, "content": {}
        }});
        for (kind, expected) in [
            ("m.poll.start", OtherEvent::PollStarted),
            ("m.sticker", OtherEvent::Sticker),
            ("m.call.invite", OtherEvent::CallStarted),
        ] {
            let item = project(&raw_event(serde_json::json!({
                "type": kind, "event_id": "$p", "sender": "@alice:example.org",
                "origin_server_ts": 1, "content": {}, "unsigned": unsigned,
            })));
            assert!(
                matches!(
                    &item,
                    Some(TimelineItem::Other { event, event_type, state: false, deleted: true, .. })
                        if *event == expected && event_type == kind
                ),
                "{kind}: {item:?}"
            );
        }
        // A deleted state event's emptied content says nothing: an emptied
        // call member event is no proof that its sender left the call. A
        // space or widget line only says it changed, so it keeps its kind and
        // its own switch.
        for (kind, expected) in [
            ("m.space.child", OtherEvent::SpaceChild),
            ("m.space.parent", OtherEvent::SpaceParent),
            ("m.widget", OtherEvent::Widget),
            ("m.call.member", OtherEvent::State),
            ("m.room.third_party_invite", OtherEvent::State),
            ("m.beacon_info", OtherEvent::State),
            ("m.room.name", OtherEvent::State),
            ("m.room.member", OtherEvent::State),
        ] {
            let item = project(&raw_event(serde_json::json!({
                "type": kind, "event_id": "$s", "sender": "@alice:example.org",
                "state_key": "k", "origin_server_ts": 1, "content": {}, "unsigned": unsigned,
            })));
            let Some(TimelineItem::Other {
                event,
                event_type,
                state,
                ..
            }) = item
            else {
                panic!("{kind}: an other item: {item:?}");
            };
            assert_eq!((event, event_type.as_str(), state), (expected, kind, true));
        }
        // GIVEN the same kind as a message and as a state event THEN only the
        // state one is marked as state.
        for (kind, state_key, expected) in [
            ("m.beacon", None, false),
            ("m.beacon_info", Some("k"), true),
        ] {
            let mut event = serde_json::json!({
                "type": kind, "event_id": "$l", "sender": "@alice:example.org",
                "origin_server_ts": 1, "content": {},
            });
            if let Some(key) = state_key {
                event["state_key"] = key.into();
            }
            let item = project(&raw_event(event));
            assert!(
                matches!(
                    item,
                    Some(TimelineItem::Other { event: OtherEvent::Location, state, .. }) if state == expected
                ),
                "{kind}: {item:?}"
            );
        }
        for kind in ["m.reaction", "m.room.redaction"] {
            let item = project(&message_like(kind, serde_json::json!({ "broken": true })));
            assert!(item.is_none(), "{kind}: {item:?}");
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
        // No change at all is no member change.
        assert_eq!(member(B, B, "join", Some("join")), None);
    }

    /// GIVEN a leave without the earlier membership, which a server leaves
    /// out where the history is hidden WHEN projected THEN it is shown as
    /// leaving when the member sent it, and as a removal when someone else
    /// did, like a join without it is shown as joining.
    #[test]
    fn given_a_leave_without_the_earlier_membership_then_it_is_still_shown() {
        const A: &str = "@alice:example.org";
        const B: &str = "@bob:example.org";
        let change = |membership| {
            Some(StateChange::Member {
                target: B.into(),
                membership,
            })
        };
        assert_eq!(member(B, B, "leave", None), change(MembershipChange::Left));
        assert_eq!(
            member(A, B, "leave", None),
            change(MembershipChange::Kicked)
        );
        assert_eq!(member(B, B, "join", None), change(MembershipChange::Joined));
        // A leave after a leave that the event does say is still no member
        // change.
        assert_eq!(member(B, B, "leave", Some("leave")), None);
    }

    /// GIVEN page items from several people WHEN their user IDs are
    /// collected for names THEN each sender and each member a change names
    /// is listed once, and a gap names no one.
    #[test]
    fn given_page_items_then_named_users_are_senders_and_change_targets() {
        let state = |sender: &str, change| TimelineItem::State {
            event_id: "$s".into(),
            sender: sender.into(),
            origin_server_ts: None,
            change,
        };
        let items = [
            TimelineItem::Message {
                event_id: "$m".into(),
                sender: "@alice:s".into(),
                body: "hi".into(),
                origin_server_ts: None,
                reply_to: None,
            },
            state(
                "@alice:s",
                StateChange::Member {
                    target: "@bob:s".into(),
                    membership: MembershipChange::Invited,
                },
            ),
            state(
                "@carol:s",
                StateChange::ProfilePicture {
                    target: "@carol:s".into(),
                },
            ),
            state("@dave:s", StateChange::Topic { topic: None }),
            TimelineItem::Gap { next_cursor: None },
        ];
        assert_eq!(
            named_users(&items).into_iter().collect::<Vec<_>>(),
            ["@alice:s", "@bob:s", "@carol:s", "@dave:s"]
        );
    }

    /// GIVEN a member changing their display name, to something hostile or
    /// to nothing, or only their picture WHEN projected THEN the name is
    /// inert bounded text, a removed name is `None`, and a picture change
    /// alone is a picture change, without the picture's address.
    #[test]
    fn given_profile_changes_then_names_are_projected_and_pictures_are_shown() {
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
            Some(StateChange::ProfilePicture { target: B.into() })
        );
        assert_eq!(
            profile(
                serde_json::json!({ "membership": "join" }),
                serde_json::json!({ "membership": "join", "avatar_url": "mxc://s/old" }),
            ),
            Some(StateChange::ProfilePicture { target: B.into() })
        );
    }

    /// GIVEN changes to the room's state WHEN projected THEN each security-
    /// relevant one is shown, names and topics are projected and empty ones
    /// are `None`, and an unknown state type is no room change.
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
            room(
                "m.room.pinned_events",
                serde_json::json!({ "pinned": ["$a"] })
            ),
            Some(StateChange::PinnedMessages {})
        );
        assert_eq!(
            room(
                "m.room.guest_access",
                serde_json::json!({ "guest_access": "can_join" })
            ),
            Some(StateChange::GuestAccess { allowed: true })
        );
        assert_eq!(
            room(
                "m.room.guest_access",
                serde_json::json!({ "guest_access": "forbidden" })
            ),
            Some(StateChange::GuestAccess { allowed: false })
        );
        // A type with no room change is a generic state line, not a change.
        assert_eq!(
            other_of(&state_event(
                "org.example.custom",
                A,
                "",
                serde_json::json!({ "x": 1 }),
                None
            )),
            Some((OtherEvent::State, "org.example.custom".into()))
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
