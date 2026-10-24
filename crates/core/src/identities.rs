//! Room members whose cross-signing identity changed (ADR-0007).
//!
//! * The SDK pins the first identity it sees for each user. A member whose
//!   identity changed since, and whom this account had not verified, is in
//!   "pin violation"; one whose verified identity changed is in "verification
//!   violation". While a room has either, the core sends nothing there
//!   (`sending.rs`).
//! * The SDK reports this only as a stream per room whose first item, when
//!   there is one, is the room's current state. [`changed_members`] polls it
//!   once without waiting: no item means no member changed. Building the
//!   stream loads the room's members, so the SDK follows their keys.
//! * Accepting pins the new identity, or withdraws the verification of the
//!   old one (which also pins). The core asks in its own window first
//!   (`session.rs`); here the change is checked again before it is applied,
//!   so an answer for a change that is gone or of another kind changes
//!   nothing.

use futures_core::Stream;
use matrix_sdk::{
    Client, Room, RoomState,
    ruma::{OwnedUserId, RoomId, UserId},
};
use matrix_sdk_crypto::IdentityState;
use persephone_contracts::{IdentityChange, RoomIdentityChanges, SecurityError};
use persephone_core_protocol::{ErrorCode, MAX_USER_ID_BYTES};
use std::{pin::pin, task::Poll};

/// Most changed members one reply lists.
pub const MAX_LISTED: usize = 16;

/// A member whose identity changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub user_id: OwnedUserId,
    /// This account had verified the old identity.
    pub was_verified: bool,
}

/// The room's members whose identity changed, now. `None` when the SDK
/// could not say, which callers treat as "do not send".
pub async fn changed_members(room: &Room) -> Option<Vec<Changed>> {
    let stream = room.subscribe_to_identity_status_changes().await.ok()?;
    let mut stream = pin!(stream);
    // The first item is ready at once when any member is not pinned or
    // verified; otherwise the stream waits for a change, which means none.
    let current = std::future::poll_fn(|context| {
        Poll::Ready(match stream.as_mut().poll_next(context) {
            Poll::Ready(Some(changes)) => changes,
            Poll::Ready(None) | Poll::Pending => Vec::new(),
        })
    })
    .await;
    Some(
        current
            .into_iter()
            .filter_map(|change| {
                let was_verified = match change.changed_to {
                    IdentityState::PinViolation => false,
                    IdentityState::VerificationViolation => true,
                    IdentityState::Verified | IdentityState::Pinned => return None,
                };
                Some(Changed {
                    user_id: change.user_id,
                    was_verified,
                })
            })
            .collect(),
    )
}

/// A user ID the page may show and name back: printable ASCII within the
/// Matrix limit, so nothing is projected away.
fn listable(user_id: &UserId) -> bool {
    user_id.as_str().len() <= MAX_USER_ID_BYTES
        && user_id.as_str().bytes().all(|byte| byte.is_ascii_graphic())
}

/// The page's view of a joined room's changed members, at most
/// [`MAX_LISTED`]. A member whose ID cannot be listed still counts: the
/// reply is then marked truncated, and sending stays paused.
pub fn listed(room_id: &str, changed: &[Changed]) -> RoomIdentityChanges {
    let shown: Vec<IdentityChange> = changed
        .iter()
        .filter(|change| listable(&change.user_id))
        .take(MAX_LISTED)
        .map(|change| IdentityChange {
            user_id: change.user_id.to_string(),
            was_verified: change.was_verified,
        })
        .collect();
    RoomIdentityChanges {
        room_id: room_id.to_owned(),
        truncated: shown.len() < changed.len(),
        changes: shown,
    }
}

/// The page's view of a room's changed members; `not_found` for a room this
/// account is not joined to, `unavailable` when the SDK cannot say.
pub async fn room_changes(
    client: &Client,
    room_id: &str,
) -> Result<RoomIdentityChanges, ErrorCode> {
    let room = joined(client, room_id).ok_or(ErrorCode::NotFound)?;
    let changed = changed_members(&room).await.ok_or(ErrorCode::Unavailable)?;
    Ok(listed(room_id, &changed))
}

fn joined(client: &Client, room_id: &str) -> Option<Room> {
    RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .filter(|room| room.state() == RoomState::Joined)
}

/// Whether `user_id`'s identity changed in this joined room, and how;
/// `None` when it did not (or the room or user is unknown).
pub async fn find(client: &Client, room_id: &str, user_id: &UserId) -> Option<Changed> {
    let room = joined(client, room_id)?;
    changed_members(&room)
        .await?
        .into_iter()
        .find(|change| change.user_id == user_id)
}

/// Accepts `user_id`'s new identity, if it is still changed in the same way
/// the user was asked about.
pub async fn accept(
    client: &Client,
    room_id: &str,
    user_id: &UserId,
    was_verified: bool,
) -> Result<(), SecurityError> {
    let current = find(client, room_id, user_id)
        .await
        .ok_or(SecurityError::Failed)?;
    if current.was_verified != was_verified {
        return Err(SecurityError::Failed);
    }
    let identity = client
        .encryption()
        .get_user_identity(user_id)
        .await
        .ok()
        .flatten()
        .ok_or(SecurityError::Failed)?;
    let done = if was_verified {
        identity.withdraw_verification().await
    } else {
        identity.pin().await
    };
    done.map_err(|_| SecurityError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(user: &str, was_verified: bool) -> Changed {
        Changed {
            user_id: UserId::parse(user).unwrap(),
            was_verified,
        }
    }

    /// GIVEN changed members WHEN listed for the page THEN at most
    /// `MAX_LISTED` printable IDs are named, and anything left out marks the
    /// list truncated rather than disappearing.
    #[test]
    fn given_changed_members_then_the_list_is_bounded_and_says_when_it_is_cut() {
        let few = [change("@a:s", false), change("@b:s", true)];
        let list = listed("!r:s", &few);
        assert_eq!(list.room_id, "!r:s");
        assert!(!list.truncated);
        assert_eq!(list.changes.len(), 2);
        assert!(list.changes[1].was_verified);

        let many: Vec<Changed> = (0..MAX_LISTED + 3)
            .map(|n| change(&format!("@u{n}:s"), false))
            .collect();
        let list = listed("!r:s", &many);
        assert_eq!(list.changes.len(), MAX_LISTED);
        assert!(list.truncated);

        // A historical ID with a character the page could not name back.
        let odd = [change("@a:s", false), change("@é:s", false)];
        let list = listed("!r:s", &odd);
        assert_eq!(list.changes.len(), 1);
        assert!(list.truncated, "an unlisted member still pauses sending");
    }
}
