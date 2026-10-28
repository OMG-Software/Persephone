use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use ts_rs::{Config, TS};

/// A time in milliseconds since the Unix epoch, at most [`UnixMillis::MAX`]
/// so that JavaScript holds it exactly. A larger value fails to deserialize.
/// It says nothing about who set the time; each field says that.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct UnixMillis(u64);

impl UnixMillis {
    /// JavaScript's largest exact integer, 2^53 - 1.
    pub const MAX: u64 = (1 << 53) - 1;

    /// `millis`, or `None` when JavaScript cannot hold it exactly.
    pub fn new(millis: u64) -> Option<Self> {
        (millis <= Self::MAX).then_some(Self(millis))
    }

    /// `millis`, capped at [`UnixMillis::MAX`].
    pub fn saturating(millis: u64) -> Self {
        Self(millis.min(Self::MAX))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for UnixMillis {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(u64::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("time out of JavaScript's exact range"))
    }
}

/// Deliberately small frontend projection; never expose SDK, authentication, or key types here.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomSummary {
    pub room_id: String,
    pub display_name: Option<String>,
    /// u32 caps this count below JavaScript's exact-integer limit.
    pub unread_count: u32,
    pub encryption: RoomEncryption,
    /// In a direct-message room with one other person, that person's user ID
    /// (untrusted, projected like a name). Otherwise none.
    pub contact: Option<String>,
}

/// Whether a room's messages are end-to-end encrypted.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomEncryption {
    Encrypted,
    /// Messages sent here are readable by the homeserver; sending needs a
    /// confirmation first (ADR-0005).
    NotEncrypted,
    /// Not known yet; the core checks before sending.
    Unknown,
}

/// Where a message the user sent is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingState {
    /// Waiting its turn (later messages in a room wait for earlier ones).
    Queued,
    Sending,
    /// The homeserver accepted it; `event_id` names the event.
    Sent,
    /// Not sent; `failure` says why. It can be retried or discarded.
    Failed,
    /// The connection failed after the request may have reached the
    /// homeserver. Retrying sends the same request again, which the
    /// homeserver recognises, so it cannot arrive twice.
    Unknown,
}

/// Why a message was not sent. Codes only.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendFailure {
    /// The room is not encrypted and sending in it was not confirmed.
    ConfirmationRequired,
    /// This device is not verified, so room keys cannot be shared from it.
    DeviceNotVerified,
    /// A member's identity changed; nothing is sent in the room until the
    /// user accepts the new identity or verifies it (ADR-0007).
    IdentityChanged,
    /// This account is not joined to the room.
    NotJoined,
    /// The homeserver refused the message.
    Rejected,
    /// Anything else.
    Failed,
}

/// What a message the user sent holds.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    Text,
    /// One image; `body` is empty. The pixels are not repeated here.
    Image,
    /// A new version of one of this account's messages; `body` is its text.
    Edit,
}

/// A message the user sent from this device in this session. Only the body
/// the user typed is held; nothing is kept after a lock or restart.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PendingMessage {
    pub local_id: u32,
    pub room_id: String,
    pub kind: PendingKind,
    pub body: String,
    pub state: PendingState,
    pub event_id: Option<String>,
    pub failure: Option<SendFailure>,
    /// When it was queued on this device, by this computer's clock.
    #[ts(type = "number")]
    pub queued_at: UnixMillis,
}

/// Serde's internally tagged wire representation is mirrored by ts-rs serde compatibility.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimelineItem {
    Message {
        event_id: String,
        sender: String,
        /// The text, without a reply's quoted fallback.
        body: String,
        /// When the sender's homeserver says it received the event
        /// (`origin_server_ts`). That server sets it and nothing checks it,
        /// so it can be wrong. `None` when the event has no usable time.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        /// The event this message replies to, as the sender claims. The
        /// page shows it only from history it has loaded itself; the
        /// sender's quoted copy of it is never shown.
        reply_to: Option<String>,
    },
    Gap {
        next_cursor: Option<String>,
    },
    /// An event that is shown but not as message text.
    Notice {
        event_id: String,
        sender: String,
        notice: NoticeKind,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
    },
    /// An `m.image` event. Nothing is fetched until the user asks
    /// (`load_image`, ADR-0009). Every field comes from the sender and is
    /// untrusted: `name` is projected like a display name; `size`, `width`
    /// and `height` are the sender's claims, for layout only, and are none
    /// when absent or out of range.
    Image {
        event_id: String,
        sender: String,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        name: String,
        /// Declared file size in bytes.
        size: Option<u32>,
        /// Declared width and height, both set or both none.
        width: Option<u32>,
        height: Option<u32>,
        /// The file is end-to-end encrypted.
        encrypted: bool,
    },
    /// A new version of the text message `replaces`, by the same sender:
    /// the core checked that both events have the same sender. Not shown as
    /// a line of its own; the page shows `body` in place of the message's,
    /// marked as edited. The newest edit in the room's order counts.
    Edit {
        event_id: String,
        sender: String,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        replaces: String,
        body: String,
    },
    /// A reaction (`m.reaction`) to the event `reacts_to`: `key`, usually an
    /// emoji, projected like a name. Not shown as a line of its own; the
    /// page counts reactions under the event they react to.
    Reaction {
        event_id: String,
        sender: String,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        reacts_to: String,
        key: String,
    },
    /// The deletion (redaction) of the event `redacts`. Not shown as a line
    /// of its own; the page shows that event, if loaded, as deleted. The
    /// homeserver decided whether `sender` may delete it.
    Redaction {
        event_id: String,
        sender: String,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        redacts: String,
    },
    /// A change to the room's members or state, shown as a line of its own.
    State {
        event_id: String,
        sender: String,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        change: StateChange,
    },
}

