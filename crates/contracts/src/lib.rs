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

/// A `u64` field that JavaScript holds exactly: at most [`UnixMillis::MAX`].
/// A larger value fails to deserialize.
fn safe_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value <= UnixMillis::MAX {
        Ok(value)
    } else {
        Err(D::Error::custom("number out of JavaScript's exact range"))
    }
}

/// The highest call playout volume, in percent: the sound as received
/// (ADR-0055).
pub const CALL_VOLUME_MAX: u8 = 100;

fn call_volume<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = u8::deserialize(deserializer)?;
    if value <= CALL_VOLUME_MAX {
        Ok(value)
    } else {
        Err(D::Error::custom("call volume over 100 percent"))
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
    /// Names the room's current picture (`m.room.avatar`) without saying
    /// where it is: [`PICTURE_ID_BYTES`] lowercase hex characters that change
    /// when the picture changes. None when the room has no picture. The page
    /// loads the picture only through `load_room_picture` (ADR-0041).
    pub picture_id: Option<String>,
    /// Another device is in this room's call now (ADR-0052). A flag only:
    /// no names.
    #[serde(default)]
    pub has_call: bool,
}

/// The length of a [`RoomSummary::picture_id`], in hex characters.
pub const PICTURE_ID_BYTES: usize = 16;

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
    /// One file; `body` is its name (ADR-0029).
    File,
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
    /// An `m.file`, `m.audio` or `m.video` event (ADR-0029). Nothing is
    /// fetched, played or shown until the user saves it (`save_file`).
    /// `name` is made safe for a file system and projected like a name;
    /// `size` is the sender's claim.
    File {
        event_id: String,
        sender: String,
        /// As for messages.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
        name: String,
        size: Option<u32>,
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
    /// A member set, changed or removed their picture in this room, and
    /// kept their display name.
    ProfilePicture {
        target: String,
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
    /// The room's pinned messages changed.
    PinnedMessages {},
    /// Guests may now join (`allowed`), or may no longer.
    GuestAccess {
        allowed: bool,
    },
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

/// Reply of `load_room_picture`: the room's picture, decoded in the
/// contained worker and scaled to fit [`ROOM_PICTURE_SIDE`] square, under an
/// opaque handle like a loaded image. `picture_id` names the picture that was
/// loaded, which may differ from the room summary's when the picture changed
/// meanwhile (ADR-0041).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomPicture {
    pub picture_id: String,
    pub image: ImageView,
}

/// The largest side of a loaded room picture, in pixels.
pub const ROOM_PICTURE_SIDE: u32 = 96;

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
    /// The display names of the senders and member-change targets of
    /// `items` whose member event the core found, at most one entry per
    /// user ID. A user ID not listed has an unknown display name, which is
    /// not the same as an entry without one.
    pub names: Vec<SenderName>,
}

/// A user's display name in a room, projected like other names. Untrusted:
/// anyone can choose any name, so the page shows the user ID beside it.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SenderName {
    pub user_id: String,
    /// `None` when the user's member event shows no display name. An unknown
    /// name is a user ID not listed at all, never an entry.
    pub display_name: Option<String>,
}

/// Reply of the `room_list` command. The core caps the number of rooms;
/// `truncated` says whether more exist than were sent.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomList {
    pub rooms: Vec<RoomSummary>,
    pub truncated: bool,
}

/// Text kept in the protected profile until it is sent or cleared.
#[derive(Clone, Debug, Default, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomDraft {
    pub body: String,
    pub reply_to: Option<String>,
}

/// This profile's room organisation. The section is untrusted text.
#[derive(Clone, Debug, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomPreference {
    pub room_id: String,
    pub favourite: bool,
    pub section: Option<String>,
}

/// A present string or JSON null. Unlike Option, this wire type refuses a
/// missing field. Keep that distinction at the untrusted request boundary.
#[derive(Deserialize)]
#[serde(untagged)]
enum RequiredNullable<T> {
    Value(T),
    Null,
}

type RequiredNullableString = RequiredNullable<String>;

impl<T> RequiredNullable<T> {
    /// Convert the validated wire value to the application's optional value.
    fn into_option(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Null => None,
        }
    }
}

impl<'de> Deserialize<'de> for RoomDraft {
    /// Require replyTo even when null, and reject unknown or duplicate fields.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            body: String,
            reply_to: RequiredNullableString,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            body: input.body,
            reply_to: input.reply_to.into_option(),
        })
    }
}

impl<'de> Deserialize<'de> for RoomPreference {
    /// Require section even when null, and reject unknown or duplicate fields.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            room_id: String,
            favourite: bool,
            section: RequiredNullableString,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            room_id: input.room_id,
            favourite: input.favourite,
            section: input.section.into_option(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomPreferences {
    pub rooms: Vec<RoomPreference>,
}

/// A match in the local message cache. All text is untrusted.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LocalSearchHit {
    pub room_id: String,
    pub event_id: String,
    pub sender: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LocalSearchResults {
    pub results: Vec<LocalSearchHit>,
    pub truncated: bool,
    pub scanned: u32,
}

/// Optional exact sender and original-event time range. All fields are required
/// on the wire, with null meaning no filter. The upper time bound is exclusive.
#[derive(Clone, Debug, Default, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LocalSearchFilters {
    pub sender: Option<String>,
    #[ts(type = "number | null")]
    pub from_ms: Option<UnixMillis>,
    #[ts(type = "number | null")]
    pub before_ms: Option<UnixMillis>,
}

impl<'de> Deserialize<'de> for LocalSearchFilters {
    /// Preserve required-nullable fields at the untrusted command boundary.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            sender: RequiredNullableString,
            from_ms: RequiredNullable<UnixMillis>,
            before_ms: RequiredNullable<UnixMillis>,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            sender: input.sender.into_option(),
            from_ms: input.from_ms.into_option(),
            before_ms: input.before_ms.into_option(),
        })
    }
}

/// A bounded context window. Its cursor resumes before the oldest retained item.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FocusedTimeline {
    pub page: TimelinePage,
    pub event_id: String,
}

/// Shared room pin references and the displayable content available to this
/// device. A reference can be unavailable or hidden by local ignore policy.
#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PinnedMessages {
    pub room_id: String,
    pub event_ids: Vec<String>,
    pub items: Vec<TimelineItem>,
    pub can_manage: bool,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    #[default]
    All,
    Images,
    Files,
}

/// A filtered history window, not a complete media index. Media bytes and URLs
/// remain in the core; existing explicit load/save commands retrieve content.
#[derive(Clone, Debug, Serialize, TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttachmentPage {
    pub room_id: String,
    pub items: Vec<TimelineItem>,
    pub next_cursor: Option<String>,
    pub scanned: u32,
    pub partial: bool,
}

