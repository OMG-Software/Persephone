//! A joined room's settings and members, and changes to them (ADR-0017).
//!
//! * Settings come from the room's state as this device last synced it,
//!   except the directory listing and the local addresses, which are asked of
//!   the homeserver; when it does not answer, those two are unknown and the
//!   rest is still sent.
//! * A state event the settings, members or a level change need that this
//!   device has not synced is asked of the homeserver, one event at a time:
//!   the power levels, join rule, history setting, guest access and topic.
//!   A Sliding Sync server sends only the state the list asks for, and it
//!   need not send state that the list asked for only after the server first
//!   sent the room. The room's creation event must be synced; without it the
//!   levels cannot be read and the reply is `unavailable`.
//! * Names, the topic and the room version are projected like names (the
//!   topic like a message body). Addresses and user IDs are sent unchanged
//!   when they are valid, visible ASCII, so the page can name them back
//!   (as for inviters in `conversations`); another member ID is projected and
//!   offers no action, and another address is not sent.
//! * What this account may change comes from the power levels by the SDK's
//!   rules, including the unlimited level of room creators in newer room
//!   versions. It is a hint for the page: the homeserver decides.
//! * Each change is one homeserver request, after the page's arguments are
//!   validated. A level change reads the power levels as above and
//!   replaces only the named entry, so entries this client does not show are
//!   kept. Published addresses this client could not send to the page are
//!   kept when the page sets the others.
//! * Encryption can only be turned on. Only the invite, knock and public join
//!   rules can be set; a creator's unlimited level cannot be changed; and this
//!   account cannot kick or ban itself (it leaves instead).
//! * Removing an address first takes it out of the published addresses, then
//!   deletes it from the directory, and only if it leads to this room.
//! * A change is not announced here: the device's copy of the room's state
//!   changes with the next sync, which reports the room and the room list as
//!   changed. Announcing it sooner would make the page read the old state.
//! * A refusal from the homeserver is `rejected`; no answer is `unavailable`.
//!   Nothing is retried.

use crate::views::{MAX_PAGE_BYTES, project_body, project_name};
use matrix_sdk::{
    Client, EncryptionState, Room, RoomMemberships, RoomState,
    notification_settings::RoomNotificationMode,
    ruma::{
        Int, OwnedRoomAliasId, OwnedUserId, RoomAliasId, RoomId, ServerName, UserId,
        api::{
            client::{
                room::{Visibility, aliases},
                state::get_state_event_for_key,
            },
            error::ErrorKind,
        },
        events::{
            AnyStateEventContent, StateEventType, TimelineEventType,
            room::{
                guest_access::{GuestAccess, RoomGuestAccessEventContent},
                history_visibility::{
                    HistoryVisibility as Visible, RoomHistoryVisibilityEventContent,
                },
                join_rules::{JoinRule as Rule, RoomJoinRulesEventContent},
                member::MembershipState,
                power_levels::{
                    RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
                    UserPowerLevel,
                },
                topic::RoomTopicEventContent,
            },
        },
        room_version_rules::AuthorizationRules,
        serde::Raw,
    },
};
use persephone_contracts::{
    ContactTrust, HistoryVisibility, JoinRule, LevelKey, Membership, PowerLevel, RoomChange,
    RoomEncryption, RoomLevel, RoomMember, RoomMembers, RoomNotifications, RoomPermissions,
    RoomReader, RoomReaders, RoomServer, RoomServers, RoomSettings,
};
use persephone_core_protocol::{
    ErrorCode, MAX_ALIAS_BYTES, MAX_ALT_ALIASES, MAX_LISTED_MEMBERS, MAX_MEMBER_FILTER_BYTES,
    MAX_ROOM_NAME_BYTES, MAX_TOPIC_BYTES, MAX_USER_ID_BYTES,
};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;

/// A change the page asked for, after validation.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// Trimmed; empty removes the name.
    Name(String),
    /// Empty removes the topic.
    Topic(String),
    JoinRule(Rule),
    History(Visible),
    GuestAccess(bool),
    Encryption,
    Published(bool),
    AddAlias(OwnedRoomAliasId),
    RemoveAlias(OwnedRoomAliasId),
    CanonicalAlias(Option<OwnedRoomAliasId>, Vec<OwnedRoomAliasId>),
    Level(LevelKey, Int),
    MemberLevel(OwnedUserId, Int),
    Kick(OwnedUserId),
    Ban(OwnedUserId),
    Unban(OwnedUserId),
    Notifications(RoomNotifications),
    Leave,
}