/// What a state event changed. Names, topics and user IDs come from the
/// event and are projected like display names; `sender` is who made the
/// change and `target` the member it was made to.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateChange {
    /// A member joined, left, was invited, removed, banned, and so on.
    Member {
        target: String,
        membership: MembershipChange,
    },
    /// A member set (or, with `None`, removed) their display name in this
    /// room.
    DisplayName {
        target: String,
        name: Option<String>,
    },
    /// The room's name was set, or removed (`None`).
    Name {
        name: Option<String>,
    },
    /// The room's topic was set, or removed (`None`).
    Topic {
        topic: Option<String>,
    },
    /// The room's picture changed.
    Avatar {},
    /// End-to-end encryption was turned on. It cannot be turned off.
    Encryption {},
    JoinRule {
        rule: JoinRule,
    },
    HistoryVisibility {
        visibility: HistoryVisibility,
    },
    /// Who may do what (power levels) changed.
    PowerLevels {},
    /// The room's published addresses changed.
    Addresses {},
    /// The room was created.
    Created {},
    /// Which homeservers may take part changed (server ACL).
    ServerAcl {},
    /// The room was replaced by a new version.
    Upgraded {},
}

/// How a member's membership changed. `sender` made the change; for
/// `joined`, `left` and `knocked` that is the member themself.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MembershipChange {
    Joined,
    Left,
    Invited,
    /// The member declined their invitation.
    InviteRejected,
    /// The invitation was withdrawn by someone else.
    InviteWithdrawn,
    /// Removed by someone else.
    Kicked,
    Banned,
    Unbanned,
    /// Asked to join.
    Knocked,
    /// Withdrew their request to join.
    KnockWithdrawn,
    /// Their request to join was refused.
    KnockDenied,
}

/// The kind of an image the user picked to send.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
pub enum ImageMime {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
}

/// Reply of `load_image`, and the preview of a picked image: pixels the core
/// holds under an opaque handle. The page fetches them with `image_band`,
/// bands `0..bands`; band `n` starts at row `n * bandRows`, and only the last
/// may be shorter. The handle stops working after `release_image`, a lock or
/// a sign-out.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ImageView {
    pub handle: String,
    /// The shown size, at most 2048 x 2048, after the EXIF orientation.
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub band_rows: u32,
}

/// Reply of `pick_image`.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImagePick {
    /// A stripped copy of the chosen file, ready to send with `send_image`
    /// (the preview's handle) or drop with `discard_image`. The preview is
    /// decoded from the stripped copy, so it shows what will be sent.
    Draft {
        preview: ImageView,
        /// The stripped copy's size in bytes.
        size: u32,
        mime: ImageMime,
    },
    /// The user closed the dialog without choosing a file.
    Cancelled {},
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// Encrypted, and this device has no key for it.
    Undecryptable,
    /// The sender chose not to share the key with this device, for example
    /// because this device is not verified.
    Withheld,
    /// Not shown: the sender's device is not signed by its owner's identity,
    /// so it could be a device the sender never added.
    UntrustedSender,
    /// A kind of event this client does not display yet.
    Unsupported,
    /// The message was deleted (redacted) by its sender or a moderator.
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimelinePage {
    pub room_id: String,
    pub items: Vec<TimelineItem>,
    pub next_cursor: Option<String>,
}

/// Reply of the `room_list` command. The core caps the number of rooms;
/// `truncated` says whether more exist than were sent.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomList {
    pub rooms: Vec<RoomSummary>,
    pub truncated: bool,
}

/// An invitation to a room that this account has not answered. Names are the
/// inviter's word, projected like any name.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InviteSummary {
    pub room_id: String,
    /// The room's name, from the invitation's room state, if it has one.
    pub room_name: Option<String>,
    /// Whether the inviter marked it as a direct-message invitation.
    pub direct: bool,
    /// The user ID of whoever sent the invitation, when the invitation says.
    pub inviter: Option<String>,
    /// The inviter's display name in that room, if they set one.
    pub inviter_name: Option<String>,
}

/// Reply of `invite_list`: every pending room invitation, capped like the
/// room list.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InviteList {
    pub invites: Vec<InviteSummary>,
    pub truncated: bool,
}

/// Reply of `start_conversation`.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationStart {
    /// A direct-message room with just this person already existed; nothing
    /// was created.
    #[serde(rename_all = "camelCase")]
    Existing { room_id: String },
    /// A new encrypted direct-message room was created and the person was
    /// invited to it.
    #[serde(rename_all = "camelCase")]
    Created { room_id: String },
    /// The person has no cross-signing identity, so they could not read
    /// encrypted messages (ADR-0005). Nothing was created.
    NoIdentity {},
    /// The homeserver refused: for example the user does not exist, or the
    /// server does not allow the invitation. Nothing was created.
    Refused {},
}