impl<'de> Deserialize<'de> for AttachmentPage {
    /// Require an explicit cursor or null, so a missing field cannot hide a page.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            room_id: String,
            items: Vec<TimelineItem>,
            next_cursor: RequiredNullableString,
            scanned: u32,
            partial: bool,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            room_id: input.room_id,
            items: input.items,
            next_cursor: input.next_cursor.into_option(),
            scanned: input.scanned,
            partial: input.partial,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    #[default]
    Unknown,
    Sliding,
    Classic,
}

/// Observations from existing sync work, with no diagnostic payloads, tokens,
/// paths or message data. Computer-clock timestamps do not establish freshness.
#[derive(Clone, Debug, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConnectionHealth {
    pub account_state: AccountState,
    pub store_state: StoreState,
    pub sync_mode: SyncMode,
    #[ts(type = "number | null")]
    pub last_attempt_at: Option<UnixMillis>,
    #[ts(type = "number | null")]
    pub last_success_at: Option<UnixMillis>,
    pub consecutive_failures: u32,
    pub retry_delay_ms: u32,
    pub last_error: Option<SignInError>,
}

impl<'de> Deserialize<'de> for ConnectionHealth {
    /// A missing observation is explicit null, not a missing protocol field.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            account_state: AccountState,
            store_state: StoreState,
            sync_mode: SyncMode,
            last_attempt_at: RequiredNullable<UnixMillis>,
            last_success_at: RequiredNullable<UnixMillis>,
            consecutive_failures: u32,
            retry_delay_ms: u32,
            last_error: RequiredNullable<SignInError>,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            account_state: input.account_state,
            store_state: input.store_state,
            sync_mode: input.sync_mode,
            last_attempt_at: input.last_attempt_at.into_option(),
            last_success_at: input.last_success_at.into_option(),
            consecutive_failures: input.consecutive_failures,
            retry_delay_ms: input.retry_delay_ms,
            last_error: input.last_error.into_option(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IgnoredUsers {
    pub users: Vec<String>,
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

/// Reply of `proxy_setting`: the profile's SOCKS5 proxy as `host:port`, or
/// none (ADR-0028).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProxySetting {
    pub proxy: Option<String>,
}

/// Reply of `discover_homeserver`: the homeserver found for a server name,
/// confirmed to speak Matrix, and whether a `.well-known` file named it
/// (ADR-0027). The page shows it before signing in to it.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HomeserverFound {
    pub homeserver: String,
    pub from_well_known: bool,
}

/// A session of this account the user has not been shown yet: its device
/// ID and name, projected like names (ADR-0026).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewSession {
    pub device_id: String,
    pub display_name: Option<String>,
}

/// Reply of `new_sessions`: at most ten; `truncated` says more exist.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewSessions {
    pub sessions: Vec<NewSession>,
    pub truncated: bool,
}

/// Whether a module loaded into Persephone has a signature that verifies.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSignature {
    Valid,
    /// No embedded or catalog signature.
    Unsigned,
    /// Signed, but the signature does not verify.
    Invalid,
    /// The platform has no signature to check (Linux).
    NotChecked,
}

/// Why a module is in the report (ADR-0047).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ModuleReason {
    /// Its signature is not an expected one (Windows, macOS).
    Signer,
    /// Named in `LD_PRELOAD` or `/etc/ld.so.preload` (Linux).
    Preloaded,
    /// A user other than root can change the file, or a folder above it
    /// (Linux).
    UserWritable,
    /// Owned by root, but not in a system folder or in Persephone's own
    /// read-only image (Linux).
    OutsideSystem,
    /// Executable memory of the core with no file on disk, or a file deleted
    /// since it was loaded (Linux).
    NoFile,
}

/// The display server the window is drawn through on Linux, for the capture
/// note in Settings (ADR-0047). It is information only: Persephone cannot
/// hide its window from capture on Linux.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisplaySession {
    /// A Wayland compositor: other apps capture the screen only through the
    /// compositor or the screen-cast portal, which asks the user.
    Wayland,
    /// An X11 server, or XWayland: any X11 client can read the window.
    X11,
    /// Not Linux, or neither could be found.
    Unknown,
}

/// A Persephone process a module is loaded in. The order is the display
/// order.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ModuleProcess {
    /// The core, which holds the keys.
    Core,
    /// The keyless UI host.
    Ui,
    /// A web view process of the UI (WebView2, WebKitGTK).
    WebView,
}

/// Whether the core has listed the modules yet.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleScanState {
    /// The first scan has not finished.
    Pending,
    Ready,
    /// This platform cannot list modules.
    Unsupported,
}

/// A module (DLL or EXE) loaded into Persephone that is not an expected one.
/// Text fields are projected like names (ADR-0026); the file name and signer
/// are at most 128 characters.
#[derive(Clone, Debug, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ThirdPartyModule {
    /// Opaque acknowledgement key, at most `MAX_MODULE_ID_BYTES`.
    pub id: String,
    pub file_name: String,
    /// The full path, at most `MAX_MODULE_PATH_BYTES`, cut with "…".
    pub path: String,
    /// The verified signer's simple name. Only for `Valid`; otherwise null.
    pub signer: Option<String>,
    pub signature: ModuleSignature,
    pub reason: ModuleReason,
    /// The processes it is loaded in: sorted, unique, one to three.
    pub processes: Vec<ModuleProcess>,
    /// When this core process first saw it.
    #[ts(type = "number")]
    pub first_seen: UnixMillis,
    /// The user marked it as reviewed.
    pub acknowledged: bool,
}

impl<'de> Deserialize<'de> for ThirdPartyModule {
    /// A missing signer is explicit null, not a missing protocol field.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            id: String,
            file_name: String,
            path: String,
            signer: RequiredNullableString,
            signature: ModuleSignature,
            reason: ModuleReason,
            processes: Vec<ModuleProcess>,
            first_seen: UnixMillis,
            acknowledged: bool,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            id: input.id,
            file_name: input.file_name,
            path: input.path,
            signer: input.signer.into_option(),
            signature: input.signature,
            reason: input.reason,
            processes: input.processes,
            first_seen: input.first_seen,
            acknowledged: input.acknowledged,
        })
    }
}

/// Reply of `third_party_modules` and `acknowledge_modules`: the modules in
/// the report, at most `MAX_THIRD_PARTY_MODULES`: unacknowledged first, then
/// by lowercase file name, then by path. `truncated` says more exist.
/// `incomplete` says the last scan could not read every process, or every
/// module of one, so code can be loaded that the list does not show.
#[derive(Clone, Debug, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ThirdPartyModules {
    pub state: ModuleScanState,
    /// Null until the first scan finishes.
    #[ts(type = "number | null")]
    pub scanned_at: Option<UnixMillis>,
    pub modules: Vec<ThirdPartyModule>,
    pub truncated: bool,
    pub incomplete: bool,
}

