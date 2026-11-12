//! The other person in a direct-message room, and how far this account trusts
//! them.
//!
//! * A room counts as a direct-message room with a contact when the account
//!   data marks it as direct with exactly one other user, and the room has
//!   exactly two members (joined or invited): this account and that user.
//!   The direct marks can outlive membership changes, so a room someone else
//!   joined is no longer a contact's room. The marks are the account's own,
//!   but the room's members are the homeserver's word.
//! * Verifying a contact needs them to be joined to the direct-message room
//!   the SDK sends the request to. The SDK picks that room itself: the first
//!   joined room marked direct for them, in the order its store holds rooms
//!   (`Client::get_dm_room`). Were the contact not in it, the SDK would invite
//!   them, and with no such room it would create one. The core checks first
//!   and refuses instead, so a request never invites anyone or creates a
//!   room. A membership change between the check and the SDK's own check is
//!   still possible; the SDK then invites the contact back into that room.
//! * An older room can keep the contact's direct mark after it stopped being
//!   a room with just the two of them (they left, their invitation is still
//!   pending, or someone else joined), and the SDK would pick it before the
//!   room the user is in. When a room with just the two of them, both joined,
//!   exists, the core removes the contact's mark from each such stale room
//!   the SDK would pick first (at most [`MAX_STALE_ROOMS`]), and waits for
//!   sync to bring the change back (at most [`ECHO_WAIT`]). Only the
//!   contact's entry changes; the rooms and their members do not. A stale
//!   room is one whose member list was read and shows it is not just the two
//!   of them; a list that cannot be read changes nothing and is `failed`.
//! * Trust is read from the local crypto store. Sync keeps it current only
//!   for users the SDK follows, so `status` makes the SDK follow the contact
//!   first (see there).

use crate::{identities, views::project_name};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    ruma::{
        OwnedDirectUserIdentifier, OwnedRoomId, OwnedUserId, RoomId, UserId,
        events::{direct::DirectEventContent, room::member::MembershipState},
    },
};
use persephone_contracts::{ContactStatus, ContactTrust, SecurityError};
use std::time::Duration;

/// At most this many stale rooms lose a contact's direct mark for one
/// verification; with more, the core refuses (`not_direct_room`).
pub const MAX_STALE_ROOMS: usize = 8;

/// How long the core waits for sync to bring back a changed direct mark.
pub const ECHO_WAIT: Duration = Duration::from_secs(10);

/// How often the core looks for the changed mark while it waits.
const ECHO_STEP: Duration = Duration::from_millis(100);

/// The one other user a direct-message room is marked for, while the room
/// has exactly two members, or `None`. Uses the member count from sync.
pub fn contact_of(client: &Client, room: &Room) -> Option<OwnedUserId> {
    let targets = room.direct_targets();
    if targets.len() != 1 || room.active_members_count() != 2 {
        return None;
    }
    let contact = targets.iter().next()?.as_user_id()?.to_owned();
    (client.user_id() != Some(contact.as_ref())).then_some(contact)
}

/// The contact's user ID as the page sees it.
pub fn shown(contact: &UserId) -> String {
    project_name(contact.as_str())
}

/// Whether the room's members (joined or invited) are exactly this account
/// and `contact`, both joined, as its member list says. `None` when the list
/// cannot be read, or there is no own user ID.
async fn just_us(client: &Client, room: &Room, contact: &UserId) -> Option<bool> {
    let own = client.user_id()?;
    let members = room.members(RoomMemberships::ACTIVE).await.ok()?;
    Some(
        members.len() == 2
            && members.iter().all(|member| {
                (member.user_id() == own || member.user_id() == contact)
                    && *member.membership() == MembershipState::Join
            }),
    )
}

/// Makes the room the SDK would send a verification request for `contact`
/// to (`get_dm_room`) one with just this account and them, both joined, by
/// removing their direct mark from the stale rooms it would pick first (see
/// the module notes). `not_direct_room` when no such room exists or too many
/// stale rooms come first; `failed` when a member list cannot be read, the
/// change cannot be saved, or sync does not bring it back in time.
async fn repair_target(client: &Client, contact: &UserId) -> Result<(), SecurityError> {
    let mut stale: Vec<OwnedRoomId> = Vec::new();
    let mut found = false;
    for room in client.get_dm_rooms(contact) {
        match just_us(client, &room, contact).await {
            Some(true) => {
                found = true;
                break;
            }
            Some(false) if stale.len() < MAX_STALE_ROOMS => stale.push(room.room_id().to_owned()),
            Some(false) => return Err(SecurityError::NotDirectRoom),
            None => return Err(SecurityError::Failed),
        }
    }
    if !found {
        return Err(SecurityError::NotDirectRoom);
    }
    if stale.is_empty() {
        return Ok(());
    }
    unmark(client, contact, &stale).await?;
    let picked_valid = || {
        client
            .get_dm_room(contact)
            .is_some_and(|room| !stale.iter().any(|id| id == room.room_id()))
    };
    let deadline = tokio::time::Instant::now() + ECHO_WAIT;
    while !picked_valid() {
        if tokio::time::Instant::now() >= deadline {
            return Err(SecurityError::Failed);
        }
        tokio::time::sleep(ECHO_STEP).await;
    }
    Ok(())
}