/// Reply of `create_room`.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoomCreation {
    /// A new encrypted room was created and everyone named was invited.
    #[serde(rename_all = "camelCase")]
    Created { room_id: String },
    /// This person has no cross-signing identity (ADR-0005). Nothing was
    /// created.
    #[serde(rename_all = "camelCase")]
    NoIdentity { user_id: String },
    /// The homeserver refused. Nothing was created.
    Refused {},
}

/// Reply of `invite_user`.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoomInvite {
    Invited {},
    /// The room is encrypted and this person has no cross-signing identity
    /// (ADR-0005). Nobody was invited.
    NoIdentity {},
    /// The homeserver refused: for example the user does not exist, is
    /// already in the room, or this account may not invite.
    Refused {},
}

/// A Matrix power level, within the range JavaScript holds exactly (the
/// Matrix limit too). Another value fails to deserialize.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PowerLevel(i64);

impl PowerLevel {
    /// JavaScript's largest exact integer, 2^53 - 1.
    pub const MAX: i64 = (1 << 53) - 1;
    pub const MIN: i64 = -Self::MAX;

    /// `level`, or `None` outside [`PowerLevel::MIN`] to [`PowerLevel::MAX`].
    pub fn new(level: i64) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&level)
            .then_some(Self(level))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for PowerLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(i64::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("power level out of range"))
    }
}

/// How people may join a room (`m.room.join_rules`). Persephone sets only
/// `invite`, `knock` and `public`; the others are shown as they are.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JoinRule {
    /// Only people who are invited.
    Invite,
    /// Anyone may ask to join; a member who may invite lets them in.
    Knock,
    /// Anyone who knows the room's ID or an address.
    Public,
    /// Members of certain other rooms (spaces).
    Restricted,
    /// Members of certain other rooms, and anyone may ask.
    KnockRestricted,
    /// Reserved by the specification; works like `invite`.
    Private,
    /// A rule this client does not know.
    Other,
}

/// Who may read a room's history (`m.room.history_visibility`).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryVisibility {
    /// Members, from the point they were invited.
    Invited,
    /// Members, from the point they joined.
    Joined,
    /// Members, all of it, including what was sent before they joined.
    Shared,
    /// Anyone, members or not. Encrypted messages stay unreadable without
    /// their keys.
    WorldReadable,
    /// A value this client does not know.
    Other,
}

/// How this account is notified of a room's messages: its push rules for the
/// room, kept on the homeserver.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomNotifications {
    /// No rule for this room: the account's defaults apply.
    Default,
    All,
    /// Mentions and keywords only.
    Mentions,
    Mute,
}

/// A power level a room requires for an action, or gives by default.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LevelKey {
    /// The level of a member not listed by name.
    UsersDefault,
    /// Send messages (any event without its own level).
    EventsDefault,
    /// Change settings without their own level.
    StateDefault,
    Invite,
    Kick,
    Ban,
    /// Remove other people's messages.
    Redact,
    /// Notify the whole room (`@room`).
    NotifyRoom,
    /// Change the room's name (`m.room.name`).
    Name,
    Topic,
    Avatar,
    /// Change the room's main and published addresses.
    CanonicalAlias,
    JoinRules,
    HistoryVisibility,
    GuestAccess,
    /// Turn on encryption.
    Encryption,
    /// Change permissions and member levels.
    PowerLevels,
    /// Change which servers may take part.
    ServerAcl,
    /// Upgrade the room to a new version.
    Tombstone,
    PinnedEvents,
}

impl LevelKey {
    /// Every key, in the order the settings show them.
    pub const ALL: [LevelKey; 20] = [
        LevelKey::UsersDefault,
        LevelKey::EventsDefault,
        LevelKey::StateDefault,
        LevelKey::Invite,
        LevelKey::Kick,
        LevelKey::Ban,
        LevelKey::Redact,
        LevelKey::NotifyRoom,
        LevelKey::Name,
        LevelKey::Topic,
        LevelKey::Avatar,
        LevelKey::CanonicalAlias,
        LevelKey::JoinRules,
        LevelKey::HistoryVisibility,
        LevelKey::GuestAccess,
        LevelKey::Encryption,
        LevelKey::PowerLevels,
        LevelKey::ServerAcl,
        LevelKey::Tombstone,
        LevelKey::PinnedEvents,
    ];
}

/// One entry of a room's power levels.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomLevel {
    pub key: LevelKey,
    #[ts(type = "number")]
    pub level: PowerLevel,
}

/// What this account may change in a room, by the room's power levels. A
/// hint for the page: the homeserver decides.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomPermissions {
    pub name: bool,
    pub topic: bool,
    /// The main and published addresses.
    pub canonical_alias: bool,
    pub join_rules: bool,
    pub history_visibility: bool,
    pub guest_access: bool,
    pub encryption: bool,
    pub power_levels: bool,
    pub invite: bool,
    pub kick: bool,
    pub ban: bool,
}

