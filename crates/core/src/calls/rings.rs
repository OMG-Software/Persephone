//! Incoming call rings and the rooms that have a call (ADR-0052).
//!
//! A ring is an untrusted `org.matrix.msc4075.rtc.notification` room event.
//! [`screen`] and [`confirm`] decide whether the core accepts one; both are
//! pure, so every rule has a test. [`Rings`] keeps the accepted rings in
//! memory only, within fixed bounds. [`CallRooms`] keeps the one flag for
//! each room that has another device in its call. Nothing here starts a
//! join, opens a connection or turns on a device.
//!
//! The same module builds the events this core sends: the ring after a join
//! ([`plan_ring`], [`notification_content`]) and the decline
//! ([`decline_content`]).

use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// The ring event type (MSC4075, unstable name).
pub const NOTIFICATION_TYPE: &str = "org.matrix.msc4075.rtc.notification";

/// The decline event type (MSC4310, unstable name).
pub const DECLINE_TYPE: &str = "org.matrix.msc4310.rtc.decline";

/// Most rings kept in all. The oldest is dropped for a new one.
pub const MAX_RINGS: usize = 4;

/// A sender gets one ring in this time; a sooner one is dropped.
pub const SENDER_GAP_MS: u64 = 30_000;

/// Most senders remembered for [`SENDER_GAP_MS`]. A full list refuses new
/// senders until the oldest entry is old enough.
const MAX_SENDERS: usize = 64;

/// When `sender_ts` is more than this after `origin_server_ts`, the ring
/// starts at `origin_server_ts`.
const MAX_SKEW_MS: u64 = 20_000;

/// A ring ends at most this long after the core received it.
const MAX_LIFE_MS: u64 = 120_000;

/// The `lifetime` of a ring this core sends.
pub const LIFETIME_MS: u64 = 90_000;

/// Longest raw event examined, in bytes.
pub const MAX_EVENT_BYTES: usize = 8 * 1024;

/// Most rooms with a call flag. The page lists at most this many rooms.
const MAX_FLAGS: usize = 4096;

/// What a ring is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Ring the user.
    Ring,
    /// Show a visual notice only. This core does not ring for it.
    Notification,
}

/// A caller's soft hint for the camera. Absent or unknown is audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Audio,
    Video,
}

/// The parts of a ring event's content that the core uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: Kind,
    pub sender_ts: u64,
    pub lifetime_ms: u64,
    pub intent: Intent,
}

/// Reads the content of a ring. `None` when a required field is missing or
/// has a wrong type, as ruma requires `notification_type`, `sender_ts` and
/// `lifetime`. An unknown `notification_type` is also `None`.
pub fn parse_notice(content: &Value) -> Option<Notice> {
    let content = content.as_object()?;
    let kind = match content.get("notification_type")?.as_str()? {
        "ring" => Kind::Ring,
        "notification" => Kind::Notification,
        _ => return None,
    };
    let intent = match content.get("m.call.intent").and_then(Value::as_str) {
        Some("video") => Intent::Video,
        _ => Intent::Audio,
    };
    Some(Notice {
        kind,
        sender_ts: content.get("sender_ts")?.as_u64()?,
        lifetime_ms: content.get("lifetime")?.as_u64()?,
        intent,
    })
}

/// When a ring ends, in Unix milliseconds. The start is `sender_ts`, unless
/// that is more than 20 s after `origin_server_ts`; then it is
/// `origin_server_ts`. The end is the start plus the lifetime, with the
/// lifetime at most 120 s, and at most 120 s after `received_ms`. The
/// lifetime cap stops an old ring from ringing again after a restart.
pub fn ends_at(notice: &Notice, origin_server_ts: u64, received_ms: u64) -> u64 {
    let start = if notice.sender_ts > origin_server_ts.saturating_add(MAX_SKEW_MS) {
        origin_server_ts
    } else {
        notice.sender_ts
    };
    start
        .saturating_add(notice.lifetime_ms.min(MAX_LIFE_MS))
        .min(received_ms.saturating_add(MAX_LIFE_MS))
}