impl<'de> Deserialize<'de> for ThirdPartyModules {
    /// A missing scan time is explicit null, not a missing protocol field.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Input {
            state: ModuleScanState,
            scanned_at: RequiredNullable<UnixMillis>,
            modules: Vec<ThirdPartyModule>,
            truncated: bool,
            incomplete: bool,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            state: input.state,
            scanned_at: input.scanned_at.into_option(),
            modules: input.modules,
            truncated: input.truncated,
            incomplete: input.incomplete,
        })
    }
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
    /// Lock the profile after this many minutes without keyboard or mouse
    /// input on this computer, from 1 to [`IDLE_LOCK_MINUTES_MAX`]; 0 never
    /// does. Off (0) by default (ADR-0025).
    pub idle_lock_minutes: u16,
    /// Desktop notifications name the sender and room; otherwise they say
    /// only that a message arrived, and no name leaves the core. On by
    /// default (ADR-0022).
    pub notification_names: bool,
    /// Load room pictures as soon as rooms are listed; otherwise each is
    /// loaded only when the user asks. Loading tells the homeserver, and the
    /// server that holds the picture, that this account looked at it. Off by
    /// default (ADR-0041).
    pub load_room_pictures: bool,
    /// Hide the main window from screenshots, screen recording and screen
    /// sharing that use the Windows capture APIs. On by default. Windows only;
    /// before Windows 10 version 2004 a capture shows it black (ADR-0045).
    pub capture_protection: bool,
    /// Which kinds of room changes the timeline shows. All on by default.
    pub timeline_events: TimelineEvents,
    /// The colours of the interface. Grey and orange by default.
    pub theme: Theme,
}

/// The colour themes of the interface, all dark. Only the page reads the
/// choice: it changes no other behaviour. Read only from one of the three
/// names (see the `Deserialize` impl).
#[derive(Clone, Copy, Debug, Default, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Dark grey surfaces with an orange accent.
    #[default]
    Orange,
    /// Nearly black surfaces with a red accent.
    Red,
    /// Nearly black surfaces with a blue accent.
    Blue,
}

/// A theme is a string: one of the three names. A derived enum would also
/// accept the object form, such as `{"red":null}`, from `serde_json`.
impl<'de> Deserialize<'de> for Theme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        const NAMES: &[&str] = &["orange", "red", "blue"];
        let name = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        match name.as_ref() {
            "orange" => Ok(Self::Orange),
            "red" => Ok(Self::Red),
            "blue" => Ok(Self::Blue),
            other => Err(serde::de::Error::unknown_variant(other, NAMES)),
        }
    }
}

/// Which kinds of room changes the timeline shows, one switch each. Only
/// the page reads them: the core sends every change either way.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimelineEvents {
    pub joins: bool,
    pub leaves: bool,
    /// Invitations, declined invitations and withdrawn invitations.
    pub invitations: bool,
    /// Members removed, banned or unbanned by someone else.
    pub removals: bool,
    /// Requests to join, withdrawn requests and refused requests.
    pub join_requests: bool,
    pub display_names: bool,
    pub profile_pictures: bool,
    pub room_name: bool,
    pub topic: bool,
    pub room_picture: bool,
    pub encryption: bool,
    pub join_rule: bool,
    pub history_visibility: bool,
    /// Who may do what (power levels).
    pub permissions: bool,
    pub addresses: bool,
    pub pinned_messages: bool,
    pub guest_access: bool,
    /// Which homeservers may take part (server ACL).
    pub server_access: bool,
    pub creation: bool,
    pub upgrades: bool,
}

impl Default for TimelineEvents {
    fn default() -> Self {
        Self {
            joins: true,
            leaves: true,
            invitations: true,
            removals: true,
            join_requests: true,
            display_names: true,
            profile_pictures: true,
            room_name: true,
            topic: true,
            room_picture: true,
            encryption: true,
            join_rule: true,
            history_visibility: true,
            permissions: true,
            addresses: true,
            pinned_messages: true,
            guest_access: true,
            server_access: true,
            creation: true,
            upgrades: true,
        }
    }
}

/// The smallest text size either setting accepts, in percent.
pub const TEXT_PERCENT_MIN: u16 = 80;
/// The largest message text size, in percent.
pub const CHAT_TEXT_PERCENT_MAX: u16 = 200;
/// The longest idle time the idle lock accepts, in minutes (a day).
pub const IDLE_LOCK_MINUTES_MAX: u16 = 24 * 60;

/// The largest interface text size, in percent: the whole interface grows
/// with it, so it stops before the window's minimum size stops fitting.
pub const INTERFACE_TEXT_PERCENT_MAX: u16 = 150;

impl AccountSettings {
    /// Whether every setting is within its bounds.
    pub fn within_bounds(&self) -> bool {
        (TEXT_PERCENT_MIN..=CHAT_TEXT_PERCENT_MAX).contains(&self.chat_text_percent)
            && (TEXT_PERCENT_MIN..=INTERFACE_TEXT_PERCENT_MAX)
                .contains(&self.interface_text_percent)
            && self.idle_lock_minutes <= IDLE_LOCK_MINUTES_MAX
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
            idle_lock_minutes: 0,
            notification_names: true,
            load_room_pictures: false,
            capture_protection: true,
            timeline_events: TimelineEvents::default(),
            theme: Theme::Orange,
        }
    }
}

/// One homeserver with members in a room: its name, projected like a name,
/// how many members it has (joined or invited), and whether it is this
/// account's own.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomServer {
    pub name: String,
    pub members: u32,
    pub own: bool,
}

/// Reply of `room_servers`: the homeservers that receive a room's messages
/// and metadata, the most members first, at most 50; `truncated` says more
/// exist.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomServers {
    pub servers: Vec<RoomServer>,
    pub truncated: bool,
}

/// One member of an encrypted room and the devices that can read its new
/// messages: under ADR-0005 room keys go only to devices their owner
/// cross-signed (`receiving`), never to the others (`refused`). From this
/// computer's crypto store, which may lag behind a device just added.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomReader {
    pub user_id: String,
    pub trust: ContactTrust,
    pub receiving: u32,
    pub refused: u32,
    /// This account: its other devices read the room too.
    pub own: bool,
}

/// Reply of `room_readers`: who can read a room's new messages, at most 100
/// members by user ID; `truncated` says more exist.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoomReaders {
    pub readers: Vec<RoomReader>,
    pub truncated: bool,
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