/// Reply of `room_settings`: a joined room's configuration. Names and the
/// topic are the homeserver's word, projected; addresses are sent only when
/// they are valid, visible ASCII, so the page can name them back.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomSettings {
    pub room_id: String,
    pub name: Option<String>,
    /// Line breaks kept, like a message body.
    pub topic: Option<String>,
    /// The main address (`m.room.canonical_alias`).
    pub canonical_alias: Option<String>,
    /// The other published addresses.
    pub alt_aliases: Vec<String>,
    /// Addresses on this account's homeserver that lead to the room; none
    /// when the homeserver did not say.
    pub local_aliases: Option<Vec<String>>,
    pub join_rule: JoinRule,
    pub history_visibility: HistoryVisibility,
    /// Guests (accounts without a password) may join.
    pub guest_access: bool,
    pub encryption: RoomEncryption,
    /// Listed in this homeserver's public room directory; none when the
    /// homeserver did not say.
    pub published: Option<bool>,
    /// Untrusted, projected like a name.
    pub room_version: Option<String>,
    /// Marked as a direct-message room.
    pub direct: bool,
    pub notifications: RoomNotifications,
    /// Every [`LevelKey`], in [`LevelKey::ALL`] order, with the level now in
    /// effect.
    pub levels: Vec<RoomLevel>,
    /// This account's level; none for a creator of a room whose version
    /// gives creators every permission.
    #[ts(type = "number | null")]
    pub own_level: Option<PowerLevel>,
    pub may: RoomPermissions,
}

/// A room member's membership, as listed in the settings.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    Join,
    Invite,
    /// Asked to join.
    Knock,
    Ban,
}

/// One member of a room, in its settings.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomMember {
    /// The member's ID unchanged when it is valid, visible ASCII; otherwise
    /// projected, and every `can` field is false.
    pub user_id: String,
    pub display_name: Option<String>,
    pub membership: Membership,
    /// None for a creator with every permission.
    #[ts(type = "number | null")]
    pub level: Option<PowerLevel>,
    /// This account.
    pub own: bool,
    /// Hints for the page, by the room's power levels.
    pub can_kick: bool,
    pub can_ban: bool,
    pub can_unban: bool,
    pub can_change_level: bool,
}

/// Reply of `room_members`: members (joined, invited, asking or banned)
/// whose ID or name contains the filter; this account first, then by
/// membership, level and ID.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomMembers {
    pub room_id: String,
    pub members: Vec<RoomMember>,
    /// More members matched than are listed.
    pub truncated: bool,
}

/// One change to a room's settings, the argument of `change_room`. Each is
/// a separate homeserver request; nothing is changed when it is refused.
/// Every variant is a struct variant, so unknown fields are refused.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoomChange {
    /// An empty name removes it.
    Name {
        name: String,
    },
    /// An empty topic removes it.
    Topic {
        topic: String,
    },
    /// `invite`, `knock` or `public` only.
    JoinRule {
        rule: JoinRule,
    },
    /// Any value but `other`.
    HistoryVisibility {
        visibility: HistoryVisibility,
    },
    GuestAccess {
        allowed: bool,
    },
    /// Turns on encryption. It cannot be turned off.
    Encryption {},
    /// Lists the room in this homeserver's public directory, or not.
    Published {
        published: bool,
    },
    /// Makes an address on this account's homeserver lead to the room.
    AddAlias {
        alias: String,
    },
    /// Removes an address from this account's homeserver, and from the
    /// room's published addresses.
    RemoveAlias {
        alias: String,
    },
    /// Sets the main address (none removes it) and the other published
    /// addresses.
    #[serde(rename_all = "camelCase")]
    CanonicalAlias {
        #[serde(deserialize_with = "Option::deserialize")]
        alias: Option<String>,
        alt_aliases: Vec<String>,
    },
    Level {
        key: LevelKey,
        #[ts(type = "number")]
        level: PowerLevel,
    },
    #[serde(rename_all = "camelCase")]
    MemberLevel {
        user_id: String,
        #[ts(type = "number")]
        level: PowerLevel,
    },
    #[serde(rename_all = "camelCase")]
    Kick {
        user_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Ban {
        user_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Unban {
        user_id: String,
    },
    Notifications {
        mode: RoomNotifications,
    },
    /// Leaves the room.
    Leave {},
}

/// Reply of `display_name`: this account's display name, projected like any
/// name.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DisplayName {
    pub display_name: Option<String>,
}

/// One device (session) signed in to this account. The homeserver's word:
/// the ID and name are projected like any name. No IP address crosses.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeviceSummary {
    pub device_id: String,
    pub display_name: Option<String>,
    /// When the homeserver last saw the device, by the homeserver's clock.
    #[ts(type = "number | null")]
    pub last_seen: Option<UnixMillis>,
    /// This device.
    pub current: bool,
    /// Whether this account's cross-signing identity signed the device, as
    /// this device's store knows it.
    pub verified: bool,
}

/// Reply of `device_list`: this device first, then the most recently seen.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeviceList {
    pub devices: Vec<DeviceSummary>,
    pub truncated: bool,
}

/// Reply of `account_settings`: this account's settings, kept in its
/// encrypted profile. A setting never saved has its default.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AccountSettings {
    /// Enter sends a message; otherwise it starts a new line. On by default.
    pub enter_sends: bool,
    /// The size of message text, in percent of the normal size, from
    /// [`TEXT_PERCENT_MIN`] to [`CHAT_TEXT_PERCENT_MAX`]. 100 by default.
    pub chat_text_percent: u16,
    /// The size of the rest of the interface, in percent of the normal size,
    /// from [`TEXT_PERCENT_MIN`] to [`INTERFACE_TEXT_PERCENT_MAX`]. 100 by
    /// default.
    pub interface_text_percent: u16,
    /// Show a desktop notification (a toast) for a message that notifies
    /// under the account's push rules, with the sender and room only, never
    /// the text. Off by default.
    pub desktop_notifications: bool,
    /// Tell the people in a room while this account is typing there
    /// (typing notices). Off by default (ADR-0023).
    pub send_typing: bool,
    /// Show the people in a room how far this account has read (public
    /// read receipts). Off by default; the fully-read marker and a private
    /// receipt, which only this account sees, are sent either way
    /// (ADR-0023).
    pub send_read_receipts: bool,
}