impl Change {
    /// The change the page sent, if every argument is valid.
    pub fn parse(change: &RoomChange) -> Option<Self> {
        Some(match change {
            RoomChange::Name { name } => (name.len() <= MAX_ROOM_NAME_BYTES
                && !name.chars().any(char::is_control))
            .then(|| Self::Name(name.trim().to_owned()))?,
            RoomChange::Topic { topic } => (topic.len() <= MAX_TOPIC_BYTES
                && !topic
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'))
            .then(|| Self::Topic(topic.trim().to_owned()))?,
            RoomChange::JoinRule { rule } => Self::JoinRule(match rule {
                JoinRule::Invite => Rule::Invite,
                JoinRule::Knock => Rule::Knock,
                JoinRule::Public => Rule::Public,
                _ => return None,
            }),
            RoomChange::HistoryVisibility { visibility } => Self::History(match visibility {
                HistoryVisibility::Invited => Visible::Invited,
                HistoryVisibility::Joined => Visible::Joined,
                HistoryVisibility::Shared => Visible::Shared,
                HistoryVisibility::WorldReadable => Visible::WorldReadable,
                HistoryVisibility::Other => return None,
            }),
            RoomChange::GuestAccess { allowed } => Self::GuestAccess(*allowed),
            RoomChange::Encryption {} => Self::Encryption,
            RoomChange::Published { published } => Self::Published(*published),
            RoomChange::AddAlias { alias } => Self::AddAlias(parse_alias(alias)?),
            RoomChange::RemoveAlias { alias } => Self::RemoveAlias(parse_alias(alias)?),
            RoomChange::CanonicalAlias { alias, alt_aliases } => {
                if alt_aliases.len() > MAX_ALT_ALIASES {
                    return None;
                }
                let main = match alias {
                    Some(alias) => Some(parse_alias(alias)?),
                    None => None,
                };
                let mut alt: Vec<OwnedRoomAliasId> = Vec::with_capacity(alt_aliases.len());
                for alias in alt_aliases {
                    let alias = parse_alias(alias)?;
                    if alt.contains(&alias) {
                        return None;
                    }
                    alt.push(alias);
                }
                Self::CanonicalAlias(main, alt)
            }
            RoomChange::Level { key, level } => Self::Level(*key, int(*level)?),
            RoomChange::MemberLevel { user_id, level } => {
                Self::MemberLevel(parse_user(user_id)?, int(*level)?)
            }
            RoomChange::Kick { user_id } => Self::Kick(parse_user(user_id)?),
            RoomChange::Ban { user_id } => Self::Ban(parse_user(user_id)?),
            RoomChange::Unban { user_id } => Self::Unban(parse_user(user_id)?),
            RoomChange::Notifications { mode } => Self::Notifications(*mode),
            RoomChange::Leave {} => Self::Leave,
        })
    }
}

fn int(level: PowerLevel) -> Option<Int> {
    Int::new(level.get())
}

fn parse_user(user_id: &str) -> Option<OwnedUserId> {
    (user_id.len() <= MAX_USER_ID_BYTES)
        .then(|| UserId::parse(user_id).ok())
        .flatten()
}

/// A room address the page sent: valid, within the limit and visible ASCII,
/// as the addresses the core sends are.
fn parse_alias(alias: &str) -> Option<OwnedRoomAliasId> {
    (alias.len() <= MAX_ALIAS_BYTES && alias.chars().all(|c| c.is_ascii_graphic()))
        .then(|| RoomAliasId::parse(alias).ok())
        .flatten()
}

/// A member filter the page sent: within the limit and free of controls.
pub fn valid_filter(filter: &str) -> bool {
    filter.len() <= MAX_MEMBER_FILTER_BYTES && !filter.chars().any(char::is_control)
}

/// An ID or address to send unchanged: within the Matrix limit and visible
/// ASCII; otherwise none.
fn identifier(id: &str) -> Option<String> {
    (id.len() <= MAX_USER_ID_BYTES && id.chars().all(|c| c.is_ascii_graphic()))
        .then(|| id.to_owned())
}

/// Addresses split into those the page is shown (at most
/// [`MAX_ALT_ALIASES`]) and the rest, which a change keeps.
fn shown_aliases(aliases: &[OwnedRoomAliasId]) -> (Vec<OwnedRoomAliasId>, Vec<OwnedRoomAliasId>) {
    let (mut shown, mut hidden): (Vec<_>, Vec<_>) = aliases
        .iter()
        .cloned()
        .partition(|alias| identifier(alias.as_str()).is_some());
    if shown.len() > MAX_ALT_ALIASES {
        hidden.extend(shown.split_off(MAX_ALT_ALIASES));
    }
    (shown, hidden)
}

/// The joined room `room_id`, or `not_found`.
fn joined(client: &Client, room_id: &str) -> Result<Room, ErrorCode> {
    RoomId::parse(room_id)
        .ok()
        .and_then(|id| client.get_room(&id))
        .filter(|room| room.state() == RoomState::Joined)
        .ok_or(ErrorCode::NotFound)
}

fn own_user(client: &Client) -> Result<OwnedUserId, ErrorCode> {
    client
        .user_id()
        .map(ToOwned::to_owned)
        .ok_or(ErrorCode::NotFound)
}

/// The room's `event_type` state event content, asked of the homeserver
/// (`GET /rooms/{roomId}/state/{eventType}/`); `None` when the room has none
/// (`M_NOT_FOUND`). Another refusal is `rejected`; no answer is
/// `unavailable`.
async fn fetch_state(
    client: &Client,
    room: &Room,
    event_type: StateEventType,
) -> Result<Option<Raw<AnyStateEventContent>>, ErrorCode> {
    let request = get_state_event_for_key::v3::Request::new(
        room.room_id().to_owned(),
        event_type,
        String::new(),
    );
    match client.send(request).await {
        Ok(response) => Ok(Some(response.into_content())),
        Err(error) if matches!(error.client_api_error_kind(), Some(ErrorKind::NotFound)) => {
            Ok(None)
        }
        Err(error) if error.client_api_error_kind().is_some() => Err(ErrorCode::Rejected),
        Err(_) => Err(ErrorCode::Unavailable),
    }
}

/// The content of a state event this device has not synced, asked of the
/// homeserver; `None` when it is synced (read it from the room), the room
/// has none, or its content is not valid for its type. The room's own
/// accessors give the specification's default in the last two cases too.
async fn unsynced<C: DeserializeOwned>(
    client: &Client,
    room: &Room,
    event_type: StateEventType,
) -> Result<Option<C>, ErrorCode> {
    if matches!(
        room.get_state_event(event_type.clone(), "").await,
        Ok(Some(_))
    ) {
        return Ok(None);
    }
    Ok(fetch_state(client, room, event_type)
        .await?
        .and_then(|content| content.deserialize_as_unchecked().ok()))
}

/// The room's power levels as last synced or, when this device has not
/// synced them, as the homeserver has them; a room without any has the
/// specification's defaults. The room's creators must be synced, and
/// content that is not valid power levels is `unavailable`.
async fn power_levels(client: &Client, room: &Room) -> Result<RoomPowerLevels, ErrorCode> {
    if let Ok(levels) = room.power_levels().await {
        return Ok(levels);
    }
    let creators = room.creators().ok_or(ErrorCode::Unavailable)?;
    let rules = room.clone_info().room_version_rules_or_default();
    let content = fetch_state(client, room, StateEventType::RoomPowerLevels).await?;
    fetched_levels(content, &rules.authorization, creators)
}

/// The power levels the homeserver sent (`None`: it has none) for a room
/// with these `rules` and `creators`. Content that is not valid power
/// levels for the room version, such as a creator listed in `users` in
/// room version 12, is `unavailable`: a change could not send it back.
fn fetched_levels(
    content: Option<Raw<AnyStateEventContent>>,
    rules: &AuthorizationRules,
    creators: Vec<OwnedUserId>,
) -> Result<RoomPowerLevels, ErrorCode> {
    let source = match content {
        Some(content) => RoomPowerLevelsSource::Original(
            content
                .deserialize_as_unchecked()
                .map_err(|_| ErrorCode::Unavailable)?,
        ),
        None => RoomPowerLevelsSource::None,
    };
    let levels = RoomPowerLevels::new(source, rules, creators);
    RoomPowerLevelsEventContent::try_from(levels.clone()).map_err(|_| ErrorCode::Unavailable)?;
    Ok(levels)
}

/// Sends `levels` as the room's new power levels.
async fn send_levels(room: &Room, levels: RoomPowerLevels) -> Result<(), ErrorCode> {
    let content = RoomPowerLevelsEventContent::try_from(levels).map_err(|_| ErrorCode::Internal)?;
    room.send_state_event(content)
        .await
        .map(drop)
        .map_err(|error| request_error(&error))
}

/// The state event a level key names, for the keys kept in `events`.
fn state_type(key: LevelKey) -> Option<StateEventType> {
    Some(match key {
        LevelKey::Name => StateEventType::RoomName,
        LevelKey::Topic => StateEventType::RoomTopic,
        LevelKey::Avatar => StateEventType::RoomAvatar,
        LevelKey::CanonicalAlias => StateEventType::RoomCanonicalAlias,
        LevelKey::JoinRules => StateEventType::RoomJoinRules,
        LevelKey::HistoryVisibility => StateEventType::RoomHistoryVisibility,
        LevelKey::GuestAccess => StateEventType::RoomGuestAccess,
        LevelKey::Encryption => StateEventType::RoomEncryption,
        LevelKey::PowerLevels => StateEventType::RoomPowerLevels,
        LevelKey::ServerAcl => StateEventType::RoomServerAcl,
        LevelKey::Tombstone => StateEventType::RoomTombstone,
        LevelKey::PinnedEvents => StateEventType::RoomPinnedEvents,
        LevelKey::UsersDefault
        | LevelKey::EventsDefault
        | LevelKey::StateDefault
        | LevelKey::Invite
        | LevelKey::Kick
        | LevelKey::Ban
        | LevelKey::Redact
        | LevelKey::NotifyRoom => return None,
    })
}

/// The level in effect for `key`: an event without its own entry needs the
/// state default.
fn level_of(levels: &RoomPowerLevels, key: LevelKey) -> Int {
    match key {
        LevelKey::UsersDefault => levels.users_default,
        LevelKey::EventsDefault => levels.events_default,
        LevelKey::StateDefault => levels.state_default,
        LevelKey::Invite => levels.invite,
        LevelKey::Kick => levels.kick,
        LevelKey::Ban => levels.ban,
        LevelKey::Redact => levels.redact,
        LevelKey::NotifyRoom => levels.notifications.room,
        other => state_type(other)
            .and_then(|state| levels.events.get(&TimelineEventType::from(state)).copied())
            .unwrap_or(levels.state_default),
    }
}

/// Sets the level for `key`, leaving every other entry as it was.
fn set_level(levels: &mut RoomPowerLevels, key: LevelKey, level: Int) {
    match key {
        LevelKey::UsersDefault => levels.users_default = level,
        LevelKey::EventsDefault => levels.events_default = level,
        LevelKey::StateDefault => levels.state_default = level,
        LevelKey::Invite => levels.invite = level,
        LevelKey::Kick => levels.kick = level,
        LevelKey::Ban => levels.ban = level,
        LevelKey::Redact => levels.redact = level,
        LevelKey::NotifyRoom => levels.notifications.room = level,
        other => {
            if let Some(state) = state_type(other) {
                levels.events.insert(state.into(), level);
            }
        }
    }
}

fn shown_level(level: UserPowerLevel) -> Option<PowerLevel> {
    match level {
        UserPowerLevel::Infinite => None,
        UserPowerLevel::Int(level) => PowerLevel::new(level.into()),
        // A level this SDK version does not know is shown like a creator's.
        _ => None,
    }
}

fn levels_list(levels: &RoomPowerLevels) -> Vec<RoomLevel> {
    LevelKey::ALL
        .iter()
        .filter_map(|&key| {
            Some(RoomLevel {
                key,
                level: PowerLevel::new(level_of(levels, key).into())?,
            })
        })
        .collect()
}

async fn encryption(room: &Room) -> RoomEncryption {
    let state = match room.encryption_state() {
        EncryptionState::Unknown => room
            .latest_encryption_state()
            .await
            .unwrap_or(EncryptionState::Unknown),
        known => known,
    };
    match state {
        EncryptionState::Encrypted => RoomEncryption::Encrypted,
        EncryptionState::NotEncrypted => RoomEncryption::NotEncrypted,
        EncryptionState::Unknown => RoomEncryption::Unknown,
    }
}

pub(crate) fn join_rule(rule: Option<Rule>) -> JoinRule {
    match rule {
        // Without a join rule only an invitation lets anyone in.
        None | Some(Rule::Invite) => JoinRule::Invite,
        Some(Rule::Knock) => JoinRule::Knock,
        Some(Rule::Public) => JoinRule::Public,
        Some(Rule::Restricted(_)) => JoinRule::Restricted,
        Some(Rule::KnockRestricted(_)) => JoinRule::KnockRestricted,
        Some(Rule::Private) => JoinRule::Private,
        Some(_) => JoinRule::Other,
    }
}

pub(crate) fn history_visibility(visibility: Option<Visible>) -> HistoryVisibility {
    match visibility {
        // The specification's default.
        None | Some(Visible::Shared) => HistoryVisibility::Shared,
        Some(Visible::Invited) => HistoryVisibility::Invited,
        Some(Visible::Joined) => HistoryVisibility::Joined,
        Some(Visible::WorldReadable) => HistoryVisibility::WorldReadable,
        Some(_) => HistoryVisibility::Other,
    }
}

fn notifications(mode: Option<RoomNotificationMode>) -> RoomNotifications {
    match mode {
        None => RoomNotifications::Default,
        Some(RoomNotificationMode::AllMessages) => RoomNotifications::All,
        Some(RoomNotificationMode::MentionsAndKeywordsOnly) => RoomNotifications::Mentions,
        Some(RoomNotificationMode::Mute) => RoomNotifications::Mute,
    }
}

/// A joined room's settings (see the module notes).
pub async fn settings(client: &Client, room_id: &str) -> Result<RoomSettings, ErrorCode> {
    let room = joined(client, room_id)?;
    let own = own_user(client)?;
    let levels = power_levels(client, &room).await?;
    let rule =
        match unsynced::<RoomJoinRulesEventContent>(client, &room, StateEventType::RoomJoinRules)
            .await?
        {
            Some(content) => Some(content.join_rule),
            None => room.join_rule(),
        };
    let history = match unsynced::<RoomHistoryVisibilityEventContent>(
        client,
        &room,
        StateEventType::RoomHistoryVisibility,
    )
    .await?
    {
        Some(content) => Some(content.history_visibility),
        None => room.history_visibility(),
    };
    let guest_access = match unsynced::<RoomGuestAccessEventContent>(
        client,
        &room,
        StateEventType::RoomGuestAccess,
    )
    .await?
    {
        Some(content) => content.guest_access,
        None => room.guest_access(),
    };
    let topic =
        match unsynced::<RoomTopicEventContent>(client, &room, StateEventType::RoomTopic).await? {
            Some(content) => Some(content.topic),
            None => room.topic(),
        };
    let encryption = encryption(&room).await;
    let published = match room.privacy_settings().get_room_visibility().await {
        Ok(Visibility::Public) => Some(true),
        Ok(Visibility::Private) => Some(false),
        _ => None,
    };
    let local_aliases = client
        .send(aliases::v3::Request::new(room.room_id().to_owned()))
        .await
        .ok()
        .map(|response| shown_aliases(&response.aliases).0)
        .map(|aliases| aliases.iter().map(ToString::to_string).collect());
    let notifications = notifications(
        client
            .notification_settings()
            .await
            .get_user_defined_room_notification_mode(room.room_id())
            .await,
    );
    let can = |state: StateEventType| levels.user_can_send_state(&own, state);
    let may = RoomPermissions {
        name: can(StateEventType::RoomName),
        topic: can(StateEventType::RoomTopic),
        canonical_alias: can(StateEventType::RoomCanonicalAlias),
        join_rules: can(StateEventType::RoomJoinRules),
        history_visibility: can(StateEventType::RoomHistoryVisibility),
        guest_access: can(StateEventType::RoomGuestAccess),
        encryption: encryption != RoomEncryption::Encrypted && can(StateEventType::RoomEncryption),
        power_levels: can(StateEventType::RoomPowerLevels),
        invite: levels.user_can_invite(&own),
        kick: levels.user_can_kick(&own),
        ban: levels.user_can_ban(&own),
    };
    Ok(RoomSettings {
        room_id: room.room_id().to_string(),
        name: room.name().map(|name| project_name(&name)),
        topic: topic.map(|topic| project_body(&topic)),
        canonical_alias: room
            .canonical_alias()
            .and_then(|alias| identifier(alias.as_str())),
        alt_aliases: shown_aliases(&room.alt_aliases())
            .0
            .iter()
            .map(ToString::to_string)
            .collect(),
        local_aliases,
        join_rule: join_rule(rule),
        history_visibility: history_visibility(history),
        guest_access: guest_access == GuestAccess::CanJoin,
        encryption,
        published,
        room_version: room.version().map(|version| project_name(version.as_str())),
        direct: room.is_direct().await.unwrap_or(false),
        notifications,
        levels: levels_list(&levels),
        own_level: shown_level(levels.for_user(&own)),
        may,
    })
}

fn membership(state: &MembershipState) -> Option<Membership> {
    Some(match state {
        MembershipState::Join => Membership::Join,
        MembershipState::Invite => Membership::Invite,
        MembershipState::Knock => Membership::Knock,
        MembershipState::Ban => Membership::Ban,
        _ => return None,
    })
}

/// Listing order: this account, then joined, invited, asking and banned
/// members; within each, the highest level first (a creator's above all),
/// then by ID.
fn rank(a: &RoomMember, b: &RoomMember) -> std::cmp::Ordering {
    let membership = |member: &RoomMember| member.membership as u8;
    let level = |member: &RoomMember| member.level.map_or(i64::MAX, PowerLevel::get);
    b.own
        .cmp(&a.own)
        .then(membership(a).cmp(&membership(b)))
        .then(level(b).cmp(&level(a)))
        .then(a.user_id.cmp(&b.user_id))
}

/// Whether a member's ID or display name contains the filter, ignoring case.
fn matches(filter: &str, user_id: &str, display_name: Option<&str>) -> bool {
    filter.is_empty()
        || user_id.to_lowercase().contains(filter)
        || display_name.is_some_and(|name| name.to_lowercase().contains(filter))
}

/// Drops members from the end until the reply fits one bounded view.
fn fit(members: &mut RoomMembers) {
    while !members.members.is_empty()
        && serde_json::to_vec(members).map_or(true, |bytes| bytes.len() > MAX_PAGE_BYTES)
    {
        members.members.pop();
        members.truncated = true;
    }
}

/// A joined room's members that match `filter` (see the module notes).
/// Who can read a joined room's new messages: up to [`MAX_LISTED_MEMBERS`]
/// joined or invited members by user ID, each with this account's trust in
/// them and their devices split by whether they receive room keys
/// (ADR-0005), from the local crypto store.
pub async fn readers(client: &Client, room_id: &str) -> Result<RoomReaders, ErrorCode> {
    let room = joined(client, room_id)?;
    let own = own_user(client)?;
    let all = room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    // Sorted and cut by reference first: only the listed IDs are copied.
    let mut sorted: Vec<&UserId> = all.iter().map(|member| member.user_id()).collect();
    sorted.sort();
    let truncated = sorted.len() > MAX_LISTED_MEMBERS;
    let members: Vec<OwnedUserId> = sorted
        .into_iter()
        .take(MAX_LISTED_MEMBERS)
        .map(ToOwned::to_owned)
        .collect();
    drop(all);
    let encryption = client.encryption();
    let mut readers = Vec::with_capacity(members.len());
    for user in members {
        let trust = crate::contacts::trust(client, &user)
            .await
            .unwrap_or(ContactTrust::NoIdentity);
        let devices = encryption
            .get_user_devices(&user)
            .await
            .map_err(|_| ErrorCode::Unavailable)?;
        let (receiving, refused) = split_devices(
            devices
                .devices()
                .map(|device| device.is_cross_signed_by_owner()),
        );
        readers.push(RoomReader {
            user_id: project_name(user.as_str()),
            trust,
            receiving,
            refused,
            own: user == own,
        });
    }
    Ok(RoomReaders { readers, truncated })
}

/// Counts devices by whether they receive room keys.
fn split_devices(receives: impl IntoIterator<Item = bool>) -> (u32, u32) {
    receives.into_iter().fold((0, 0), |(yes, no), receives| {
        if receives {
            (yes.saturating_add(1), no)
        } else {
            (yes, no.saturating_add(1))
        }
    })
}

/// Most homeservers a `room_servers` reply lists.
pub const MAX_LISTED_SERVERS: usize = 50;

/// The homeservers with joined or invited members in a joined room, the
/// most members first (then by name), at most [`MAX_LISTED_SERVERS`].
pub async fn servers(client: &Client, room_id: &str) -> Result<RoomServers, ErrorCode> {
    let room = joined(client, room_id)?;
    let own = own_user(client)?;
    let members = room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    Ok(count_servers(
        members.iter().map(|member| member.user_id()),
        own.server_name(),
    ))
}

/// Counts users by homeserver; see [`servers`]. The map and the list made
/// from it hold one borrowed name and a count per distinct server, so they
/// are bounded by the member list the SDK already holds and far smaller
/// than it. Ranking needs every count, so neither is cut before the sort;
/// only the [`MAX_LISTED_SERVERS`] listed names are copied.
fn count_servers<'a>(users: impl IntoIterator<Item = &'a UserId>, own: &ServerName) -> RoomServers {
    let mut counts: BTreeMap<&ServerName, u32> = BTreeMap::new();
    for user in users {
        *counts.entry(user.server_name()).or_default() += 1;
    }
    let mut servers: Vec<(&ServerName, u32)> = counts.into_iter().collect();
    // Most members first; names break ties, as the map already sorted them.
    servers.sort_by_key(|server| std::cmp::Reverse(server.1));
    let truncated = servers.len() > MAX_LISTED_SERVERS;
    RoomServers {
        servers: servers
            .into_iter()
            .take(MAX_LISTED_SERVERS)
            .map(|(name, members)| RoomServer {
                name: project_name(name.as_str()),
                members,
                own: name == own,
            })
            .collect(),
        truncated,
    }
}