/// One room from a public room directory. Everything here is the
/// directory's word: names and the topic are projected like any name or
/// message body, and IDs are sent unchanged only when they are valid.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PublicRoom {
    pub room_id: String,
    pub name: Option<String>,
    /// The room's main address, such as `#rust:example.org`.
    pub alias: Option<String>,
    /// Cut short by the core to at most 500 characters.
    pub topic: Option<String>,
    /// The directory's count of joined members.
    pub members: u32,
    /// Anyone, joined or not, may read the room's history.
    pub world_readable: bool,
    /// Guest accounts may join.
    pub guests_can_join: bool,
    /// The room lets anyone join, so `join_public_room` may join it. A room
    /// that asks to knock, or only lets in members of another room, is not.
    pub joinable: bool,
    /// This account has already joined the room.
    pub joined: bool,
}

/// Reply of `public_rooms`: one page of a public room directory, in the
/// directory's order.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PublicRoomPage {
    pub rooms: Vec<PublicRoom>,
    /// The token for the next page, or none on the last page.
    pub next: Option<String>,
    /// The directory's estimate of how many rooms it lists, if it gave one.
    pub total: Option<u32>,
}

/// Reply of `join_public_room`: the room joined.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct JoinedRoom {
    pub room_id: String,
}

/// State of the local encrypted store. Codes only: no paths, key material or
/// error text reach the UI.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StoreState {
    /// No profile was given; nothing is stored.
    NotConfigured,
    /// The store is open with its key (unsealed by the profile passphrase
    /// when the profile has one, ADR-0050).
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
    /// The homeserver has no OAuth 2.0 provider to sign in with, or it
    /// refused to register Persephone (ADR-0030).
    OidcUnavailable,
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
    /// Signed out because the homeserver ended the session: it was signed
    /// out from another device, or the account was reset, deactivated or
    /// removed. This device's local profile is deleted as for a sign-out.
    SessionEnded,
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
    /// The core's own window is asking for the profile passphrase, to turn
    /// it on or off (ADR-0050).
    WaitingForPassphrase,
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
    /// The homeserver already holds a key backup whose key this device does
    /// not have, so recovery was not set up over it.
    BackupExistsOnServer,
    /// The recovery key verified this device, but the key backup's key it
    /// holds is missing or does not match the homeserver's backup.
    BackupKeyUnavailable,
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
    /// Too many wrong entries of the current profile passphrase, so the
    /// passphrase was not turned off.
    PassphraseRejected,
    /// The profile's key file or the OS vault failed, so the passphrase
    /// setting did not change.
    ProfileUnavailable,
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
    /// This verified device holds the account's private keys, but cannot use
    /// a key backup: the homeserver's backup has a key it lacks, or recovery
    /// is incomplete. It may delete that backup and start a new one, after a
    /// confirmation in the core's own window (ADR-0040).
    pub can_start_new_backup: bool,
    /// The local profile asks for a passphrase before it opens (ADR-0050).
    /// Off by default.
    pub profile_passphrase: bool,
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
            can_start_new_backup: false,
            profile_passphrase: false,
        }
    }
}

/// Why a call cannot start in a room (ADR-0043).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallUnavailable {
    /// This computer has no reviewed call containment.
    UnsupportedPlatform,
    /// The room is unknown or not joined.
    NotJoined,
    /// A call never runs without end-to-end encryption.
    NotEncrypted,
    /// This user's power level is below what the room requires to send the
    /// call member state event, so the call could not join.
    NotPermitted,
    /// The homeserver offers no call server.
    NoCallServer,
    /// The homeserver offers a call server, but none that Persephone can
    /// use (ADR-0056).
    CallServerUnsupported,
    /// The homeserver could not be asked.
    ServerUnreachable,
}

/// Reply of `call_readiness`. Codes and a count only.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallReadiness {
    pub available: bool,
    /// Why not, when `available` is false.
    pub reason: Option<CallUnavailable>,
    /// Other devices in this room's call, at most [`CALL_PARTICIPANTS_MAX`].
    /// Reported also when the call is unavailable.
    pub active_members: u8,
}

/// Most other devices a call shows.
pub const CALL_PARTICIPANTS_MAX: usize = 32;

/// Where the call is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallPhase {
    Idle,
    Joining,
    Active,
    Ended,
}

/// Another device in the call. The IDs are validated Matrix IDs (untrusted,
/// shown as text).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallParticipant {
    pub user_id: String,
    pub device_id: String,
    pub speaking: bool,
    /// The call is connected to the call server it publishes to: the own
    /// one, or another one the user agreed to connect to (ADR-0054).
    /// Otherwise it cannot be heard or seen.
    pub reachable: bool,
    /// This device holds a key for it. Without one it cannot be heard.
    pub keyed: bool,
    /// It shares its screen (ADR-0053). The sender's label is only a
    /// display hint; shown as text.
    pub sharing: bool,
}

/// Why a call ended.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallEnd {
    Left,
    ConnectionFailed,
    AudioDevice,
    Containment,
    WorkerStopped,
    /// The call process ended with an internal error (a panic or an abort).
    WorkerCrashed,
    MembershipLost,
    KeysUnavailable,
    SessionEnded,
    Internal,
    /// The chosen microphone or speaker is not connected.
    DeviceNotFound,
}

/// Why a join was refused or failed.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallJoinError {
    AlreadyInCall,
    NotJoined,
    NotEncrypted,
    NoCallServer,
    /// The homeserver offers a call server, but none that Persephone can
    /// use (ADR-0056).
    CallServerUnsupported,
    /// The profile has a proxy and the user did not allow a direct call.
    Declined,
    UnsupportedPlatform,
    Containment,
    NoWorker,
    NoDelayedEvents,
    Membership,
    CallServer,
    Keys,
    ProxyUnreadable,
    /// The saved call devices could not be read; the call never falls back
    /// to the defaults.
    DeviceSettingUnreadable,
}

/// One microphone, speaker or camera. The name is untrusted operating system text,
/// shown as plain text only. The ID is the platform's device ID, kept and
/// returned unchanged.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallDevice {
    pub id: String,
    pub name: String,
}

/// Reply of `call_devices`: the devices now connected and the saved choice
/// (ADR-0043, ADR-0048). `supported` is false where calls have no reviewed
/// containment; the lists are then empty. A saved choice may name a device
/// that is not connected now. `unreadable` is true when the saved record
/// could not be read; all choices are then null and the volume is 100.
/// `volume` is the saved playout volume, 0 to [`CALL_VOLUME_MAX`]
/// (ADR-0055).
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallDevices {
    pub supported: bool,
    pub recording: Vec<CallDevice>,
    pub playout: Vec<CallDevice>,
    pub cameras: Vec<CallDevice>,
    pub selected_recording: Option<String>,
    pub selected_playout: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub selected_camera: Option<String>,
    #[serde(deserialize_with = "call_volume")]
    pub volume: u8,
    pub unreadable: bool,
}