/// The smallest text size either setting accepts, in percent.
pub const TEXT_PERCENT_MIN: u16 = 80;
/// The largest message text size, in percent.
pub const CHAT_TEXT_PERCENT_MAX: u16 = 200;
/// The largest interface text size, in percent: the whole interface grows
/// with it, so it stops before the window's minimum size stops fitting.
pub const INTERFACE_TEXT_PERCENT_MAX: u16 = 150;

impl AccountSettings {
    /// Whether every setting is within its bounds.
    pub fn within_bounds(&self) -> bool {
        (TEXT_PERCENT_MIN..=CHAT_TEXT_PERCENT_MAX).contains(&self.chat_text_percent)
            && (TEXT_PERCENT_MIN..=INTERFACE_TEXT_PERCENT_MAX)
                .contains(&self.interface_text_percent)
    }
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            enter_sends: true,
            chat_text_percent: 100,
            interface_text_percent: 100,
            desktop_notifications: false,
            send_typing: false,
            send_read_receipts: false,
        }
    }
}

/// Reply of `room_activity`: who else is typing in a room, and who else has
/// read up to the event the page asked about (ADR-0023). Each list holds at
/// most ten user IDs, projected like names; `truncated` says more exist.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomActivity {
    pub typing: Vec<String>,
    pub read_by: Vec<String>,
    pub truncated: bool,
}

/// When this account is notified of messages, in every room without a rule
/// of its own: its push rules on the homeserver (ADR-0022).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    /// Every message.
    All,
    /// Mentions and keywords only.
    Mentions,
    /// Nothing: the push rules' master switch is on.
    None,
    /// Read only: the rules differ by kind of room (set by another client).
    /// Setting a level replaces them.
    Custom,
}

/// Reply of `notification_level`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NotificationSettings {
    pub level: NotificationLevel,
}

/// The kind of a third-party identifier (3PID).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreePidMedium {
    Email,
    /// A phone number (MSISDN), in international form without the `+`.
    Msisdn,
}

/// An email address or phone number on this account, as the homeserver
/// lists it (projected like a name).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ThreePid {
    pub medium: ThreePidMedium,
    pub address: String,
}

/// Reply of `three_pids` (ADR-0020).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ThreePidList {
    pub three_pids: Vec<ThreePid>,
    pub truncated: bool,
    /// The homeserver lets this account add and remove them (its
    /// `m.3pid_changes` capability, on unless it says otherwise).
    pub can_change: bool,
    /// An address is waiting to be confirmed: `add_three_pid` adds it.
    pub pending: Option<ThreePid>,
}

/// How a change to the account (its password, an email address or a phone
/// number) ended. Codes only: no text from the homeserver.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountChange {
    Done,
    /// The homeserver sent a link (email) or a code (phone); add the address
    /// once the user has it.
    TokenSent,
    /// The user closed one of the core's windows.
    Cancelled,
    /// No native prompt on this platform.
    Unsupported,
    /// The homeserver refused the current password.
    InvalidPassword,
    /// The homeserver thinks the new password is too weak.
    WeakPassword,
    /// The address was not confirmed: the link was not opened yet, or the
    /// code was wrong.
    NotConfirmed,
    /// Another account already has this address.
    InUse,
    /// The homeserver does not allow this address.
    Denied,
    /// The homeserver does not support this kind of address, or needs an
    /// identity server to confirm it, which Persephone does not use.
    MediumNotSupported,
    /// The homeserver does not let this account change its addresses or
    /// password.
    ChangesDisabled,
    /// The homeserver asks for a step other than the password.
    UnsupportedAuth,
    /// Nothing is waiting to be added, or that address is not listed.
    NothingPending,
    /// Too many attempts; the homeserver asks to wait.
    RateLimited,
    /// The homeserver could not be reached.
    Unreachable,
    Failed,
}

/// One local profile (account) on this computer. Only its slot number:
/// nothing about the account is kept outside its encrypted store.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProfileSlot {
    pub slot: u32,
    /// The profile open now.
    pub active: bool,
}

/// Reply of `profile_list`, in slot order. Empty with synthetic data.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProfileList {
    pub profiles: Vec<ProfileSlot>,
    /// Whether a free slot is left for `add_profile`.
    pub can_add: bool,
}

/// One match from the homeserver's user directory.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UserMatch {
    pub user_id: String,
    /// The display name the directory gave, projected like any name.
    pub display_name: Option<String>,
}

/// Reply of `search_users`. `truncated` says whether the homeserver had more
/// matches than were sent.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UserSearch {
    pub users: Vec<UserMatch>,
    pub truncated: bool,
}

