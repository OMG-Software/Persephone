//! Direct-message conversations: starting one with a user, answering an
//! invitation to one, and the homeserver's user directory.
//!
//! * Starting opens an existing encrypted direct-message room with just that
//!   user when there is one: marked direct for them (`contacts::contact_of`)
//!   and with exactly two members, joined or invited, this account and them,
//!   as the room's member list says. A room someone else joined, or one whose
//!   members cannot be read, is not reused; an unencrypted one never is.
//!   Otherwise the user must have a cross-signing identity, because room
//!   keys go only to cross-signed devices (ADR-0005) and they could not read
//!   anything; then the SDK creates an encrypted room (`Client::create_dm`:
//!   encryption in the initial state, the trusted private chat preset) and
//!   invites them. There is no unencrypted variant.
//! * Only invitations marked direct are listed and answered here. The
//!   inviter's name is the homeserver's word, projected like any name.
//!   Accepting joins the room, which the SDK then marks as direct.
//! * A user ID sent as an identifier (a directory match the page may start a
//!   conversation with, an inviter) is sent unchanged, never projected: a
//!   projection could cut a valid ID short. Only IDs within the Matrix limit
//!   made of visible ASCII are sent; another directory match is left out,
//!   and another inviter is shown projected (accepting and declining use the
//!   room ID).
//! * Directory search sends the term to the homeserver. The page asks for it
//!   only when the user has turned directory search on.
//! * A failure to reach the homeserver is `unavailable`; an answer refusing
//!   the request is `Refused` (or `not_found` for an invitation that is no
//!   longer pending). Nothing is retried.

use crate::{
    contacts,
    views::{MAX_ROOMS, project_name},
};
use matrix_sdk::{
    Client, EncryptionState, Room, RoomMemberships, RoomState,
    ruma::{OwnedUserId, RoomId, UserId},
};
use persephone_contracts::{ConversationStart, InviteList, InviteSummary, UserMatch, UserSearch};
use persephone_core_protocol::{
    ErrorCode, MAX_SEARCH_RESULTS, MAX_SEARCH_TERM_BYTES, MAX_USER_ID_BYTES,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// One operation of a kind at a time; a second is `limit_exceeded`.
#[derive(Default)]
pub struct Busy(AtomicBool);

impl Busy {
    pub fn begin(&self) -> Result<BusyGuard<'_>, ErrorCode> {
        self.0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| BusyGuard(&self.0))
            .map_err(|_| ErrorCode::LimitExceeded)
    }
}

/// Ends the operation when dropped.
pub struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A user ID the page sent, if it is a valid Matrix user ID within the
/// protocol's limit.
pub fn parse_user(user_id: &str) -> Option<OwnedUserId> {
    (user_id.len() <= MAX_USER_ID_BYTES)
        .then(|| UserId::parse(user_id).ok())
        .flatten()
}

/// A user ID to send as an identifier: unchanged, if it is within the Matrix
/// limit and made of visible ASCII; otherwise none (see the module notes).
fn identifier(user: &UserId) -> Option<String> {
    let id = user.as_str();
    (id.len() <= MAX_USER_ID_BYTES && id.chars().all(|c| c.is_ascii_graphic()))
        .then(|| id.to_owned())
}

/// A search term the page sent, if it is not blank and within the limit.
pub fn valid_term(term: &str) -> bool {
    term.len() <= MAX_SEARCH_TERM_BYTES
        && !term.trim().is_empty()
        && !term.chars().any(char::is_control)
}

/// Direct-message invitations, in room ID order, at most [`MAX_ROOMS`].
pub async fn invite_list(client: &Client) -> InviteList {
    let mut rooms = client.invited_rooms();
    rooms.sort_by(|a, b| a.room_id().cmp(b.room_id()));
    let mut invites = Vec::new();
    let mut truncated = false;
    for room in rooms {
        let Some(invite) = direct_invite(&room).await else {
            continue;
        };
        if invites.len() == MAX_ROOMS {
            truncated = true;
            break;
        }
        invites.push(invite);
    }
    InviteList { invites, truncated }
}