/// Most video tiles a call shows: the own camera, four remote cameras and
/// the one remote screen share (ADR-0053).
pub const CALL_VIDEO_TILES_MAX: usize = 6;

/// The camera, as the page shows it (ADR-0048).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallCamera {
    Off,
    /// Asked to start; the worker has not answered yet.
    Starting,
    On,
    /// The chosen camera is not connected.
    NotFound,
    /// No camera, the OS denied it, it is in use, or capture failed.
    Unavailable,
}

/// The own screen share, as the page shows it (ADR-0053).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallScreen {
    Off,
    /// Asked to start; the core's chooser or warning, the portal or the
    /// worker has not answered yet.
    Starting,
    On,
    /// The chosen screen is gone or has another size.
    NotFound,
    /// Screen sharing cannot run here (this OS, no portal, or capture
    /// failed).
    Unavailable,
    /// The OS or the portal refused the capture.
    Refused,
}

/// A remote device, by the same IDs [`CallParticipant`] carries.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallParticipantRef {
    pub user_id: String,
    pub device_id: String,
}

/// One video tile. Tile 0 is the own camera (`own` is true and
/// `participant` is null). Tiles 1 to 4 show a remote camera. Tile 5 shows
/// the one decoded remote screen share (ADR-0053). The picture itself comes
/// from `call_video_frame`.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallVideoTile {
    /// 0 to 5.
    pub tile: u8,
    #[serde(deserialize_with = "Option::deserialize")]
    pub participant: Option<CallParticipantRef>,
    pub own: bool,
    /// The tile's stream number. It changes when the tile starts showing a
    /// new stream; `call_video_frame` gives pictures of this stream only.
    /// At most 2^53 - 1, so the page holds it exactly.
    #[serde(deserialize_with = "safe_u64")]
    #[ts(type = "number")]
    pub stream: u64,
}

/// Reply of every call command except `call_readiness`. Codes, bounded IDs
/// and flags only: never a key, token, URL or peer text.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallStatus {
    pub phase: CallPhase,
    pub room_id: Option<String>,
    /// The microphone is muted. Always true while `deafened`.
    pub muted: bool,
    /// All remote call audio is off and the microphone is muted (ADR-0055).
    pub deafened: bool,
    /// The playout volume of the call, 0 to [`CALL_VOLUME_MAX`] percent. 100
    /// when there is no call.
    #[serde(deserialize_with = "call_volume")]
    pub volume: u8,
    /// At most [`CALL_PARTICIPANTS_MAX`], other devices only.
    pub participants: Vec<CallParticipant>,
    /// Set while `phase` is `ended`.
    pub ended: Option<CallEnd>,
    /// Set when this reply refuses a join.
    pub join_error: Option<CallJoinError>,
    pub camera: CallCamera,
    /// The own screen share (ADR-0053).
    pub screen: CallScreen,
    /// The active video tiles, at most [`CALL_VIDEO_TILES_MAX`].
    pub video: Vec<CallVideoTile>,
    /// The other member of a direct chat declined this call's ring
    /// (ADR-0052). The call goes on until the user leaves.
    #[serde(default)]
    pub ring_declined: bool,
}

/// Most incoming rings at once (ADR-0052).
pub const CALL_RINGS_MAX: usize = 4;

/// What the caller meant: a soft hint for the camera.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallIntent {
    Audio,
    Video,
}

/// One incoming call ring (ADR-0052). `caller` is a validated user ID; the
/// page shows it as inert text. `seconds_left` is whole seconds until the
/// ring ends, at least 1 and at most 120.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallRing {
    /// A number the core gives each ring, so a new ring with the same room
    /// and caller can be told apart. It names nothing in Matrix.
    pub ring_id: u32,
    pub room_id: String,
    pub caller: String,
    pub intent: CallIntent,
    pub seconds_left: u32,
}

/// Reply of `call_rings` and `call_decline_ring`: the rings now running,
/// newest first, at most [`CALL_RINGS_MAX`].
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallRings {
    pub rings: Vec<CallRing>,
}