/// State of the local encrypted store. Codes only: no paths, key material or
/// error text reach the UI.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StoreState {
    /// No profile was given; nothing is stored.
    NotConfigured,
    /// The store is open with its key, unsealed by the profile passphrase.
    Protected,
    /// Locked on request; the stores are closed.
    Locked,
    /// A lock was requested and new work is refused, but admitted work has not
    /// finished yet; the stores are still open. Locking again resumes the wait.
    Locking,
    /// The core is asking for the passphrase (or confirming a reset) in its
    /// own window and opening the stores. The result follows as a session
    /// change.
    Unlocking,
    /// The stores did not close cleanly while locking. The session stays
    /// locked until the application restarts.
    LockFailed,
    /// Locked after a Matrix client ran. The SDK cannot yet prove all its
    /// background work stopped, so the stores reopen only after a restart.
    RestartRequired,
    /// A store exists but its key does not; the store was left untouched.
    KeyMissing,
    /// The key exists but the OS vault could not unwrap it, or it is
    /// malformed.
    KeyUnreadable,
    /// The passphrase was wrong too many times; Unlock may try again.
    SecretRejected,
    /// The store refused the key.
    KeyRejected,
    /// Another process holds the profile.
    ProfileInUse,
    /// The store could not be opened for another reason.
    StoreUnavailable,
    /// This platform has no supported vault yet.
    Unsupported,
}

/// Reply of the `app_status` command. It carries no system, account or user
/// data.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AppStatus {
    /// The application version, from the desktop crate's manifest.
    pub version: String,
    /// The commit the core was built from: its first 12 hex digits, or
    /// "unknown". It does not show uncommitted changes.
    pub revision: String,
    /// The current development milestone, e.g. "P1".
    pub milestone: String,
    pub store: StoreState,
    pub account: AccountStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    SignedOut,
    /// The core is asking for credentials or talking to the homeserver.
    SigningIn,
    SignedIn,
    SignInFailed,
}

/// Why signing in or syncing failed. Codes only.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignInError {
    /// The user closed the credential prompt.
    Cancelled,
    /// The homeserver refused the username or password.
    InvalidCredentials,
    /// The homeserver could not be reached or did not answer as Matrix.
    Unreachable,
    /// The URL is not one this client accepts (for example plain HTTP).
    HomeserverRefused,
    /// No native credential prompt on this platform.
    Unsupported,
    /// Signed in, but syncing failed.
    SyncFailed,
    /// Signed out locally, but the homeserver could not be told: the server
    /// session may stay active until it expires or is revoked elsewhere.
    SessionNotRevoked,
    /// Registering: someone already has that username.
    UsernameTaken,
    /// Registering: the homeserver does not accept that username.
    InvalidUsername,
    /// Registering: the username is reserved, for example for a bridge.
    UsernameReserved,
    /// Registering: the homeserver does not let anyone register here.
    RegistrationDisabled,
    /// Registering: the homeserver did not accept the registration token.
    TokenRejected,
    /// Registering: the homeserver thinks the password is too weak.
    WeakPassword,
    /// Registering: the homeserver asks for a step Persephone cannot do,
    /// such as an email address, a captcha or accepting its terms.
    UnsupportedRegistration,
    /// Too many attempts; the homeserver asks to wait.
    RateLimited,
    /// Anything else.
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AccountStatus {
    pub state: AccountState,
    /// The signed-in Matrix user ID, at most 255 bytes.
    pub user_id: Option<String>,
    pub error: Option<SignInError>,
}

impl AccountStatus {
    pub fn signed_out() -> Self {
        Self {
            state: AccountState::SignedOut,
            user_id: None,
            error: None,
        }
    }
}

/// Whether this account's encryption identity is set up and trusted here.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdentityState {
    /// Not known yet (signed out, locked, or not yet asked of the homeserver).
    Unknown,
    /// The account has no cross-signing identity: encryption can be set up.
    None,
    /// The account has an identity, but this device is not verified with it:
    /// verify it from another device or with the recovery key.
    Unverified,
    /// This device is verified with the account's identity and can send.
    Verified,
}

/// Whether the account's keys are kept in secret storage on the homeserver.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    Unknown,
    Disabled,
    /// Set up, but this device lacks some of the stored secrets.
    Incomplete,
    Enabled,
}

/// What the core is doing with the account's encryption.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecurityOperation {
    Idle,
    /// The core's own window is asking for the account password.
    WaitingForPassword,
    /// The core's own window is asking for the recovery key.
    WaitingForRecoveryKey,
    /// The core's own window is asking whether to accept a contact's new
    /// identity (ADR-0007).
    WaitingForConfirmation,
    /// Talking to the homeserver.
    Working,
}

/// Why an encryption operation failed. Codes only.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecurityError {
    /// The user closed the prompt.
    Cancelled,
    /// No native prompt on this platform.
    Unsupported,
    /// The homeserver refused the password.
    InvalidPassword,
    /// The account already has an identity: verify this device instead.
    IdentityExists,
    /// The recovery key did not open the account's secret storage.
    WrongRecoveryKey,
    /// The homeserver already holds a key backup this device cannot use.
    BackupExistsOnServer,
    /// Recovery was set up, but its key could not be shown.
    RecoveryKeyNotShown,
    /// The homeserver could not be reached.
    Unreachable,
    /// The contact has no cross-signing identity, so there is nothing to
    /// verify.
    ContactHasNoIdentity,
    /// This device lacks the account's key for signing other people
    /// (verify it with the recovery key first).
    CannotSignContacts,
    /// The room is not a direct-message room with one other person who is
    /// still in it.
    NotDirectRoom,
    /// This device lacks the account's key for signing its other devices
    /// (verify it with the recovery key first).
    CannotSignDevices,
    /// The homeserver does not list that device for this account.
    DeviceNotFound,
    /// A new recovery key was made and shown, but saving it to a file failed.
    RecoveryKeyNotSaved,
    /// Anything else.
    Failed,
}