pub async fn members(
    client: &Client,
    room_id: &str,
    filter: &str,
) -> Result<RoomMembers, ErrorCode> {
    let room = joined(client, room_id)?;
    let own = own_user(client)?;
    let levels = power_levels(client, &room).await?;
    let all = room
        .members(
            RoomMemberships::JOIN
                | RoomMemberships::INVITE
                | RoomMemberships::KNOCK
                | RoomMemberships::BAN,
        )
        .await
        .map_err(|_| ErrorCode::Unavailable)?;
    let filter = filter.trim().to_lowercase();
    let mut listed: Vec<RoomMember> = all
        .iter()
        .filter(|member| matches(&filter, member.user_id().as_str(), member.display_name()))
        .filter_map(|member| {
            let target = member.user_id();
            let membership = membership(member.membership())?;
            let level = levels.for_user(target);
            let is_own = target == own;
            let id = identifier(target.as_str());
            // Another member's ID that cannot be sent back offers no action.
            let other = id.is_some() && !is_own;
            Some(RoomMember {
                user_id: id.unwrap_or_else(|| project_name(target.as_str())),
                display_name: member.display_name().map(project_name),
                membership,
                level: shown_level(level),
                own: is_own,
                can_kick: other
                    && membership != Membership::Ban
                    && levels.user_can_kick_user(&own, target),
                can_ban: other
                    && membership != Membership::Ban
                    && levels.user_can_ban_user(&own, target),
                can_unban: other
                    && membership == Membership::Ban
                    && levels.user_can_unban_user(&own, target),
                can_change_level: (other || is_own)
                    && matches!(membership, Membership::Join | Membership::Invite)
                    && level != UserPowerLevel::Infinite
                    && levels.user_can_change_user_power_level(&own, target),
            })
        })
        .collect();
    listed.sort_by(rank);
    let truncated = listed.len() > MAX_LISTED_MEMBERS;
    listed.truncate(MAX_LISTED_MEMBERS);
    let mut reply = RoomMembers {
        room_id: room.room_id().to_string(),
        members: listed,
        truncated,
    };
    fit(&mut reply);
    Ok(reply)
}