impl CallStatus {
    pub fn idle() -> Self {
        Self {
            phase: CallPhase::Idle,
            room_id: None,
            muted: false,
            deafened: false,
            volume: CALL_VOLUME_MAX,
            participants: Vec::new(),
            ended: None,
            join_error: None,
            camera: CallCamera::Off,
            screen: CallScreen::Off,
            video: Vec::new(),
            ring_declined: false,
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
        picture_id: Some("0123456789abcdef".into()),
        has_call: true,
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
        names: vec![
            SenderName {
                user_id: "@alice:server.invalid".into(),
                display_name: Some("Alice <b>admin</b>".into()),
            },
            SenderName {
                user_id: "@bob:server.invalid".into(),
                display_name: None,
            },
        ],
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

pub fn sample_room_picture() -> RoomPicture {
    RoomPicture {
        picture_id: "0123456789abcdef".into(),
        image: ImageView {
            handle: "00112233445566778899aabbccddeeff".into(),
            width: 96,
            height: 96,
            bands: 1,
            band_rows: 96,
        },
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
        format!("export {}", CallUnavailable::decl(&config)),
        format!("export {}", CallReadiness::decl(&config)),
        format!("export {}", CallPhase::decl(&config)),
        format!("export {}", CallParticipant::decl(&config)),
        format!("export {}", CallEnd::decl(&config)),
        format!("export {}", CallJoinError::decl(&config)),
        format!("export {}", CallCamera::decl(&config)),
        format!("export {}", CallScreen::decl(&config)),
        format!("export {}", CallParticipantRef::decl(&config)),
        format!("export {}", CallVideoTile::decl(&config)),
        format!("export {}", CallStatus::decl(&config)),
        format!("export {}", CallIntent::decl(&config)),
        format!("export {}", CallRing::decl(&config)),
        format!("export {}", CallRings::decl(&config)),
        format!("export {}", CallDevice::decl(&config)),
        format!("export {}", CallDevices::decl(&config)),
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
        format!("export {}", RoomDraft::decl(&config)),
        format!("export {}", RoomPreference::decl(&config)),
        format!("export {}", RoomPreferences::decl(&config)),
        format!("export {}", LocalSearchHit::decl(&config)),
        format!("export {}", LocalSearchResults::decl(&config)),
        format!("export {}", LocalSearchFilters::decl(&config)),
        format!("export {}", FocusedTimeline::decl(&config)),
        format!("export {}", PinnedMessages::decl(&config)),
        format!("export {}", AttachmentKind::decl(&config)),
        format!("export {}", AttachmentPage::decl(&config)),
        format!("export {}", SyncMode::decl(&config)),
        format!("export {}", ConnectionHealth::decl(&config)),
        format!("export {}", IgnoredUsers::decl(&config)),
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
        format!("export {}", ProxySetting::decl(&config)),
        format!("export {}", HomeserverFound::decl(&config)),
        format!("export {}", NewSession::decl(&config)),
        format!("export {}", NewSessions::decl(&config)),
        format!("export {}", ModuleSignature::decl(&config)),
        format!("export {}", ModuleReason::decl(&config)),
        format!("export {}", DisplaySession::decl(&config)),
        format!("export {}", ModuleProcess::decl(&config)),
        format!("export {}", ModuleScanState::decl(&config)),
        format!("export {}", ThirdPartyModule::decl(&config)),
        format!("export {}", ThirdPartyModules::decl(&config)),
        format!("export {}", TimelineEvents::decl(&config)),
        format!("export {}", Theme::decl(&config)),
        format!("export {}", AccountSettings::decl(&config)),
        format!("export {}", RoomActivity::decl(&config)),
        format!("export {}", RoomServer::decl(&config)),
        format!("export {}", RoomServers::decl(&config)),
        format!("export {}", RoomReader::decl(&config)),
        format!("export {}", RoomReaders::decl(&config)),
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
        format!("export {}", PublicRoom::decl(&config)),
        format!("export {}", PublicRoomPage::decl(&config)),
        format!("export {}", JoinedRoom::decl(&config)),
        format!("export {}", NoticeKind::decl(&config)),
        format!("export {}", MembershipChange::decl(&config)),
        format!("export {}", StateChange::decl(&config)),
        format!("export {}", TimelineItem::decl(&config)),
        format!("export {}", SenderName::decl(&config)),
        format!("export {}", TimelinePage::decl(&config)),
        format!("export {}", ImageMime::decl(&config)),
        format!("export {}", ImageView::decl(&config)),
        format!("export {}", RoomPicture::decl(&config)),
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

#[cfg(test)]
mod personal_contract_tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn strict_round_trip<T: DeserializeOwned + Serialize + PartialEq + std::fmt::Debug>(value: T) {
        let mut json = serde_json::to_value(&value).unwrap();
        assert_eq!(serde_json::from_value::<T>(json.clone()).unwrap(), value);
        json.as_object_mut()
            .unwrap()
            .insert("extra".into(), true.into());
        assert!(serde_json::from_value::<T>(json).is_err());
    }

    /// Check explicit nullable cursors and reject missing or invalid cursor fields.
    #[test]
    fn attachment_page_requires_an_explicit_nullable_cursor() {
        let page = serde_json::json!({"roomId":"!room:example.invalid","items":[],"nextCursor":null,"scanned":0,"partial":true});
        for cursor in [serde_json::Value::Null, serde_json::json!("opaque-cursor")] {
            let mut input = page.clone();
            input["nextCursor"] = cursor;
            let decoded: AttachmentPage = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), input);
        }
        let mut missing = page.clone();
        missing.as_object_mut().unwrap().remove("nextCursor");
        assert!(serde_json::from_value::<AttachmentPage>(missing).is_err());
        for cursor in [
            serde_json::json!(false),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let mut input = page.clone();
            input["nextCursor"] = cursor;
            assert!(serde_json::from_value::<AttachmentPage>(input).is_err());
        }
        for duplicate in [
            r#"{"roomId":"!room:example.invalid","items":[],"nextCursor":null,"nextCursor":"opaque-cursor","scanned":0,"partial":true}"#,
            r#"{"roomId":"!room:example.invalid","items":[],"nextCursor":null,"nextCursor":null,"scanned":0,"partial":true}"#,
        ] {
            assert!(serde_json::from_str::<AttachmentPage>(duplicate).is_err());
        }
    }

    /// Check navigation fixtures round-trip and reject unknown fields.
    #[test]
    fn navigation_fixtures_round_trip_and_reject_unexpected_data() {
        /// Round-trip one typed fixture and reject an added unknown field.
        fn fixture<T: DeserializeOwned + Serialize>(wire: &str) {
            let value: serde_json::Value = serde_json::from_str(wire).unwrap();
            let decoded: T = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), value);
            let mut extra = value;
            extra["accessToken"] = "secret".into();
            assert!(serde_json::from_value::<T>(extra).is_err());
        }
        fixture::<FocusedTimeline>(include_str!("../fixtures/focused-timeline.json"));
        fixture::<PinnedMessages>(include_str!("../fixtures/pinned-messages.json"));
        fixture::<AttachmentPage>(include_str!("../fixtures/attachment-page.json"));
        fixture::<ConnectionHealth>(include_str!("../fixtures/connection-health.json"));
        fixture::<CallReadiness>(include_str!("../fixtures/call-readiness.json"));
        fixture::<CallStatus>(include_str!("../fixtures/call-status.json"));
        fixture::<CallRings>(include_str!("../fixtures/call-rings.json"));
        fixture::<LocalSearchFilters>(include_str!("../fixtures/local-search-filters.json"));
        let mut focused: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/focused-timeline.json")).unwrap();
        focused["page"]["url"] = "mxc://unsafe".into();
        assert!(serde_json::from_value::<FocusedTimeline>(focused).is_err());
        assert!(serde_json::from_str::<AttachmentKind>("\"videos\"").is_err());
        assert!(serde_json::from_str::<SyncMode>("\"polling\"").is_err());
    }