/// How far this account trusts the other person in a direct-message room.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContactTrust {
    /// Not known yet.
    Unknown,
    /// The contact has no cross-signing identity; they receive no keys
    /// (ADR-0005).
    NoIdentity,
    /// The contact has an identity this account has not verified.
    Unverified,
    /// This account verified the contact's identity.
    Verified,
    /// The contact's identity changed since this account first saw it, and
    /// the user has not accepted the new one: sending to them is paused
    /// (ADR-0007).
    Changed,
    /// The contact's identity changed after this account verified it:
    /// verify them again or withdraw the verification.
    VerifiedChanged,
}

/// A room member whose cross-signing identity changed (ADR-0007).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IdentityChange {
    /// A valid Matrix user ID (printable ASCII, at most 255 bytes), so the
    /// page can name it back to the core; untrusted text all the same.
    pub user_id: String,
    /// This account had verified the old identity.
    pub was_verified: bool,
}

/// Reply of `identity_changes`: the members of a room whose identity changed.
/// While any is listed, nothing is sent in the room.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomIdentityChanges {
    pub room_id: String,
    pub changes: Vec<IdentityChange>,
    /// More members changed than are listed.
    pub truncated: bool,
}

/// Reply of `contact_status`: the other person in a direct-message room.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ContactStatus {
    pub room_id: String,
    /// Untrusted, projected like a name.
    pub user_id: String,
    pub trust: ContactTrust,
}

/// Where a verification with another of the account's devices is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPhase {
    /// Another device asked to verify; this one can accept or decline.
    Incoming,
    /// Waiting for the other device.
    Waiting,
    /// Compare the emoji with the other device.
    Compare,
    /// This device confirmed; waiting for the other one.
    Confirmed,
    Done,
    Cancelled,
}

/// One emoji of the comparison, from the fixed table of the Matrix
/// specification. Public comparison data, not a secret.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SasEmoji {
    pub symbol: String,
    pub description: String,
}

/// A verification with another device of the same account, or with a
/// contact. The page acts on it by `flow_id`; the core refuses an action for
/// any other flow.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VerificationView {
    pub flow_id: String,
    pub phase: VerificationPhase,
    /// Seven emoji once they can be compared, otherwise empty.
    pub emoji: Vec<SasEmoji>,
    /// The other device's name as it reported it (untrusted, projected).
    pub other_device: Option<String>,
    /// The contact's user ID when this verifies another person (untrusted,
    /// projected); none when it verifies this device.
    pub contact: Option<String>,
}

/// Reply of `security_status` and the encryption commands. Codes only: never
/// a key, password or recovery key.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SecurityStatus {
    pub identity: IdentityState,
    pub recovery: RecoveryState,
    pub operation: SecurityOperation,
    pub error: Option<SecurityError>,
    pub verification: Option<VerificationView>,
    /// This device can set up recovery alone (it holds the private keys of a
    /// published identity whose recovery is not set up).
    pub can_set_up_recovery: bool,
}

impl SecurityStatus {
    pub fn unknown() -> Self {
        Self {
            identity: IdentityState::Unknown,
            recovery: RecoveryState::Unknown,
            operation: SecurityOperation::Idle,
            error: None,
            verification: None,
            can_set_up_recovery: false,
        }
    }
}

pub fn sample_room_summary() -> RoomSummary {
    RoomSummary {
        room_id: "!example:server.invalid".into(),
        display_name: None,
        unread_count: u32::MAX,
        encryption: RoomEncryption::Encrypted,
        contact: Some("@alice:server.invalid".into()),
    }
}

pub fn sample_timeline_page() -> TimelinePage {
    TimelinePage {
        room_id: "!example:server.invalid".into(),
        items: vec![
            TimelineItem::Message {
                event_id: "$event".into(),
                sender: "@alice:server.invalid".into(),
                body: "hostile <img src=x onerror=alert(1)>".into(),
                origin_server_ts: UnixMillis::new(1_767_258_000_000),
                reply_to: None,
            },
            TimelineItem::Notice {
                event_id: "$encrypted".into(),
                sender: "@bob:server.invalid".into(),
                notice: NoticeKind::Undecryptable,
                origin_server_ts: None,
            },
            TimelineItem::Image {
                event_id: "$image".into(),
                sender: "@alice:server.invalid".into(),
                origin_server_ts: UnixMillis::new(1_767_258_060_000),
                name: "cat <script>.png".into(),
                size: Some(48_213),
                width: Some(640),
                height: Some(480),
                encrypted: true,
            },
            TimelineItem::State {
                event_id: "$joined".into(),
                sender: "@bob:server.invalid".into(),
                origin_server_ts: UnixMillis::new(1_767_258_090_000),
                change: StateChange::Member {
                    target: "@bob:server.invalid".into(),
                    membership: MembershipChange::Joined,
                },
            },
            TimelineItem::Gap { next_cursor: None },
        ],
        next_cursor: Some("cursor-2".into()),
    }
}

