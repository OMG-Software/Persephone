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
//!   the SDK sends the request to. The SDK picks that room itself
//!   (`Client::get_dm_room`); were the contact not in it, the SDK would invite
//!   them, and with no such room it would create one. The core checks both
//!   first and refuses instead, so a request never invites anyone or creates a
//!   room. A membership change between the check and the SDK's own check is
//!   still possible; the SDK then invites the contact back into that room.
//! * Trust is read from the local crypto store. Sync keeps it current only
//!   for users the SDK follows, so `status` makes the SDK follow the contact
//!   first (see there).

use crate::{identities, views::project_name};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    ruma::{OwnedUserId, RoomId, UserId, events::room::member::MembershipState},
};
use persephone_contracts::{ContactStatus, ContactTrust, SecurityError};

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
/// and `contact`, both joined, as its member list says.
async fn just_us(client: &Client, room: &Room, contact: &UserId) -> bool {
    let Some(own) = client.user_id() else {
        return false;
    };
    room.members(RoomMemberships::ACTIVE)
        .await
        .is_ok_and(|members| {
            members.len() == 2
                && members.iter().all(|member| {
                    (member.user_id() == own || member.user_id() == contact)
                        && *member.membership() == MembershipState::Join
                })
        })
}

/// The contact of a joined direct-message room, if they are still joined to
/// it and to the room the SDK would send a verification request to.
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
    let target = client
        .get_dm_room(&contact)
        .ok_or(SecurityError::NotDirectRoom)?;
    if !just_us(client, &room, &contact).await || !just_us(client, &target, &contact).await {
        return Err(SecurityError::NotDirectRoom);
    }
    Ok(contact)
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