    /// GIVEN the call DTOs WHEN given an unknown field, code or a wrong
    /// type THEN they are refused; the idle status is the documented shape.
    #[test]
    fn call_contracts_refuse_unknown_fields_and_codes() {
        assert_eq!(
            serde_json::to_string(&CallStatus::idle()).unwrap(),
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#
        );
        for wire in [
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"token":"x"}"#,
            r#"{"phase":"paused","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[]}"#,
            r#"{"phase":"ended","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":"crashed","joinError":null,"camera":"off","screen":"off","video":[]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":"busy","camera":"off","screen":"off","video":[]}"#,
            r#"{"phase":"idle","roomId":null,"muted":"no","deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[]}"#,
            r#"{"phase":"active","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[{"userId":"@a:s","deviceId":"D","speaking":false,"reachable":true,"keyed":true,"sharing":false,"url":"x"}],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[]}"#,
            // The camera and video fields are required, and the camera is a closed set.
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"video":[]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off"}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"notFound","video":[]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"broken","video":[]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":true,"video":[]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[{"tile":256,"participant":null,"own":true}]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[{"tile":-1,"participant":null,"own":true}]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[{"tile":0,"participant":null,"own":true,"url":"x"}]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[{"tile":1,"participant":{"userId":"@a:s"},"own":false}]}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[{"tile":1,"participant":{"userId":"@a:s","deviceId":"D","x":1},"own":false}]}"#,
            // The deafen flag and the volume are required, and the volume is 0 to 100.
            r#"{"phase":"idle","roomId":null,"muted":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":"no","volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":101,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":-1,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#,
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":50.5,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[],"ringDeclined":false}"#,
        ] {
            assert!(serde_json::from_str::<CallStatus>(wire).is_err(), "{wire}");
        }
        let devices = |volume: &str| {
            format!(
                r#"{{"supported":true,"recording":[],"playout":[],"cameras":[],"selectedRecording":null,"selectedPlayout":null,"selectedCamera":null,"volume":{volume},"unreadable":false}}"#
            )
        };
        assert!(serde_json::from_str::<CallDevices>(&devices("0")).is_ok());
        assert!(serde_json::from_str::<CallDevices>(&devices("100")).is_ok());
        for volume in ["101", "-1", "255", "50.5", "null", "\"50\""] {
            assert!(
                serde_json::from_str::<CallDevices>(&devices(volume)).is_err(),
                "{volume}"
            );
        }
        for wire in [
            r#"{"available":true,"reason":null,"activeMembers":256}"#,
            r#"{"available":true,"reason":null,"activeMembers":-1}"#,
            r#"{"available":false,"reason":"no_microphone","activeMembers":0}"#,
            r#"{"available":true,"reason":null,"activeMembers":0,"sfu":"wss://x"}"#,
        ] {
            assert!(
                serde_json::from_str::<CallReadiness>(wire).is_err(),
                "{wire}"
            );
        }
    }

    /// GIVEN the ring DTOs WHEN given an unknown field, intent or a wrong
    /// type THEN they are refused; a status without the flag reads as not
    /// declined, and a room summary without it as having no call.
    #[test]
    fn ring_contracts_refuse_unknown_fields_and_intents() {
        for wire in [
            r#"{"rings":[{"ringId":1,"roomId":"!r:s","caller":"@a:s","intent":"video","secondsLeft":5,"name":"x"}]}"#,
            r#"{"rings":[{"ringId":1,"roomId":"!r:s","caller":"@a:s","intent":"hologram","secondsLeft":5}]}"#,
            r#"{"rings":[{"ringId":1,"roomId":"!r:s","caller":"@a:s","intent":"audio","secondsLeft":-1}]}"#,
            r#"{"rings":[{"ringId":-1,"roomId":"!r:s","caller":"@a:s","intent":"audio","secondsLeft":5}]}"#,
            r#"{"rings":[{"roomId":"!r:s","caller":"@a:s","intent":"audio","secondsLeft":5}]}"#,
            r#"{"rings":[{"ringId":1,"roomId":"!r:s","caller":"@a:s","intent":"audio"}]}"#,
            r#"{"rings":[],"token":"x"}"#,
        ] {
            assert!(serde_json::from_str::<CallRings>(wire).is_err(), "{wire}");
        }
        let status: CallStatus = serde_json::from_str(
            r#"{"phase":"idle","roomId":null,"muted":false,"deafened":false,"volume":100,"participants":[],"ended":null,"joinError":null,"camera":"off","screen":"off","video":[]}"#,
        )
        .unwrap();
        assert!(!status.ring_declined);
        let summary: RoomSummary = serde_json::from_str(
            r#"{"roomId":"!r:s","displayName":null,"unreadCount":0,"encryption":"encrypted"}"#,
        )
        .unwrap();
        assert!(!summary.has_call);
    }

    /// Reject missing, duplicate, or invalid nullable fields and unsafe timestamps.
    #[test]
    fn navigation_nullable_fields_are_required_strict_and_javascript_safe() {
        let filters = serde_json::json!({"sender":null,"fromMs":null,"beforeMs":null});
        let health = serde_json::json!({"accountState":"signed_out","storeState":"protected","syncMode":"unknown","lastAttemptAt":null,"lastSuccessAt":null,"consecutiveFailures":0,"retryDelayMs":0,"lastError":null});
        for (value, fields, is_health) in [
            (filters, vec!["sender", "fromMs", "beforeMs"], false),
            (
                health,
                vec!["lastAttemptAt", "lastSuccessAt", "lastError"],
                true,
            ),
        ] {
            let accepts = |v: serde_json::Value| {
                if is_health {
                    serde_json::from_value::<ConnectionHealth>(v).is_ok()
                } else {
                    serde_json::from_value::<LocalSearchFilters>(v).is_ok()
                }
            };
            assert!(accepts(value.clone()));
            let mut extra = value.clone();
            extra["token"] = "secret".into();
            assert!(!accepts(extra));
            for field in fields {
                let mut missing = value.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(!accepts(missing));
                let mut wrong = value.clone();
                wrong[field] = serde_json::json!([]);
                assert!(!accepts(wrong));
                let wire = serde_json::to_string(&value).unwrap();
                let duplicate = format!("{},\"{}\":null}}", &wire[..wire.len() - 1], field);
                assert!(if is_health {
                    serde_json::from_str::<ConnectionHealth>(&duplicate).is_err()
                } else {
                    serde_json::from_str::<LocalSearchFilters>(&duplicate).is_err()
                });
                if field != "sender" && field != "lastError" {
                    for (time, valid) in [
                        (0_u64, true),
                        (9_007_199_254_740_991, true),
                        (9_007_199_254_740_992, false),
                    ] {
                        let mut input = value.clone();
                        input[field] = time.into();
                        assert_eq!(accepts(input), valid);
                    }
                    for time in [serde_json::json!(-1), serde_json::json!(0.5)] {
                        let mut input = value.clone();
                        input[field] = time;
                        assert!(!accepts(input));
                    }
                }
            }
        }
    }

    /// GIVEN a module report WHEN it crosses the wire THEN it round-trips,
    /// refuses extra fields, and a missing or wrong nullable field fails.
    #[test]
    fn module_report_round_trips_and_requires_explicit_nulls() {
        let module = serde_json::json!({"id":"a.dll\u{1f}unsigned","fileName":"a.dll","path":"C:\\x\\a.dll","signer":null,"signature":"unsigned","reason":"signer","processes":["core","web_view"],"firstSeen":1,"acknowledged":false});
        let report = serde_json::json!({"state":"ready","scannedAt":null,"modules":[module],"truncated":false,"incomplete":true});
        let decoded: ThirdPartyModules = serde_json::from_value(report.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), report);
        assert_eq!(decoded.modules[0].processes[1], ModuleProcess::WebView);
        strict_round_trip(decoded.modules[0].clone());
        strict_round_trip(decoded);
        for field in ["scannedAt", "modules", "truncated", "incomplete", "state"] {
            let mut missing = report.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ThirdPartyModules>(missing).is_err());
        }
        let mut stamped = report.clone();
        stamped["scannedAt"] = 9_007_199_254_740_991_u64.into();
        assert!(serde_json::from_value::<ThirdPartyModules>(stamped).is_ok());
        for bad in [
            serde_json::json!(9_007_199_254_740_992_u64),
            serde_json::json!(-1),
            serde_json::json!("1"),
        ] {
            let mut input = report.clone();
            input["scannedAt"] = bad;
            assert!(serde_json::from_value::<ThirdPartyModules>(input).is_err());
        }
        let wire = serde_json::to_string(&report).unwrap();
        let duplicate = format!("{},\"scannedAt\":null}}", &wire[..wire.len() - 1]);
        assert!(serde_json::from_str::<ThirdPartyModules>(&duplicate).is_err());
        for field in ["signer", "id", "firstSeen", "acknowledged", "reason"] {
            let mut input = report.clone();
            input["modules"][0].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ThirdPartyModules>(input).is_err());
        }
        let mut signed = report.clone();
        signed["modules"][0]["signer"] = "Example Ltd".into();
        signed["modules"][0]["signature"] = "valid".into();
        assert!(serde_json::from_value::<ThirdPartyModules>(signed).is_ok());
        let mut wrong = report.clone();
        wrong["modules"][0]["signer"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ThirdPartyModules>(wrong).is_err());
        for bad in ["\"pending2\"", "\"Ready\""] {
            assert!(serde_json::from_str::<ModuleScanState>(bad).is_err());
        }
        assert!(serde_json::from_str::<ModuleProcess>("\"webview\"").is_err());
        assert!(serde_json::from_str::<ModuleSignature>("\"signed\"").is_err());
        // Linux rows: no signature to check, and a reason other than the signer.
        let mut linux = report.clone();
        linux["modules"][0]["signature"] = "not_checked".into();
        for reason in ["preloaded", "user_writable", "outside_system", "no_file"] {
            linux["modules"][0]["reason"] = reason.into();
            let decoded: ThirdPartyModules = serde_json::from_value(linux.clone()).unwrap();
            assert_eq!(serde_json::to_value(&decoded).unwrap(), linux);
        }
        assert!(serde_json::from_str::<ModuleReason>("\"writable\"").is_err());
        assert!(serde_json::from_str::<ModuleSignature>("\"notChecked\"").is_err());
    }

    /// GIVEN each display session THEN it round-trips as its snake_case
    /// name; GIVEN another spelling THEN it is refused.
    #[test]
    fn display_session_round_trips_and_refuses_other_spellings() {
        for (session, wire) in [
            (DisplaySession::Wayland, "\"wayland\""),
            (DisplaySession::X11, "\"x11\""),
            (DisplaySession::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&session).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<DisplaySession>(wire).unwrap(),
                session
            );
        }
        for bad in ["\"X11\"", "\"x_11\"", "\"xwayland\"", "null"] {
            assert!(
                serde_json::from_str::<DisplaySession>(bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn personal_views_round_trip_and_reject_unknown_fields() {
        strict_round_trip(RoomDraft {
            body: "<script> inert draft\n".into(),
            reply_to: Some("$reply".into()),
        });
        strict_round_trip(RoomPreference {
            room_id: "!room:server.invalid".into(),
            favourite: true,
            section: Some("Work".into()),
        });
        strict_round_trip(RoomPreferences { rooms: Vec::new() });
        strict_round_trip(LocalSearchHit {
            room_id: "!room:server.invalid".into(),
            event_id: "$event".into(),
            sender: "@user:server.invalid".into(),
            body: "<img> inert match".into(),
        });
        strict_round_trip(LocalSearchResults {
            results: Vec::new(),
            truncated: false,
            scanned: u32::MAX,
        });
        strict_round_trip(IgnoredUsers {
            users: vec!["@user:server.invalid".into()],
            truncated: false,
        });
    }

    #[test]
    fn personal_views_use_camel_case_and_require_nullable_fields() {
        assert_eq!(
            serde_json::to_value(RoomDraft::default()).unwrap(),
            serde_json::json!({ "body": "", "replyTo": null })
        );
        assert!(
            serde_json::from_value::<RoomDraft>(serde_json::json!({ "body": "draft" })).is_err()
        );
        assert!(
            serde_json::from_value::<RoomPreference>(
                serde_json::json!({ "roomId": "!room:server.invalid", "favourite": false })
            )
            .is_err()
        );
        let types = generated_typescript();
        for name in [
            "RoomDraft",
            "RoomPreference",
            "RoomPreferences",
            "LocalSearchHit",
            "LocalSearchResults",
            "IgnoredUsers",
        ] {
            assert!(types.contains(&format!("export type {name} =")), "{name}");
        }
    }

    #[test]
    fn required_nullable_strings_accept_null_or_string_and_reject_missing_wrong_or_duplicate_fields()
     {
        for value in [serde_json::Value::Null, serde_json::json!("$reply")] {
            let input = serde_json::json!({"body":"draft", "replyTo":value});
            let draft: RoomDraft = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(serde_json::to_value(draft).unwrap(), input);
            let input = serde_json::json!({"roomId":"!r:s", "favourite":false, "section":value});
            let preference: RoomPreference = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(serde_json::to_value(preference).unwrap(), input);
        }
        for value in [
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            assert!(
                serde_json::from_value::<RoomDraft>(
                    serde_json::json!({"body":"draft", "replyTo":value})
                )
                .is_err()
            );
            assert!(
                serde_json::from_value::<RoomPreference>(
                    serde_json::json!({"roomId":"!r:s", "favourite":false, "section":value})
                )
                .is_err()
            );
        }
        for input in [
            r#"{"body":"draft"}"#,
            r#"{"body":"draft","replyTo":null,"replyTo":"$reply"}"#,
            r#"{"body":"draft","replyTo":null,"extra":false}"#,
        ] {
            assert!(serde_json::from_str::<RoomDraft>(input).is_err(), "{input}");
        }
        for input in [
            r#"{"roomId":"!r:s","favourite":false}"#,
            r#"{"roomId":"!r:s","favourite":false,"section":null,"section":"Work"}"#,
            r#"{"roomId":"!r:s","favourite":false,"section":null,"extra":false}"#,
        ] {
            assert!(
                serde_json::from_str::<RoomPreference>(input).is_err(),
                "{input}"
            );
        }
    }
}
