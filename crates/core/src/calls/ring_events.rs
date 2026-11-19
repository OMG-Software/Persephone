//! The sync handlers for incoming rings and call flags (ADR-0052).
//!
//! Two handlers run for the life of the sync loop. One reads ring and
//! decline room events, after decryption. The other reads call-membership
//! state events, keeps each room's call flag and ends rings whose sender is
//! no longer in the call. All decisions are in [`super::rings`]; this module
//! gathers the facts and passes the result on. A ring never starts a join,
//! opens a connection or turns on a device.

use super::controller::now_ms;
use super::membership::{EVENT_TYPE as MEMBERSHIP_TYPE, read_members};
use super::rings::{
    DECLINE_TYPE, Early, Incoming, Late, MAX_EVENT_BYTES, NOTIFICATION_TYPE, Rings, confirm,
    declined_event, screen,
};
use crate::session::SessionListener;
use matrix_sdk::{
    Client, Room, RoomState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedEventId, OwnedUserId,
        events::{AnySyncMessageLikeEvent, AnySyncStateEvent},
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Weak};

/// What the handlers need from the session. Held weakly: the SDK keeps
/// handlers as long as the client.
pub trait Env: Send + Sync {
    fn listener(&self) -> Option<Arc<dyn SessionListener>>;
    /// `Some(names)` while desktop notifications are on: whether they may
    /// name the room and the caller (ADR-0022).
    fn toast(&self) -> Option<bool>;
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    event_id: OwnedEventId,
    sender: OwnedUserId,
    origin_server_ts: u64,
    content: Value,
}

/// Registers both handlers. The caller removes them with the client's
/// `remove_event_handler`.
pub fn register(client: &Client, env: Weak<dyn Env>) -> [EventHandlerHandle; 2] {
    let messages = {
        let env = env.clone();
        client.add_event_handler(move |raw: Raw<AnySyncMessageLikeEvent>, room: Room| {
            let env = env.clone();
            async move {
                if raw.json().get().len() > MAX_EVENT_BYTES {
                    return;
                }
                let Ok(event) = serde_json::from_str::<Envelope>(raw.json().get()) else {
                    return;
                };
                let Some(env) = env.upgrade() else {
                    return;
                };
                match event.kind.as_str() {
                    NOTIFICATION_TYPE => ring_arrived(&*env, &room, event).await,
                    DECLINE_TYPE => decline_arrived(&*env, &room, event),
                    _ => {}
                }
            }
        })
    };
    let members = client.add_event_handler(move |raw: Raw<AnySyncStateEvent>, room: Room| {
        let env = env.clone();
        async move {
            #[derive(Deserialize)]
            struct Kind {
                #[serde(rename = "type")]
                kind: String,
            }
            if !serde_json::from_str::<Kind>(raw.json().get())
                .is_ok_and(|k| k.kind == MEMBERSHIP_TYPE)
            {
                return;
            }
            let Some(env) = env.upgrade() else {
                return;
            };
            members_changed(&*env, &room).await;
        }
    });
    [messages, members]
}

/// A ring event: the checks, then the ring and its one notification.
async fn ring_arrived(env: &dyn Env, room: &Room, event: Envelope) {
    let Some(listener) = env.listener() else {
        return;
    };
    let Some(rings) = listener.rings() else {
        return;
    };
    let client = room.client();
    let now = now_ms();
    let incoming = Incoming {
        room_id: room.room_id().to_owned(),
        event_id: event.event_id,
        sender: event.sender,
        origin_server_ts: event.origin_server_ts,
        content: event.content,
    };
    let early = Early {
        room_joined: room.state() == RoomState::Joined,
        open_call_room: rings.is_open_room(room.room_id()),
        sender_is_self: client.user_id() == Some(&*incoming.sender),
    };
    let Ok(ring) = screen(&incoming, early, now) else {
        return;
    };
    // The sender bound first, so a flood costs no lookups.
    if rings.sender_allowed(&ring.sender, now).is_err() {
        return;
    }
    let late = Late {
        joined_member: joined_member(room, &ring.sender).await,
        ignored: match crate::abuse::ignored_set(&client).await {
            Ok(ignored) => ignored.contains(&ring.sender),
            // Unreadable ignore state refuses the ring.
            Err(_) => true,
        },
        in_call: match read_members(room, now).await {
            Ok(members) => members.iter().any(|m| m.user_id == ring.sender),
            Err(_) => false,
        },
    };
    if confirm(late).is_err() {
        return;
    }
    let sender = ring.sender.clone();
    if rings.admit(ring, now_ms()).is_err() {
        return;
    }
    listener.rings_changed();
    if let Some(allowed) = env.toast() {
        // Without names, none leaves the core. The lookup runs only when the
        // names are allowed.
        let (room_name, caller) = if allowed {
            crate::notifications::names(&client, room, &sender).await
        } else {
            (String::new(), String::new())
        };
        let (room_name, caller) = super::rings::toast_names(allowed, room_name, caller);
        listener.notify_call(&room_name, &caller);
    }
}

async fn joined_member(room: &Room, sender: &OwnedUserId) -> bool {
    use matrix_sdk::ruma::events::room::member::MembershipState;
    match room.get_member(sender).await {
        Ok(Some(member)) => *member.membership() == MembershipState::Join,
        _ => false,
    }
}

/// A decline sent by this user from another device ends the ring here too.
fn decline_arrived(env: &dyn Env, room: &Room, event: Envelope) {
    if room.client().user_id() != Some(&*event.sender) {
        return;
    }
    let Some(notification) = declined_event(&event.content) else {
        return;
    };
    let Some(listener) = env.listener() else {
        return;
    };
    let Some(rings) = listener.rings() else {
        return;
    };
    if rings.retain(|ring| ring.event_id != notification) {
        listener.rings_changed();
    }
}

/// A call-membership change: this room's flag, and the rings whose sender
/// left the call.
async fn members_changed(env: &dyn Env, room: &Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    let Some(listener) = env.listener() else {
        return;
    };
    let (Some(rings), Some(call_rooms)) = (listener.rings(), listener.call_rooms()) else {
        return;
    };
    let client = room.client();
    let Ok(members) = read_members(room, now_ms()).await else {
        return;
    };
    let own = (client.user_id(), client.device_id());
    let until = members
        .iter()
        .filter(|m| (Some(&*m.user_id), Some(&*m.device_id)) != own)
        .map(|m| m.expires_ts)
        .max();
    if call_rooms.set(room.room_id(), until) {
        listener.room_call_changed(room.room_id().as_str());
    }
    if end_rings_of_missing_callers(rings, room, &members) {
        listener.rings_changed();
    }
}

fn end_rings_of_missing_callers(
    rings: &Rings,
    room: &Room,
    members: &[super::membership::ActiveMember],
) -> bool {
    rings.retain(|ring| {
        ring.room_id != room.room_id() || members.iter().any(|m| m.user_id == ring.sender)
    })
}

/// How long a decline or a ring send waits for the homeserver.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Sends the decline of notification `event_id` in `room_id` as a normal
/// room event; the SDK encrypts it in an encrypted room. Once: no retry.
/// Whether the homeserver accepted it.
pub async fn send_decline(
    client: &Client,
    room_id: &matrix_sdk::ruma::RoomId,
    event_id: &matrix_sdk::ruma::EventId,
) -> bool {
    let Some(room) = client.get_room(room_id) else {
        return false;
    };
    let content = super::rings::decline_content(event_id.as_str());
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, room.send_raw(DECLINE_TYPE, content)).await,
        Ok(Ok(_))
    )
}