/// Why the core did not accept a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The event is too large or has no usable fields.
    Malformed,
    /// The room is not joined.
    NotJoined,
    /// The room is the open call's room.
    OpenCall,
    /// The sender is this user.
    OwnSender,
    /// The type is `notification`.
    NotARing,
    /// The ring ended before it arrived.
    Expired,
    /// The sender is not a joined member.
    NotMember,
    /// The user ignores the sender.
    Ignored,
    /// The sender has no current call membership in the room.
    NoMembership,
    /// The sender rang less than [`SENDER_GAP_MS`] ago.
    TooSoon,
    /// Too many senders are remembered.
    TooManySenders,
}

/// A raw ring event, after the handler read its envelope.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub room_id: OwnedRoomId,
    pub event_id: OwnedEventId,
    pub sender: OwnedUserId,
    pub origin_server_ts: u64,
    pub content: Value,
}

/// What the core knows without a lookup.
#[derive(Clone, Copy, Debug)]
pub struct Early {
    pub room_joined: bool,
    /// The room is the one the open call (or a join in progress) uses.
    pub open_call_room: bool,
    pub sender_is_self: bool,
}

/// What a lookup shows about the sender.
#[derive(Clone, Copy, Debug)]
pub struct Late {
    pub joined_member: bool,
    pub ignored: bool,
    /// `read_members` reports a current membership of the sender.
    pub in_call: bool,
}

/// A ring that passed [`screen`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ring {
    pub room_id: OwnedRoomId,
    pub event_id: OwnedEventId,
    pub sender: OwnedUserId,
    pub intent: Intent,
    /// Unix milliseconds.
    pub ends_ms: u64,
}

/// The checks that need no lookup: room, own sender, type and expiry.
pub fn screen(event: &Incoming, early: Early, now_ms: u64) -> Result<Ring, Refusal> {
    if !early.room_joined {
        return Err(Refusal::NotJoined);
    }
    if early.open_call_room {
        return Err(Refusal::OpenCall);
    }
    if early.sender_is_self {
        return Err(Refusal::OwnSender);
    }
    let notice = parse_notice(&event.content).ok_or(Refusal::Malformed)?;
    if notice.kind != Kind::Ring {
        return Err(Refusal::NotARing);
    }
    let ends_ms = ends_at(&notice, event.origin_server_ts, now_ms);
    if ends_ms <= now_ms {
        return Err(Refusal::Expired);
    }
    Ok(Ring {
        room_id: event.room_id.clone(),
        event_id: event.event_id.clone(),
        sender: event.sender.clone(),
        intent: notice.intent,
        ends_ms,
    })
}