pub fn sample_image_view() -> ImageView {
    ImageView {
        handle: "0123456789abcdef0123456789abcdef".into(),
        width: 2048,
        height: 1536,
        bands: 12,
        band_rows: 128,
    }
}

pub fn sample_image_pick() -> ImagePick {
    ImagePick::Draft {
        preview: ImageView {
            handle: "fedcba9876543210fedcba9876543210".into(),
            width: 640,
            height: 480,
            bands: 1,
            band_rows: 409,
        },
        size: 48_213,
        mime: ImageMime::Jpeg,
    }
}

pub fn sample_pending_image() -> PendingMessage {
    PendingMessage {
        local_id: 3,
        room_id: "!example:server.invalid".into(),
        kind: PendingKind::Image,
        body: String::new(),
        state: PendingState::Sending,
        event_id: None,
        failure: None,
        queued_at: UnixMillis::saturating(1_767_258_120_000),
    }
}

pub fn generated_typescript() -> String {
    let config = Config::default();
    let mut output = [
        format!("export {}", StoreState::decl(&config)),
        format!("export {}", AccountState::decl(&config)),
        format!("export {}", SignInError::decl(&config)),
        format!("export {}", AccountStatus::decl(&config)),
        format!("export {}", AppStatus::decl(&config)),
        format!("export {}", IdentityState::decl(&config)),
        format!("export {}", RecoveryState::decl(&config)),
        format!("export {}", SecurityOperation::decl(&config)),
        format!("export {}", SecurityError::decl(&config)),
        format!("export {}", VerificationPhase::decl(&config)),
        format!("export {}", SasEmoji::decl(&config)),
        format!("export {}", VerificationView::decl(&config)),
        format!("export {}", SecurityStatus::decl(&config)),
        format!("export {}", ContactTrust::decl(&config)),
        format!("export {}", ContactStatus::decl(&config)),
        format!("export {}", IdentityChange::decl(&config)),
        format!("export {}", RoomIdentityChanges::decl(&config)),
        format!("export {}", RoomEncryption::decl(&config)),
        format!("export {}", RoomSummary::decl(&config)),
        format!("export {}", PendingState::decl(&config)),
        format!("export {}", SendFailure::decl(&config)),
        format!("export {}", PendingKind::decl(&config)),
        format!("export {}", PendingMessage::decl(&config)),
        format!("export {}", RoomList::decl(&config)),
        format!("export {}", InviteSummary::decl(&config)),
        format!("export {}", InviteList::decl(&config)),
        format!("export {}", ConversationStart::decl(&config)),
        format!("export {}", RoomCreation::decl(&config)),
        format!("export {}", RoomInvite::decl(&config)),
        format!("export {}", JoinRule::decl(&config)),
        format!("export {}", HistoryVisibility::decl(&config)),
        format!("export {}", RoomNotifications::decl(&config)),
        format!("export {}", LevelKey::decl(&config)),
        format!("export {}", RoomLevel::decl(&config)),
        format!("export {}", RoomPermissions::decl(&config)),
        format!("export {}", RoomSettings::decl(&config)),
        format!("export {}", Membership::decl(&config)),
        format!("export {}", RoomMember::decl(&config)),
        format!("export {}", RoomMembers::decl(&config)),
        format!("export {}", RoomChange::decl(&config)),
        format!("export {}", DisplayName::decl(&config)),
        format!("export {}", DeviceSummary::decl(&config)),
        format!("export {}", DeviceList::decl(&config)),
        format!("export {}", AccountSettings::decl(&config)),
        format!("export {}", RoomActivity::decl(&config)),
        format!("export {}", NotificationLevel::decl(&config)),
        format!("export {}", NotificationSettings::decl(&config)),
        format!("export {}", ThreePidMedium::decl(&config)),
        format!("export {}", ThreePid::decl(&config)),
        format!("export {}", ThreePidList::decl(&config)),
        format!("export {}", AccountChange::decl(&config)),
        format!("export {}", ProfileSlot::decl(&config)),
        format!("export {}", ProfileList::decl(&config)),
        format!("export {}", UserMatch::decl(&config)),
        format!("export {}", UserSearch::decl(&config)),
        format!("export {}", NoticeKind::decl(&config)),
        format!("export {}", MembershipChange::decl(&config)),
        format!("export {}", StateChange::decl(&config)),
        format!("export {}", TimelineItem::decl(&config)),
        format!("export {}", TimelinePage::decl(&config)),
        format!("export {}", ImageMime::decl(&config)),
        format!("export {}", ImageView::decl(&config)),
        format!("export {}", ImagePick::decl(&config)),
    ]
    .join("\n\n")
        + "\n\n";
    output.push_str(&format!(
        "export const roomSummaryFixture = {} as const satisfies RoomSummary;\n\n",
        serde_json::to_string_pretty(&sample_room_summary()).expect("DTO serializes")
    ));
    output.push_str(&format!(
        "export const timelinePageFixture = {} as const satisfies TimelinePage;\n",
        serde_json::to_string_pretty(&sample_timeline_page()).expect("DTO serializes")
    ));
    output
}