/// The invitation of an invited room marked direct, or `None`.
async fn direct_invite(room: &Room) -> Option<InviteSummary> {
    if room.state() != RoomState::Invited || !room.is_direct().await.ok()? {
        return None;
    }
    let details = room.invite_details().await.ok()?;
    Some(InviteSummary {
        room_id: room.room_id().to_string(),
        inviter: identifier(&details.inviter_id)
            .unwrap_or_else(|| project_name(details.inviter_id.as_str())),
        inviter_name: details
            .inviter
            .and_then(|inviter| inviter.display_name().map(project_name)),
    })
}

/// The invited direct-message room `room_id`, or `not_found`.
async fn invited_room(client: &Client, room_id: &str) -> Result<Room, ErrorCode> {
    let room = RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .ok_or(ErrorCode::NotFound)?;
    match direct_invite(&room).await {
        Some(_) => Ok(room),
        None => Err(ErrorCode::NotFound),
    }
}

/// Joins the room of a direct-message invitation.
pub async fn accept(client: &Client, room_id: &str) -> Result<(), ErrorCode> {
    let room = invited_room(client, room_id).await?;
    room.join().await.map_err(|error| request_error(&error))
}

/// Refuses a direct-message invitation by leaving its room.
pub async fn decline(client: &Client, room_id: &str) -> Result<(), ErrorCode> {
    let room = invited_room(client, room_id).await?;
    room.leave().await.map_err(|error| request_error(&error))
}

/// Opens or creates a direct-message conversation with `user` (see the
/// module notes). Starting one with this account itself is
/// `invalid_argument`.
pub async fn start(client: &Client, user: &UserId) -> Result<ConversationStart, ErrorCode> {
    if client.user_id() == Some(user) {
        return Err(ErrorCode::InvalidArgument);
    }
    if let Some(room) = existing(client, user).await {
        return Ok(ConversationStart::Existing {
            room_id: room.room_id().to_string(),
        });
    }
    let encryption = client.encryption();
    // The store may not follow this user yet: ask the homeserver before
    // saying they have no identity.
    let identity = match encryption.get_user_identity(user).await {
        Ok(Some(identity)) => Some(identity),
        Ok(None) => encryption
            .request_user_identity(user)
            .await
            .map_err(|_| ErrorCode::Unavailable)?,
        Err(_) => return Err(ErrorCode::Unavailable),
    };
    if identity.is_none() {
        return Ok(ConversationStart::NoIdentity {});
    }
    match client.create_dm(user).await {
        Ok(room) => Ok(ConversationStart::Created {
            room_id: room.room_id().to_string(),
        }),
        Err(error) if error.client_api_error_kind().is_some() => Ok(ConversationStart::Refused {}),
        Err(_) => Err(ErrorCode::Unavailable),
    }
}

/// A joined, encrypted direct-message room whose members are just this
/// account and `user` (joined or invited). An unencrypted room is skipped, so
/// starting creates an encrypted one instead.
async fn existing(client: &Client, user: &UserId) -> Option<Room> {
    for room in client.get_dm_rooms(user) {
        if contacts::contact_of(client, &room).as_deref() == Some(user)
            && just_us(client, &room, user).await
            && encrypted(&room).await
        {
            return Some(room);
        }
    }
    None
}

/// Whether the room's member list (joined or invited) is exactly this account
/// and `user`. The direct marks and the member count can outlive membership
/// changes; the list may be loaded from the homeserver. A failure, or no own
/// user ID, is `false`.
async fn just_us(client: &Client, room: &Room, user: &UserId) -> bool {
    let Some(own) = client.user_id() else {
        return false;
    };
    room.members(RoomMemberships::ACTIVE)
        .await
        .is_ok_and(|members| {
            members.len() == 2
                && members.iter().any(|member| member.user_id() == own)
                && members.iter().any(|member| member.user_id() == user)
        })
}

/// Whether the room is encrypted. A state not known yet is asked of the
/// homeserver; a failure to tell counts as not encrypted.
async fn encrypted(room: &Room) -> bool {
    let state = match room.encryption_state() {
        EncryptionState::Unknown => room.latest_encryption_state().await.ok(),
        known => Some(known),
    };
    matches!(state, Some(EncryptionState::Encrypted))
}