/// Removes `rooms` from the account's direct marks for `contact` only, from
/// the account data the client last synced.
async fn unmark(
    client: &Client,
    contact: &UserId,
    rooms: &[OwnedRoomId],
) -> Result<(), SecurityError> {
    let account = client.account();
    let mut content = account
        .account_data::<DirectEventContent>()
        .await
        .map_err(|_| SecurityError::Failed)?
        .ok_or(SecurityError::Failed)?
        .deserialize()
        .map_err(|_| SecurityError::Failed)?;
    let key = OwnedDirectUserIdentifier::from(contact.to_owned());
    let Some(marked) = content.get_mut(&key) else {
        return Err(SecurityError::Failed);
    };
    marked.retain(|room| !rooms.contains(room));
    if marked.is_empty() {
        content.remove(&key);
    }
    account
        .set_account_data(content)
        .await
        .map(|_| ())
        .map_err(|_| SecurityError::Failed)
}

/// The room the SDK will send a verification request for `contact` to, after
/// [`repair_target`], checked once more just before the request.
async fn checked_target(client: &Client, contact: &UserId) -> Result<(), SecurityError> {
    repair_target(client, contact).await?;
    let target = client
        .get_dm_room(contact)
        .ok_or(SecurityError::NotDirectRoom)?;
    match just_us(client, &target, contact).await {
        Some(true) => Ok(()),
        Some(false) => Err(SecurityError::NotDirectRoom),
        None => Err(SecurityError::Failed),
    }
}

/// The contact of a joined direct-message room, if they are still joined to
/// it and to the room the SDK would send a verification request to. Stale
/// rooms the SDK would pick first lose the contact's direct mark (see the
/// module notes).
pub async fn verifiable_contact(
    client: &Client,
    room_id: &str,
) -> Result<OwnedUserId, SecurityError> {
    let room = RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(SecurityError::NotDirectRoom)?;
    let contact = contact_of(client, &room).ok_or(SecurityError::NotDirectRoom)?;
    match just_us(client, &room, &contact).await {
        Some(true) => {}
        Some(false) => return Err(SecurityError::NotDirectRoom),
        None => return Err(SecurityError::Failed),
    }
    checked_target(client, &contact).await?;
    Ok(contact)
}

/// Checks that `member` may be verified from this joined room: they are a
/// joined or invited member of it and not this account, and the room the SDK
/// would send a verification request to (`get_dm_room`) has exactly two
/// members, this account and them, both joined. Stale rooms the SDK would
/// pick first lose the member's direct mark (see the module notes). `failed`
/// when they are not a member here; `not_direct_room` when there is no such
/// direct-message room, so the page can offer to start one. Nothing is
/// created or joined.
pub async fn verifiable_member(
    client: &Client,
    room_id: &str,
    member: &UserId,
) -> Result<(), SecurityError> {
    let room = RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(SecurityError::Failed)?;
    if client.user_id().is_none_or(|own| own == member) {
        return Err(SecurityError::Failed);
    }
    let listed = room
        .get_member(member)
        .await
        .map_err(|_| SecurityError::Failed)?
        .ok_or(SecurityError::Failed)?;
    if !matches!(
        listed.membership(),
        MembershipState::Join | MembershipState::Invite
    ) {
        return Err(SecurityError::Failed);
    }
    checked_target(client, member).await
}

/// How far this account trusts `contact`, as the local crypto store says.
/// `None` when the store has no identity for them.
pub async fn trust(client: &Client, contact: &UserId) -> Option<ContactTrust> {
    match client.encryption().get_user_identity(contact).await {
        Ok(Some(identity)) if identity.is_verified() => Some(ContactTrust::Verified),
        Ok(Some(_)) => Some(ContactTrust::Unverified),
        Ok(None) => None,
        Err(_) => Some(ContactTrust::Unknown),
    }
}

/// The contact of a joined direct-message room, or `None` for any other
/// room.
///
/// Sync loads members lazily, so the SDK may not yet follow the contact's
/// keys. When the local store has no identity for them, the room's members
/// are loaded (which makes the SDK follow every member's keys from then on)
/// and the homeserver is asked once for the contact's keys. Both are bounded
/// by the client's request timeout; a failure is `unknown`, never
/// `no_identity`.
pub async fn status(client: &Client, room: &Room) -> Option<ContactStatus> {
    let contact = contact_of(client, room)?;
    let trust = match trust(client, &contact).await {
        Some(trust) => trust,
        None => {
            let _ = room.sync_members().await;
            match client.encryption().request_user_identity(&contact).await {
                Ok(Some(identity)) if identity.is_verified() => ContactTrust::Verified,
                Ok(Some(_)) => ContactTrust::Unverified,
                Ok(None) => ContactTrust::NoIdentity,
                Err(_) => ContactTrust::Unknown,
            }
        }
    };
    // An identity that changed reads as unverified until the room says so
    // (ADR-0007); a failure to tell reads `unknown`.
    let trust = if trust == ContactTrust::Unverified {
        match identities::changed_members(room).await {
            Some(changed) => match changed.iter().find(|change| change.user_id == contact) {
                Some(change) if change.was_verified => ContactTrust::VerifiedChanged,
                Some(_) => ContactTrust::Changed,
                None => trust,
            },
            None => ContactTrust::Unknown,
        }
    } else {
        trust
    };
    Some(ContactStatus {
        room_id: room.room_id().to_string(),
        user_id: shown(&contact),
        trust,
    })
}
