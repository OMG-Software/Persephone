//! Conversations: starting a direct-message one with a user, creating a named
//! room and inviting people to it, answering an invitation, and the
//! homeserver's user directory.
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
//! * A created room is encrypted from its first state event and invite-only
//!   (the private chat preset). Everyone invited, to a new room or to an
//!   encrypted joined one, must have a cross-signing identity, as above.
//! * Every pending invitation is listed and may be answered: direct or not,
//!   from this homeserver or over federation. The inviter's name and the
//!   room's name are the homeserver's word, projected like any name. A
//!   direct invitation is joined as a direct room, which the SDK marks so.
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
    ruma::{
        OwnedUserId, RoomId, UserId,
        api::client::room::create_room,
        events::{InitialStateEvent, room::encryption::RoomEncryptionEventContent},
    },
};
use persephone_contracts::{
    ConversationStart, InviteList, InviteSummary, RoomCreation, RoomInvite, UserMatch, UserSearch,
};
use persephone_core_protocol::{
    ErrorCode, MAX_INVITEES, MAX_ROOM_NAME_BYTES, MAX_SEARCH_RESULTS, MAX_SEARCH_TERM_BYTES,
    MAX_USER_ID_BYTES,
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

/// Pending invitations, in room ID order, at most [`MAX_ROOMS`].
pub async fn invite_list(client: &Client) -> InviteList {
    let mut rooms = client.invited_rooms();
    rooms.sort_by(|a, b| a.room_id().cmp(b.room_id()));
    let truncated = rooms.len() > MAX_ROOMS;
    let mut invites = Vec::new();
    for room in rooms.into_iter().take(MAX_ROOMS) {
        if let Some(invite) = invite(&room).await {
            invites.push(invite);
        }
    }
    InviteList { invites, truncated }
}

/// The invitation of an invited room, or `None` for another room. An
/// invitation whose details cannot be read is still listed: the stripped
/// state a remote homeserver sends may lack the inviter's member event, and
/// the room ID alone is enough to answer it.
async fn invite(room: &Room) -> Option<InviteSummary> {
    if room.state() != RoomState::Invited {
        return None;
    }
    let details = room.invite_details().await.ok();
    Some(InviteSummary {
        room_id: room.room_id().to_string(),
        room_name: room.name().map(|name| project_name(&name)),
        direct: room.is_direct().await.unwrap_or(false),
        inviter: details.as_ref().map(|details| {
            identifier(&details.inviter_id)
                .unwrap_or_else(|| project_name(details.inviter_id.as_str()))
        }),
        inviter_name: details
            .and_then(|details| details.inviter)
            .and_then(|inviter| inviter.display_name().map(project_name)),
    })
}

/// The invited room `room_id`, or `not_found`.
fn invited_room(client: &Client, room_id: &str) -> Result<Room, ErrorCode> {
    RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .filter(|room| room.state() == RoomState::Invited)
        .ok_or(ErrorCode::NotFound)
}

/// Joins the room of an invitation.
pub async fn accept(client: &Client, room_id: &str) -> Result<(), ErrorCode> {
    let room = invited_room(client, room_id)?;
    room.join().await.map_err(|error| request_error(&error))
}

/// Refuses an invitation by leaving its room.
pub async fn decline(client: &Client, room_id: &str) -> Result<(), ErrorCode> {
    let room = invited_room(client, room_id)?;
    room.leave().await.map_err(|error| request_error(&error))
}

/// A room name the page sent, if it is not blank, within the limit and free
/// of control characters.
pub fn valid_room_name(name: &str) -> bool {
    name.len() <= MAX_ROOM_NAME_BYTES
        && !name.trim().is_empty()
        && !name.chars().any(char::is_control)
}

/// The people a `create_room` call names, if each is a valid user ID, there
/// are at most [`MAX_INVITEES`] and none is named twice.
pub fn parse_invitees(invite: &[String]) -> Option<Vec<OwnedUserId>> {
    if invite.len() > MAX_INVITEES {
        return None;
    }
    let mut users: Vec<OwnedUserId> = Vec::with_capacity(invite.len());
    for user_id in invite {
        let user = parse_user(user_id)?;
        if users.contains(&user) {
            return None;
        }
        users.push(user);
    }
    Some(users)
}

/// Whether `user` has a cross-signing identity. The store may not follow
/// them yet, so the homeserver is asked before saying no.
async fn has_identity(client: &Client, user: &UserId) -> Result<bool, ErrorCode> {
    let encryption = client.encryption();
    match encryption.get_user_identity(user).await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => encryption
            .request_user_identity(user)
            .await
            .map(|identity| identity.is_some())
            .map_err(|_| ErrorCode::Unavailable),
        Err(_) => Err(ErrorCode::Unavailable),
    }
}

/// Creates an encrypted, invite-only room named `name` and invites `invite`
/// to it. Everyone invited must have a cross-signing identity, because room
/// keys go only to cross-signed devices (ADR-0005); otherwise nothing is
/// created. Inviting this account itself is `invalid_argument`.
pub async fn create_room(
    client: &Client,
    name: &str,
    invite: Vec<OwnedUserId>,
) -> Result<RoomCreation, ErrorCode> {
    if invite.iter().any(|user| client.user_id() == Some(user)) {
        return Err(ErrorCode::InvalidArgument);
    }
    for user in &invite {
        if !has_identity(client, user).await? {
            return Ok(RoomCreation::NoIdentity {
                user_id: identifier(user).unwrap_or_else(|| project_name(user.as_str())),
            });
        }
    }
    let mut request = create_room::v3::Request::new();
    request.name = Some(name.to_owned());
    request.invite = invite;
    // Invite-only, history visible to members; invitees are not made
    // administrators, as the trusted preset would.
    request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
    request.initial_state = vec![
        InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        )
        .to_raw_any(),
    ];
    match client.create_room(request).await {
        Ok(room) => Ok(RoomCreation::Created {
            room_id: room.room_id().to_string(),
        }),
        Err(error) if error.client_api_error_kind().is_some() => Ok(RoomCreation::Refused {}),
        Err(_) => Err(ErrorCode::Unavailable),
    }
}

/// Invites `user` to the joined room `room_id`. In an encrypted room (or one
/// whose state is not known) they must have a cross-signing identity.
pub async fn invite_user(
    client: &Client,
    room_id: &str,
    user: &UserId,
) -> Result<RoomInvite, ErrorCode> {
    let room = RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)?;
    if client.user_id() == Some(user) {
        return Err(ErrorCode::InvalidArgument);
    }
    let plain = matches!(
        room.latest_encryption_state().await,
        Ok(EncryptionState::NotEncrypted)
    );
    if !plain && !has_identity(client, user).await? {
        return Ok(RoomInvite::NoIdentity {});
    }
    match room.invite_user_by_id(user).await {
        Ok(()) => Ok(RoomInvite::Invited {}),
        Err(error) if error.client_api_error_kind().is_some() => Ok(RoomInvite::Refused {}),
        Err(_) => Err(ErrorCode::Unavailable),
    }
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
    if !has_identity(client, user).await? {
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