/// The checks that need a lookup.
pub fn confirm(late: Late) -> Result<(), Refusal> {
    if !late.joined_member {
        Err(Refusal::NotMember)
    } else if late.ignored {
        Err(Refusal::Ignored)
    } else if !late.in_call {
        Err(Refusal::NoMembership)
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct RingState {
    /// Oldest first.
    rings: Vec<Ring>,
    /// When the core last accepted a ring from each sender.
    senders: Vec<(OwnedUserId, u64)>,
    /// The room of the open call, or of a join in progress.
    open_room: Option<OwnedRoomId>,
}

impl RingState {
    fn forget_old_senders(&mut self, now_ms: u64) {
        self.senders
            .retain(|(_, at)| now_ms.saturating_sub(*at) < SENDER_GAP_MS || *at > now_ms);
    }
}

/// The accepted rings, in memory only.
#[derive(Default)]
pub struct Rings {
    state: Mutex<RingState>,
}

impl Rings {
    fn state(&self) -> std::sync::MutexGuard<'_, RingState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a ring from `sender` would pass the sender bound now. The
    /// handler asks before it makes lookups.
    pub fn sender_allowed(&self, sender: &UserId, now_ms: u64) -> Result<(), Refusal> {
        let mut state = self.state();
        state.forget_old_senders(now_ms);
        if state.senders.iter().any(|(user, _)| user == sender) {
            Err(Refusal::TooSoon)
        } else if state.senders.len() >= MAX_SENDERS {
            Err(Refusal::TooManySenders)
        } else {
            Ok(())
        }
    }

    /// Keeps `ring` within the bounds: one for each room (the newer one
    /// replaces the older), at most [`MAX_RINGS`] (the oldest is dropped),
    /// and one for each sender in [`SENDER_GAP_MS`].
    pub fn admit(&self, ring: Ring, now_ms: u64) -> Result<(), Refusal> {
        if ring.ends_ms <= now_ms {
            return Err(Refusal::Expired);
        }
        let mut state = self.state();
        if state.open_room.as_ref() == Some(&ring.room_id) {
            return Err(Refusal::OpenCall);
        }
        state.forget_old_senders(now_ms);
        if state.senders.iter().any(|(user, _)| *user == ring.sender) {
            return Err(Refusal::TooSoon);
        }
        if state.senders.len() >= MAX_SENDERS {
            return Err(Refusal::TooManySenders);
        }
        state.senders.push((ring.sender.clone(), now_ms));
        state.rings.retain(|kept| kept.room_id != ring.room_id);
        if state.rings.len() >= MAX_RINGS {
            state.rings.remove(0);
        }
        state.rings.push(ring);
        Ok(())
    }

    /// The rings still running, newest first. Expired rings go.
    pub fn live(&self, now_ms: u64) -> Vec<Ring> {
        let mut state = self.state();
        state.rings.retain(|ring| ring.ends_ms > now_ms);
        state.rings.iter().rev().cloned().collect()
    }

    /// The ring in `room` that has not expired: its event and its sender.
    pub fn target(&self, room: &RoomId, now_ms: u64) -> Option<(OwnedEventId, OwnedUserId)> {
        self.state()
            .rings
            .iter()
            .find(|ring| ring.room_id == room && ring.ends_ms > now_ms)
            .map(|ring| (ring.event_id.clone(), ring.sender.clone()))
    }

    /// Ends the ring in `room`. Whether there was one.
    pub fn end_room(&self, room: &RoomId) -> bool {
        let mut state = self.state();
        let before = state.rings.len();
        state.rings.retain(|ring| ring.room_id != room);
        state.rings.len() != before
    }

    /// Ends the rings `keep` does not keep. Whether any ended.
    pub fn retain(&self, mut keep: impl FnMut(&Ring) -> bool) -> bool {
        let mut state = self.state();
        let before = state.rings.len();
        state.rings.retain(|ring| keep(ring));
        state.rings.len() != before
    }

    /// Sets the room of the open call, or of a join in progress, and ends
    /// its ring. Whether a ring ended.
    pub fn set_open_room(&self, room: Option<&RoomId>) -> bool {
        let mut state = self.state();
        state.open_room = room.map(RoomId::to_owned);
        match room {
            Some(room) => {
                let before = state.rings.len();
                state.rings.retain(|ring| ring.room_id != room);
                state.rings.len() != before
            }
            None => false,
        }
    }

    /// Whether `room` is the open call's room.
    pub fn is_open_room(&self, room: &RoomId) -> bool {
        self.state().open_room.as_deref() == Some(room)
    }

    /// Drops every ring and every sender. A lock or a sign-out calls it.
    pub fn clear(&self) -> bool {
        let mut state = self.state();
        let any = !state.rings.is_empty();
        *state = RingState::default();
        any
    }
}

/// The rooms that have another device in their call, with the end of the
/// latest of those memberships. One flag for each room, no names.
#[derive(Default)]
pub struct CallRooms {
    rooms: Mutex<HashMap<OwnedRoomId, u64>>,
}

impl CallRooms {
    fn rooms(&self) -> std::sync::MutexGuard<'_, HashMap<OwnedRoomId, u64>> {
        self.rooms.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets or clears the flag of `room`. `until` is when the latest other
    /// membership ends, or `None` when there is none. Whether the flag
    /// changed.
    pub fn set(&self, room: &RoomId, until: Option<u64>) -> bool {
        let mut rooms = self.rooms();
        match until {
            Some(until) => {
                if !rooms.contains_key(room) && rooms.len() >= MAX_FLAGS {
                    return false;
                }
                rooms.insert(room.to_owned(), until) != Some(until)
            }
            None => rooms.remove(room).is_some(),
        }
    }

    /// Whether `room` has another device in its call at `now_ms`.
    pub fn active(&self, room: &RoomId, now_ms: u64) -> bool {
        self.rooms().get(room).is_some_and(|until| *until > now_ms)
    }

    /// Drops the flags of rooms `keep` does not keep.
    pub fn retain(&self, mut keep: impl FnMut(&RoomId) -> bool) {
        self.rooms().retain(|room, _| keep(room));
    }

    pub fn clear(&self) {
        self.rooms().clear();
    }
}

/// How this device rings when it starts a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// `ring`, mentioning the one other member of a direct chat.
    Ring(OwnedUserId),
    /// `notification`, mentioning the room.
    Notification,
}

/// The ring to send, or `None` for no ring. A direct chat with exactly one
/// other joined member rings that member. Any other room gets a
/// `notification`, which mentions the room, so it needs the right to notify
/// the room; without it nothing is sent.
pub fn plan_ring(
    contact: Option<OwnedUserId>,
    joined_members: u64,
    may_notify_room: bool,
) -> Option<Plan> {
    match contact {
        Some(contact) if joined_members == 2 => Some(Plan::Ring(contact)),
        _ if may_notify_room => Some(Plan::Notification),
        _ => None,
    }
}

/// The names a ring's desktop notification carries: the room's and the
/// caller's when the account's setting allows names, else two empty strings.
/// The notification text never has more than that (ADR-0022, ADR-0052).
pub fn toast_names(allowed: bool, room: String, caller: String) -> (String, String) {
    if allowed {
        (room, caller)
    } else {
        (String::new(), String::new())
    }
}

/// The content of the ring this device sends.
pub fn notification_content(
    plan: &Plan,
    membership_event: &str,
    sender_ts: u64,
    video: bool,
) -> Value {
    let (kind, mentions) = match plan {
        Plan::Ring(contact) => ("ring", json!({ "user_ids": [contact] })),
        Plan::Notification => ("notification", json!({ "room": true })),
    };
    json!({
        "notification_type": kind,
        "m.mentions": mentions,
        "m.relates_to": { "rel_type": "m.reference", "event_id": membership_event },
        "sender_ts": sender_ts,
        "lifetime": LIFETIME_MS,
        "m.call.intent": if video { "video" } else { "audio" },
    })
}

/// The content of a decline of the notification `event_id`.
pub fn decline_content(event_id: &str) -> Value {
    json!({ "m.relates_to": { "rel_type": "m.reference", "event_id": event_id } })
}

/// What a decline event says: the notification it names. `None` unless it
/// is a reference relation with a well-formed event ID.
pub fn declined_event(content: &Value) -> Option<OwnedEventId> {
    let relation = content.get("m.relates_to")?;
    if relation.get("rel_type")?.as_str()? != "m.reference" {
        return None;
    }
    OwnedEventId::try_from(relation.get("event_id")?.as_str()?).ok()
}

/// The ring this device sent, and whether the other member of its direct
/// chat declined it (ADR-0052). A decline counts only when it references
/// this ring and comes from that member.
pub struct Watch {
    contact: OwnedUserId,
    event_id: Mutex<Option<OwnedEventId>>,
    declined: AtomicBool,
}

impl Watch {
    pub fn new(contact: OwnedUserId) -> Self {
        Self {
            contact,
            event_id: Mutex::new(None),
            declined: AtomicBool::new(false),
        }
    }

    /// Records the event ID the homeserver gave the sent ring.
    pub fn sent(&self, event_id: OwnedEventId) {
        *self.event_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(event_id);
    }

    /// Looks at a decline event. Whether it declined this ring.
    pub fn observe(&self, sender: &UserId, content: &Value) -> bool {
        if sender != self.contact {
            return false;
        }
        let Some(target) = declined_event(content) else {
            return false;
        };
        let own = self.event_id.lock().unwrap_or_else(|e| e.into_inner());
        if own.as_ref() == Some(&target) {
            self.declined.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn declined(&self) -> bool {
        self.declined.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{event_id, room_id, user_id};

    const NOW: u64 = 1_000_000_000_000;

    fn content(kind: &str, sender_ts: u64, lifetime: u64) -> Value {
        json!({
            "notification_type": kind,
            "sender_ts": sender_ts,
            "lifetime": lifetime,
            "m.call.intent": "video",
        })
    }

    fn incoming(content: Value, origin: u64) -> Incoming {
        Incoming {
            room_id: room_id!("!r:example.org").to_owned(),
            event_id: event_id!("$n:example.org").to_owned(),
            sender: user_id!("@alice:example.org").to_owned(),
            origin_server_ts: origin,
            content,
        }
    }

    fn early() -> Early {
        Early {
            room_joined: true,
            open_call_room: false,
            sender_is_self: false,
        }
    }

    fn ring(room: &str, sender: &str, ends_ms: u64) -> Ring {
        Ring {
            room_id: matrix_sdk::ruma::OwnedRoomId::try_from(room).unwrap(),
            event_id: event_id!("$n:example.org").to_owned(),
            sender: OwnedUserId::try_from(sender).unwrap(),
            intent: Intent::Audio,
            ends_ms,
        }
    }

    #[test]
    fn given_a_fresh_ring_when_screened_then_it_is_accepted_with_its_intent() {
        let event = incoming(content("ring", NOW, 90_000), NOW);
        let ring = screen(&event, early(), NOW + 1_000).unwrap();
        assert_eq!(ring.intent, Intent::Video);
        assert_eq!(ring.ends_ms, NOW + 90_000);
        assert_eq!(ring.sender, user_id!("@alice:example.org"));
    }

    #[test]
    fn given_no_or_unknown_intent_then_the_ring_is_audio() {
        for intent in [None, Some("hologram")] {
            let mut body = content("ring", NOW, 90_000);
            body.as_object_mut().unwrap().remove("m.call.intent");
            if let Some(intent) = intent {
                body["m.call.intent"] = json!(intent);
            }
            let ring = screen(&incoming(body, NOW), early(), NOW).unwrap();
            assert_eq!(ring.intent, Intent::Audio);
        }
    }

    #[test]
    fn given_a_notification_type_then_the_ring_is_refused() {
        let event = incoming(content("notification", NOW, 90_000), NOW);
        assert_eq!(screen(&event, early(), NOW), Err(Refusal::NotARing));
    }

    #[test]
    fn given_bad_content_then_the_ring_is_malformed() {
        for body in [
            json!({}),
            json!("ring"),
            json!({"notification_type": "ring", "lifetime": 1}),
            json!({"notification_type": "ring", "sender_ts": 1}),
            json!({"notification_type": "ring", "sender_ts": "1", "lifetime": 1}),
            json!({"notification_type": "ring", "sender_ts": 1, "lifetime": -1}),
            json!({"notification_type": "shout", "sender_ts": 1, "lifetime": 1}),
            json!({"notification_type": 4, "sender_ts": 1, "lifetime": 1}),
        ] {
            assert_eq!(
                screen(&incoming(body.clone(), NOW), early(), NOW),
                Err(Refusal::Malformed),
                "{body}"
            );
        }
    }

    #[test]
    fn given_an_unusable_room_or_sender_then_the_ring_is_refused() {
        let event = incoming(content("ring", NOW, 90_000), NOW);
        let not_joined = Early {
            room_joined: false,
            ..early()
        };
        assert_eq!(screen(&event, not_joined, NOW), Err(Refusal::NotJoined));
        let open = Early {
            open_call_room: true,
            ..early()
        };
        assert_eq!(screen(&event, open, NOW), Err(Refusal::OpenCall));
        let own = Early {
            sender_is_self: true,
            ..early()
        };
        assert_eq!(screen(&event, own, NOW), Err(Refusal::OwnSender));
    }

    #[test]
    fn given_an_expired_ring_then_it_is_refused() {
        let event = incoming(content("ring", NOW, 90_000), NOW);
        assert_eq!(screen(&event, early(), NOW + 90_000), Err(Refusal::Expired));
        assert!(screen(&event, early(), NOW + 89_999).is_ok());
    }

    #[test]
    fn given_a_sender_clock_far_ahead_then_the_server_time_starts_the_ring() {
        // sender_ts is 21 s after the server's time: the server's time counts.
        let event = incoming(content("ring", NOW + 21_000, 90_000), NOW);
        let ring = screen(&event, early(), NOW).unwrap();
        assert_eq!(ring.ends_ms, NOW + 90_000);
        // 20 s after is still inside the rule.
        let event = incoming(content("ring", NOW + 20_000, 90_000), NOW);
        let ring = screen(&event, early(), NOW).unwrap();
        assert_eq!(ring.ends_ms, NOW + 110_000);
    }

    #[test]
    fn given_a_sender_clock_behind_then_its_time_still_starts_the_ring() {
        let event = incoming(content("ring", NOW - 60_000, 90_000), NOW);
        let ring = screen(&event, early(), NOW).unwrap();
        assert_eq!(ring.ends_ms, NOW + 30_000);
    }

    #[test]
    fn given_a_long_lifetime_then_the_ring_ends_at_most_120_seconds_after_its_start() {
        let event = incoming(content("ring", NOW, u64::MAX), NOW);
        let ring = screen(&event, early(), NOW + 5_000).unwrap();
        assert_eq!(ring.ends_ms, NOW + 120_000);
        let event = incoming(content("ring", NOW, 600_000), NOW);
        let ring = screen(&event, early(), NOW).unwrap();
        assert_eq!(ring.ends_ms, NOW + 120_000);
    }

    /// GIVEN an old ring with a long lifetime, as a restart's sync gives it
    /// WHEN it is screened THEN it has expired and does not ring again.
    #[test]
    fn given_an_old_ring_with_a_long_lifetime_then_it_does_not_ring_again() {
        let event = incoming(content("ring", NOW - 600_000, u64::MAX), NOW - 600_000);
        assert!(screen(&event, early(), NOW).is_err());
    }

    #[test]
    fn given_the_lookups_then_each_missing_fact_refuses_the_ring() {
        let ok = Late {
            joined_member: true,
            ignored: false,
            in_call: true,
        };
        assert_eq!(confirm(ok), Ok(()));
        assert_eq!(
            confirm(Late {
                joined_member: false,
                ..ok
            }),
            Err(Refusal::NotMember)
        );
        assert_eq!(
            confirm(Late {
                ignored: true,
                ..ok
            }),
            Err(Refusal::Ignored)
        );
        assert_eq!(
            confirm(Late {
                in_call: false,
                ..ok
            }),
            Err(Refusal::NoMembership)
        );
    }

    #[test]
    fn given_a_second_ring_in_a_room_then_it_replaces_the_first() {
        let rings = Rings::default();
        rings
            .admit(ring("!r:s", "@a:s", NOW + 10_000), NOW)
            .unwrap();
        rings
            .admit(ring("!r:s", "@b:s", NOW + 20_000), NOW)
            .unwrap();
        let live = rings.live(NOW);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].sender, user_id!("@b:s"));
    }

    #[test]
    fn given_five_rooms_then_the_oldest_ring_is_dropped_and_four_remain() {
        let rings = Rings::default();
        for n in 0..5 {
            rings
                .admit(
                    ring(&format!("!r{n}:s"), &format!("@u{n}:s"), NOW + 10_000),
                    NOW + n,
                )
                .unwrap();
        }
        let live = rings.live(NOW + 5);
        assert_eq!(live.len(), MAX_RINGS);
        assert!(live.iter().all(|ring| ring.room_id != "!r0:s"));
        // Newest first.
        assert_eq!(live[0].room_id, "!r4:s");
    }

    #[test]
    fn given_a_sender_who_rang_within_30_seconds_then_the_next_ring_is_dropped() {
        let rings = Rings::default();
        rings
            .admit(ring("!a:s", "@a:s", NOW + 90_000), NOW)
            .unwrap();
        assert_eq!(
            rings.admit(ring("!b:s", "@a:s", NOW + 90_000), NOW + 29_999),
            Err(Refusal::TooSoon)
        );
        assert_eq!(
            rings.sender_allowed(user_id!("@a:s"), NOW + 29_999),
            Err(Refusal::TooSoon)
        );
        assert_eq!(rings.live(NOW).len(), 1);
        rings
            .admit(ring("!b:s", "@a:s", NOW + 130_000), NOW + 30_000)
            .unwrap();
        assert_eq!(rings.live(NOW + 30_000).len(), 2);
    }

    #[test]
    fn given_many_senders_then_the_list_is_bounded() {
        let rings = Rings::default();
        for n in 0..MAX_SENDERS {
            rings
                .admit(ring("!r:s", &format!("@u{n}:s"), NOW + 10_000), NOW)
                .unwrap();
        }
        assert_eq!(
            rings.admit(ring("!r:s", "@late:s", NOW + 10_000), NOW),
            Err(Refusal::TooManySenders)
        );
        assert_eq!(
            rings.sender_allowed(user_id!("@late:s"), NOW),
            Err(Refusal::TooManySenders)
        );
        // They age out.
        assert!(
            rings
                .sender_allowed(user_id!("@late:s"), NOW + 30_000)
                .is_ok()
        );
    }

    #[test]
    fn given_an_expired_ring_then_it_does_not_show_and_cannot_be_admitted() {
        let rings = Rings::default();
        assert_eq!(
            rings.admit(ring("!r:s", "@a:s", NOW), NOW),
            Err(Refusal::Expired)
        );
        rings.admit(ring("!r:s", "@a:s", NOW + 5_000), NOW).unwrap();
        assert_eq!(rings.live(NOW + 4_999).len(), 1);
        assert!(rings.live(NOW + 5_000).is_empty());
        assert!(rings.target(room_id!("!r:s"), NOW + 5_000).is_none());
    }

    #[test]
    fn given_a_join_in_a_room_then_its_ring_ends_and_new_ones_are_refused() {
        let rings = Rings::default();
        rings.admit(ring("!r:s", "@a:s", NOW + 5_000), NOW).unwrap();
        rings.admit(ring("!q:s", "@b:s", NOW + 5_000), NOW).unwrap();
        assert!(rings.set_open_room(Some(room_id!("!r:s"))));
        assert_eq!(rings.live(NOW).len(), 1);
        assert_eq!(
            rings.admit(ring("!r:s", "@c:s", NOW + 5_000), NOW),
            Err(Refusal::OpenCall)
        );
        assert!(rings.is_open_room(room_id!("!r:s")));
        rings.set_open_room(None);
        rings.admit(ring("!r:s", "@c:s", NOW + 5_000), NOW).unwrap();
    }

    #[test]
    fn given_a_ring_when_it_ends_by_decline_or_leave_then_it_is_gone() {
        let rings = Rings::default();
        rings.admit(ring("!r:s", "@a:s", NOW + 5_000), NOW).unwrap();
        assert!(rings.end_room(room_id!("!r:s")));
        assert!(!rings.end_room(room_id!("!r:s")));
        rings.admit(ring("!r:s", "@b:s", NOW + 5_000), NOW).unwrap();
        // The sender's membership ended.
        assert!(rings.retain(|ring| ring.sender != "@b:s"));
        assert!(rings.live(NOW).is_empty());
        rings.admit(ring("!r:s", "@c:s", NOW + 5_000), NOW).unwrap();
        assert!(rings.clear());
        assert!(rings.live(NOW).is_empty());
    }

    #[test]
    fn given_a_ring_then_its_decline_target_is_its_event_and_sender() {
        let rings = Rings::default();
        rings.admit(ring("!r:s", "@a:s", NOW + 5_000), NOW).unwrap();
        let (event, sender) = rings.target(room_id!("!r:s"), NOW).unwrap();
        assert_eq!(event, event_id!("$n:example.org"));
        assert_eq!(sender, user_id!("@a:s"));
        assert!(rings.target(room_id!("!other:s"), NOW).is_none());
    }

    #[test]
    fn given_other_members_in_a_call_then_the_room_has_a_flag_until_they_expire() {
        let rooms = CallRooms::default();
        let room = room_id!("!r:s");
        assert!(!rooms.active(room, NOW));
        assert!(rooms.set(room, Some(NOW + 1_000)));
        assert!(!rooms.set(room, Some(NOW + 1_000)));
        assert!(rooms.active(room, NOW));
        assert!(!rooms.active(room, NOW + 1_000));
        assert!(rooms.set(room, None));
        assert!(!rooms.set(room, None));
        assert!(!rooms.active(room, NOW));
    }

    #[test]
    fn given_rooms_that_are_left_then_their_flags_go() {
        let rooms = CallRooms::default();
        rooms.set(room_id!("!a:s"), Some(NOW + 1_000));
        rooms.set(room_id!("!b:s"), Some(NOW + 1_000));
        rooms.retain(|room| room == "!a:s");
        assert!(rooms.active(room_id!("!a:s"), NOW));
        assert!(!rooms.active(room_id!("!b:s"), NOW));
        rooms.clear();
        assert!(!rooms.active(room_id!("!a:s"), NOW));
    }

    #[test]
    fn given_a_room_kind_then_the_ring_choice_follows_the_rules() {
        let contact = Some(user_id!("@bob:s").to_owned());
        assert_eq!(
            plan_ring(contact.clone(), 2, false),
            Some(Plan::Ring(user_id!("@bob:s").to_owned()))
        );
        // A direct chat that has a third joined member is a group.
        assert_eq!(
            plan_ring(contact.clone(), 3, true),
            Some(Plan::Notification)
        );
        assert_eq!(plan_ring(None, 2, true), Some(Plan::Notification));
        // No right to notify the room: nothing is sent.
        assert_eq!(plan_ring(contact, 3, false), None);
        assert_eq!(plan_ring(None, 5, false), None);
    }

    #[test]
    fn given_a_plan_then_the_sent_content_is_exact() {
        let ring = notification_content(
            &Plan::Ring(user_id!("@bob:s").to_owned()),
            "$member:s",
            NOW,
            false,
        );
        assert_eq!(
            ring,
            json!({
                "notification_type": "ring",
                "m.mentions": { "user_ids": ["@bob:s"] },
                "m.relates_to": { "rel_type": "m.reference", "event_id": "$member:s" },
                "sender_ts": NOW,
                "lifetime": 90_000,
                "m.call.intent": "audio",
            })
        );
        let notice = notification_content(&Plan::Notification, "$member:s", NOW, true);
        assert_eq!(notice["notification_type"], "notification");
        assert_eq!(notice["m.mentions"], json!({ "room": true }));
        assert_eq!(notice["m.call.intent"], "video");
        // Our own ring parses back as a ring with the same timing.
        assert_eq!(parse_notice(&ring).unwrap().lifetime_ms, 90_000);
    }

    #[test]
    fn given_a_declined_own_ring_then_only_the_contacts_decline_of_it_counts() {
        let watch = Watch::new(user_id!("@bob:s").to_owned());
        // Before the homeserver named the ring, nothing can match.
        assert!(!watch.observe(user_id!("@bob:s"), &decline_content("$ring:s")));
        watch.sent(event_id!("$ring:s").to_owned());
        assert!(!watch.observe(user_id!("@carol:s"), &decline_content("$ring:s")));
        assert!(!watch.observe(user_id!("@bob:s"), &decline_content("$other:s")));
        assert!(!watch.observe(user_id!("@bob:s"), &json!({})));
        assert!(!watch.declined());
        assert!(watch.observe(user_id!("@bob:s"), &decline_content("$ring:s")));
        assert!(watch.declined());
    }

    #[test]
    fn given_the_names_setting_then_a_ring_notification_names_the_caller_or_nobody() {
        assert_eq!(
            toast_names(true, "Plans".into(), "Bob".into()),
            ("Plans".to_owned(), "Bob".to_owned())
        );
        assert_eq!(
            toast_names(false, "Plans".into(), "Bob".into()),
            (String::new(), String::new())
        );
    }

    #[test]
    fn given_a_notification_event_then_the_decline_references_it() {
        assert_eq!(
            decline_content("$n:s"),
            json!({ "m.relates_to": { "rel_type": "m.reference", "event_id": "$n:s" } })
        );
    }

    #[test]
    fn given_decline_content_then_only_a_reference_names_an_event() {
        assert_eq!(
            declined_event(&decline_content("$n:s")),
            Some(event_id!("$n:s").to_owned())
        );
        for body in [
            json!({}),
            json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": "$n:s"}}),
            json!({"m.relates_to": {"rel_type": "m.reference", "event_id": "n"}}),
            json!({"m.relates_to": {"rel_type": "m.reference"}}),
        ] {
            assert_eq!(declined_event(&body), None, "{body}");
        }
    }
}