/// Searches the homeserver's user directory, leaving out this account.
pub async fn search(client: &Client, term: &str) -> Result<UserSearch, ErrorCode> {
    let response = client
        .search_users(term, MAX_SEARCH_RESULTS as u64)
        .await
        .map_err(|error| {
            if error.as_client_api_error().is_some() {
                ErrorCode::Rejected
            } else {
                ErrorCode::Unavailable
            }
        })?;
    let own = client.user_id();
    let mut users: Vec<UserMatch> = response
        .results
        .iter()
        .filter(|user| Some(user.user_id.as_ref()) != own)
        .filter_map(|user| {
            Some(UserMatch {
                user_id: identifier(&user.user_id)?,
                display_name: user.display_name.as_deref().map(project_name),
            })
        })
        .collect();
    let truncated = response.limited || users.len() > MAX_SEARCH_RESULTS;
    users.truncate(MAX_SEARCH_RESULTS);
    Ok(UserSearch { users, truncated })
}

/// A refusal from the homeserver is `rejected`; anything else, such as no
/// answer at all, is `unavailable`.
fn request_error(error: &matrix_sdk::Error) -> ErrorCode {
    if error.client_api_error_kind().is_some() {
        ErrorCode::Rejected
    } else {
        ErrorCode::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN user IDs from the homeserver WHEN sent as identifiers THEN a
    /// valid ID is sent unchanged however long, up to the Matrix limit, and an
    /// ID that is not visible ASCII is not sent.
    #[test]
    fn given_user_ids_then_identifiers_are_unchanged_or_left_out() {
        let long = format!("@{}:example.org", "b".repeat(200));
        let long_id = UserId::parse(&long).unwrap();
        assert_eq!(identifier(&long_id).as_deref(), Some(long.as_str()));
        let bob = UserId::parse("@bob:example.org").unwrap();
        assert_eq!(identifier(&bob).as_deref(), Some("@bob:example.org"));
        // Historical IDs may hold characters a current ID may not.
        for odd in ["@b\u{e9}b:example.org", "@b\u{202e}b:example.org"] {
            if let Ok(id) = UserId::parse(odd) {
                assert_eq!(identifier(&id), None, "{odd:?}");
            }
        }
    }

    /// GIVEN user IDs from the page WHEN parsed THEN only valid Matrix user
    /// IDs within the limit pass.
    #[test]
    fn given_user_ids_then_only_valid_ones_within_the_limit_pass() {
        assert!(parse_user("@bob:example.org").is_some());
        let long = format!("@{}:example.org", "b".repeat(MAX_USER_ID_BYTES));
        for invalid in [
            "",
            "bob",
            "bob:example.org",
            "@bob",
            "!room:example.org",
            long.as_str(),
        ] {
            assert!(parse_user(invalid).is_none(), "{invalid}");
        }
    }

    /// GIVEN search terms WHEN checked THEN blank, oversized or
    /// control-bearing terms are refused.
    #[test]
    fn given_search_terms_then_blank_long_or_control_terms_are_refused() {
        assert!(valid_term("bob"));
        assert!(valid_term(&"b".repeat(MAX_SEARCH_TERM_BYTES)));
        for invalid in [
            String::new(),
            "   ".into(),
            "b".repeat(MAX_SEARCH_TERM_BYTES + 1),
            "bo\nb".into(),
            "b\u{7}".into(),
        ] {
            assert!(!valid_term(&invalid), "{invalid:?}");
        }
    }

    /// GIVEN a busy slot WHEN a second operation begins THEN it is refused,
    /// and allowed again once the first ends.
    #[test]
    fn given_a_busy_slot_then_a_second_operation_waits_for_the_first() {
        let busy = Busy::default();
        let first = busy.begin().unwrap();
        assert_eq!(busy.begin().err(), Some(ErrorCode::LimitExceeded));
        drop(first);
        assert!(busy.begin().is_ok());
    }
}
