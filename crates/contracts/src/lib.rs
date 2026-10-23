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
    /// A verified member changed identity; nothing is sent to them until
    /// that is resolved.
    IdentityChanged,
    /// This account is not joined to the room.
    NotJoined,
    /// The homeserver refused the message.
    Rejected,
    /// Anything else.
    Failed,
}

/// A message the user sent from this device in this session. Only the body
/// the user typed is held; nothing is kept after a lock or restart.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PendingMessage {
    pub local_id: u32,
    pub room_id: String,
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
        body: String,
        /// When the sender's homeserver says it received the event
        /// (`origin_server_ts`). That server sets it and nothing checks it,
        /// so it can be wrong. `None` when the event has no usable time.
        #[ts(type = "number | null")]
        origin_server_ts: Option<UnixMillis>,
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
    /// Anything else.
    Failed,
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

/// A verification with another device of the same account. The page acts on
/// it by `flow_id`; the core refuses an action for any other flow.
#[derive(Clone, Debug, Deserialize, Serialize, TS, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VerificationView {
    pub flow_id: String,
    pub phase: VerificationPhase,
    /// Seven emoji once they can be compared, otherwise empty.
    pub emoji: Vec<SasEmoji>,
    /// The other device's name as it reported it (untrusted, projected).
    pub other_device: Option<String>,
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
            },
            TimelineItem::Notice {
                event_id: "$encrypted".into(),
                sender: "@bob:server.invalid".into(),
                notice: NoticeKind::Undecryptable,
                origin_server_ts: None,
            },
            TimelineItem::Gap { next_cursor: None },
        ],
        next_cursor: Some("cursor-2".into()),
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
        format!("export {}", RoomEncryption::decl(&config)),
        format!("export {}", RoomSummary::decl(&config)),
        format!("export {}", PendingState::decl(&config)),
        format!("export {}", SendFailure::decl(&config)),
        format!("export {}", PendingMessage::decl(&config)),
        format!("export {}", RoomList::decl(&config)),
        format!("export {}", NoticeKind::decl(&config)),
        format!("export {}", TimelineItem::decl(&config)),
        format!("export {}", TimelinePage::decl(&config)),
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
