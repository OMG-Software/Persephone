//! What other people in a room are doing: who is typing, and who has read
//! up to an event (ADR-0023).
//!
//! * Typing notices arrive as ephemeral sync events. The sync loop keeps the
//!   newest list for each room here, for at most [`MAX_TYPING_ROOMS`] rooms
//!   (the oldest dropped first) and [`MAX_SHOWN_NAMES`] users each. A lock
//!   clears it with the rest of the session.
//! * Read receipts come from the SDK's state store, for one event the page
//!   names: public receipts of the main timeline and unthreaded ones.
//! * Names are user IDs projected like display names; this account's own
//!   user is left out. Nothing here is ever sent: whether this account sends
//!   typing notices and public receipts is the account's settings' choice.

use crate::views::project_name;
use matrix_sdk::{
    Client, RoomState,
    ruma::{
        EventId, RoomId, UserId,
        events::receipt::{ReceiptThread, ReceiptType},
    },
};
use persephone_contracts::RoomActivity;
use persephone_core_protocol::ErrorCode;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Mutex, MutexGuard},
};

/// Most rooms whose typing list is kept.
pub const MAX_TYPING_ROOMS: usize = 64;
/// Most names in either list of a [`RoomActivity`].
pub const MAX_SHOWN_NAMES: usize = 10;

/// The newest typing list of each room, oldest room first.
#[derive(Default)]
pub struct Typing {
    rooms: Mutex<VecDeque<(String, Vec<String>)>>,
}

impl Typing {
    fn rooms(&self) -> MutexGuard<'_, VecDeque<(String, Vec<String>)>> {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Replaces the room's list. One more user than shown is kept, so the
    /// page can tell the list was cut.
    pub fn set<'a>(&self, room: &str, users: impl IntoIterator<Item = &'a UserId>) {
        let users: Vec<String> = users
            .into_iter()
            .take(MAX_SHOWN_NAMES + 1)
            .map(|user| user.as_str().to_owned())
            .collect();
        let mut rooms = self.rooms();
        rooms.retain(|(r, _)| r != room);
        if users.is_empty() {
            return;
        }
        rooms.push_back((room.to_owned(), users));
        while rooms.len() > MAX_TYPING_ROOMS {
            rooms.pop_front();
        }
    }

    /// The users typing in the room.
    pub fn get(&self, room: &str) -> Vec<String> {
        self.rooms()
            .iter()
            .find(|(r, _)| r == room)
            .map(|(_, users)| users.clone())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        self.rooms().clear();
    }
}

/// Projected names of `users` other than `own`, at most [`MAX_SHOWN_NAMES`],
/// and whether more were left out.
fn shown<'a>(
    users: impl IntoIterator<Item = &'a str>,
    own: Option<&UserId>,
) -> (Vec<String>, bool) {
    let mut names = Vec::new();
    let mut truncated = false;
    let mut seen = BTreeSet::new();
    for user in users {
        if own.is_some_and(|own| own.as_str() == user) || !seen.insert(user) {
            continue;
        }
        if names.len() == MAX_SHOWN_NAMES {
            truncated = true;
            break;
        }
        names.push(project_name(user));
    }
    (names, truncated)
}

/// Who is typing in the joined room `room_id`, and, when `event_id` is
/// given, who has read up to that event.
pub async fn room_activity(
    client: &Client,
    typing: &Typing,
    room_id: &str,
    event_id: Option<&EventId>,
) -> Result<RoomActivity, ErrorCode> {
    let room = RoomId::parse(room_id)
        .ok()
        .and_then(|room_id| client.get_room(&room_id))
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)?;
    let own = client.user_id();
    let typists = typing.get(room_id);
    let (typing, typing_cut) = shown(typists.iter().map(String::as_str), own);
    let mut readers = Vec::new();
    if let Some(event_id) = event_id {
        for thread in [ReceiptThread::Unthreaded, ReceiptThread::Main] {
            let receipts = room
                .load_event_receipts(ReceiptType::Read, &thread, event_id)
                .await
                .map_err(|_| ErrorCode::Unavailable)?;
            readers.extend(receipts.into_iter().map(|(user, _)| user));
        }
    }
    let (read_by, read_cut) = shown(readers.iter().map(|user| user.as_str()), own);
    Ok(RoomActivity {
        typing,
        read_by,
        truncated: typing_cut || read_cut,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::OwnedUserId;

    fn user(name: &str) -> OwnedUserId {
        OwnedUserId::try_from(format!("@{name}:example.org")).unwrap()
    }

    /// GIVEN typing lists for more rooms than kept WHEN set THEN the oldest
    /// rooms go first, an empty list removes a room, and each list keeps one
    /// more user than shown.
    #[test]
    fn given_typing_lists_then_rooms_and_users_are_bounded() {
        let typing = Typing::default();
        for n in 0..=MAX_TYPING_ROOMS {
            typing.set(&format!("!r{n}:s"), [&*user("a")]);
        }
        assert!(typing.get("!r0:s").is_empty(), "oldest room dropped");
        assert_eq!(typing.get("!r1:s"), ["@a:example.org"]);
        typing.set("!r1:s", []);
        assert!(typing.get("!r1:s").is_empty());
        let many: Vec<OwnedUserId> = (0..50).map(|n| user(&format!("u{n}"))).collect();
        typing.set("!big:s", many.iter().map(|user| &**user));
        assert_eq!(typing.get("!big:s").len(), MAX_SHOWN_NAMES + 1);
        typing.clear();
        assert!(typing.get("!big:s").is_empty());
    }

    /// GIVEN users with this account among them, a repeat and a hostile ID
    /// WHEN shown THEN this account and repeats are left out, IDs are
    /// projected, and a list over the limit is cut and says so.
    #[test]
    fn given_users_then_own_and_repeats_are_left_out_and_the_list_bounded() {
        let own = user("me");
        let (names, cut) = shown(
            [
                "@me:example.org",
                "@b:example.org",
                "@b:example.org",
                "@c\u{202E}:s",
            ],
            Some(&own),
        );
        assert_eq!(names, ["@b:example.org", "@c\u{FFFD}:s"]);
        assert!(!cut);
        let many: Vec<String> = (0..=MAX_SHOWN_NAMES).map(|n| format!("@u{n}:s")).collect();
        let (names, cut) = shown(many.iter().map(String::as_str), Some(&own));
        assert_eq!(names.len(), MAX_SHOWN_NAMES);
        assert!(cut);
    }
}