/// Makes one change to a joined room (see the module notes).
pub async fn change(client: &Client, room_id: &str, change: &Change) -> Result<(), ErrorCode> {
    let room = joined(client, room_id)?;
    let own = own_user(client)?;
    match change {
        Change::Name(name) => room
            .set_name(name.clone())
            .await
            .map(drop)
            .map_err(|error| request_error(&error)),
        Change::Topic(topic) => room
            .set_room_topic(topic)
            .await
            .map(drop)
            .map_err(|error| request_error(&error)),
        Change::JoinRule(rule) => room
            .privacy_settings()
            .update_join_rule(rule.clone())
            .await
            .map_err(|error| request_error(&error)),
        Change::History(visibility) => room
            .privacy_settings()
            .update_room_history_visibility(visibility.clone())
            .await
            .map_err(|error| request_error(&error)),
        Change::GuestAccess(allowed) => room
            .send_state_event(RoomGuestAccessEventContent::new(if *allowed {
                GuestAccess::CanJoin
            } else {
                GuestAccess::Forbidden
            }))
            .await
            .map(drop)
            .map_err(|error| request_error(&error)),
        Change::Encryption => room
            .enable_encryption()
            .await
            .map_err(|error| request_error(&error)),
        Change::Published(published) => room
            .privacy_settings()
            .update_room_visibility(if *published {
                Visibility::Public
            } else {
                Visibility::Private
            })
            .await
            .map_err(|error| request_error(&error)),
        Change::AddAlias(alias) => {
            match room
                .privacy_settings()
                .publish_room_alias_in_room_directory(alias)
                .await
            {
                Ok(true) => Ok(()),
                // Taken already.
                Ok(false) => Err(ErrorCode::Rejected),
                Err(error) => Err(request_error(&error)),
            }
        }
        Change::RemoveAlias(alias) => remove_alias(client, &room, alias).await,
        Change::CanonicalAlias(main, alt) => {
            // Published addresses the page was not shown are kept.
            let (_, hidden) = shown_aliases(&room.alt_aliases());
            let mut alt = alt.clone();
            for alias in hidden {
                if !alt.contains(&alias) {
                    alt.push(alias);
                }
            }
            room.privacy_settings()
                .update_canonical_alias(main.clone(), alt)
                .await
                .map_err(|error| request_error(&error))
        }
        Change::Level(key, level) => {
            let mut levels = power_levels(client, &room).await?;
            set_level(&mut levels, *key, *level);
            send_levels(&room, levels).await
        }
        Change::MemberLevel(user, level) => {
            let mut levels = power_levels(client, &room).await?;
            if levels.for_user(user) == UserPowerLevel::Infinite {
                return Err(ErrorCode::InvalidArgument);
            }
            // As the SDK's `update_power_levels`, which reads only synced
            // levels: a member at the default level needs no entry.
            if *level == levels.users_default {
                levels.users.remove(user);
            } else {
                levels.users.insert(user.clone(), *level);
            }
            send_levels(&room, levels).await
        }
        Change::Kick(user) | Change::Ban(user) | Change::Unban(user) if *user == own => {
            Err(ErrorCode::InvalidArgument)
        }
        Change::Kick(user) => room
            .kick_user(user, None)
            .await
            .map_err(|error| request_error(&error)),
        Change::Ban(user) => room
            .ban_user(user, None)
            .await
            .map_err(|error| request_error(&error)),
        Change::Unban(user) => room
            .unban_user(user, None)
            .await
            .map_err(|error| request_error(&error)),
        Change::Notifications(mode) => {
            let settings = client.notification_settings().await;
            let result = match mode {
                RoomNotifications::Default => {
                    settings
                        .delete_user_defined_room_rules(room.room_id())
                        .await
                }
                RoomNotifications::All => {
                    settings
                        .set_room_notification_mode(
                            room.room_id(),
                            RoomNotificationMode::AllMessages,
                        )
                        .await
                }
                RoomNotifications::Mentions => {
                    settings
                        .set_room_notification_mode(
                            room.room_id(),
                            RoomNotificationMode::MentionsAndKeywordsOnly,
                        )
                        .await
                }
                RoomNotifications::Mute => {
                    settings
                        .set_room_notification_mode(room.room_id(), RoomNotificationMode::Mute)
                        .await
                }
            };
            // The SDK does not say whether the homeserver refused or did not
            // answer.
            result.map_err(|_| ErrorCode::Unavailable)
        }
        Change::Leave => room.leave().await.map_err(|error| request_error(&error)),
    }
}

/// Removes `alias` from the room's published addresses, then from the
/// directory, if it leads to this room; otherwise `not_found`.
async fn remove_alias(client: &Client, room: &Room, alias: &RoomAliasId) -> Result<(), ErrorCode> {
    match client.resolve_room_alias(alias).await {
        Ok(response) if response.room_id == room.room_id() => {}
        Ok(_) => return Err(ErrorCode::NotFound),
        Err(error) if error.as_client_api_error().is_some() => return Err(ErrorCode::NotFound),
        Err(_) => return Err(ErrorCode::Unavailable),
    }
    let main = room.canonical_alias();
    let alt = room.alt_aliases();
    if main.as_deref() == Some(alias) || alt.iter().any(|known| known == alias) {
        let main = main.filter(|main| main != alias);
        let alt = alt.into_iter().filter(|known| known != alias).collect();
        room.privacy_settings()
            .update_canonical_alias(main, alt)
            .await
            .map_err(|error| request_error(&error))?;
    }
    client.remove_room_alias(alias).await.map_err(|error| {
        if error.as_client_api_error().is_some() {
            ErrorCode::Rejected
        } else {
            ErrorCode::Unavailable
        }
    })
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

    /// GIVEN devices that are and are not cross-signed by their owner WHEN
    /// split THEN each counts on its side, and no devices count as none.
    #[test]
    fn given_devices_then_they_split_by_whether_they_receive_keys() {
        assert_eq!(split_devices([true, false, true, true]), (3, 1));
        assert_eq!(split_devices([]), (0, 0));
    }

    /// GIVEN members on several homeservers WHEN counted THEN each server
    /// appears once with its count, the most members first and then by
    /// name, this account's own marked, and more than the limit cut.
    #[test]
    fn given_members_then_servers_are_counted_and_bounded() {
        let users: Vec<OwnedUserId> = [
            "@a:big.example",
            "@b:big.example",
            "@c:big.example",
            "@me:home.example",
            "@d:zed.example",
            "@e:alpha.example",
        ]
        .into_iter()
        .map(|id| OwnedUserId::try_from(id).unwrap())
        .collect();
        let own = <&ServerName>::try_from("home.example").unwrap();
        let counted = count_servers(users.iter().map(|user| &**user), own);
        let listed: Vec<(&str, u32, bool)> = counted
            .servers
            .iter()
            .map(|server| (server.name.as_str(), server.members, server.own))
            .collect();
        assert_eq!(
            listed,
            [
                ("big.example", 3, false),
                ("alpha.example", 1, false),
                ("home.example", 1, true),
                ("zed.example", 1, false),
            ]
        );
        assert!(!counted.truncated);

        let many: Vec<OwnedUserId> = (0..=MAX_LISTED_SERVERS)
            .map(|n| OwnedUserId::try_from(format!("@u:s{n}.example")).unwrap())
            .collect();
        let counted = count_servers(many.iter().map(|user| &**user), own);
        assert_eq!(counted.servers.len(), MAX_LISTED_SERVERS);
        assert!(counted.truncated);
    }
    use matrix_sdk::ruma::{
        events::room::power_levels::RoomPowerLevelsSource, room_version_rules::RoomVersionRules,
    };

    fn level(n: i64) -> PowerLevel {
        PowerLevel::new(n).unwrap()
    }

    fn defaults() -> RoomPowerLevels {
        RoomPowerLevels::new(
            RoomPowerLevelsSource::None,
            &RoomVersionRules::V11.authorization,
            [UserId::parse("@creator:s").unwrap()],
        )
    }

    /// GIVEN changes the page may send WHEN parsed THEN valid ones pass,
    /// texts are trimmed, and every invalid argument is refused.
    #[test]
    fn given_room_changes_then_only_valid_arguments_pass() {
        assert_eq!(
            Change::parse(&RoomChange::Name {
                name: "  Plans ".into()
            }),
            Some(Change::Name("Plans".into()))
        );
        assert_eq!(
            Change::parse(&RoomChange::Name {
                name: String::new()
            }),
            Some(Change::Name(String::new()))
        );
        assert_eq!(
            Change::parse(&RoomChange::Topic {
                topic: "a\n\tb".into()
            }),
            Some(Change::Topic("a\n\tb".into()))
        );
        assert_eq!(
            Change::parse(&RoomChange::JoinRule {
                rule: JoinRule::Knock
            }),
            Some(Change::JoinRule(Rule::Knock))
        );
        assert_eq!(
            Change::parse(&RoomChange::CanonicalAlias {
                alias: Some("#a:s".into()),
                alt_aliases: vec!["#b:s".into()],
            }),
            Some(Change::CanonicalAlias(
                Some(RoomAliasId::parse("#a:s").unwrap()),
                vec![RoomAliasId::parse("#b:s").unwrap()]
            ))
        );
        assert_eq!(
            Change::parse(&RoomChange::MemberLevel {
                user_id: "@b:s".into(),
                level: level(-1),
            }),
            Some(Change::MemberLevel(
                UserId::parse("@b:s").unwrap(),
                Int::new(-1).unwrap()
            ))
        );
        let long_user = format!("@{}:s", "b".repeat(MAX_USER_ID_BYTES));
        for invalid in [
            RoomChange::Name {
                name: "a\nb".into(),
            },
            RoomChange::Name {
                name: "a".repeat(MAX_ROOM_NAME_BYTES + 1),
            },
            RoomChange::Topic {
                topic: "a\u{7}".into(),
            },
            RoomChange::Topic {
                topic: "a".repeat(MAX_TOPIC_BYTES + 1),
            },
            RoomChange::JoinRule {
                rule: JoinRule::Restricted,
            },
            RoomChange::JoinRule {
                rule: JoinRule::Private,
            },
            RoomChange::JoinRule {
                rule: JoinRule::Other,
            },
            RoomChange::HistoryVisibility {
                visibility: HistoryVisibility::Other,
            },
            RoomChange::AddAlias {
                alias: "a:s".into(),
            },
            RoomChange::AddAlias {
                alias: "#caf\u{e9}:s".into(),
            },
            RoomChange::RemoveAlias {
                alias: "!r:s".into(),
            },
            RoomChange::CanonicalAlias {
                alias: Some("nope".into()),
                alt_aliases: Vec::new(),
            },
            RoomChange::CanonicalAlias {
                alias: None,
                alt_aliases: vec!["#a:s".into(), "#a:s".into()],
            },
            RoomChange::CanonicalAlias {
                alias: None,
                alt_aliases: (0..=MAX_ALT_ALIASES).map(|n| format!("#a{n}:s")).collect(),
            },
            RoomChange::Kick {
                user_id: "bob".into(),
            },
            RoomChange::Ban { user_id: long_user },
            RoomChange::Unban {
                user_id: "!r:s".into(),
            },
            RoomChange::MemberLevel {
                user_id: "@b".into(),
                level: level(0),
            },
        ] {
            assert_eq!(Change::parse(&invalid), None, "{invalid:?}");
        }
    }

    /// GIVEN power levels WHEN read THEN every key is listed in order with
    /// the level in effect; an event without its own entry needs the state
    /// default.
    #[test]
    fn given_power_levels_then_each_key_has_its_effective_level() {
        let mut levels = defaults();
        levels.events.remove(&StateEventType::RoomTopic.into());
        levels.state_default = Int::new(40).unwrap();
        let listed = levels_list(&levels);
        let keys: Vec<LevelKey> = listed.iter().map(|entry| entry.key).collect();
        assert_eq!(keys, LevelKey::ALL);
        let of = |key| listed.iter().find(|entry| entry.key == key).unwrap().level;
        assert_eq!(of(LevelKey::Topic), level(40));
        assert_eq!(of(LevelKey::Ban), level(50));
        assert_eq!(of(LevelKey::UsersDefault), level(0));
        assert_eq!(of(LevelKey::NotifyRoom), level(50));
    }

    /// GIVEN power levels with an entry this client does not show WHEN one
    /// key is set THEN only that entry changes.
    #[test]
    fn given_a_level_change_then_only_its_entry_changes() {
        let mut levels = defaults();
        let custom = TimelineEventType::from("org.example.custom");
        levels.events.insert(custom.clone(), Int::new(7).unwrap());
        let before = levels.clone();
        set_level(&mut levels, LevelKey::Topic, Int::new(20).unwrap());
        assert_eq!(level_of(&levels, LevelKey::Topic), Int::new(20).unwrap());
        assert_eq!(levels.events.get(&custom), Some(&Int::new(7).unwrap()));
        assert_eq!(levels.state_default, before.state_default);
        set_level(&mut levels, LevelKey::Kick, Int::new(1).unwrap());
        assert_eq!(levels.kick, Int::new(1).unwrap());
        assert_eq!(levels.ban, before.ban);
        for key in LevelKey::ALL {
            let mut levels = defaults();
            set_level(&mut levels, key, Int::new(33).unwrap());
            assert_eq!(level_of(&levels, key), Int::new(33).unwrap(), "{key:?}");
        }
    }

    fn member(user_id: &str, membership: Membership, level: Option<i64>, own: bool) -> RoomMember {
        RoomMember {
            user_id: user_id.into(),
            display_name: None,
            membership,
            level: level.and_then(PowerLevel::new),
            own,
            can_kick: false,
            can_ban: false,
            can_unban: false,
            can_change_level: false,
        }
    }

    /// GIVEN members WHEN listed THEN this account comes first, then by
    /// membership, a creator above every level, higher levels, then ID.
    #[test]
    fn given_members_then_they_are_listed_in_settings_order() {
        let mut members = [
            member("@ban:s", Membership::Ban, Some(0), false),
            member("@b:s", Membership::Join, Some(0), false),
            member("@inv:s", Membership::Invite, Some(100), false),
            member("@a:s", Membership::Join, Some(0), false),
            member("@mod:s", Membership::Join, Some(50), false),
            member("@creator:s", Membership::Join, None, false),
            member("@me:s", Membership::Join, Some(0), true),
            member("@knock:s", Membership::Knock, Some(0), false),
        ];
        members.sort_by(rank);
        let ids: Vec<&str> = members.iter().map(|m| m.user_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "@me:s",
                "@creator:s",
                "@mod:s",
                "@a:s",
                "@b:s",
                "@inv:s",
                "@knock:s",
                "@ban:s"
            ]
        );
    }

    /// GIVEN a filter WHEN members are matched THEN the ID or the name may
    /// contain it, ignoring case; an empty filter matches everyone.
    #[test]
    fn given_a_filter_then_ids_and_names_match_ignoring_case() {
        assert!(matches("", "@b:s", None));
        assert!(matches("bo", "@BOB:s", None));
        assert!(matches("ali", "@x:s", Some("Alice")));
        assert!(!matches("ali", "@x:s", Some("Bob")));
        assert!(!matches("ali", "@x:s", None));
        assert!(valid_filter(""));
        assert!(!valid_filter("a\nb"));
        assert!(!valid_filter(&"a".repeat(MAX_MEMBER_FILTER_BYTES + 1)));
    }

    fn longest(count: usize) -> RoomMembers {
        // Four bytes a character, at the projection's length limit.
        let name = "\u{1F600}".repeat(128);
        RoomMembers {
            room_id: "!r:s".into(),
            members: (0..count)
                .map(|n| {
                    let mut m = member(
                        &format!("@{n:0>240}:example.org"),
                        Membership::Join,
                        Some(PowerLevel::MIN),
                        false,
                    );
                    m.display_name = Some(name.clone());
                    m
                })
                .collect(),
            truncated: false,
        }
    }

    /// GIVEN the listed number of members with the longest IDs and names
    /// WHEN the reply is built THEN it fits one view whole; GIVEN more THEN
    /// members are dropped from the end until it fits.
    #[test]
    fn given_long_members_then_the_reply_is_cut_to_the_view_budget() {
        let mut whole = longest(MAX_LISTED_MEMBERS);
        fit(&mut whole);
        assert_eq!(whole.members.len(), MAX_LISTED_MEMBERS);
        assert!(!whole.truncated);
        let mut reply = longest(3 * MAX_LISTED_MEMBERS);
        fit(&mut reply);
        assert!(serde_json::to_vec(&reply).unwrap().len() <= MAX_PAGE_BYTES);
        assert!(reply.truncated);
        assert!(!reply.members.is_empty());
        let mut small = RoomMembers {
            room_id: "!r:s".into(),
            members: vec![member("@a:s", Membership::Join, Some(0), true)],
            truncated: false,
        };
        fit(&mut small);
        assert_eq!(small.members.len(), 1);
        assert!(!small.truncated);
    }

    /// GIVEN published addresses WHEN split for the page THEN only valid,
    /// visible ASCII ones are shown, at most the limit; the rest are kept
    /// apart so a change preserves them.
    #[test]
    fn given_addresses_then_unshowable_ones_are_kept_apart() {
        let mut aliases: Vec<OwnedRoomAliasId> = (0..MAX_ALT_ALIASES + 2)
            .map(|n| RoomAliasId::parse(format!("#a{n}:s")).unwrap())
            .collect();
        if let Ok(odd) = RoomAliasId::parse("#caf\u{e9}:s") {
            aliases.insert(0, odd);
        }
        let (shown, hidden) = shown_aliases(&aliases);
        assert_eq!(shown.len(), MAX_ALT_ALIASES);
        assert_eq!(shown.len() + hidden.len(), aliases.len());
        assert!(shown.iter().all(|alias| alias.as_str().is_ascii()));
    }

    /// GIVEN room state without a join rule or history setting WHEN shown
    /// THEN the specification's default is shown.
    #[test]
    fn given_missing_room_state_then_the_defaults_are_shown() {
        assert_eq!(join_rule(None), JoinRule::Invite);
        assert_eq!(join_rule(Some(Rule::Public)), JoinRule::Public);
        assert_eq!(history_visibility(None), HistoryVisibility::Shared);
        assert_eq!(
            history_visibility(Some(Visible::WorldReadable)),
            HistoryVisibility::WorldReadable
        );
        assert_eq!(notifications(None), RoomNotifications::Default);
        assert_eq!(
            notifications(Some(RoomNotificationMode::MentionsAndKeywordsOnly)),
            RoomNotifications::Mentions
        );
    }

    fn content(json: &str) -> Option<Raw<AnyStateEventContent>> {
        Some(Raw::from_json_string(json.to_owned()).unwrap())
    }

    /// GIVEN power levels the homeserver sent because this device had not
    /// synced them WHEN read THEN valid ones are used as sent and none are
    /// the specification's defaults; content that is not power levels, or
    /// that lists a room version 12 creator in `users`, is `unavailable`.
    #[test]
    fn given_fetched_power_levels_then_only_valid_ones_are_used() {
        let creator = UserId::parse("@creator:s").unwrap();
        let other = UserId::parse("@other:s").unwrap();
        let v12 = &RoomVersionRules::V12.authorization;

        let levels = fetched_levels(
            content(r#"{"users":{"@other:s":50},"state_default":75}"#),
            v12,
            vec![creator.clone()],
        )
        .unwrap();
        assert_eq!(levels.for_user(&other), UserPowerLevel::Int(50.into()));
        assert_eq!(levels.for_user(&creator), UserPowerLevel::Infinite);
        assert_eq!(levels.state_default, Int::from(75));

        let none = fetched_levels(None, v12, vec![creator.clone()]).unwrap();
        assert_eq!(none.state_default, Int::from(50));
        assert_eq!(none.users_default, Int::from(0));

        assert_eq!(
            fetched_levels(
                content(r#"{"users":{"@creator:s":100}}"#),
                v12,
                vec![creator.clone()]
            )
            .err(),
            Some(ErrorCode::Unavailable)
        );
        // Before room version 12 a creator has an entry like anyone else.
        assert!(
            fetched_levels(
                content(r#"{"users":{"@creator:s":100}}"#),
                &RoomVersionRules::V11.authorization,
                vec![creator.clone()]
            )
            .is_ok()
        );
        assert_eq!(
            fetched_levels(content(r#"{"users":"not a map"}"#), v12, vec![creator]).err(),
            Some(ErrorCode::Unavailable)
        );
    }
}
