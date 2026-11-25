//! Messages between the keyless UI process and the key-holding core
//! (ADR-0004).
//!
//! Each call is one control frame from the UI and one control frame back,
//! using the bounded frame protocol from `persephone-media-protocol`. Control
//! frames are at most 128 KiB, and the receiver rejects an oversized or
//! malformed header before allocating the body. This crate deliberately does
//! not depend on the core, so the UI's dependency graph contains no SDK, store
//! or key code.
//!
//! One call has a second kind of reply: `image_band` is answered by a
//! [`PixelReply`] control frame followed at once by exactly one `RgbaPixels`
//! frame of at most [`MAX_BAND_BYTES`] ([`send_pixel_reply`]). The UI reads
//! the core's frames with [`receive_from_core`], which checks the pixel
//! frame's header against its reply before it allocates the body.
//!
//! `call_video_frame` has the same shape: a [`VideoReply`] control frame
//! followed at once by exactly one `RgbaPixels` frame of at most
//! [`MAX_CALL_VIDEO_BYTES`] ([`send_video_reply`], ADR-0048). When no newer
//! picture exists, it gets an ordinary `Reply` whose outcome is `ok` with a
//! null value, and no pixel frame follows. [`receive_from_core`] checks the
//! pixel frame's header against its reply before it allocates the body.
//!
//! One call carries bytes the other way: `paste_image` is a control frame
//! followed at once by exactly one `EncodedImage` frame of the length it
//! declares, at most [`MAX_PASTED_IMAGE_BYTES`] ([`send_pasted_image`],
//! ADR-0042). The core reads both with [`receive_request`], which checks the
//! image frame's header against the request and reserves its budget before
//! it allocates the body.

pub use persephone_contracts::{AccountSettings, NotificationLevel, RoomChange, ThreePidMedium};
use persephone_media_protocol::{
    FrameError, FrameHeader, FrameKind, MAX_DIMENSION, MIB, RGBA_BYTES_PER_PIXEL, ReceivedFrame,
    ResourceBudgets, read_frame, write_frame,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

mod command_name;

pub const MAX_LOCAL_SEARCH_BYTES: usize = 128;
pub const MAX_SECTION_BYTES: usize = 64;
pub const MAX_REPORT_REASON_BYTES: usize = 1000;
pub const MAX_SAFE_TIMESTAMP: u64 = 9_007_199_254_740_991;
/// The most modules a report lists.
pub const MAX_THIRD_PARTY_MODULES: usize = 128;
/// The longest acknowledgement key of a module.
pub const MAX_MODULE_ID_BYTES: usize = 512;
/// The longest module path in a report.
pub const MAX_MODULE_PATH_BYTES: usize = 1024;
/// Most events one `timeline_events` call reads again.
pub const MAX_TIMELINE_EVENTS: usize = 20;

/// Bound shared local filters before relay. Matrix identifier syntax is checked
/// again in the core. A missing date range never authorizes extra cache reads.
pub fn search_filters_within_limits(filters: &persephone_contracts::LocalSearchFilters) -> bool {
    filters.sender.as_ref().is_none_or(|sender| {
        !sender.is_empty()
            && sender.len() <= MAX_USER_ID_BYTES
            && !sender.chars().any(char::is_control)
    }) && filters
        .from_ms
        .is_none_or(|time| time.get() <= MAX_SAFE_TIMESTAMP)
        && filters
            .before_ms
            .is_none_or(|time| time.get() <= MAX_SAFE_TIMESTAMP)
        && match (filters.from_ms, filters.before_ms) {
            (Some(from), Some(before)) => from < before,
            _ => true,
        }
}

/// Cheap checks for the relay and authoritative receiver. Identifier syntax
/// and encrypted-record capacity are checked by the core as well.
pub fn personal_call_within_limits(call: &Call) -> bool {
    let text = |value: &str, max: usize| value.len() <= max && !value.chars().any(char::is_control);
    let body = |value: &str, max: usize| {
        value.len() <= max
            && !value
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
    };
    match call {
        Call::RoomDraft { room_id } => text(room_id, MAX_ROOM_ID_BYTES),
        Call::SaveRoomDraft { room_id, draft } => {
            text(room_id, MAX_ROOM_ID_BYTES)
                && body(&draft.body, MAX_MESSAGE_BYTES)
                && draft
                    .reply_to
                    .as_ref()
                    .is_none_or(|id| text(id, MAX_EVENT_ID_BYTES))
        }
        Call::RoomPreferences {} | Call::RoomDrafts {} | Call::IgnoredUsers {} => true,
        Call::SetRoomPreference { preference } => {
            text(&preference.room_id, MAX_ROOM_ID_BYTES)
                && preference
                    .section
                    .as_ref()
                    .is_none_or(|s| !s.trim().is_empty() && text(s, MAX_SECTION_BYTES))
        }
        Call::SearchMessages {
            term,
            room_id,
            filters,
        } => {
            !term.trim().is_empty()
                && text(term, MAX_LOCAL_SEARCH_BYTES)
                && room_id
                    .as_ref()
                    .is_none_or(|id| text(id, MAX_ROOM_ID_BYTES))
                && search_filters_within_limits(filters)
        }
        Call::ConnectionHealth {} | Call::ThirdPartyModules {} => true,
        Call::AcknowledgeModules { ids } => {
            (1..=MAX_THIRD_PARTY_MODULES).contains(&ids.len())
                && ids
                    .iter()
                    .all(|id| !id.is_empty() && id.len() <= MAX_MODULE_ID_BYTES)
        }
        Call::JumpToDate {
            room_id,
            timestamp_ms,
        } => text(room_id, MAX_ROOM_ID_BYTES) && *timestamp_ms <= MAX_SAFE_TIMESTAMP,
        Call::EventContext { room_id, event_id }
        | Call::SetPinned {
            room_id, event_id, ..
        } => text(room_id, MAX_ROOM_ID_BYTES) && text(event_id, MAX_EVENT_ID_BYTES),
        Call::TimelineEvents { room_id, event_ids } => {
            text(room_id, MAX_ROOM_ID_BYTES)
                && (1..=MAX_TIMELINE_EVENTS).contains(&event_ids.len())
                && event_ids.iter().enumerate().all(|(index, id)| {
                    !id.is_empty()
                        && text(id, MAX_EVENT_ID_BYTES)
                        && !event_ids[..index].contains(id)
                })
        }
        Call::PinnedMessages { room_id } => text(room_id, MAX_ROOM_ID_BYTES),
        Call::Attachments {
            room_id,
            cursor,
            filters,
            ..
        } => {
            text(room_id, MAX_ROOM_ID_BYTES)
                && cursor
                    .as_ref()
                    .is_none_or(|cursor| text(cursor, MAX_CURSOR_BYTES))
                && search_filters_within_limits(filters)
        }
        Call::SetIgnoredUser { user_id, .. } => text(user_id, MAX_USER_ID_BYTES),
        Call::ReportAbuse {
            room_id,
            event_id,
            reason,
        } => {
            text(room_id, MAX_ROOM_ID_BYTES)
                && text(event_id, MAX_EVENT_ID_BYTES)
                && !reason.trim().is_empty()
                && body(reason, MAX_REPORT_REASON_BYTES)
        }
        _ => false,
    }
}

/// A command the UI may ask the core to run. Unknown commands and unknown
/// fields fail to decode.
///
/// Every variant is a struct variant, even without arguments: serde does not
/// apply `deny_unknown_fields` to unit variants of an internally tagged enum,
/// so `AppStatus` as a unit variant silently accepted extra fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    AppStatus {},
    RoomList {},
    /// Encrypted per-room text draft. No draft is kept in the UI host.
    RoomDraft {
        room_id: String,
    },
    RoomDrafts {},
    SaveRoomDraft {
        room_id: String,
        draft: persephone_contracts::RoomDraft,
    },
    RoomPreferences {},
    SetRoomPreference {
        preference: persephone_contracts::RoomPreference,
    },
    /// Search only bounded local history; never send the term to a server.
    SearchMessages {
        term: String,
        #[serde(deserialize_with = "Option::deserialize")]
        room_id: Option<String>,
        filters: persephone_contracts::LocalSearchFilters,
    },
    JumpToDate {
        room_id: String,
        timestamp_ms: u64,
    },
    EventContext {
        room_id: String,
        event_id: String,
    },
    /// Read these events of a room again, decrypted with the keys the core
    /// holds now: 1 to [`MAX_TIMELINE_EVENTS`] different event IDs. Replies
    /// with a `TimelinePage` of their items (no cursor), in the given order;
    /// an event that cannot be read is left out. The UI asks for loaded
    /// items that were undecryptable. The core reads them only when room
    /// keys arrived for the room since the last such call while it was
    /// subscribed; otherwise the page has no items and nothing is fetched.
    TimelineEvents {
        room_id: String,
        event_ids: Vec<String>,
    },
    PinnedMessages {
        room_id: String,
    },
    SetPinned {
        room_id: String,
        event_id: String,
        pinned: bool,
    },
    Attachments {
        room_id: String,
        #[serde(deserialize_with = "Option::deserialize")]
        cursor: Option<String>,
        kind: persephone_contracts::AttachmentKind,
        filters: persephone_contracts::LocalSearchFilters,
    },
    ConnectionHealth {},
    IgnoredUsers {},
    SetIgnoredUser {
        user_id: String,
        ignored: bool,
    },
    /// Explicit report to this account's homeserver, not the project.
    ReportAbuse {
        room_id: String,
        event_id: String,
        reason: String,
    },
    /// A page of a room's history, older than `cursor` (or the newest page).
    TimelinePage {
        room_id: String,
        /// Required, though it may be null: serde would otherwise treat a
        /// missing field as `None`.
        #[serde(deserialize_with = "Option::deserialize")]
        cursor: Option<String>,
    },
    /// Start sending [`Event::RoomChanged`] for this room.
    Subscribe {
        room_id: String,
    },
    Unsubscribe {
        room_id: String,
    },
    /// Close the protected session: refuse new work, wait for admitted work,
    /// then close the stores. Replies with the resulting store state.
    LockSession {},
    /// Reopen the protected session. The core asks for the profile passphrase
    /// in its own window; nothing secret crosses this channel. Replies at once
    /// with the store state (`unlocking` while the prompt is open); the
    /// outcome arrives as [`Event::SessionChanged`].
    UnlockSession {},
    /// Delete this computer's profile after a forgotten passphrase. Only asks:
    /// the core shows its own warning, and deletes nothing without an explicit
    /// yes there. Replies like `UnlockSession`.
    ResetProfile {},
    /// Sign in to this homeserver. The core asks for the username and password
    /// itself, in a native prompt; nothing secret crosses this channel. Replies
    /// at once with the account status; the outcome arrives as
    /// [`Event::SessionChanged`].
    SignIn {
        homeserver: String,
    },
    /// The profile's SOCKS5 proxy (ADR-0028). Available whenever the profile
    /// is unlocked, signed in or not.
    ProxySetting {},
    /// Set the profile's SOCKS5 proxy as `host:port` (or after `socks5://`),
    /// at most [`MAX_PROXY_BYTES`]; null removes it. It applies to
    /// connections made afterwards.
    SetProxySetting {
        /// Required, though it may be null.
        #[serde(deserialize_with = "Option::deserialize")]
        proxy: Option<String>,
    },
    /// Whether the diagnostic log is on, and its file (ADR-0058).
    DiagnosticsStatus {},
    /// Turn the diagnostic log on or off; the reply is whether it is on. On
    /// asks the user first in the core's own window, so it can take until
    /// they answer; a "no" replies `false`. Off needs no question and keeps
    /// the file until the next start. Each change raises
    /// [`Event::DiagnosticsChanged`].
    SetDiagnostics {
        on: bool,
    },
    /// Records for the diagnostic log from the UI host or the page: at most
    /// [`MAX_DIAGNOSTIC_RECORDS`], each bounded as [`DiagnosticRecord`]
    /// says. Ignored while the log is off. The UI host sets `source`; a
    /// page's records always arrive as `page`.
    DiagnosticRecords {
        source: RecordSource,
        records: Vec<DiagnosticRecord>,
    },
    /// The call microphones and speakers, and the saved choice (ADR-0043).
    /// Lists the devices through a contained call worker, so it can take
    /// seconds. Available whenever the profile is unlocked.
    CallDevices {},
    /// Save the chosen call microphone, speaker and camera as platform device
    /// IDs, each at most [`MAX_CALL_DEVICE_ID_BYTES`]; null is the system default.
    /// They apply to calls joined afterwards.
    SetCallDevices {
        /// Required, though it may be null.
        #[serde(deserialize_with = "Option::deserialize")]
        recording: Option<String>,
        /// Required, though it may be null.
        #[serde(deserialize_with = "Option::deserialize")]
        playout: Option<String>,
        /// The camera, as a platform device ID. Required, though it may be
        /// null (the first camera the system lists).
        #[serde(deserialize_with = "Option::deserialize")]
        camera: Option<String>,
    },
    /// Find the homeserver of a server name such as `example.org`, through
    /// its `.well-known/matrix/client` file, and confirm it speaks Matrix
    /// (ADR-0027). Answered when done; `not_found` when no Matrix server is
    /// there.
    DiscoverHomeserver {
        server_name: String,
    },
    /// Test one call device in a contained call worker (ADR-0059): listen
    /// on a microphone, play a tone on a speaker or show a camera. `device`
    /// is a platform device ID of at most [`MAX_CALL_DEVICE_ID_BYTES`], or
    /// null for the system default. Replies the test's status at once;
    /// `call_device_test_changed` follows each change. A test that runs is
    /// stopped first. Refused with the state `in_call` while a call runs.
    CallDeviceTest {
        test: persephone_contracts::CallDeviceTestKind,
        /// Required, though it may be null.
        #[serde(deserialize_with = "Option::deserialize")]
        device: Option<String>,
    },
    /// Stop the device test that runs, if any: with `id`, only when that
    /// test is still the newest (ADR-0059). Replies null.
    CallDeviceTestStop {
        id: Option<u64>,
    },
    /// The newest device test's status.
    CallDeviceTestStatus {},
    /// Sign in to this homeserver through its OAuth 2.0 provider (OIDC,
    /// ADR-0030). The core asks in its own window before the provider's page
    /// opens in the browser; the password goes to the provider, never here.
    /// Replies at once like `SignIn`; the outcome arrives as
    /// [`Event::SessionChanged`].
    SignInWithOidc {
        homeserver: String,
    },
    /// Register a new account named `username` (a localpart) on this
    /// homeserver, then sign in to it. The core checks that the name is free
    /// first, then asks for the password (twice) and, when the homeserver
    /// needs one, the registration token, in its own windows; nothing secret
    /// crosses this channel. Replies at once like `SignIn`; the outcome, or
    /// why it failed (such as `username_taken`), arrives as
    /// [`Event::SessionChanged`].
    Register {
        homeserver: String,
        username: String,
    },
    /// Sign out: revoke the session on the homeserver if it can be reached,
    /// then delete this device's local profile (keys first). Replies with the
    /// resulting store state.
    ///
    /// Without `discard_keys`, the core first makes sure this device's room
    /// keys are in the key backup. When they are not, nothing changes and the
    /// call fails with [`ErrorCode::KeysNotBackedUp`]. `discard_keys` is the
    /// user's choice to sign out and lose them.
    SignOut {
        discard_keys: bool,
    },
    /// The account's encryption state (codes only).
    SecurityStatus {},
    /// Set up cross-signing and recovery for an account without an identity.
    /// The core asks for the password in its own window and shows the
    /// recovery key in its own window; neither crosses this channel. Replies
    /// at once with the status; progress arrives as [`Event::SecurityChanged`].
    SetUpEncryption {},
    /// Verify this device with the recovery key, asked in the core's own
    /// window. Replies at once with the status, like `SetUpEncryption`.
    RecoverWithKey {},
    /// Queue a text message. Replies at once with the queued message; its
    /// progress arrives as [`Event::RoomChanged`] for the room.
    SendMessage {
        room_id: String,
        body: String,
        /// The event in this room the message replies to. Required, though
        /// it may be null.
        #[serde(deserialize_with = "Option::deserialize")]
        reply_to: Option<String>,
    },
    /// Queue a new version of one of this account's text messages in this
    /// room. Replies like `SendMessage`. When it is sent, the core checks
    /// that the message is this account's own text message, and fails it
    /// as `rejected` otherwise.
    EditMessage {
        room_id: String,
        event_id: String,
        body: String,
    },
    /// Open a link from a message in the user's browser. The core checks it
    /// (only http and https, at most [`MAX_LINK_BYTES`]) and asks in its own
    /// window first (ADR-0024). Replies `true` when opened, `false` when the
    /// user said no; `invalid_argument` for a refused link.
    OpenLink {
        url: String,
    },
    /// Who else is typing in a joined room and, when `event_id` is given,
    /// who else has read up to that event.
    RoomActivity {
        room_id: String,
        /// Required, though it may be null.
        #[serde(deserialize_with = "Option::deserialize")]
        event_id: Option<String>,
    },
    /// Tell a joined room this account started or stopped typing. Refused as
    /// `rejected` while the account's settings do not send typing notices
    /// (ADR-0023).
    SetTyping {
        room_id: String,
        typing: bool,
    },
    /// Mark a joined room read up to this event: its fully-read marker and
    /// a private read receipt, which only this account's devices see, and a
    /// public read receipt when the account's settings send them
    /// (ADR-0023).
    MarkRead {
        room_id: String,
        event_id: String,
    },
    /// React to this event in a joined room with `key`, usually an emoji:
    /// some visible text of at most [`MAX_REACTION_KEY_BYTES`], without
    /// control characters. Sent like a message: in an encrypted room it is
    /// encrypted, in a room that is not, only after `allow_unencrypted`, and
    /// not while a member's identity change waits (`rejected` then). A
    /// reaction is taken back with `delete_message` on its own event.
    React {
        room_id: String,
        event_id: String,
        key: String,
    },
    /// Delete (redact) this event in a joined room, for everyone. The
    /// homeserver decides whether this account may.
    DeleteMessage {
        room_id: String,
        event_id: String,
    },
    /// The room's messages sent from this device in this session.
    PendingMessages {
        room_id: String,
    },
    /// Send a failed or unknown message again, with the same transaction ID.
    RetryMessage {
        local_id: u32,
    },
    /// Drop a message that is not being sent.
    DiscardMessage {
        local_id: u32,
    },
    /// Record the user's consent to send unencrypted messages in this room.
    AllowUnencrypted {
        room_id: String,
    },
    /// Set up recovery alone for a verified identity without it (an earlier
    /// attempt failed). The core shows the new recovery key in its own window.
    SetUpRecovery {},
    /// Ask the account's other devices to verify this one by comparing
    /// emoji. Progress arrives as [`Event::SecurityChanged`].
    VerifyThisDevice {},
    /// Accept a verification another of the account's devices asked for.
    AcceptVerification {
        flow_id: String,
    },
    /// Confirm that the emoji match those on the other device.
    ConfirmVerification {
        flow_id: String,
    },
    /// Cancel a verification; during the comparison, say the emoji differ.
    CancelVerification {
        flow_id: String,
    },
    /// The other person in a direct-message room, and how far this account
    /// trusts them.
    ContactStatus {
        room_id: String,
    },
    /// Ask the other person in this direct-message room to verify each other
    /// by comparing emoji. The core finds the person itself; the request goes
    /// to an existing direct-message room with them, never a new one.
    /// Progress arrives as [`Event::SecurityChanged`].
    VerifyContact {
        room_id: String,
    },
    /// Ask a member of this joined room to verify each other by comparing
    /// emoji. The core checks that they are a member of the room, then sends
    /// the request to an existing direct-message room with just them, as for
    /// [`Call::VerifyContact`]; it never creates or joins a room. Progress
    /// arrives as [`Event::SecurityChanged`].
    VerifyMember {
        room_id: String,
        user_id: String,
    },
    /// The members of this room whose identity changed (ADR-0007).
    IdentityChanges {
        room_id: String,
    },
    /// Accept the new identity of a member of this room whose identity
    /// changed: pin it, or withdraw the verification of the old one. Only
    /// asks: the core confirms in its own window and changes nothing without
    /// an explicit yes there. Replies at once with the status; the outcome
    /// arrives as [`Event::SecurityChanged`].
    AcceptIdentity {
        room_id: String,
        user_id: String,
    },
    /// Download, decrypt and decode the image of this `m.image` event in the
    /// contained worker (ADR-0009). Replies with an image view: an opaque
    /// handle, the shown size and the number of pixel bands. Nothing is
    /// fetched until the user asks.
    LoadImage {
        room_id: String,
        event_id: String,
    },
    /// Download and decode the current picture of a joined room in the
    /// contained worker (ADR-0041): a server thumbnail when there is one,
    /// otherwise the file. Replies with a room picture: the picture's ID and
    /// an image view. `not_found` when the room has no picture.
    LoadRoomPicture {
        room_id: String,
    },
    /// One band of rows of a loaded or picked image. Answered by a
    /// [`PixelReply`] and one pixel frame, or by an ordinary error reply.
    ImageBand {
        handle: String,
        band: u32,
    },
    /// Drop a loaded image and its pixels.
    ReleaseImage {
        handle: String,
    },
    /// Ask the user for a PNG or JPEG file to send in this room, in the
    /// core's own file dialog; the page never names a path. Replies with a
    /// draft (a stripped copy and its preview) or `cancelled`.
    PickImage {
        room_id: String,
    },
    /// Attach an image the user pasted into this room (ADR-0042). The request
    /// is followed at once by exactly one `EncodedImage` frame of `length`
    /// bytes. The core treats the bytes as hostile, like a picked file: the
    /// guest strips and decodes them. Replies with a draft, like `PickImage`.
    PasteImage {
        room_id: String,
        length: u32,
    },
    /// Queue a picked image (by its draft handle) for this room. Replies with
    /// the queued message, like `SendMessage`.
    SendImage {
        room_id: String,
        handle: String,
    },
    /// Drop a picked image that was not sent.
    DiscardImage {
        handle: String,
    },
    /// Ask the user for a file to send in this room, in the core's own file
    /// dialog, and queue it at once (ADR-0029); the page never names a path.
    /// Replies with the queued message, or null when the user cancelled.
    PickFile {
        room_id: String,
    },
    /// Download and decrypt the file of this event, then ask the user where
    /// to save it in the core's own Save dialog and write it (ADR-0029).
    /// Replies `true` when saved and `false` when the user cancelled.
    SaveFile {
        room_id: String,
        event_id: String,
    },
    /// Room invitations this account has not answered, direct-message or
    /// not, from this homeserver or another one.
    InviteList {},
    /// Join the room of this invitation.
    AcceptInvite {
        room_id: String,
    },
    /// Refuse this invitation (leave the room).
    DeclineInvite {
        room_id: String,
    },
    /// Create an encrypted, invite-only room with this name and invite these
    /// people (at most [`MAX_INVITEES`]). Refused when one of them has no
    /// cross-signing identity (ADR-0005); nothing is created then.
    CreateRoom {
        name: String,
        invite: Vec<String>,
    },
    /// Invite this person to a joined room. In an encrypted room, refused
    /// when they have no cross-signing identity.
    InviteUser {
        room_id: String,
        user_id: String,
    },
    /// This account's display name, as the homeserver has it.
    DisplayName {},
    /// Set this account's display name; an empty name removes it.
    SetDisplayName {
        name: String,
    },
    /// The devices (sessions) signed in to this account.
    DeviceList {},
    /// This account's settings, kept in its encrypted profile.
    AccountSettings {},
    /// The sessions of this account the user has not been shown yet
    /// (ADR-0026). The first time, the current ones are recorded and none
    /// is new.
    NewSessions {},
    /// Record that the user has seen these sessions (at most
    /// [`MAX_NEW_SESSION_IDS`] device IDs, as `new_sessions` gave them).
    AcknowledgeSessions {
        device_ids: Vec<String>,
    },
    /// The third-party modules loaded into Persephone's processes. Replies
    /// with `ThirdPartyModules`.
    ThirdPartyModules {},
    /// Mark these modules as reviewed (1 to [`MAX_THIRD_PARTY_MODULES`] ids,
    /// each non-empty and at most [`MAX_MODULE_ID_BYTES`], as
    /// `third_party_modules` gave them). Unknown ids are ignored. Replies
    /// with the updated `ThirdPartyModules`.
    AcknowledgeModules {
        ids: Vec<String>,
    },
    /// Sign out another session of this account (ADR-0026). The core asks
    /// for the account password in its own window. Replies when done with
    /// an `AccountChange` code; this device's own ID is `invalid_argument`
    /// (use `sign_out`).
    RemoveDevice {
        device_id: String,
    },
    /// Replace this account's settings. Every setting is a required field
    /// of the record, which refuses unknown ones, so the page always sends
    /// the whole set. Text sizes outside their bounds are
    /// `invalid_argument`.
    SetAccountSettings {
        settings: AccountSettings,
    },
    /// Change this account's password (ADR-0019). The core asks for the
    /// current password and the new one (twice) in its own windows; nothing
    /// secret crosses this channel. `sign_out_others` signs out every other
    /// device of the account, as the specification does by default. Replies
    /// when done with an `AccountChange` code.
    ChangePassword {
        sign_out_others: bool,
    },
    /// The email addresses and phone numbers on this account (ADR-0020).
    ThreePids {},
    /// Ask the homeserver to send a confirmation link to this email address,
    /// to add it. Replaces any address waiting to be added. Never binds it
    /// to an identity server.
    RequestEmailToken {
        email: String,
    },
    /// Ask the homeserver to send a code to this phone number, given as a
    /// two-letter country code and the number as dialled there.
    RequestPhoneToken {
        country: String,
        phone_number: String,
    },
    /// Add the address waiting to be added, once it is confirmed. The core
    /// asks for a phone's code and for the account password in its own
    /// windows.
    AddThreePid {},
    /// Remove an email address or phone number from this account.
    RemoveThreePid {
        medium: ThreePidMedium,
        address: String,
    },
    /// Ask another of this account's devices to verify each other by
    /// comparing emoji, from a verified device. Progress arrives as
    /// [`Event::SecurityChanged`], as for `VerifyThisDevice`.
    VerifyDevice {
        device_id: String,
    },
    /// Replace the recovery key (ADR-0021). The core warns in its own window
    /// that the old key stops working, then shows the new key there and
    /// offers to save it to a file it writes itself. The key never crosses
    /// this channel. Replies at once with the status.
    ChangeRecoveryKey {},
    /// Turn the local profile passphrase on or off (ADR-0050). The core asks
    /// for the passphrase in its own window: a new one (twice) to turn it
    /// on, the current one to turn it off. The passphrase never crosses this
    /// channel. Replies at once with the status; progress and the result
    /// arrive as [`Event::SecurityChanged`]. An unchanged setting, a locked
    /// profile or a running security operation leaves the status as it is.
    SetProfilePassphrase {
        on: bool,
    },
    /// Delete the homeserver's key backup, which this verified device cannot
    /// use, and start a new one with a new recovery key (ADR-0040). The core
    /// asks in its own window first, then shows the new key as for
    /// `ChangeRecoveryKey`. Replies at once with the status.
    StartNewBackup {},
    /// This account's notification level, from its push rules (ADR-0022).
    NotificationLevel {},
    /// Set this account's notification level in its push rules. `custom`
    /// is `invalid_argument`.
    SetNotificationLevel {
        level: NotificationLevel,
    },
    /// The local profiles (accounts) on this computer, by slot number. No
    /// path or account name crosses.
    ProfileList {},
    /// Make a new empty profile and switch to it. Replies at once; the core
    /// then closes the window, locks the stores and starts again with the new
    /// profile (ADR-0015).
    AddProfile {},
    /// Switch to the profile in this slot, as `AddProfile` does.
    SwitchProfile {
        slot: u32,
    },
    /// Whether an audio call can start in this joined room (ADR-0043), and
    /// how many other devices are in its call. Replies `CallReadiness`.
    CallReadiness {
        room_id: String,
    },
    /// Join the call in this joined, encrypted room, with the camera on when
    /// `video` is true (ADR-0048). With a profile
    /// proxy the core first asks, in its own window, whether the call may
    /// connect directly. Replies `CallStatus`; a refusal is a `joinError` in
    /// it, not an error code. Progress arrives as [`Event::CallChanged`].
    CallJoin {
        room_id: String,
        video: bool,
    },
    /// Leave the call, or cancel a join in progress. Replies `CallStatus`.
    CallLeave {},
    /// Mute or unmute the microphone. Replies `CallStatus`; the worker's
    /// answer arrives as [`Event::CallChanged`].
    CallMute {
        muted: bool,
    },
    /// Deafen or undeafen the call (ADR-0055): all remote audio goes off and
    /// the microphone mutes, or remote audio comes back and the microphone
    /// returns to the user's own mute choice. Unmuting with [`Call::CallMute`]
    /// also undeafens. Replies `CallStatus`; the worker's answer arrives as
    /// [`Event::CallChanged`].
    CallDeafen {
        deafened: bool,
    },
    /// Set the call playout volume, 0 to [`MAX_CALL_VOLUME_PERCENT`]
    /// (ADR-0055). It is saved for later calls and applies at once to a
    /// running call. Replies `CallStatus`.
    CallVolume {
        percent: u8,
    },
    /// Set the automatic microphone level and the manual microphone gain
    /// (ADR-0062). `gain` is a whole percent, 0 to
    /// [`MAX_CALL_MIC_GAIN_PERCENT`] in steps of [`CALL_MIC_GAIN_STEP_PERCENT`].
    /// Both are saved for later calls and apply at once to a running call.
    /// Replies `CallStatus`.
    CallMicrophone {
        auto_level: bool,
        gain: u16,
    },
    /// Start or stop the call statistics (ADR-0066). While on, the core
    /// asks the worker to read them and sends [`Event::CallStats`], at most
    /// about once a second. Only the call window may send it. Off at every
    /// call start, and off again when the call ends. Replies `null`.
    CallStats {
        on: bool,
    },
    /// Turn the camera on or off during a call. Replies `CallStatus`, with the
    /// camera `starting` when turned on; the worker's answer arrives as
    /// [`Event::CallChanged`]. Refused when no call is connected.
    CallCamera {
        on: bool,
    },
    /// Start or stop sharing the screen during a call (ADR-0053). The page
    /// names no screen, window or handle: on a start the core shows its own
    /// chooser or warning, and a cancel leaves the share off. Replies
    /// `CallStatus`, with the screen `starting` when started; the worker's
    /// answer arrives as [`Event::CallChanged`]. Refused when no call is
    /// connected.
    CallScreen {
        on: bool,
    },
    /// The newest picture on a video tile (0 is the own camera, 1 to 4 are
    /// remote cameras, 5 is the remote screen share), if its number is above
    /// `after` and it belongs to the tile's
    /// stream `stream` (from the call status). Answered at once with a
    /// [`VideoReply`] and one pixel frame, or with an `ok` reply holding null
    /// when there is no such picture.
    CallVideoFrame {
        tile: u8,
        after: u64,
        stream: u64,
    },
    /// The call as the page shows it. Replies `CallStatus`.
    CallStatus {},
    /// The incoming call rings now running (ADR-0052). Replies `CallRings`.
    CallRings {},
    /// Decline the ring in this room (ADR-0052): the core sends the decline
    /// event once, without a retry, and ends the ring. Replies `CallRings`.
    CallDeclineRing {
        room_id: String,
    },
    /// Open a direct-message conversation with this user: an existing room
    /// with just them, or a new encrypted room they are invited to. Refused
    /// when they have no cross-signing identity (ADR-0005).
    StartConversation {
        user_id: String,
    },
    /// Search the homeserver's user directory. The page sends this only when
    /// the user has turned directory search on: the server sees every term.
    SearchUsers {
        term: String,
    },
    /// One page of a public room directory: this account's homeserver's
    /// when `server` is none, otherwise the named server's, which the
    /// homeserver asks over federation. `filter` narrows the page to rooms
    /// whose name, address or topic contains it; `since` is the `next` token
    /// of the page before. Both servers see the filter.
    PublicRooms {
        #[serde(deserialize_with = "Option::deserialize")]
        server: Option<String>,
        #[serde(deserialize_with = "Option::deserialize")]
        filter: Option<String>,
        #[serde(deserialize_with = "Option::deserialize")]
        since: Option<String>,
    },
    /// Join a room that a `public_rooms` page of this session listed as
    /// open to anyone. Any other room is `not_found`.
    JoinPublicRoom {
        room_id: String,
    },
    /// A joined room's settings: names, addresses, access, encryption,
    /// notifications and power levels, with what this account may change.
    RoomSettings {
        room_id: String,
    },
    /// A joined room's members (joined, invited, asking or banned) whose ID
    /// or display name contains `filter` (empty: all), at most
    /// [`MAX_LISTED_MEMBERS`].
    RoomMembers {
        room_id: String,
        filter: String,
    },
    /// The homeservers with members (joined or invited) in a joined room:
    /// every one of them receives its messages and metadata.
    RoomServers {
        room_id: String,
    },
    /// Who can read a joined room's new messages: each joined or invited
    /// member, how far this account trusts them, and how many of their
    /// devices receive room keys or are refused them (ADR-0005).
    RoomReaders {
        room_id: String,
    },
    /// Change one setting of a joined room, or act on one member. The
    /// homeserver decides whether this account may.
    ChangeRoom {
        room_id: String,
        change: RoomChange,
    },
}

/// Longest email address either side accepts, in bytes (RFC 5321's path
/// limit less its angle brackets).
pub const MAX_EMAIL_BYTES: usize = 254;

/// Longest phone number either side accepts, in bytes.
pub const MAX_PHONE_NUMBER_BYTES: usize = 32;

/// The exact length of a country code (ISO 3166-1 alpha-2).
pub const COUNTRY_CODE_BYTES: usize = 2;

/// Longest 3PID address either side accepts in `remove_three_pid`.
pub const MAX_THREE_PID_ADDRESS_BYTES: usize = MAX_EMAIL_BYTES;

/// Most email addresses and phone numbers a `three_pids` reply lists.
pub const MAX_THREE_PIDS: usize = 50;

/// Most device IDs one `acknowledge_sessions` call carries.
pub const MAX_NEW_SESSION_IDS: usize = 10;

/// Longest device ID either side accepts in a call.
pub const MAX_DEVICE_ID_BYTES: usize = persephone_contracts::CALL_DEVICE_ID_BYTES_MAX;

/// Longest room or sender name in a [`Event::Notification`], in bytes.
pub const MAX_NOTIFICATION_NAME_BYTES: usize = 512;

/// Longest room topic either side accepts, in bytes.
pub const MAX_TOPIC_BYTES: usize = 4096;

/// Longest room address either side accepts, in bytes (the Matrix limit).
pub const MAX_ALIAS_BYTES: usize = 255;

/// Most published addresses besides the main one.
pub const MAX_ALT_ALIASES: usize = 10;

/// Longest member filter either side accepts, in bytes.
pub const MAX_MEMBER_FILTER_BYTES: usize = 256;

/// Most members a `room_members` reply lists.
pub const MAX_LISTED_MEMBERS: usize = 100;

/// Whether every text in `change` is within its limit. The receiver still
/// validates the content; this only keeps oversized input off the channel.
pub fn room_change_within_limits(change: &RoomChange) -> bool {
    match change {
        RoomChange::Name { name } => name.len() <= MAX_ROOM_NAME_BYTES,
        RoomChange::Topic { topic } => topic.len() <= MAX_TOPIC_BYTES,
        RoomChange::AddAlias { alias } | RoomChange::RemoveAlias { alias } => {
            alias.len() <= MAX_ALIAS_BYTES
        }
        RoomChange::CanonicalAlias { alias, alt_aliases } => {
            alias
                .as_ref()
                .is_none_or(|alias| alias.len() <= MAX_ALIAS_BYTES)
                && alt_aliases.len() <= MAX_ALT_ALIASES
                && alt_aliases
                    .iter()
                    .all(|alias| alias.len() <= MAX_ALIAS_BYTES)
        }
        RoomChange::MemberLevel { user_id, .. }
        | RoomChange::Kick { user_id }
        | RoomChange::Ban { user_id }
        | RoomChange::Unban { user_id } => user_id.len() <= MAX_USER_ID_BYTES,
        RoomChange::JoinRule { .. }
        | RoomChange::HistoryVisibility { .. }
        | RoomChange::GuestAccess { .. }
        | RoomChange::Encryption {}
        | RoomChange::Published { .. }
        | RoomChange::Level { .. }
        | RoomChange::Notifications { .. }
        | RoomChange::Leave {} => true,
    }
}

/// Longest user-directory search term either side accepts, in bytes.
pub const MAX_SEARCH_TERM_BYTES: usize = 256;

/// Most matches a user-directory search returns.
pub const MAX_SEARCH_RESULTS: usize = 10;

/// Most rooms one `public_rooms` page lists.
pub const MAX_PUBLIC_ROOMS: usize = 30;

/// Longest public-room filter either side accepts, in bytes.
pub const MAX_DIRECTORY_FILTER_BYTES: usize = 256;

/// Longest `public_rooms` page token either side accepts, in bytes. A
/// longer token from the homeserver ends the listing.
pub const MAX_DIRECTORY_TOKEN_BYTES: usize = 512;

/// Most rooms one listing shows across its pages. The page stops asking for
/// more pages at this count.
pub const MAX_LISTED_ROOMS: usize = 300;

/// Most listed rooms the core remembers as joinable for `join_public_room`;
/// the oldest are forgotten first. A listing's last page can offer up to a
/// whole page past [`MAX_LISTED_ROOMS`], so every room a listing shows stays
/// joinable.
pub const MAX_OFFERED_ROOMS: usize = MAX_LISTED_ROOMS + MAX_PUBLIC_ROOMS;

/// Longest room name either side accepts, in bytes.
pub const MAX_ROOM_NAME_BYTES: usize = 255;

/// Most people a `create_room` call may invite.
pub const MAX_INVITEES: usize = 10;

/// Longest display name either side accepts, in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 256;

/// Most local profiles (slots 1 to this number) on one computer.
pub const MAX_PROFILES: u32 = 8;

/// Longest link either side accepts in `open_link`, in bytes.
pub const MAX_LINK_BYTES: usize = 2048;

/// Longest reaction key either side accepts, in bytes.
pub const MAX_REACTION_KEY_BYTES: usize = 64;

/// Longest event ID either side accepts in a call (the Matrix limit).
pub const MAX_EVENT_ID_BYTES: usize = 255;

/// The exact length of an image handle: 128 random bits as lowercase hex.
pub const IMAGE_HANDLE_BYTES: usize = 32;

/// Most pixel bytes in the one `RgbaPixels` frame after a [`PixelReply`].
pub const MAX_BAND_BYTES: u64 = MIB;

/// Longest user ID either side accepts in a call (the Matrix limit).
pub const MAX_USER_ID_BYTES: usize = persephone_contracts::CALL_USER_ID_BYTES_MAX;

/// Longest verification flow ID either side accepts.
pub const MAX_FLOW_ID_BYTES: usize = 128;

/// Longest message body either side accepts, in bytes (the display limit).
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// Longest username (localpart) either side accepts when registering.
pub const MAX_USERNAME_BYTES: usize = 255;

/// Longest proxy address either side accepts, in bytes.
pub const MAX_PROXY_BYTES: usize = 300;

/// Longest call device ID either side accepts, in bytes.
pub const MAX_CALL_DEVICE_ID_BYTES: usize = 512;

/// Video tiles in a call: tile 0 is the own camera, tiles 1 to 4 are remote
/// cameras, tile 5 is the remote screen share (ADR-0053).
pub const CALL_VIDEO_TILES: u8 = 6;
/// The highest call playout volume, in percent (ADR-0055).
pub const MAX_CALL_VOLUME_PERCENT: u8 = 100;
/// The highest microphone gain, in percent (ADR-0062).
pub const MAX_CALL_MIC_GAIN_PERCENT: u16 = 400;
/// The microphone gain moves in steps of this many percent (ADR-0062).
pub const CALL_MIC_GAIN_STEP_PERCENT: u16 = 5;

/// The tile of the remote screen share.
pub const CALL_SCREEN_TILE: u8 = 5;

/// The longest side of a call camera picture, in pixels.
pub const MAX_CALL_VIDEO_SIDE: u32 = 640;

/// The most pixels in one call camera picture.
pub const MAX_CALL_VIDEO_PIXELS: u32 = 640 * 360;

/// The longest side of a screen share picture, in pixels.
pub const MAX_CALL_SCREEN_SIDE: u32 = 1280;

/// The most pixels in one screen share picture: 1280 x 720.
pub const MAX_CALL_SCREEN_PIXELS: u32 = 1280 * 720;

/// The longest side of a picture on this tile.
pub const fn max_call_video_side(tile: u8) -> u32 {
    if tile == CALL_SCREEN_TILE {
        MAX_CALL_SCREEN_SIDE
    } else {
        MAX_CALL_VIDEO_SIDE
    }
}

/// The most pixels in a picture on this tile.
pub const fn max_call_video_pixels(tile: u8) -> u32 {
    if tile == CALL_SCREEN_TILE {
        MAX_CALL_SCREEN_PIXELS
    } else {
        MAX_CALL_VIDEO_PIXELS
    }
}

/// The most bytes in the one `RgbaPixels` frame after a [`VideoReply`].
pub const MAX_CALL_VIDEO_BYTES: u64 = MAX_CALL_VIDEO_PIXELS as u64 * 4;

/// The largest picture rotation: quarter turns clockwise, 0 to 3.
pub const MAX_CALL_VIDEO_ROTATION: u8 = 3;

/// Longest server name either side accepts in `discover_homeserver`, in
/// bytes (a DNS name and a port).
pub const MAX_SERVER_NAME_BYTES: usize = 261;

/// Longest homeserver URL either side accepts in a call.
pub const MAX_HOMESERVER_URL_BYTES: usize = 2048;

/// Most rooms the UI may subscribe to at once.
pub const MAX_SUBSCRIPTIONS: usize = 16;

/// The top of the call's sound level meter (ADR-0055): 0 dBFS. 0 is
/// -60 dBFS or quieter. The contract's ceiling, so the core, the host and
/// the page check one number.
pub const MAX_CALL_SOUND_LEVEL: u8 = persephone_contracts::CALL_SOUND_LEVEL_MAX;

/// A notification from the core. Events carry no message content: the UI
/// fetches content through bounded calls. A burst of changes to one room
/// coalesces into one pending event, so the core's queue is bounded by the
/// number of subscriptions.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    RoomChanged {
        room_id: String,
    },
    /// The session locked, unlocked or changed state: drop everything shown
    /// from it and ask for `app_status`. Sent before the lock waits for
    /// admitted work, so the UI clears first.
    SessionChanged {},
    /// Rooms were added, removed or changed: fetch the room list again.
    /// Coalesced like room events.
    RoomListChanged {},
    /// The account's encryption state changed: ask for `security_status`.
    /// Coalesced; it does not clear what the UI shows.
    SecurityChanged {},
    /// The account's status changed within the same session, such as the
    /// homeserver refusing its presence: ask for `app_status`. Content-free
    /// and coalesced like `SecurityChanged`; it does not clear what the UI
    /// shows.
    AccountChanged {},
    /// The call changed state, or its participants did: ask for
    /// `call_status`. Coalesced; it carries no call data.
    CallChanged {},
    /// A new video picture is waiting: ask for `call_video_frame` on each
    /// active tile. Content-free and coalesced like `CallChanged`.
    CallVideo {},
    /// The loudest sound the call plays now changed (ADR-0055): 0 to
    /// [`MAX_CALL_SOUND_LEVEL`], on a decibel scale. A number only, no call
    /// data. Coalesced: only the latest waits, and at most about ten leave
    /// the core each second. A value over the top is refused by the UI host.
    CallSoundLevel {
        level: u8,
    },
    /// The sound of the other devices in the call changed (ADR-0059): each
    /// device that is not silent now, at most [`MAX_CALL_LEVELS`], with
    /// validated IDs and a level of 0 to [`MAX_CALL_SOUND_LEVEL`]. Empty
    /// when the call ends. Coalesced like `CallSoundLevel`.
    CallLevels {
        #[serde(deserialize_with = "bounded_levels")]
        levels: Vec<persephone_contracts::CallParticipantLevel>,
    },
    /// The call's technical details (ADR-0066), sent only while
    /// [`Call::CallStats`] is on: numbers with units, and the validated IDs
    /// of the devices. Coalesced: only the newest waits. A reading that is
    /// out of bounds is refused whole.
    #[serde(deserialize_with = "call_stats_content")]
    CallStats {
        stats: persephone_contracts::CallStats,
    },
    /// A device test started, changed or ended (ADR-0059): ask for
    /// `call_device_test_status`. Content-free and coalesced.
    CallDeviceTestChanged {},
    /// An incoming call ring started, ended or changed (ADR-0052): ask for
    /// `call_rings`. Content-free and coalesced like `CallChanged`.
    RingsChanged {},
    /// The list of third-party modules changed: ask for `third_party_modules`.
    /// Content-free and coalesced like `SecurityChanged`.
    ModulesChanged {},
    /// The diagnostic log was turned on or off (ADR-0058). The UI host and
    /// the page send their records only while it is on. Coalesced; `on` is
    /// the state when the event left the core.
    DiagnosticsChanged {
        on: bool,
    },
    /// A message arrived that the account's push rules say to notify of, and
    /// desktop notifications are on (ADR-0022). The UI process shows a
    /// desktop notification with these names and never forwards it to the
    /// page. Names only, never message text; each is projected and at most
    /// [`MAX_NOTIFICATION_NAME_BYTES`]. Both are empty when the account's
    /// settings leave names out. Coalesced: only the latest waits. `call` is
    /// true for an incoming call ring (ADR-0052): the text is then "Incoming
    /// call", with the caller's name only when `sender` is not empty.
    Notification {
        room: String,
        sender: String,
        #[serde(default)]
        call: bool,
    },
}

/// Most devices in one `call_levels` event: the most call participants.
pub const MAX_CALL_LEVELS: usize = persephone_contracts::CALL_PARTICIPANTS_MAX;

/// Whether every level names a device with bounded IDs and is in range.
pub fn levels_within_limits(levels: &[persephone_contracts::CallParticipantLevel]) -> bool {
    levels.len() <= MAX_CALL_LEVELS
        && levels.iter().all(|level| {
            level.user_id.len() <= MAX_USER_ID_BYTES
                && level.device_id.len() <= MAX_DEVICE_ID_BYTES
                && level.level <= MAX_CALL_SOUND_LEVEL
        })
}

/// Reads the content of [`Event::CallStats`]. Only a JSON object with the one
/// key `stats` is read: the derived reader would also take a positional
/// array. A repeated or unknown key is an error.
fn call_stats_content<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<persephone_contracts::CallStats, D::Error> {
    struct Content;
    impl<'de> serde::de::Visitor<'de> for Content {
        type Value = persephone_contracts::CallStats;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object with the key `stats`")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            enum Field {
                Stats,
            }
            let mut stats = None;
            while let Some(Field::Stats) = map.next_key::<Field>()? {
                if stats.replace(map.next_value::<WireStatsValue>()?).is_some() {
                    return Err(A::Error::duplicate_field("stats"));
                }
            }
            stats
                .map(|stats| stats.0)
                .ok_or_else(|| A::Error::missing_field("stats"))
        }
    }
    deserializer.deserialize_map(Content)
}

/// A statistics reading, read by [`bounded_stats`].
struct WireStatsValue(persephone_contracts::CallStats);

impl<'de> Deserialize<'de> for WireStatsValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        bounded_stats(deserializer).map(WireStatsValue)
    }
}

/// Wraps a deserializer so that a struct reads only from a JSON object. A
/// derived struct reader also takes a positional array, which is not the
/// contract.
struct MapOnlyDeserializer<D>(D);

impl<'de, D: serde::Deserializer<'de>> serde::Deserializer<'de> for MapOnlyDeserializer<D> {
    type Error = D::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_map(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// A value read through [`MapOnlyDeserializer`].
struct MapOnly<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapOnly<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(MapOnlyDeserializer(deserializer)).map(MapOnly)
    }
}

/// Reads a statistics reading. Only a JSON object is read, each list stops
/// at its limit before it builds one more entry, and each ID is bounded
/// before it is copied. `CallStats::within_limits` then checks the numbers
/// and repeats: a reading that fails is refused whole.
fn bounded_stats<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<persephone_contracts::CallStats, D::Error> {
    let stats = deserializer.deserialize_map(WireStats)?;
    if stats.within_limits() {
        Ok(stats)
    } else {
        Err(serde::de::Error::custom("call statistics out of bounds"))
    }
}

/// The map-only reader of a statistics reading. Every key is required, and
/// the keys of the parts that may be null take `null`.
struct WireStats;

impl<'de> serde::de::Visitor<'de> for WireStats {
    type Value = persephone_contracts::CallStats;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a call statistics object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Field {
            Microphone,
            Camera,
            Screen,
            Participants,
            Connections,
        }
        let (mut microphone, mut camera, mut screen) = (None, None, None);
        let (mut participants, mut connections) = (None, None);
        while let Some(field) = map.next_key::<Field>()? {
            let fresh = match field {
                Field::Microphone => microphone
                    .replace(
                        map.next_value::<Option<MapOnly<persephone_contracts::CallStatsAudioSent>>>()?,
                    )
                    .is_none(),
                Field::Camera => camera
                    .replace(
                        map.next_value::<Option<MapOnly<persephone_contracts::CallStatsVideoSent>>>()?,
                    )
                    .is_none(),
                Field::Screen => screen
                    .replace(
                        map.next_value::<Option<MapOnly<persephone_contracts::CallStatsVideoSent>>>()?,
                    )
                    .is_none(),
                Field::Participants => participants
                    .replace(map.next_value::<BoundedList<
                        WireParticipant,
                        { persephone_contracts::CALL_PARTICIPANTS_MAX },
                    >>()?)
                    .is_none(),
                Field::Connections => connections
                    .replace(map.next_value::<BoundedList<
                        MapOnly<persephone_contracts::CallStatsConnection>,
                        { persephone_contracts::CALL_STATS_CONNECTIONS_MAX },
                    >>()?)
                    .is_none(),
            };
            if !fresh {
                return Err(A::Error::custom("duplicate call statistics field"));
            }
        }
        let participants = participants.ok_or_else(|| A::Error::missing_field("participants"))?;
        Ok(persephone_contracts::CallStats {
            microphone: microphone
                .ok_or_else(|| A::Error::missing_field("microphone"))?
                .map(|audio| audio.0),
            camera: camera
                .ok_or_else(|| A::Error::missing_field("camera"))?
                .map(|video| video.0),
            screen: screen
                .ok_or_else(|| A::Error::missing_field("screen"))?
                .map(|video| video.0),
            participants: participants
                .0
                .into_iter()
                .map(|participant| persephone_contracts::CallStatsParticipant {
                    user_id: participant.user_id.0,
                    device_id: participant.device_id.0,
                    audio: participant.audio.map(|audio| audio.0),
                    video: participant.video.map(|video| video.0),
                })
                .collect(),
            connections: connections
                .ok_or_else(|| A::Error::missing_field("connections"))?
                .0
                .into_iter()
                .map(|connection| connection.0)
                .collect(),
        })
    }
}

/// One device of a statistics reading as it crosses, with its IDs bounded.
/// Only a JSON object is read, and every key is required.
struct WireParticipant {
    user_id: BoundedText<MAX_USER_ID_BYTES>,
    device_id: BoundedText<MAX_DEVICE_ID_BYTES>,
    audio: Option<MapOnly<persephone_contracts::CallStatsAudioReceived>>,
    video: Option<MapOnly<persephone_contracts::CallStatsVideoReceived>>,
}

impl<'de> Deserialize<'de> for WireParticipant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Field {
            UserId,
            DeviceId,
            Audio,
            Video,
        }
        struct Entry;
        impl<'de> serde::de::Visitor<'de> for Entry {
            type Value = WireParticipant;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a call statistics participant object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::Error;
                let (mut user_id, mut device_id, mut audio, mut video) = (None, None, None, None);
                while let Some(field) = map.next_key::<Field>()? {
                    let fresh = match field {
                        Field::UserId => user_id.replace(map.next_value()?).is_none(),
                        Field::DeviceId => device_id.replace(map.next_value()?).is_none(),
                        Field::Audio => audio.replace(map.next_value()?).is_none(),
                        Field::Video => video.replace(map.next_value()?).is_none(),
                    };
                    if !fresh {
                        return Err(A::Error::custom("duplicate call statistics field"));
                    }
                }
                Ok(WireParticipant {
                    user_id: user_id.ok_or_else(|| A::Error::missing_field("userId"))?,
                    device_id: device_id.ok_or_else(|| A::Error::missing_field("deviceId"))?,
                    audio: audio.ok_or_else(|| A::Error::missing_field("audio"))?,
                    video: video.ok_or_else(|| A::Error::missing_field("video"))?,
                })
            }
        }
        deserializer.deserialize_map(Entry)
    }
}

/// Reads a level list and refuses an entry past [`MAX_CALL_LEVELS`] or an
/// overlong ID before it keeps it, so a hostile list allocates at most the
/// limits, and then a level out of range.
fn bounded_levels<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<persephone_contracts::CallParticipantLevel>, D::Error> {
    let BoundedList(levels) = BoundedList::<WireLevel, MAX_CALL_LEVELS>::deserialize(deserializer)?;
    if levels
        .iter()
        .any(|level| level.level > MAX_CALL_SOUND_LEVEL)
    {
        return Err(serde::de::Error::custom("call levels out of bounds"));
    }
    Ok(levels
        .into_iter()
        .map(|level| persephone_contracts::CallParticipantLevel {
            user_id: level.user_id.0,
            device_id: level.device_id.0,
            level: level.level,
        })
        .collect())
}

/// A list of at most `N` entries. The entry past the limit is refused
/// before it is built: a full list only checks for one more, unbuilt.
struct BoundedList<T, const N: usize>(Vec<T>);

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedList<T, N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct List<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for List<T, N> {
            type Value = BoundedList<T, N>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a list of at most {N} entries")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                loop {
                    if entries.len() == N {
                        if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                            return Err(serde::de::Error::custom(format_args!(
                                "list over its limit of {N} entries"
                            )));
                        }
                        return Ok(BoundedList(entries));
                    }
                    let Some(entry) = seq.next_element()? else {
                        return Ok(BoundedList(entries));
                    };
                    entries.push(entry);
                }
            }
        }
        deserializer.deserialize_seq(List(std::marker::PhantomData))
    }
}

/// One entry of a level list as it crosses, with its IDs bounded. Only a
/// JSON object is read: a derived visitor would also take a positional
/// array, which is not the contract.
struct WireLevel {
    user_id: BoundedText<MAX_USER_ID_BYTES>,
    device_id: BoundedText<MAX_DEVICE_ID_BYTES>,
    level: u8,
}

impl<'de> Deserialize<'de> for WireLevel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Field {
            UserId,
            DeviceId,
            Level,
        }
        struct Entry;
        impl<'de> serde::de::Visitor<'de> for Entry {
            type Value = WireLevel;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a call level object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::Error;
                let (mut user_id, mut device_id, mut level) = (None, None, None);
                while let Some(field) = map.next_key::<Field>()? {
                    let fresh = match field {
                        Field::UserId => user_id.replace(map.next_value()?).is_none(),
                        Field::DeviceId => device_id.replace(map.next_value()?).is_none(),
                        Field::Level => level.replace(map.next_value()?).is_none(),
                    };
                    if !fresh {
                        return Err(A::Error::custom("duplicate call level field"));
                    }
                }
                Ok(WireLevel {
                    user_id: user_id.ok_or_else(|| A::Error::missing_field("userId"))?,
                    device_id: device_id.ok_or_else(|| A::Error::missing_field("deviceId"))?,
                    level: level.ok_or_else(|| A::Error::missing_field("level"))?,
                })
            }
        }
        deserializer.deserialize_map(Entry)
    }
}

/// A string of at most `N` bytes, refused before it is copied.
struct BoundedText<const N: usize>(String);

impl<'de, const N: usize> Deserialize<'de> for BoundedText<N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Text<const N: usize>;
        impl<const N: usize> serde::de::Visitor<'_> for Text<N> {
            type Value = BoundedText<N>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a string of at most {N} bytes")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                if text.len() > N {
                    return Err(E::custom("text too long"));
                }
                Ok(BoundedText(text.to_owned()))
            }
        }
        deserializer.deserialize_str(Text::<N>)
    }
}

/// Most records in one `diagnostic_records` call.
pub const MAX_DIAGNOSTIC_RECORDS: usize = 64;
/// Longest record message, in bytes.
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
/// Longest record target, in bytes.
pub const MAX_DIAGNOSTIC_TARGET_BYTES: usize = 64;
/// Most bytes of one call's records as JSON. JSON escaping can make a
/// control character six bytes, so records within their own limits can
/// still exceed the control frame; this keeps the call well under it. A
/// full batch of plain records at their limits (about 72 KiB) fits.
pub const MAX_DIAGNOSTIC_BATCH_BYTES: usize = 96 * 1024;

/// The process a `diagnostic_records` call speaks for.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordSource {
    Ui,
    Page,
}

/// A record's level.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordLevel {
    Error,
    Warn,
    Info,
    Debug,
}

/// One record from the UI host or the page: a target of at most
/// [`MAX_DIAGNOSTIC_TARGET_BYTES`] and a message of at most
/// [`MAX_DIAGNOSTIC_MESSAGE_BYTES`]. The core redacts both again.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRecord {
    pub level: RecordLevel,
    pub target: String,
    pub message: String,
}

impl DiagnosticRecord {
    pub fn within_bounds(&self) -> bool {
        self.target.len() <= MAX_DIAGNOSTIC_TARGET_BYTES
            && self.message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES
    }

    /// This record's size as JSON, with escapes.
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }

    /// Whether `records` may go in one call: at most
    /// [`MAX_DIAGNOSTIC_RECORDS`], each within its bounds, and at most
    /// [`MAX_DIAGNOSTIC_BATCH_BYTES`] as JSON.
    pub fn batch_fits(records: &[Self]) -> bool {
        records.len() <= MAX_DIAGNOSTIC_RECORDS
            && records.iter().all(Self::within_bounds)
            && serde_json::to_vec(records)
                .is_ok_and(|bytes| bytes.len() <= MAX_DIAGNOSTIC_BATCH_BYTES)
    }
}

impl Call {
    /// The command's name, such as `proxy_setting`, without its arguments:
    /// none of them is copied (ADR-0058).
    pub fn command(&self) -> Option<String> {
        command_name::tag_of(self)
    }
}

/// Every frame from the core to the UI: a reply to a call, or an event.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreMessage {
    Reply(Reply),
    Event(Event),
}

/// Longest room ID either side accepts in a call (Matrix limits IDs to 255 bytes).
pub const MAX_ROOM_ID_BYTES: usize = 255;
/// Longest opaque timeline cursor either side accepts in a call.
pub const MAX_CURSOR_BYTES: usize = 64;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: u32,
    pub call: Call,
}

/// Errors the core reports to the UI. Codes only; no text from the core's
/// internals crosses the boundary.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unavailable,
    Internal,
    /// An argument failed validation.
    InvalidArgument,
    /// The named room or cursor does not exist.
    NotFound,
    /// A limit such as [`MAX_SUBSCRIPTIONS`] would be exceeded.
    LimitExceeded,
    /// The session is locked (or locking); protected views are unavailable.
    Locked,
    /// The image was refused: too large, malformed, not PNG or JPEG, it
    /// failed its integrity check, or the server refused the file (a 4xx
    /// status other than 404 or 429). Trying again gives the same answer.
    Rejected,
    /// Sign-out did not start: this device's room keys are not all in a key
    /// backup, so signing out would lose encrypted history.
    KeysNotBackedUp,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Ok(serde_json::Value),
    Error(ErrorCode),
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub id: u32,
    pub outcome: Outcome,
}

/// Announces the pixel frame that follows it: `rows` rows of `width` RGBA
/// pixels, band number `band` of the image, for request `id`. On the wire it
/// is `{"pixel_reply":{...}}` in a control frame.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PixelReply {
    pub id: u32,
    pub width: u32,
    pub rows: u32,
    pub band: u32,
}

impl PixelReply {
    /// The pixel frame header this reply announces, or `None` when the band
    /// is empty, too wide or over [`MAX_BAND_BYTES`].
    pub fn frame_header(&self) -> Option<FrameHeader> {
        if self.width == 0 || self.rows == 0 || self.width > MAX_DIMENSION {
            return None;
        }
        let length = u64::from(self.width)
            .checked_mul(u64::from(self.rows))?
            .checked_mul(RGBA_BYTES_PER_PIXEL)
            .filter(|bytes| *bytes <= MAX_BAND_BYTES)?;
        Some(FrameHeader {
            kind: FrameKind::RgbaPixels,
            length,
            width: self.width,
            height: self.rows,
        })
    }
}

/// Announces the video picture that follows it: tile `tile`, picture number
/// `seq`, turned `rotation` quarter turns clockwise, `width` by `height` RGBA
/// pixels, for request `id`. On the wire it is `{"video_reply":{...}}` in a
/// control frame.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VideoReply {
    pub id: u32,
    pub tile: u8,
    pub seq: u64,
    pub rotation: u8,
    pub width: u32,
    pub height: u32,
}

impl VideoReply {
    /// The pixel frame header this reply announces, or `None` when the tile
    /// or rotation is out of range, the picture number is not JavaScript-safe,
    /// or the picture is empty, too wide or too tall, or over
    /// [`MAX_CALL_VIDEO_PIXELS`].
    pub fn frame_header(&self) -> Option<FrameHeader> {
        if self.tile >= CALL_VIDEO_TILES
            || self.rotation > MAX_CALL_VIDEO_ROTATION
            || self.seq > MAX_SAFE_TIMESTAMP
        {
            return None;
        }
        // The limits are the tile's: checked before any buffer exists.
        let side = max_call_video_side(self.tile);
        if !(1..=side).contains(&self.width) || !(1..=side).contains(&self.height) {
            return None;
        }
        let pixels = u64::from(self.width) * u64::from(self.height);
        if pixels > u64::from(max_call_video_pixels(self.tile)) {
            return None;
        }
        Some(FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: pixels * RGBA_BYTES_PER_PIXEL,
            width: self.width,
            height: self.height,
        })
    }
}

/// A [`VideoReply`] and its pixel frame. The frame keeps its budget
/// reservation until it is dropped.
pub struct VideoPicture {
    pub reply: VideoReply,
    pub pixels: ReceivedFrame,
}

/// Cheap checks for the relay and authoritative receiver on the call video
/// commands: [`Call::CallVideoFrame`] needs a tile below [`CALL_VIDEO_TILES`]
/// and a JavaScript-safe `after` and `stream`; a camera ID in [`Call::SetCallDevices`]
/// must be non-empty, at most [`MAX_CALL_DEVICE_ID_BYTES`] and free of
/// control characters; [`Call::CallVolume`] is at most
/// [`MAX_CALL_VOLUME_PERCENT`]; [`Call::CallMicrophone`] has a gain of at most
/// [`MAX_CALL_MIC_GAIN_PERCENT`] on a step of [`CALL_MIC_GAIN_STEP_PERCENT`].
/// Any other call is not constrained here and
/// gives `true`.
pub fn call_video_within_limits(call: &Call) -> bool {
    match call {
        Call::CallVideoFrame {
            tile,
            after,
            stream,
        } => {
            *tile < CALL_VIDEO_TILES
                && *after <= MAX_SAFE_TIMESTAMP
                && *stream <= MAX_SAFE_TIMESTAMP
        }
        Call::SetCallDevices { camera, .. } => camera.as_ref().is_none_or(|id| {
            !id.is_empty()
                && id.len() <= MAX_CALL_DEVICE_ID_BYTES
                && !id.chars().any(char::is_control)
        }),
        Call::CallDeviceTest { device, .. } => device.as_ref().is_none_or(|id| {
            !id.is_empty()
                && id.len() <= MAX_CALL_DEVICE_ID_BYTES
                && !id.chars().any(char::is_control)
        }),
        Call::CallVolume { percent } => *percent <= MAX_CALL_VOLUME_PERCENT,
        Call::CallMicrophone { gain, .. } => {
            *gain <= MAX_CALL_MIC_GAIN_PERCENT && gain.is_multiple_of(CALL_MIC_GAIN_STEP_PERCENT)
        }
        _ => true,
    }
}

/// A [`PixelReply`] and its pixel frame. The frame keeps its budget
/// reservation until it is dropped.
pub struct PixelBand {
    pub reply: PixelReply,
    pub pixels: ReceivedFrame,
}

/// What the UI reads from the core: a message, a band of pixels, or a video
/// picture.
pub enum FromCore {
    Message(CoreMessage),
    Pixels(PixelBand),
    Video(VideoPicture),
}

/// Every control message from the core: [`CoreMessage`]'s two forms and the
/// pixel and video replies, in the same externally tagged form.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Wire<R, P, V> {
    Reply(R),
    Event(Event),
    PixelReply(P),
    VideoReply(V),
}

/// The fixed frame header length of `persephone-media-protocol`.
const FRAME_HEADER_LEN: usize = 17;

#[derive(Debug)]
pub enum ReceiveError {
    /// The peer closed the stream, at or within a frame.
    Closed,
    /// The frame header failed validation; its body was not read.
    Frame(FrameError),
    /// The frame was not a control frame.
    UnexpectedKind,
    /// The body was not a valid message.
    Malformed,
}

/// Writes one message as a control frame. Fails without writing anything if
/// the encoded message exceeds the control frame limit.
pub fn send<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    let header = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, header, &body)
}

/// Reads one message. The frame header is validated and the budget reserved
/// before the body is allocated.
pub fn receive<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<T, ReceiveError> {
    let frame = read_frame(reader, budgets).map_err(|error| match error {
        FrameError::Io(_) => ReceiveError::Closed,
        other => ReceiveError::Frame(other),
    })?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReceiveError::UnexpectedKind);
    }
    serde_json::from_slice(frame.body()).map_err(|_| ReceiveError::Malformed)
}

fn frame_error(error: FrameError) -> ReceiveError {
    match error {
        FrameError::Io(_) => ReceiveError::Closed,
        other => ReceiveError::Frame(other),
    }
}

/// Writes a [`PixelReply`] and then its pixel frame. Everything is checked
/// before anything is written: `pixels` must be exactly `width * rows` RGBA
/// pixels and at most [`MAX_BAND_BYTES`]. The caller holds its output lock
/// across this call, so no other frame comes between the two.
pub fn send_pixel_reply<W: Write>(
    writer: &mut W,
    reply: &PixelReply,
    pixels: &[u8],
) -> io::Result<()> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    let header = reply
        .frame_header()
        .ok_or_else(|| invalid("pixel band outside the band limit"))?;
    if u64::try_from(pixels.len()).ok() != Some(header.length) {
        return Err(invalid("pixel band length does not match its reply"));
    }
    FrameHeader::decode(header.encode())
        .map_err(|_| invalid("pixel frame header fails receiver validation"))?;
    let body = serde_json::to_vec(&Wire::<(), &PixelReply, ()>::PixelReply(reply))
        .map_err(io::Error::other)?;
    let control = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, control, &body)?;
    write_frame(writer, header, pixels)
}

/// Writes a [`VideoReply`] and then its pixel frame. Everything is checked
/// before anything is written: the reply must be inside the video limits, and
/// `pixels` must be exactly `width * height` RGBA pixels. The caller holds
/// its output lock across this call, so no other frame comes between the two.
pub fn send_video_reply<W: Write>(
    writer: &mut W,
    reply: &VideoReply,
    pixels: &[u8],
) -> io::Result<()> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    let header = reply
        .frame_header()
        .ok_or_else(|| invalid("video picture outside the video limits"))?;
    if u64::try_from(pixels.len()).ok() != Some(header.length) {
        return Err(invalid("video picture length does not match its reply"));
    }
    FrameHeader::decode(header.encode())
        .map_err(|_| invalid("pixel frame header fails receiver validation"))?;
    let body = serde_json::to_vec(&Wire::<(), (), &VideoReply>::VideoReply(reply))
        .map_err(io::Error::other)?;
    let control = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, control, &body)?;
    write_frame(writer, header, pixels)
}

/// The most bytes of one pasted image, the same limit as a picked file.
pub const MAX_PASTED_IMAGE_BYTES: u32 = 20 * 1024 * 1024;

/// Writes a `paste_image` request and then its image frame. Everything is
/// checked before anything is written: the call must be `PasteImage`, and
/// `bytes` must be exactly its `length`, from 1 to [`MAX_PASTED_IMAGE_BYTES`].
/// The caller holds its output lock across this call, so no other frame
/// comes between the two.
pub fn send_pasted_image<W: Write>(
    writer: &mut W,
    request: &Request,
    bytes: &[u8],
) -> io::Result<()> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    let Call::PasteImage { length, .. } = &request.call else {
        return Err(invalid("only paste_image carries an image frame"));
    };
    if *length == 0
        || *length > MAX_PASTED_IMAGE_BYTES
        || u32::try_from(bytes.len()).ok() != Some(*length)
    {
        return Err(invalid("pasted image length outside its request or limit"));
    }
    let header = FrameHeader {
        kind: FrameKind::EncodedImage,
        length: u64::from(*length),
        width: 0,
        height: 0,
    };
    FrameHeader::decode(header.encode())
        .map_err(|_| invalid("image frame header fails receiver validation"))?;
    send(writer, request)?;
    write_frame(writer, header, bytes)
}

/// What the core reads from the UI: a request, or a pasted image with its
/// request.
pub enum ToCore {
    Request(Request),
    /// The image frame, read under its `EncodedInFlight` reservation.
    Paste(Request, ReceivedFrame),
    /// A paste whose image did not fit the budget: its bytes were read and
    /// dropped, so the channel stays in step, and it is answered with
    /// `limit_exceeded`.
    PasteRefused(Request),
}

/// Reads one request (the core side). After a `paste_image` request, the
/// next frame must be an `EncodedImage` frame of exactly the declared
/// length, at most [`MAX_PASTED_IMAGE_BYTES`]. That header is checked before
/// the budget is reserved and the body allocated; anything else breaks the
/// channel.
pub fn receive_request<R: Read>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<ToCore, ReceiveError> {
    let request: Request = receive(reader, budgets)?;
    let Call::PasteImage { length, .. } = &request.call else {
        return Ok(ToCore::Request(request));
    };
    if *length == 0 || *length > MAX_PASTED_IMAGE_BYTES {
        return Err(ReceiveError::Malformed);
    }
    let mut wire = [0; FRAME_HEADER_LEN];
    reader
        .read_exact(&mut wire)
        .map_err(|_| ReceiveError::Closed)?;
    let header = FrameHeader::decode(wire).map_err(ReceiveError::Frame)?;
    if header.kind != FrameKind::EncodedImage {
        return Err(ReceiveError::UnexpectedKind);
    }
    if header.length != u64::from(*length) {
        return Err(ReceiveError::Malformed);
    }
    match read_frame(&mut (&wire[..]).chain(&mut *reader), budgets) {
        Ok(frame) => Ok(ToCore::Paste(request, frame)),
        // Refused before the body was read or allocated: read past it in
        // small pieces.
        Err(FrameError::Budget(_)) => {
            let skipped = io::copy(&mut reader.by_ref().take(header.length), &mut io::sink())
                .map_err(|_| ReceiveError::Closed)?;
            if skipped != header.length {
                return Err(ReceiveError::Closed);
            }
            Ok(ToCore::PasteRefused(request))
        }
        Err(error) => Err(frame_error(error)),
    }
}

/// Reads one message, pixel band or video picture from the core (the UI
/// side). After a [`PixelReply`] or [`VideoReply`], the next frame must be an
/// `RgbaPixels` frame whose width, height and length match it. That header is
/// checked before the body is allocated, and the body is read with the
/// budgeted reader. A reply over its limit is refused without reading the
/// frame that follows.
pub fn receive_from_core<R: Read>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<FromCore, ReceiveError> {
    let frame = read_frame(reader, budgets).map_err(frame_error)?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReceiveError::UnexpectedKind);
    }
    let (expected, reply) =
        match serde_json::from_slice::<Wire<Reply, PixelReply, VideoReply>>(frame.body()) {
            Ok(Wire::Reply(reply)) => return Ok(FromCore::Message(CoreMessage::Reply(reply))),
            Ok(Wire::Event(event)) => return Ok(FromCore::Message(CoreMessage::Event(event))),
            Ok(Wire::PixelReply(reply)) => (reply.frame_header(), Announced::Band(reply)),
            Ok(Wire::VideoReply(reply)) => (reply.frame_header(), Announced::Video(reply)),
            Err(_) => return Err(ReceiveError::Malformed),
        };
    drop(frame);
    let expected = expected.ok_or(ReceiveError::Malformed)?;
    let mut wire = [0; FRAME_HEADER_LEN];
    reader
        .read_exact(&mut wire)
        .map_err(|_| ReceiveError::Closed)?;
    let header = FrameHeader::decode(wire).map_err(ReceiveError::Frame)?;
    if header.kind != FrameKind::RgbaPixels {
        return Err(ReceiveError::UnexpectedKind);
    }
    if header != expected {
        return Err(ReceiveError::Malformed);
    }
    // The header was checked; give it back to the budgeted reader together
    // with the body that follows it.
    let pixels = read_frame(&mut (&wire[..]).chain(reader), budgets).map_err(frame_error)?;
    Ok(match reply {
        Announced::Band(reply) => FromCore::Pixels(PixelBand { reply, pixels }),
        Announced::Video(reply) => FromCore::Video(VideoPicture { reply, pixels }),
    })
}

/// The reply that announced a pixel frame.
enum Announced {
    Band(PixelReply),
    Video(VideoReply),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: FrameKind, body: &[u8]) -> Vec<u8> {
        let mut wire = FrameHeader {
            kind,
            length: body.len() as u64,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(body);
        wire
    }

    #[test]
    fn given_request_and_reply_when_sent_then_they_round_trip() {
        let mut wire = Vec::new();
        let request = Request {
            id: 7,
            call: Call::AppStatus {},
        };
        send(&mut wire, &request).unwrap();
        assert_eq!(
            String::from_utf8(wire[17..].to_vec()).unwrap(),
            r#"{"id":7,"call":{"command":"app_status"}}"#
        );
        let reply = Reply {
            id: 7,
            outcome: Outcome::Error(ErrorCode::Unavailable),
        };
        send(&mut wire, &reply).unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        assert_eq!(
            receive::<_, Request>(&mut reader, &budgets).unwrap(),
            request
        );
        assert_eq!(receive::<_, Reply>(&mut reader, &budgets).unwrap(), reply);
        assert!(matches!(
            receive::<_, Reply>(&mut reader, &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    #[test]
    fn given_core_messages_when_sent_then_replies_and_events_are_distinguished() {
        let mut wire = Vec::new();
        let reply = CoreMessage::Reply(Reply {
            id: 3,
            outcome: Outcome::Error(ErrorCode::LimitExceeded),
        });
        let event = CoreMessage::Event(Event::RoomChanged {
            room_id: "!r:example.invalid".into(),
        });
        let session = CoreMessage::Event(Event::SessionChanged {});
        let list = CoreMessage::Event(Event::RoomListChanged {});
        let security = CoreMessage::Event(Event::SecurityChanged {});
        let calls = CoreMessage::Event(Event::CallChanged {});
        let video = CoreMessage::Event(Event::CallVideo {});
        let modules = CoreMessage::Event(Event::ModulesChanged {});
        let account = CoreMessage::Event(Event::AccountChanged {});
        send(&mut wire, &reply).unwrap();
        send(&mut wire, &event).unwrap();
        send(&mut wire, &session).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"session_changed":{}}}"#),
            "wire form of the session event"
        );
        send(&mut wire, &list).unwrap();
        send(&mut wire, &security).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"security_changed":{}}}"#),
            "wire form of the security event"
        );
        send(&mut wire, &calls).unwrap();
        send(&mut wire, &video).unwrap();
        send(&mut wire, &modules).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).contains(r#"{"event":{"call_changed":{}}}"#),
            "wire form of the call event"
        );
        assert!(
            String::from_utf8_lossy(&wire).contains(r#"{"event":{"call_video":{}}}"#),
            "wire form of the call video event"
        );
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"modules_changed":{}}}"#),
            "wire form of the modules event"
        );
        send(&mut wire, &account).unwrap();
        assert!(
            String::from_utf8_lossy(&wire).ends_with(r#"{"event":{"account_changed":{}}}"#),
            "wire form of the account event"
        );
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        for expected in [
            reply, event, session, list, security, calls, video, modules, account,
        ] {
            assert_eq!(
                receive::<_, CoreMessage>(&mut reader, &budgets).unwrap(),
                expected
            );
        }
        for body in [
            &br#"{"event":{"room_changed":{"room_id":"!r","body":"x"}}}"#[..],
            br#"{"event":{"message_added":{"room_id":"!r"}}}"#,
            br#"{"reply":{"id":1,"outcome":{"ok":null}},"event":null}"#,
            br#"{"event":{"session_changed":{"state":"locked"}}}"#,
            br#"{"event":"session_changed"}"#,
            br#"{"event":{"room_list_changed":{"rooms":[]}}}"#,
            br#"{"event":{"security_changed":{"identity":"verified"}}}"#,
            br#"{"event":{"security_changed":{"recovery_key":"x"}}}"#,
            // Content-free: no reason text crosses with a presence refusal.
            br#"{"event":{"account_changed":{"error":"Presence is disabled"}}}"#,
            br#"{"event":"account_changed"}"#,
            br#"{"event":{"call_changed":{"room_id":"!r:s"}}}"#,
            br#"{"event":{"call_changed":{"participants":[]}}}"#,
            br#"{"event":"call_changed"}"#,
            br#"{"event":{"call_video":{"tile":1}}}"#,
            br#"{"event":"call_video"}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, CoreMessage>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// GIVEN the contact calls WHEN sent THEN only the room ID crosses, in the
    /// documented wire form.
    #[test]
    fn given_contact_calls_then_only_the_room_id_crosses() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::ContactStatus {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"contact_status","room_id":"!r:s"}}"#,
            ),
            (
                Call::VerifyContact {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"verify_contact","room_id":"!r:s"}}"#,
            ),
            (
                Call::VerifyMember {
                    room_id: "!r:s".into(),
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"verify_member","room_id":"!r:s","user_id":"@b:s"}}"#,
            ),
            (
                Call::IdentityChanges {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"identity_changes","room_id":"!r:s"}}"#,
            ),
            (
                Call::AcceptIdentity {
                    room_id: "!r:s".into(),
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"accept_identity","room_id":"!r:s","user_id":"@b:s"}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
    }

    /// GIVEN the conversation and directory calls WHEN sent THEN only the
    /// room ID, user ID, search term or directory page arguments cross, in
    /// the documented wire form.
    #[test]
    fn given_conversation_calls_then_only_their_documented_arguments_cross() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::InviteList {},
                r#"{"id":1,"call":{"command":"invite_list"}}"#,
            ),
            (
                Call::AcceptInvite {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"accept_invite","room_id":"!r:s"}}"#,
            ),
            (
                Call::DeclineInvite {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"decline_invite","room_id":"!r:s"}}"#,
            ),
            (
                Call::StartConversation {
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"start_conversation","user_id":"@b:s"}}"#,
            ),
            (
                Call::SearchUsers { term: "bob".into() },
                r#"{"id":1,"call":{"command":"search_users","term":"bob"}}"#,
            ),
            (
                Call::PublicRooms {
                    server: None,
                    filter: None,
                    since: None,
                },
                r#"{"id":1,"call":{"command":"public_rooms","server":null,"filter":null,"since":null}}"#,
            ),
            (
                Call::PublicRooms {
                    server: Some("example.org".into()),
                    filter: Some("rust".into()),
                    since: Some("p30".into()),
                },
                r#"{"id":1,"call":{"command":"public_rooms","server":"example.org","filter":"rust","since":"p30"}}"#,
            ),
            (
                Call::JoinPublicRoom {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"join_public_room","room_id":"!r:s"}}"#,
            ),
            (
                Call::Register {
                    homeserver: "https://h".into(),
                    username: "al".into(),
                },
                r#"{"id":1,"call":{"command":"register","homeserver":"https://h","username":"al"}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
    }

    /// GIVEN the room, account and profile calls WHEN sent THEN only their
    /// documented arguments cross: names, IDs and a slot number, never a path.
    #[test]
    fn given_room_account_and_profile_calls_then_they_have_a_strict_wire_form() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::CreateRoom {
                    name: "Plans".into(),
                    invite: vec!["@b:s".into()],
                },
                r#"{"id":1,"call":{"command":"create_room","name":"Plans","invite":["@b:s"]}}"#,
            ),
            (
                Call::InviteUser {
                    room_id: "!r:s".into(),
                    user_id: "@b:s".into(),
                },
                r#"{"id":1,"call":{"command":"invite_user","room_id":"!r:s","user_id":"@b:s"}}"#,
            ),
            (
                Call::DisplayName {},
                r#"{"id":1,"call":{"command":"display_name"}}"#,
            ),
            (
                Call::SendMessage {
                    room_id: "!r:s".into(),
                    body: "yes".into(),
                    reply_to: Some("$e".into()),
                },
                r#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"yes","reply_to":"$e"}}"#,
            ),
            (
                Call::SendMessage {
                    room_id: "!r:s".into(),
                    body: "hi".into(),
                    reply_to: None,
                },
                r#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"hi","reply_to":null}}"#,
            ),
            (
                Call::PickFile {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"pick_file","room_id":"!r:s"}}"#,
            ),
            (
                Call::SaveFile {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                },
                r#"{"id":1,"call":{"command":"save_file","room_id":"!r:s","event_id":"$e"}}"#,
            ),
            (
                Call::RoomReaders {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"room_readers","room_id":"!r:s"}}"#,
            ),
            (
                Call::RoomServers {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"room_servers","room_id":"!r:s"}}"#,
            ),
            (
                Call::OpenLink {
                    url: "https://example.org/".into(),
                },
                r#"{"id":1,"call":{"command":"open_link","url":"https://example.org/"}}"#,
            ),
            (
                Call::RoomActivity {
                    room_id: "!r:s".into(),
                    event_id: Some("$e".into()),
                },
                r#"{"id":1,"call":{"command":"room_activity","room_id":"!r:s","event_id":"$e"}}"#,
            ),
            (
                Call::SetTyping {
                    room_id: "!r:s".into(),
                    typing: true,
                },
                r#"{"id":1,"call":{"command":"set_typing","room_id":"!r:s","typing":true}}"#,
            ),
            (
                Call::MarkRead {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                },
                r#"{"id":1,"call":{"command":"mark_read","room_id":"!r:s","event_id":"$e"}}"#,
            ),
            (
                Call::React {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                    key: "👍".into(),
                },
                r#"{"id":1,"call":{"command":"react","room_id":"!r:s","event_id":"$e","key":"👍"}}"#,
            ),
            (
                Call::DeleteMessage {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                },
                r#"{"id":1,"call":{"command":"delete_message","room_id":"!r:s","event_id":"$e"}}"#,
            ),
            (
                Call::EditMessage {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                    body: "fixed".into(),
                },
                r#"{"id":1,"call":{"command":"edit_message","room_id":"!r:s","event_id":"$e","body":"fixed"}}"#,
            ),
            (
                Call::SetDisplayName { name: "Al".into() },
                r#"{"id":1,"call":{"command":"set_display_name","name":"Al"}}"#,
            ),
            (
                Call::NewSessions {},
                r#"{"id":1,"call":{"command":"new_sessions"}}"#,
            ),
            (
                Call::SignInWithOidc {
                    homeserver: "https://example.org/".into(),
                },
                r#"{"id":1,"call":{"command":"sign_in_with_oidc","homeserver":"https://example.org/"}}"#,
            ),
            (
                Call::ProxySetting {},
                r#"{"id":1,"call":{"command":"proxy_setting"}}"#,
            ),
            (
                Call::SetProxySetting {
                    proxy: Some("127.0.0.1:9050".into()),
                },
                r#"{"id":1,"call":{"command":"set_proxy_setting","proxy":"127.0.0.1:9050"}}"#,
            ),
            (
                Call::SetProxySetting { proxy: None },
                r#"{"id":1,"call":{"command":"set_proxy_setting","proxy":null}}"#,
            ),
            (
                Call::DiagnosticsStatus {},
                r#"{"id":1,"call":{"command":"diagnostics_status"}}"#,
            ),
            (
                Call::SetDiagnostics { on: true },
                r#"{"id":1,"call":{"command":"set_diagnostics","on":true}}"#,
            ),
            (
                Call::DiagnosticRecords {
                    source: RecordSource::Page,
                    records: vec![DiagnosticRecord {
                        level: RecordLevel::Warn,
                        target: "bridge".into(),
                        message: "room_list error locked".into(),
                    }],
                },
                r#"{"id":1,"call":{"command":"diagnostic_records","source":"page","records":[{"level":"warn","target":"bridge","message":"room_list error locked"}]}}"#,
            ),
            (
                Call::CallDevices {},
                r#"{"id":1,"call":{"command":"call_devices"}}"#,
            ),
            (
                Call::SetCallDevices {
                    recording: Some("{0.0.1}.{a}".into()),
                    playout: None,
                    camera: Some("usb#cam".into()),
                },
                r#"{"id":1,"call":{"command":"set_call_devices","recording":"{0.0.1}.{a}","playout":null,"camera":"usb#cam"}}"#,
            ),
            (
                Call::SetCallDevices {
                    recording: None,
                    playout: None,
                    camera: None,
                },
                r#"{"id":1,"call":{"command":"set_call_devices","recording":null,"playout":null,"camera":null}}"#,
            ),
            (
                Call::DiscoverHomeserver {
                    server_name: "example.org".into(),
                },
                r#"{"id":1,"call":{"command":"discover_homeserver","server_name":"example.org"}}"#,
            ),
            (
                Call::AcknowledgeSessions {
                    device_ids: vec!["DEV".into()],
                },
                r#"{"id":1,"call":{"command":"acknowledge_sessions","device_ids":["DEV"]}}"#,
            ),
            (
                Call::ThirdPartyModules {},
                r#"{"id":1,"call":{"command":"third_party_modules"}}"#,
            ),
            (
                Call::AcknowledgeModules {
                    ids: vec!["a.dll".into()],
                },
                r#"{"id":1,"call":{"command":"acknowledge_modules","ids":["a.dll"]}}"#,
            ),
            (
                Call::RemoveDevice {
                    device_id: "DEV".into(),
                },
                r#"{"id":1,"call":{"command":"remove_device","device_id":"DEV"}}"#,
            ),
            (
                Call::DeviceList {},
                r#"{"id":1,"call":{"command":"device_list"}}"#,
            ),
            (
                Call::AccountSettings {},
                r#"{"id":1,"call":{"command":"account_settings"}}"#,
            ),
            (
                Call::SetAccountSettings {
                    settings: AccountSettings {
                        enter_sends: false,
                        chat_text_percent: 120,
                        interface_text_percent: 90,
                        desktop_notifications: true,
                        send_typing: false,
                        send_read_receipts: true,
                        idle_lock_minutes: 15,
                        notification_names: false,
                        load_room_pictures: true,
                        capture_protection: false,
                        timeline_events: persephone_contracts::TimelineEvents {
                            leaves: false,
                            ..persephone_contracts::TimelineEvents::default()
                        },
                        theme: persephone_contracts::Theme::Red,
                        share_presence: false,
                        presence: persephone_contracts::PresenceStatus::Unavailable,
                    },
                },
                r#"{"id":1,"call":{"command":"set_account_settings","settings":{"enterSends":false,"chatTextPercent":120,"interfaceTextPercent":90,"desktopNotifications":true,"sendTyping":false,"sendReadReceipts":true,"idleLockMinutes":15,"notificationNames":false,"loadRoomPictures":true,"captureProtection":false,"timelineEvents":{"joins":true,"leaves":false,"invitations":true,"removals":true,"joinRequests":true,"displayNames":true,"profilePictures":true,"roomName":true,"topic":true,"roomPicture":true,"encryption":true,"joinRule":true,"historyVisibility":true,"permissions":true,"addresses":true,"pinnedMessages":true,"guestAccess":true,"serverAccess":true,"creation":true,"upgrades":true,"verification":true,"calls":true,"callSignalling":true,"polls":true,"stickers":true,"locations":true,"spaces":true,"widgets":true,"otherEvents":true,"deletedMessages":true},"theme":"red","sharePresence":false,"presence":"unavailable"}}}"#,
            ),
            (
                Call::ProfileList {},
                r#"{"id":1,"call":{"command":"profile_list"}}"#,
            ),
            (
                Call::AddProfile {},
                r#"{"id":1,"call":{"command":"add_profile"}}"#,
            ),
            (
                Call::SwitchProfile { slot: 2 },
                r#"{"id":1,"call":{"command":"switch_profile","slot":2}}"#,
            ),
            (
                Call::CallReadiness {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"call_readiness","room_id":"!r:s"}}"#,
            ),
            (
                Call::CallJoin {
                    room_id: "!r:s".into(),
                    video: true,
                },
                r#"{"id":1,"call":{"command":"call_join","room_id":"!r:s","video":true}}"#,
            ),
            (
                Call::CallCamera { on: false },
                r#"{"id":1,"call":{"command":"call_camera","on":false}}"#,
            ),
            (
                Call::CallScreen { on: true },
                r#"{"id":1,"call":{"command":"call_screen","on":true}}"#,
            ),
            (
                Call::CallStats { on: true },
                r#"{"id":1,"call":{"command":"call_stats","on":true}}"#,
            ),
            (
                Call::CallVideoFrame {
                    tile: 4,
                    after: 9,
                    stream: 2,
                },
                r#"{"id":1,"call":{"command":"call_video_frame","tile":4,"after":9,"stream":2}}"#,
            ),
            (
                Call::CallLeave {},
                r#"{"id":1,"call":{"command":"call_leave"}}"#,
            ),
            (
                Call::CallMute { muted: true },
                r#"{"id":1,"call":{"command":"call_mute","muted":true}}"#,
            ),
            (
                Call::CallDeafen { deafened: true },
                r#"{"id":1,"call":{"command":"call_deafen","deafened":true}}"#,
            ),
            (
                Call::CallVolume { percent: 35 },
                r#"{"id":1,"call":{"command":"call_volume","percent":35}}"#,
            ),
            (
                Call::CallMicrophone {
                    auto_level: false,
                    gain: 250,
                },
                r#"{"id":1,"call":{"command":"call_microphone","auto_level":false,"gain":250}}"#,
            ),
            (
                Call::CallStatus {},
                r#"{"id":1,"call":{"command":"call_status"}}"#,
            ),
            (
                Call::CallRings {},
                r#"{"id":1,"call":{"command":"call_rings"}}"#,
            ),
            (
                Call::CallDeclineRing {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"call_decline_ring","room_id":"!r:s"}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"create_room","invite":[]}}"#[..],
            br#"{"id":1,"call":{"command":"create_room","name":"x","invite":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"create_room","name":"x","invite":[],"encrypted":false}}"#,
            br#"{"id":1,"call":{"command":"create_room","name":"x","invite":[],"preset":"public_chat"}}"#,
            br#"{"id":1,"call":{"command":"invite_user","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"invite_user","room_id":"!r:s","user_id":"@b:s","reason":"x"}}"#,
            br#"{"id":1,"call":{"command":"display_name","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"set_display_name"}}"#,
            br#"{"id":1,"call":{"command":"set_display_name","name":null}}"#,
            br#"{"id":1,"call":{"command":"device_list","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"delete_device","device_id":"D"}}"#,
            br#"{"id":1,"call":{"command":"account_settings","slot":2}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings"}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":"false"}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":null}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":true,"theme":"x"}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":true,"chat_text_percent":100,"interface_text_percent":100}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","settings":{"enterSends":true,"chatTextPercent":-1,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false}}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","settings":{"enterSends":true,"chatTextPercent":70000,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false}}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","settings":{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false}}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","settings":{"enterSends":true,"chatTextPercent":100,"interfaceTextPercent":100,"desktopNotifications":false,"sendTyping":false,"sendReadReceipts":false,"extra":1}}}"#,
            br#"{"id":1,"call":{"command":"set_account_settings","enter_sends":true,"chat_text_percent":100,"interface_text_percent":100,"desktop_notifications":false}}"#,
            br#"{"id":1,"call":{"command":"profile_list","all":true}}"#,
            br#"{"id":1,"call":{"command":"add_profile","dir":"C:\\x"}}"#,
            br#"{"id":1,"call":{"command":"switch_profile"}}"#,
            br#"{"id":1,"call":{"command":"switch_profile","slot":-1}}"#,
            br#"{"id":1,"call":{"command":"switch_profile","slot":"2"}}"#,
            br#"{"id":1,"call":{"command":"switch_profile","path":"C:\\x"}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// GIVEN the account calls of ADR-0019 to ADR-0022 WHEN sent THEN only
    /// their documented, non-secret arguments cross: no password, code,
    /// recovery key or path field exists, and anything else is refused.
    #[test]
    fn given_account_change_calls_then_they_have_a_strict_wire_form() {
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::ChangePassword {
                    sign_out_others: true,
                },
                r#"{"id":1,"call":{"command":"change_password","sign_out_others":true}}"#,
            ),
            (
                Call::ThreePids {},
                r#"{"id":1,"call":{"command":"three_pids"}}"#,
            ),
            (
                Call::SignOut {
                    discard_keys: false,
                },
                r#"{"id":1,"call":{"command":"sign_out","discard_keys":false}}"#,
            ),
            (
                Call::SignOut { discard_keys: true },
                r#"{"id":1,"call":{"command":"sign_out","discard_keys":true}}"#,
            ),
            (
                Call::RequestEmailToken {
                    email: "a@example.org".into(),
                },
                r#"{"id":1,"call":{"command":"request_email_token","email":"a@example.org"}}"#,
            ),
            (
                Call::RequestPhoneToken {
                    country: "GB".into(),
                    phone_number: "07700900123".into(),
                },
                r#"{"id":1,"call":{"command":"request_phone_token","country":"GB","phone_number":"07700900123"}}"#,
            ),
            (
                Call::AddThreePid {},
                r#"{"id":1,"call":{"command":"add_three_pid"}}"#,
            ),
            (
                Call::RemoveThreePid {
                    medium: ThreePidMedium::Msisdn,
                    address: "447700900123".into(),
                },
                r#"{"id":1,"call":{"command":"remove_three_pid","medium":"msisdn","address":"447700900123"}}"#,
            ),
            (
                Call::VerifyDevice {
                    device_id: "ABCDEF".into(),
                },
                r#"{"id":1,"call":{"command":"verify_device","device_id":"ABCDEF"}}"#,
            ),
            (
                Call::ChangeRecoveryKey {},
                r#"{"id":1,"call":{"command":"change_recovery_key"}}"#,
            ),
            (
                Call::SetProfilePassphrase { on: true },
                r#"{"id":1,"call":{"command":"set_profile_passphrase","on":true}}"#,
            ),
            (
                Call::StartNewBackup {},
                r#"{"id":1,"call":{"command":"start_new_backup"}}"#,
            ),
            (
                Call::NotificationLevel {},
                r#"{"id":1,"call":{"command":"notification_level"}}"#,
            ),
            (
                Call::SetNotificationLevel {
                    level: NotificationLevel::Mentions,
                },
                r#"{"id":1,"call":{"command":"set_notification_level","level":"mentions"}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"change_password"}}"#[..],
            br#"{"id":1,"call":{"command":"change_password","sign_out_others":"yes"}}"#,
            br#"{"id":1,"call":{"command":"change_password","sign_out_others":true,"new_password":"x"}}"#,
            br#"{"id":1,"call":{"command":"three_pids","medium":"email"}}"#,
            br#"{"id":1,"call":{"command":"request_email_token"}}"#,
            br#"{"id":1,"call":{"command":"request_email_token","email":"a@b","id_server":"is.example"}}"#,
            br#"{"id":1,"call":{"command":"request_phone_token","phone_number":"1"}}"#,
            br#"{"id":1,"call":{"command":"add_three_pid","sid":"s"}}"#,
            br#"{"id":1,"call":{"command":"add_three_pid","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"remove_three_pid","medium":"fax","address":"1"}}"#,
            br#"{"id":1,"call":{"command":"remove_three_pid","address":"a@b"}}"#,
            br#"{"id":1,"call":{"command":"verify_device"}}"#,
            br#"{"id":1,"call":{"command":"verify_device","device_id":null}}"#,
            br#"{"id":1,"call":{"command":"change_recovery_key","key":"x"}}"#,
            br#"{"id":1,"call":{"command":"change_recovery_key","path":"C:\\x"}}"#,
            br#"{"id":1,"call":{"command":"set_profile_passphrase"}}"#,
            br#"{"id":1,"call":{"command":"set_profile_passphrase","on":null}}"#,
            br#"{"id":1,"call":{"command":"set_profile_passphrase","on":"yes"}}"#,
            br#"{"id":1,"call":{"command":"set_profile_passphrase","on":true,"passphrase":"x"}}"#,
            br#"{"id":1,"call":{"command":"start_new_backup","key":"x"}}"#,
            br#"{"id":1,"call":{"command":"start_new_backup","delete":true}}"#,
            br#"{"id":1,"call":{"command":"set_notification_level"}}"#,
            br#"{"id":1,"call":{"command":"set_notification_level","level":"loud"}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        // The notification event carries two names and nothing else.
        let event = CoreMessage::Event(Event::Notification {
            room: "Plans".into(),
            sender: "Bob".into(),
            call: false,
        });
        let mut sent = Vec::new();
        send(&mut sent, &event).unwrap();
        assert_eq!(
            String::from_utf8(sent[17..].to_vec()).unwrap(),
            r#"{"event":{"notification":{"room":"Plans","sender":"Bob","call":false}}}"#
        );
        // A ring's notification and its coalesced page event (ADR-0052).
        let event = CoreMessage::Event(Event::Notification {
            room: String::new(),
            sender: String::new(),
            call: true,
        });
        let mut sent = Vec::new();
        send(&mut sent, &event).unwrap();
        assert_eq!(
            String::from_utf8(sent[17..].to_vec()).unwrap(),
            r#"{"event":{"notification":{"room":"","sender":"","call":true}}}"#
        );
        let mut sent = Vec::new();
        send(&mut sent, &CoreMessage::Event(Event::RingsChanged {})).unwrap();
        assert_eq!(
            String::from_utf8(sent[17..].to_vec()).unwrap(),
            r#"{"event":{"rings_changed":{}}}"#
        );
    }

    /// GIVEN the room settings calls WHEN sent THEN they cross in the
    /// documented wire form; missing, extra, null, unknown or out-of-range
    /// fields are refused, inside the change as well as around it.
    #[test]
    fn given_room_settings_calls_then_they_have_a_strict_wire_form() {
        use persephone_contracts::{JoinRule, LevelKey, PowerLevel};
        let budgets = ResourceBudgets::default();
        for (call, wire) in [
            (
                Call::RoomSettings {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"room_settings","room_id":"!r:s"}}"#,
            ),
            (
                Call::RoomMembers {
                    room_id: "!r:s".into(),
                    filter: "bo".into(),
                },
                r#"{"id":1,"call":{"command":"room_members","room_id":"!r:s","filter":"bo"}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::JoinRule {
                        rule: JoinRule::Knock,
                    },
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"join_rule","rule":"knock"}}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::CanonicalAlias {
                        alias: None,
                        alt_aliases: vec!["#a:s".into()],
                    },
                },
                r##"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"canonical_alias","alias":null,"altAliases":["#a:s"]}}}"##,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::Level {
                        key: LevelKey::NotifyRoom,
                        level: PowerLevel::new(-5).unwrap(),
                    },
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"notify_room","level":-5}}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::MemberLevel {
                        user_id: "@b:s".into(),
                        level: PowerLevel::new(PowerLevel::MAX).unwrap(),
                    },
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"member_level","userId":"@b:s","level":9007199254740991}}}"#,
            ),
            (
                Call::ChangeRoom {
                    room_id: "!r:s".into(),
                    change: RoomChange::Leave {},
                },
                r#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"leave"}}}"#,
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"room_settings"}}"#[..],
            br#"{"id":1,"call":{"command":"room_settings","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"room_members","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"room_members","room_id":"!r:s","filter":null}}"#,
            br#"{"id":1,"call":{"command":"room_members","room_id":"!r:s","filter":"","limit":500}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"name"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"name","name":"x","topic":"y"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"encryption","enabled":false}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"leave","forget":true}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"join_rule","rule":"open"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"canonical_alias","altAliases":[]}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"canonical_alias","alias":null,"alt_aliases":[]}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"ban","level":9007199254740992}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"ban","level":-9007199254740992}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"ban","level":1.5}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"level","key":"m.room.name","level":1}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"member_level","user_id":"@b:s","level":1}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"kick","userId":"@b:s","reason":"x"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"redact","eventId":"$e"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"notifications","mode":"loud"}}}"#,
            br#"{"id":1,"call":{"command":"change_room","room_id":"!r:s","change":{"kind":"state","type":"m.room.server_acl","content":{}}}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// GIVEN changes with text at and over each limit THEN only those within
    /// every limit pass the length check.
    #[test]
    fn given_room_changes_then_texts_over_their_limit_are_caught() {
        let at = |n: usize| "a".repeat(n);
        for (change, within) in [
            (
                RoomChange::Name {
                    name: at(MAX_ROOM_NAME_BYTES),
                },
                true,
            ),
            (
                RoomChange::Name {
                    name: at(MAX_ROOM_NAME_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::Topic {
                    topic: at(MAX_TOPIC_BYTES),
                },
                true,
            ),
            (
                RoomChange::Topic {
                    topic: at(MAX_TOPIC_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::AddAlias {
                    alias: at(MAX_ALIAS_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::RemoveAlias {
                    alias: at(MAX_ALIAS_BYTES + 1),
                },
                false,
            ),
            (
                RoomChange::CanonicalAlias {
                    alias: Some(at(MAX_ALIAS_BYTES)),
                    alt_aliases: vec![at(MAX_ALIAS_BYTES); MAX_ALT_ALIASES],
                },
                true,
            ),
            (
                RoomChange::CanonicalAlias {
                    alias: None,
                    alt_aliases: vec![String::new(); MAX_ALT_ALIASES + 1],
                },
                false,
            ),
            (
                RoomChange::CanonicalAlias {
                    alias: None,
                    alt_aliases: vec![at(MAX_ALIAS_BYTES + 1)],
                },
                false,
            ),
            (
                RoomChange::Ban {
                    user_id: at(MAX_USER_ID_BYTES + 1),
                },
                false,
            ),
            (RoomChange::Leave {}, true),
        ] {
            assert_eq!(room_change_within_limits(&change), within, "{change:?}");
        }
    }

    #[test]
    fn given_unknown_command_field_or_kind_then_receive_refuses() {
        let budgets = ResourceBudgets::default();
        for body in [
            &br#"{"id":1,"call":{"command":"export_keys"}}"#[..],
            br#"{"id":1,"call":{"command":"app_status"},"extra":true}"#,
            br#"{"id":1,"call":{"command":"app_status","arg":1}}"#,
            br#"{"id":-1,"call":{"command":"app_status"}}"#,
            br#"{"id":1,"call":{"command":"room_list","limit":5}}"#,
            br#"{"id":1,"call":{"command":"timeline_page","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"timeline_page","room_id":"!r:s","cursor":null,"x":1}}"#,
            br#"{"id":1,"call":{"command":"lock_session","force":true}}"#,
            br#"{"id":1,"call":{"command":"unlock_session","key":"00"}}"#,
            br#"{"id":1,"call":{"command":"unlock_session","passphrase":"x"}}"#,
            br#"{"id":1,"call":{"command":"reset_profile","confirm":true}}"#,
            br#"{"id":1,"call":{"command":"sign_in"}}"#,
            br#"{"id":1,"call":{"command":"sign_in_with_oidc"}}"#,
            br#"{"id":1,"call":{"command":"sign_in_with_oidc","homeserver":"https://h","code":"x"}}"#,
            br#"{"id":1,"call":{"command":"sign_in","homeserver":"https://h","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"sign_in","homeserver":null}}"#,
            br#"{"id":1,"call":{"command":"register","homeserver":"https://h"}}"#,
            br#"{"id":1,"call":{"command":"register","homeserver":"https://h","username":"al","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"register","homeserver":"https://h","username":"al","token":"x"}}"#,
            br#"{"id":1,"call":{"command":"register","username":"al"}}"#,
            br#"{"id":1,"call":{"command":"sign_out","keep_data":true}}"#,
            br#"{"id":1,"call":{"command":"sign_out"}}"#,
            br#"{"id":1,"call":{"command":"sign_out","discard_keys":"yes"}}"#,
            br#"{"id":1,"call":{"command":"sign_out","discard_keys":true,"keep_data":true}}"#,
            br#"{"id":1,"call":{"command":"security_status","user":"@a:s"}}"#,
            br#"{"id":1,"call":{"command":"call_readiness"}}"#,
            br#"{"id":1,"call":{"command":"call_readiness","room_id":"!r:s","video":true}}"#,
            br#"{"id":1,"call":{"command":"call_join"}}"#,
            br#"{"id":1,"call":{"command":"call_join","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"call_join","room_id":"!r:s","video":"yes"}}"#,
            br#"{"id":1,"call":{"command":"call_join","room_id":"!r:s","video":false,"proxy_ok":true}}"#,
            br#"{"id":1,"call":{"command":"call_join","room_id":null,"video":false}}"#,
            br#"{"id":1,"call":{"command":"call_camera"}}"#,
            br#"{"id":1,"call":{"command":"call_camera","on":1}}"#,
            br#"{"id":1,"call":{"command":"call_camera","on":true,"device":"x"}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame"}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":1}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","after":0}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":1,"after":0}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":256,"after":0,"stream":0}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":-1,"after":0,"stream":0}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":1,"after":-1,"stream":0}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":1,"after":0,"stream":-1}}"#,
            br#"{"id":1,"call":{"command":"call_video_frame","tile":1,"after":0,"stream":0,"x":1}}"#,
            br#"{"id":1,"call":{"command":"call_leave","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"call_mute"}}"#,
            br#"{"id":1,"call":{"command":"call_mute","muted":"yes"}}"#,
            br#"{"id":1,"call":{"command":"call_mute","muted":true,"user":"@a:s"}}"#,
            br#"{"id":1,"call":{"command":"call_deafen"}}"#,
            br#"{"id":1,"call":{"command":"call_deafen","deafened":1}}"#,
            br#"{"id":1,"call":{"command":"call_deafen","deafened":true,"x":1}}"#,
            br#"{"id":1,"call":{"command":"call_volume"}}"#,
            br#"{"id":1,"call":{"command":"call_volume","percent":256}}"#,
            br#"{"id":1,"call":{"command":"call_volume","percent":-1}}"#,
            br#"{"id":1,"call":{"command":"call_volume","percent":50.5}}"#,
            br#"{"id":1,"call":{"command":"call_volume","percent":"50"}}"#,
            br#"{"id":1,"call":{"command":"call_microphone"}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":true}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","gain":100}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":1,"gain":100}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":true,"gain":-5}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":true,"gain":65536}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":true,"gain":50.5}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":true,"gain":"100"}}"#,
            br#"{"id":1,"call":{"command":"call_microphone","auto_level":true,"gain":100,"x":1}}"#,
            br#"{"id":1,"call":{"command":"call_status","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"call_rings","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"call_decline_ring"}}"#,
            br#"{"id":1,"call":{"command":"call_decline_ring","room_id":"!r:s","event_id":"$e"}}"#,
            br#"{"id":1,"call":{"command":"call_decline_ring","room_id":7}}"#,
            br#"{"id":1,"call":{"command":"set_up_encryption","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"recover_with_key","recovery_key":"x"}}"#,
            br#"{"id":1,"call":{"command":"export_room_keys"}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x","encrypted":false}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x","txn_id":"t"}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x"}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x","reply_to":1}}"#,
            br#"{"id":1,"call":{"command":"send_message","room_id":"!r:s","body":"x","reply_to":null,"edit":"$e"}}"#,
            br#"{"id":1,"call":{"command":"edit_message","room_id":"!r:s","body":"x"}}"#,
            br#"{"id":1,"call":{"command":"delete_message","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"react","room_id":"!r:s","event_id":"$e"}}"#,
            br#"{"id":1,"call":{"command":"mark_read","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"room_activity","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"open_link"}}"#,
            br#"{"id":1,"call":{"command":"room_servers"}}"#,
            br#"{"id":1,"call":{"command":"pick_file","room_id":"!r:s","path":"C:/x"}}"#,
            br#"{"id":1,"call":{"command":"save_file","room_id":"!r:s","event_id":"$e","path":"C:/x"}}"#,
            br#"{"id":1,"call":{"command":"save_file","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"new_sessions","all":true}}"#,
            br#"{"id":1,"call":{"command":"discover_homeserver"}}"#,
            br#"{"id":1,"call":{"command":"set_proxy_setting"}}"#,
            br#"{"id":1,"call":{"command":"set_proxy_setting","proxy":"h:1","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"proxy_setting","proxy":null}}"#,
            br#"{"id":1,"call":{"command":"set_diagnostics"}}"#,
            br#"{"id":1,"call":{"command":"set_diagnostics","on":"yes"}}"#,
            br#"{"id":1,"call":{"command":"diagnostics_status","on":true}}"#,
            br#"{"id":1,"call":{"command":"diagnostic_records","source":"core","records":[]}}"#,
            br#"{"id":1,"call":{"command":"diagnostic_records","source":"page","records":[{"level":"trace","target":"t","message":"m"}]}}"#,
            br#"{"id":1,"call":{"command":"diagnostic_records","source":"page","records":[{"level":"info","target":"t","message":"m","fields":{}}]}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices"}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","recording":null}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","playout":null}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","recording":null,"playout":null}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","recording":null,"camera":null}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","recording":null,"playout":null,"camera":null,"x":1}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","recording":1,"playout":null,"camera":null}}"#,
            br#"{"id":1,"call":{"command":"set_call_devices","recording":null,"playout":null,"camera":1}}"#,
            br#"{"id":1,"call":{"command":"call_devices","recording":null}}"#,
            br#"{"id":1,"call":{"command":"discover_homeserver","server_name":"e.org","url":"x"}}"#,
            br#"{"id":1,"call":{"command":"acknowledge_sessions"}}"#,
            br#"{"id":1,"call":{"command":"acknowledge_sessions","device_ids":"DEV"}}"#,
            br#"{"id":1,"call":{"command":"remove_device"}}"#,
            br#"{"id":1,"call":{"command":"remove_device","device_id":"DEV","password":"x"}}"#,
            br#"{"id":1,"call":{"command":"room_readers","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"room_servers","room_id":"!r:s","limit":5}}"#,
            br#"{"id":1,"call":{"command":"open_link","url":"https://e.org","confirmed":true}}"#,
            br#"{"id":1,"call":{"command":"set_typing","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"set_typing","room_id":"!r:s","typing":"yes"}}"#,
            br#"{"id":1,"call":{"command":"mark_read","room_id":"!r:s","event_id":"$e","public":true}}"#,
            br#"{"id":1,"call":{"command":"react","room_id":"!r:s","event_id":"$e","key":null}}"#,
            br#"{"id":1,"call":{"command":"delete_message","room_id":"!r:s","event_id":"$e","reason":"spam"}}"#,
            br#"{"id":1,"call":{"command":"edit_message","room_id":"!r:s","event_id":null,"body":"x"}}"#,
            br#"{"id":1,"call":{"command":"edit_message","room_id":"!r:s","event_id":"$e","body":"x","sender":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"retry_message","local_id":-1}}"#,
            br#"{"id":1,"call":{"command":"retry_message","local_id":4294967296}}"#,
            br#"{"id":1,"call":{"command":"discard_message","local_id":"1"}}"#,
            br#"{"id":1,"call":{"command":"allow_unencrypted","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"pending_messages"}}"#,
            br#"{"id":1,"call":{"command":"set_up_recovery","passphrase":"x"}}"#,
            br#"{"id":1,"call":{"command":"verify_this_device","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"accept_verification"}}"#,
            br#"{"id":1,"call":{"command":"confirm_verification","flow_id":"f","emoji":[]}}"#,
            br#"{"id":1,"call":{"command":"cancel_verification","flow_id":null}}"#,
            br#"{"id":1,"call":{"command":"trust_device","device_id":"D"}}"#,
            br#"{"id":1,"call":{"command":"contact_status"}}"#,
            br#"{"id":1,"call":{"command":"contact_status","room_id":"!r:s","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_contact"}}"#,
            br#"{"id":1,"call":{"command":"verify_contact","room_id":null}}"#,
            br#"{"id":1,"call":{"command":"verify_contact","room_id":"!r:s","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_contact","room_id":"!r:s","create_room":true}}"#,
            br#"{"id":1,"call":{"command":"verify_user","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_member"}}"#,
            br#"{"id":1,"call":{"command":"verify_member","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_member","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"verify_member","room_id":"!r:s","user_id":null}}"#,
            br#"{"id":1,"call":{"command":"verify_member","room_id":"!r:s","user_id":"@b:s","create_room":true}}"#,
            br#"{"id":1,"call":{"command":"identity_changes"}}"#,
            br#"{"id":1,"call":{"command":"identity_changes","room_id":"!r:s","all":true}}"#,
            br#"{"id":1,"call":{"command":"accept_identity","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"accept_identity","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"accept_identity","room_id":"!r:s","user_id":"@b:s","confirmed":true}}"#,
            br#"{"id":1,"call":{"command":"pin_identity","user_id":"@b:s"}}"#,
            br#"{"id":1,"call":{"command":"invite_list","all":true}}"#,
            br#"{"id":1,"call":{"command":"accept_invite"}}"#,
            br#"{"id":1,"call":{"command":"decline_invite","room_id":"!r:s","ban":true}}"#,
            br#"{"id":1,"call":{"command":"start_conversation"}}"#,
            br#"{"id":1,"call":{"command":"start_conversation","user_id":"@b:s","encrypted":false}}"#,
            br#"{"id":1,"call":{"command":"start_conversation","user_id":"@b:s","invite":["@c:s"]}}"#,
            br#"{"id":1,"call":{"command":"search_users"}}"#,
            br#"{"id":1,"call":{"command":"search_users","term":"b","limit":500}}"#,
            br#"{"id":1,"call":{"command":"public_rooms","server":null,"filter":null,"since":null,"limit":500}}"#,
            br#"{"id":1,"call":{"command":"public_rooms","server":7,"filter":null,"since":null}}"#,
            br#"{"id":1,"call":{"command":"public_rooms"}}"#,
            br#"{"id":1,"call":{"command":"public_rooms","server":null,"filter":null}}"#,
            br#"{"id":1,"call":{"command":"join_public_room"}}"#,
            br#"{"id":1,"call":{"command":"join_public_room","room_id":"!r:s","via":["evil.example"]}}"#,
            br#"{"id":1,"call":{"command":"create_room","name":"x"}}"#,
            b"not json",
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let mut reader = Cursor::new(frame(FrameKind::EncodedImage, b"{}"));
        assert!(matches!(
            receive::<_, Request>(&mut reader, &budgets),
            Err(ReceiveError::UnexpectedKind)
        ));
    }

    /// A header announcing more than the control limit is refused before the
    /// body is read; the stream position proves nothing past the header was
    /// consumed.
    #[test]
    fn given_oversized_control_header_then_body_is_never_read() {
        let mut wire = FrameHeader {
            kind: FrameKind::Control,
            length: 64 * 1024 * 1024,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(&[b'x'; 4096]);
        let mut reader = Cursor::new(wire);
        let result = receive::<_, Request>(&mut reader, &ResourceBudgets::default());
        assert!(matches!(
            result,
            Err(ReceiveError::Frame(FrameError::TooLarge))
        ));
        assert_eq!(reader.position(), 17);
    }

    /// GIVEN the image calls WHEN sent THEN they cross in the documented wire
    /// form and round trip; missing, extra, null or wrongly typed fields are
    /// refused.
    #[test]
    fn given_image_calls_then_they_have_a_strict_wire_form() {
        let budgets = ResourceBudgets::default();
        let handle = "0123456789abcdef0123456789abcdef";
        for (call, wire) in [
            (
                Call::LoadImage {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                },
                r#"{"id":1,"call":{"command":"load_image","room_id":"!r:s","event_id":"$e"}}"#
                    .to_owned(),
            ),
            (
                Call::LoadRoomPicture {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"load_room_picture","room_id":"!r:s"}}"#.to_owned(),
            ),
            (
                Call::ImageBand {
                    handle: handle.into(),
                    band: 3,
                },
                format!(
                    r#"{{"id":1,"call":{{"command":"image_band","handle":"{handle}","band":3}}}}"#
                ),
            ),
            (
                Call::ReleaseImage {
                    handle: handle.into(),
                },
                format!(r#"{{"id":1,"call":{{"command":"release_image","handle":"{handle}"}}}}"#),
            ),
            (
                Call::PickImage {
                    room_id: "!r:s".into(),
                },
                r#"{"id":1,"call":{"command":"pick_image","room_id":"!r:s"}}"#.to_owned(),
            ),
            (
                Call::SendImage {
                    room_id: "!r:s".into(),
                    handle: handle.into(),
                },
                format!(
                    r#"{{"id":1,"call":{{"command":"send_image","room_id":"!r:s","handle":"{handle}"}}}}"#
                ),
            ),
            (
                Call::DiscardImage {
                    handle: handle.into(),
                },
                format!(r#"{{"id":1,"call":{{"command":"discard_image","handle":"{handle}"}}}}"#),
            ),
        ] {
            let request = Request { id: 1, call };
            let mut sent = Vec::new();
            send(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent[17..].to_vec()).unwrap(), wire);
            let mut reader = Cursor::new(sent);
            assert_eq!(
                receive::<_, Request>(&mut reader, &budgets).unwrap(),
                request
            );
        }
        for body in [
            &br#"{"id":1,"call":{"command":"load_image","room_id":"!r:s"}}"#[..],
            br#"{"id":1,"call":{"command":"load_image","room_id":"!r:s","event_id":null}}"#,
            br#"{"id":1,"call":{"command":"load_image","room_id":"!r:s","event_id":"$e","url":"mxc://s/m"}}"#,
            br#"{"id":1,"call":{"command":"load_room_picture"}}"#,
            br#"{"id":1,"call":{"command":"load_room_picture","room_id":null}}"#,
            br#"{"id":1,"call":{"command":"load_room_picture","room_id":"!r:s","url":"mxc://s/m"}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h"}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":-1}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":4294967296}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":"0"}}"#,
            br#"{"id":1,"call":{"command":"image_band","handle":"h","band":0,"rows":9}}"#,
            br#"{"id":1,"call":{"command":"release_image"}}"#,
            br#"{"id":1,"call":{"command":"pick_image"}}"#,
            br#"{"id":1,"call":{"command":"pick_image","room_id":"!r:s","path":"C:\\x.png"}}"#,
            br#"{"id":1,"call":{"command":"send_image","room_id":"!r:s"}}"#,
            br#"{"id":1,"call":{"command":"send_image","handle":"h"}}"#,
            br#"{"id":1,"call":{"command":"send_image","room_id":"!r:s","handle":"h","filename":"a.png"}}"#,
            br#"{"id":1,"call":{"command":"discard_image","handle":1}}"#,
            br#"{"id":1,"call":{"command":"decode_image","bytes":"AAAA"}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(
                matches!(
                    receive::<_, Request>(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        // The new error code's wire form.
        assert_eq!(
            serde_json::to_string(&ErrorCode::Rejected).unwrap(),
            r#""rejected""#
        );
    }

    fn pixels_frame(width: u32, height: u32, body: &[u8]) -> Vec<u8> {
        let mut wire = FrameHeader {
            kind: FrameKind::RgbaPixels,
            length: body.len() as u64,
            width,
            height,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(body);
        wire
    }

    fn pixel_reply_frame(reply: &str) -> Vec<u8> {
        frame(FrameKind::Control, reply.as_bytes())
    }

    /// GIVEN a pixel reply between an event and a reply WHEN the UI reads THEN
    /// it gets the event, the band with its pixels and the reply, in order;
    /// the documented wire form is used, and the old reader refuses it.
    #[test]
    fn given_a_pixel_reply_then_the_band_arrives_whole_between_messages() {
        let mut wire = Vec::new();
        send(&mut wire, &CoreMessage::Event(Event::SecurityChanged {})).unwrap();
        let reply = PixelReply {
            id: 9,
            width: 2,
            rows: 3,
            band: 1,
        };
        let pixels: Vec<u8> = (0..24).collect();
        let start = wire.len();
        send_pixel_reply(&mut wire, &reply, &pixels).unwrap();
        let documented = r#"{"pixel_reply":{"id":9,"width":2,"rows":3,"band":1}}"#;
        let body = start + 17;
        assert_eq!(
            String::from_utf8_lossy(&wire[body..body + documented.len()]),
            documented
        );
        assert_eq!(
            wire[body + documented.len()],
            FrameKind::RgbaPixels as u8,
            "the pixel frame follows at once"
        );
        send(
            &mut wire,
            &CoreMessage::Reply(Reply {
                id: 10,
                outcome: Outcome::Ok(serde_json::Value::Null),
            }),
        )
        .unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire.clone());
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Ok(FromCore::Message(CoreMessage::Event(
                Event::SecurityChanged {}
            )))
        ));
        match receive_from_core(&mut reader, &budgets) {
            Ok(FromCore::Pixels(band)) => {
                assert_eq!(band.reply, reply);
                assert_eq!(band.pixels.body(), &pixels[..]);
                assert_eq!(band.pixels.header().width, 2);
                assert_eq!(band.pixels.header().height, 3);
            }
            _ => panic!("expected a pixel band"),
        }
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Ok(FromCore::Message(CoreMessage::Reply(Reply { id: 10, .. })))
        ));
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Err(ReceiveError::Closed)
        ));
        // A reader that knows only messages refuses the pixel reply.
        let mut reader = Cursor::new(wire[start..].to_vec());
        assert!(matches!(
            receive::<_, CoreMessage>(&mut reader, &budgets),
            Err(ReceiveError::Malformed)
        ));
    }

    /// GIVEN pixel frames that do not match their reply WHEN read THEN each is
    /// refused, and the pixel body is never read.
    #[test]
    fn given_a_mismatched_or_missing_pixel_frame_then_receive_refuses() {
        let budgets = ResourceBudgets::default();
        let reply = r#"{"pixel_reply":{"id":1,"width":2,"rows":2,"band":0}}"#;
        let body = [7u8; 16];
        for (follower, expected) in [
            // Another width, height, or both.
            (pixels_frame(4, 1, &body), "malformed"),
            (pixels_frame(1, 4, &body), "malformed"),
            // A control or encoded frame instead of pixels.
            (frame(FrameKind::Control, b"{}"), "kind"),
            (frame(FrameKind::EncodedImage, &body), "kind"),
            // Nothing at all, or a cut-off header.
            (Vec::new(), "closed"),
            (pixels_frame(2, 2, &body)[..9].to_vec(), "closed"),
        ] {
            let mut wire = pixel_reply_frame(reply);
            let header_end = wire.len() + 17;
            wire.extend_from_slice(&follower);
            let mut reader = Cursor::new(wire);
            let result = receive_from_core(&mut reader, &budgets);
            let matched = match (expected, &result) {
                ("malformed", Err(ReceiveError::Malformed)) => {
                    reader.position() == header_end as u64
                }
                ("kind", Err(ReceiveError::UnexpectedKind)) => {
                    reader.position() == header_end as u64
                }
                ("closed", Err(ReceiveError::Closed)) => true,
                _ => false,
            };
            assert!(matched, "{expected}: {:?}", result.err());
        }
        // A truncated body is the core closing mid-frame.
        let mut wire = pixel_reply_frame(reply);
        wire.extend_from_slice(&pixels_frame(2, 2, &body)[..25]);
        assert!(matches!(
            receive_from_core(&mut Cursor::new(wire), &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    /// GIVEN a pixel reply over the band limit, empty, too wide or with extra
    /// fields WHEN read THEN it is refused before the next frame is touched;
    /// WHEN the core tries to send one THEN nothing is written.
    #[test]
    fn given_an_oversized_or_malformed_pixel_reply_then_nothing_crosses() {
        let budgets = ResourceBudgets::default();
        for reply in [
            // 1024 x 257 x 4 is just over 1 MiB.
            r#"{"pixel_reply":{"id":1,"width":1024,"rows":257,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":0,"rows":1,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":1,"rows":0,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":8193,"rows":1,"band":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":1,"rows":1,"band":0,"x":0}}"#,
            r#"{"pixel_reply":{"id":1,"width":1,"rows":1}}"#,
        ] {
            let mut wire = pixel_reply_frame(reply);
            let end = wire.len() as u64;
            wire.extend_from_slice(&pixels_frame(1, 1, &[0; 4]));
            let mut reader = Cursor::new(wire);
            assert!(
                matches!(
                    receive_from_core(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{reply}"
            );
            assert_eq!(reader.position(), end, "{reply}");
        }
        // The largest band that fits: 1024 x 256.
        let largest = PixelReply {
            id: 1,
            width: 1024,
            rows: 256,
            band: 0,
        };
        let mut wire = Vec::new();
        send_pixel_reply(&mut wire, &largest, &vec![1; 1024 * 256 * 4]).unwrap();
        assert!(matches!(
            receive_from_core(&mut Cursor::new(wire), &budgets),
            Ok(FromCore::Pixels(_))
        ));
        for (reply, length) in [
            (
                PixelReply {
                    rows: 257,
                    ..largest
                },
                1024 * 257 * 4,
            ),
            (PixelReply { rows: 2, ..largest }, 1024 * 4),
            (
                PixelReply {
                    width: 0,
                    ..largest
                },
                0,
            ),
        ] {
            let mut wire = Vec::new();
            assert!(send_pixel_reply(&mut wire, &reply, &vec![0; length]).is_err());
            assert!(wire.is_empty(), "{reply:?}");
        }
    }

    /// GIVEN module calls WHEN the argument limits are checked THEN 1 to 128
    /// ids of 1 to 512 bytes pass and every other shape is refused.
    #[test]
    fn given_module_calls_when_limits_are_checked_then_bounds_hold() {
        let ack = |ids: Vec<String>| Call::AcknowledgeModules { ids };
        assert!(personal_call_within_limits(&Call::ThirdPartyModules {}));
        assert!(personal_call_within_limits(&ack(vec!["a".into()])));
        assert!(personal_call_within_limits(&ack(vec![
            "x".repeat(MAX_MODULE_ID_BYTES)
        ])));
        assert!(personal_call_within_limits(&ack(
            vec!["a".to_string(); MAX_THIRD_PARTY_MODULES]
        )));
        assert!(!personal_call_within_limits(&ack(Vec::new())));
        assert!(!personal_call_within_limits(&ack(vec![
            "a".to_string();
            MAX_THIRD_PARTY_MODULES
                + 1
        ])));
        assert!(!personal_call_within_limits(&ack(vec![String::new()])));
        assert!(!personal_call_within_limits(&ack(vec![
            "a".into(),
            String::new()
        ])));
        assert!(!personal_call_within_limits(&ack(vec![
            "x".repeat(MAX_MODULE_ID_BYTES + 1)
        ])));
        // Two bytes per character: 257 characters are 514 bytes.
        assert!(!personal_call_within_limits(&ack(vec!["é".repeat(257)])));
        assert!(serde_json::from_str::<Call>(r#"{"command":"acknowledge_modules"}"#).is_err());
        assert!(
            serde_json::from_str::<Call>(r#"{"command":"third_party_modules","x":1}"#).is_err()
        );
    }

    #[test]
    fn given_message_over_control_limit_then_send_writes_nothing() {
        let mut wire = Vec::new();
        let huge = serde_json::Value::String("x".repeat(200 * 1024));
        assert!(send(&mut wire, &huge).is_err());
        assert!(wire.is_empty());
    }

    fn video_reply(tile: u8, width: u32, height: u32) -> VideoReply {
        VideoReply {
            id: 5,
            tile,
            seq: 42,
            rotation: 1,
            width,
            height,
        }
    }

    /// GIVEN a video reply between an event and a reply WHEN the UI reads THEN
    /// it gets the event, the picture with its pixels and the reply, in order;
    /// the documented wire form is used, and a reader that knows only
    /// messages refuses it.
    #[test]
    fn given_a_video_reply_then_the_picture_arrives_whole_between_messages() {
        let mut wire = Vec::new();
        send(&mut wire, &CoreMessage::Event(Event::CallVideo {})).unwrap();
        let reply = video_reply(3, 4, 2);
        let pixels: Vec<u8> = (0..32).collect();
        let start = wire.len();
        send_video_reply(&mut wire, &reply, &pixels).unwrap();
        let documented =
            r#"{"video_reply":{"id":5,"tile":3,"seq":42,"rotation":1,"width":4,"height":2}}"#;
        let body = start + 17;
        assert_eq!(
            String::from_utf8_lossy(&wire[body..body + documented.len()]),
            documented
        );
        assert_eq!(
            wire[body + documented.len()],
            FrameKind::RgbaPixels as u8,
            "the pixel frame follows at once"
        );
        send(
            &mut wire,
            &CoreMessage::Reply(Reply {
                id: 6,
                outcome: Outcome::Ok(serde_json::Value::Null),
            }),
        )
        .unwrap();
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire.clone());
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Ok(FromCore::Message(CoreMessage::Event(Event::CallVideo {})))
        ));
        match receive_from_core(&mut reader, &budgets) {
            Ok(FromCore::Video(picture)) => {
                assert_eq!(picture.reply, reply);
                assert_eq!(picture.pixels.body(), &pixels[..]);
                assert_eq!(picture.pixels.header().width, 4);
                assert_eq!(picture.pixels.header().height, 2);
            }
            _ => panic!("expected a video picture"),
        }
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Ok(FromCore::Message(CoreMessage::Reply(Reply { id: 6, .. })))
        ));
        assert!(matches!(
            receive_from_core(&mut reader, &budgets),
            Err(ReceiveError::Closed)
        ));
        let mut reader = Cursor::new(wire[start..].to_vec());
        assert!(matches!(
            receive::<_, CoreMessage>(&mut reader, &budgets),
            Err(ReceiveError::Malformed)
        ));
    }

    /// GIVEN no newer picture WHEN the core answers THEN it is an ordinary
    /// `ok` reply with null and no pixel frame follows.
    #[test]
    fn given_no_newer_picture_then_the_reply_is_ok_null_without_pixels() {
        let mut wire = Vec::new();
        send(
            &mut wire,
            &CoreMessage::Reply(Reply {
                id: 8,
                outcome: Outcome::Ok(serde_json::Value::Null),
            }),
        )
        .unwrap();
        let documented = r#"{"reply":{"id":8,"outcome":{"ok":null}}}"#;
        assert_eq!(String::from_utf8_lossy(&wire[17..]), documented);
        assert!(matches!(
            receive_from_core(&mut Cursor::new(wire), &ResourceBudgets::default()),
            Ok(FromCore::Message(CoreMessage::Reply(Reply { id: 8, .. })))
        ));
    }

    /// GIVEN pixel frames that do not match their video reply WHEN read THEN
    /// each is refused, and the pixel body is never read.
    #[test]
    fn given_a_mismatched_or_missing_video_frame_then_receive_refuses() {
        let budgets = ResourceBudgets::default();
        let reply =
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":2,"height":2}}"#;
        let body = [7u8; 16];
        for (follower, expected) in [
            (pixels_frame(4, 1, &body), "malformed"),
            (pixels_frame(1, 4, &body), "malformed"),
            (frame(FrameKind::Control, b"{}"), "kind"),
            (frame(FrameKind::EncodedImage, &body), "kind"),
            (Vec::new(), "closed"),
            (pixels_frame(2, 2, &body)[..9].to_vec(), "closed"),
        ] {
            let mut wire = pixel_reply_frame(reply);
            let header_end = wire.len() + 17;
            wire.extend_from_slice(&follower);
            let mut reader = Cursor::new(wire);
            let result = receive_from_core(&mut reader, &budgets);
            let matched = match (expected, &result) {
                ("malformed", Err(ReceiveError::Malformed))
                | ("kind", Err(ReceiveError::UnexpectedKind)) => {
                    reader.position() == header_end as u64
                }
                ("closed", Err(ReceiveError::Closed)) => true,
                _ => false,
            };
            assert!(matched, "{expected}");
        }
        let mut wire = pixel_reply_frame(reply);
        wire.extend_from_slice(&pixels_frame(2, 2, &body)[..25]);
        assert!(matches!(
            receive_from_core(&mut Cursor::new(wire), &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    /// GIVEN a video reply outside the video limits, or with extra or missing
    /// fields WHEN read THEN it is refused before the next frame is touched;
    /// WHEN the core tries to send one THEN nothing is written.
    #[test]
    fn given_an_out_of_limit_video_reply_then_nothing_crosses() {
        let budgets = ResourceBudgets::default();
        for reply in [
            r#"{"video_reply":{"id":1,"tile":6,"seq":1,"rotation":0,"width":1,"height":1}}"#,
            r#"{"video_reply":{"id":1,"tile":5,"seq":1,"rotation":0,"width":1281,"height":1}}"#,
            r#"{"video_reply":{"id":1,"tile":5,"seq":1,"rotation":0,"width":1280,"height":721}}"#,
            r#"{"video_reply":{"id":1,"tile":4,"seq":1,"rotation":0,"width":1280,"height":720}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":4,"width":1,"height":1}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":0,"height":1}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":1,"height":0}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":641,"height":1}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":1,"height":641}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":640,"height":361}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":9007199254740992,"rotation":0,"width":1,"height":1}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":1,"height":1,"x":0}}"#,
            r#"{"video_reply":{"id":1,"tile":0,"seq":1,"rotation":0,"width":1}}"#,
            r#"{"video_reply":{"id":1,"tile":256,"seq":1,"rotation":0,"width":1,"height":1}}"#,
        ] {
            let mut wire = pixel_reply_frame(reply);
            let end = wire.len() as u64;
            wire.extend_from_slice(&pixels_frame(1, 1, &[0; 4]));
            let mut reader = Cursor::new(wire);
            assert!(
                matches!(
                    receive_from_core(&mut reader, &budgets),
                    Err(ReceiveError::Malformed)
                ),
                "{reply}"
            );
            assert_eq!(reader.position(), end, "{reply}");
        }
        // The largest picture that fits: 640 x 360, and its turned form.
        for (width, height) in [(640, 360), (360, 640), (1, 1)] {
            let largest = video_reply(4, width, height);
            let mut wire = Vec::new();
            let pixels = vec![1; width as usize * height as usize * 4];
            send_video_reply(&mut wire, &largest, &pixels).unwrap();
            assert!(matches!(
                receive_from_core(&mut Cursor::new(wire), &budgets),
                Ok(FromCore::Video(_))
            ));
        }
        // The screen tile takes up to 1280 x 720, and its turned form.
        for (width, height) in [(1280, 720), (720, 1280)] {
            let screen = video_reply(5, width, height);
            let mut wire = Vec::new();
            let pixels = vec![1; width as usize * height as usize * 4];
            send_video_reply(&mut wire, &screen, &pixels).unwrap();
            assert!(matches!(
                receive_from_core(&mut Cursor::new(wire), &budgets),
                Ok(FromCore::Video(_))
            ));
        }
        for (reply, length) in [
            (video_reply(6, 2, 2), 16),
            (video_reply(5, 1281, 1), 1281 * 4),
            (video_reply(5, 1280, 721), 1280 * 721 * 4),
            (video_reply(4, 1280, 720), 1280 * 720 * 4),
            (
                VideoReply {
                    rotation: 4,
                    ..video_reply(0, 2, 2)
                },
                16,
            ),
            (video_reply(0, 640, 361), 640 * 361 * 4),
            (video_reply(0, 641, 1), 641 * 4),
            (video_reply(0, 0, 2), 0),
            (video_reply(0, 2, 2), 15),
            (video_reply(0, 2, 2), 20),
            (
                VideoReply {
                    seq: MAX_SAFE_TIMESTAMP + 1,
                    ..video_reply(0, 2, 2)
                },
                16,
            ),
        ] {
            let mut wire = Vec::new();
            assert!(send_video_reply(&mut wire, &reply, &vec![0; length]).is_err());
            assert!(wire.is_empty(), "{reply:?}");
        }
    }

    /// GIVEN video and camera calls WHEN their limits are checked THEN tiles
    /// 0 to 5 and safe cursors pass, and every other value is refused.
    #[test]
    fn given_video_calls_when_limits_are_checked_then_bounds_hold() {
        let frame = |tile, after, stream| Call::CallVideoFrame {
            tile,
            after,
            stream,
        };
        assert!(call_video_within_limits(&frame(0, 0, 0)));
        assert!(call_video_within_limits(&frame(
            5,
            MAX_SAFE_TIMESTAMP,
            MAX_SAFE_TIMESTAMP
        )));
        assert!(!call_video_within_limits(&frame(6, 0, 0)));
        assert!(!call_video_within_limits(&frame(255, 0, 0)));
        assert!(!call_video_within_limits(&frame(
            0,
            MAX_SAFE_TIMESTAMP + 1,
            0
        )));
        assert!(!call_video_within_limits(&frame(
            0,
            0,
            MAX_SAFE_TIMESTAMP + 1
        )));
        let devices = |camera: Option<String>| Call::SetCallDevices {
            recording: None,
            playout: None,
            camera,
        };
        assert!(call_video_within_limits(&devices(None)));
        assert!(call_video_within_limits(&devices(Some("c".into()))));
        assert!(call_video_within_limits(&devices(Some(
            "c".repeat(MAX_CALL_DEVICE_ID_BYTES)
        ))));
        assert!(!call_video_within_limits(&devices(Some(String::new()))));
        assert!(!call_video_within_limits(&devices(Some(
            "c".repeat(MAX_CALL_DEVICE_ID_BYTES + 1)
        ))));
        assert!(!call_video_within_limits(&devices(Some("a\nb".into()))));
        assert!(call_video_within_limits(&Call::CallCamera { on: true }));
        assert!(call_video_within_limits(&Call::CallScreen { on: true }));
        // The volume is 0 to 100; the wire type already refuses 256 and up.
        assert!(call_video_within_limits(&Call::CallVolume { percent: 0 }));
        assert!(call_video_within_limits(&Call::CallVolume {
            percent: MAX_CALL_VOLUME_PERCENT
        }));
        assert!(!call_video_within_limits(&Call::CallVolume {
            percent: MAX_CALL_VOLUME_PERCENT + 1
        }));
        assert!(!call_video_within_limits(&Call::CallVolume {
            percent: 255
        }));
        // The microphone gain is 0 to 400 percent in steps of 5.
        let gain = |gain| Call::CallMicrophone {
            auto_level: true,
            gain,
        };
        for accepted in [0, 5, 100, 395, MAX_CALL_MIC_GAIN_PERCENT] {
            assert!(call_video_within_limits(&gain(accepted)), "{accepted}");
        }
        for refused in [1, 99, 401, 405, 1000, u16::MAX] {
            assert!(!call_video_within_limits(&gain(refused)), "{refused}");
        }
    }

    /// GIVEN video replies of many sizes on camera tiles and the screen tile
    /// WHEN their frame headers are made THEN the tile decides the limit: a
    /// screen picture up to 1280x720 passes only on tile 5, a camera picture
    /// stays at 640x360, and every size over a limit is refused.
    #[test]
    fn given_video_replies_when_headers_are_made_then_the_tile_limit_decides() {
        let reply = |tile, width, height| VideoReply {
            id: 1,
            tile,
            seq: 1,
            rotation: 0,
            width,
            height,
        };
        let header = reply(CALL_SCREEN_TILE, 1280, 720)
            .frame_header()
            .expect("a full screen picture");
        assert_eq!(header.length, 3_686_400);
        assert!(reply(CALL_SCREEN_TILE, 720, 1280).frame_header().is_some());
        assert!(reply(CALL_SCREEN_TILE, 1281, 720).frame_header().is_none());
        assert!(reply(CALL_SCREEN_TILE, 1280, 721).frame_header().is_none());
        assert!(reply(CALL_SCREEN_TILE, 0, 720).frame_header().is_none());
        for tile in 0..CALL_SCREEN_TILE {
            assert!(reply(tile, 640, 360).frame_header().is_some(), "{tile}");
            assert!(reply(tile, 1280, 720).frame_header().is_none(), "{tile}");
            assert!(reply(tile, 641, 360).frame_header().is_none(), "{tile}");
            assert!(reply(tile, 640, 361).frame_header().is_none(), "{tile}");
        }
        assert!(reply(CALL_VIDEO_TILES, 1, 1).frame_header().is_none());
    }

    /// GIVEN the screen command WHEN it is decoded THEN it carries only the
    /// flag: any source, device, handle, id or missing flag is refused.
    #[test]
    fn given_the_screen_command_then_the_page_can_name_no_source() {
        let decoded = |json: &[u8]| serde_json::from_slice::<Request>(json);
        assert!(decoded(br#"{"id":1,"call":{"command":"call_screen","on":true}}"#).is_ok());
        assert!(decoded(br#"{"id":1,"call":{"command":"call_screen","on":false}}"#).is_ok());
        for json in [
            &br#"{"id":1,"call":{"command":"call_screen"}}"#[..],
            br#"{"id":1,"call":{"command":"call_screen","on":1}}"#,
            br#"{"id":1,"call":{"command":"call_screen","on":true,"source":"1"}}"#,
            br#"{"id":1,"call":{"command":"call_screen","on":true,"monitor":0}}"#,
            br#"{"id":1,"call":{"command":"call_screen","on":true,"node":7}}"#,
            br#"{"id":1,"call":{"command":"call_screen","on":true,"window":"x"}}"#,
        ] {
            assert!(decoded(json).is_err(), "{}", String::from_utf8_lossy(json));
        }
    }
}

#[cfg(test)]
mod personal_tests {
    use super::*;

    /// Check strict personal command schemas and required nullable fields.
    #[test]
    fn personal_wire_round_trips_and_requires_nullable_fields() {
        for json in [
            r#"{"command":"room_draft","room_id":"!r:s"}"#,
            r#"{"command":"save_room_draft","room_id":"!r:s","draft":{"body":"draft","replyTo":null}}"#,
            r#"{"command":"room_preferences"}"#,
            r#"{"command":"set_room_preference","preference":{"roomId":"!r:s","favourite":true,"section":null}}"#,
            r#"{"command":"search_messages","term":"text","room_id":null,"filters":{"sender":null,"fromMs":null,"beforeMs":null}}"#,
            r#"{"command":"ignored_users"}"#,
            r#"{"command":"set_ignored_user","user_id":"@u:s","ignored":true}"#,
            r#"{"command":"report_abuse","room_id":"!r:s","event_id":"$e","reason":"Spam"}"#,
        ] {
            let call: Call = serde_json::from_str(json).unwrap();
            assert!(personal_call_within_limits(&call));
            assert_eq!(
                serde_json::from_str::<Call>(&serde_json::to_string(&call).unwrap()).unwrap(),
                call
            );
        }
        for json in [
            r#"{"command":"room_preferences","extra":0}"#,
            r#"{"command":"search_messages","term":"text"}"#,
            r#"{"command":"save_room_draft","room_id":"!r:s","draft":{"body":"draft"}}"#,
            r#"{"command":"save_room_draft","room_id":"!r:s","draft":{"body":"draft","replyTo":null,"path":"bad"}}"#,
            r#"{"command":"set_room_preference","preference":{"roomId":"!r:s","favourite":true}}"#,
        ] {
            assert!(serde_json::from_str::<Call>(json).is_err(), "{json}");
        }
    }

    /// Check UTF-8 byte limits and reject disallowed control characters.
    #[test]
    fn personal_limits_count_utf8_bytes_and_reject_control_characters() {
        let search = |term| Call::SearchMessages {
            filters: Default::default(),
            term,
            room_id: None,
        };
        assert!(personal_call_within_limits(&search("é".repeat(64))));
        for text in ["é".repeat(65), " ".into(), "a\nb".into()] {
            assert!(!personal_call_within_limits(&search(text)));
        }
        assert!(!personal_call_within_limits(&Call::ReportAbuse {
            room_id: "!r:s".into(),
            event_id: "$e".into(),
            reason: "x".repeat(1001)
        }));
    }
}

#[cfg(test)]
mod navigation_wire_tests {
    use super::*;
    /// Build valid navigation command fixtures with explicit nullable filters.
    fn calls() -> Vec<serde_json::Value> {
        let filters = serde_json::json!({"sender":null,"fromMs":null,"beforeMs":null});
        vec![
            serde_json::json!({"command":"search_messages","term":"text","room_id":null,"filters":filters}),
            serde_json::json!({"command":"jump_to_date","room_id":"!r:s","timestamp_ms":MAX_SAFE_TIMESTAMP}),
            serde_json::json!({"command":"event_context","room_id":"!r:s","event_id":"$e"}),
            serde_json::json!({"command":"pinned_messages","room_id":"!r:s"}),
            serde_json::json!({"command":"set_pinned","room_id":"!r:s","event_id":"$e","pinned":true}),
            serde_json::json!({"command":"attachments","room_id":"!r:s","cursor":null,"kind":"all","filters":filters}),
            serde_json::json!({"command":"connection_health"}),
        ]
    }
    /// Reject missing and extra navigation fields while accepting valid commands.
    #[test]
    fn navigation_commands_require_exact_wire_fields() {
        for value in calls() {
            let call: Call = serde_json::from_value(value.clone()).unwrap();
            assert!(personal_call_within_limits(&call));
            assert_eq!(serde_json::to_value(call).unwrap(), value);
            let mut extra = value.clone();
            extra["secret"] = true.into();
            assert!(serde_json::from_value::<Call>(extra).is_err());
            for key in value
                .as_object()
                .unwrap()
                .keys()
                .filter(|key| *key != "command")
            {
                let mut missing = value.clone();
                missing.as_object_mut().unwrap().remove(key);
                assert!(
                    serde_json::from_value::<Call>(missing).is_err(),
                    "missing {key}: {value}"
                );
                let mut wrong = value.clone();
                wrong[key] = serde_json::json!([]);
                assert!(
                    serde_json::from_value::<Call>(wrong).is_err(),
                    "wrong {key}: {value}"
                );
                let wire = serde_json::to_string(&value).unwrap();
                let duplicate = format!("{},\"{}\":{}}}", &wire[..wire.len() - 1], key, value[key]);
                assert!(
                    serde_json::from_str::<Call>(&duplicate).is_err(),
                    "duplicate {key}"
                );
            }
        }
    }
    /// Reject oversized identifiers and cursors, unsafe timestamps, and invalid ranges.
    #[test]
    fn navigation_limits_reject_oversized_ids_cursors_and_invalid_ranges() {
        for value in calls() {
            for (key, limit) in [
                ("room_id", MAX_ROOM_ID_BYTES),
                ("event_id", MAX_EVENT_ID_BYTES),
                ("cursor", MAX_CURSOR_BYTES),
            ] {
                if value.get(key).is_none() {
                    continue;
                }
                let mut oversized = value.clone();
                oversized[key] = "é".repeat(limit / 2 + 1).into();
                assert!(!personal_call_within_limits(
                    &serde_json::from_value(oversized).unwrap()
                ));
            }
        }
        assert!(!personal_call_within_limits(&Call::JumpToDate {
            room_id: "!r:s".into(),
            timestamp_ms: MAX_SAFE_TIMESTAMP + 1
        }));
        for filters in [
            serde_json::json!({"sender":"","fromMs":null,"beforeMs":null}),
            serde_json::json!({"sender":"@u:\ns","fromMs":null,"beforeMs":null}),
            serde_json::json!({"sender":"é".repeat(MAX_USER_ID_BYTES / 2 + 1),"fromMs":null,"beforeMs":null}),
            serde_json::json!({"sender":null,"fromMs":2,"beforeMs":2}),
            serde_json::json!({"sender":null,"fromMs":3,"beforeMs":2}),
        ] {
            assert!(!search_filters_within_limits(
                &serde_json::from_value(filters).unwrap()
            ));
        }
    }
}

/// The pasted image: a request and its image frame (ADR-0042).
#[cfg(test)]
mod paste_tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: FrameKind, body: &[u8]) -> Vec<u8> {
        let mut wire = FrameHeader {
            kind,
            length: body.len() as u64,
            width: 0,
            height: 0,
        }
        .encode()
        .to_vec();
        wire.extend_from_slice(body);
        wire
    }

    fn paste(length: u32) -> Request {
        Request {
            id: 7,
            call: Call::PasteImage {
                room_id: "!r:s".into(),
                length,
            },
        }
    }

    /// GIVEN a pasted image WHEN sent and received THEN the core gets the
    /// request and exactly those bytes, and nothing more is read.
    #[test]
    fn given_a_pasted_image_then_its_request_and_bytes_cross_together() {
        let bytes = b"\x89PNG not really".to_vec();
        let mut wire = Vec::new();
        send_pasted_image(&mut wire, &paste(bytes.len() as u32), &bytes).unwrap();
        wire.extend_from_slice(&frame(
            FrameKind::Control,
            br#"{"id":8,"call":{"command":"app_status"}}"#,
        ));
        let mut reader = Cursor::new(wire);
        let budgets = ResourceBudgets::default();
        let Ok(ToCore::Paste(request, image)) = receive_request(&mut reader, &budgets) else {
            panic!("a paste with its image");
        };
        assert_eq!(request, paste(bytes.len() as u32));
        assert_eq!(image.header().kind, FrameKind::EncodedImage);
        assert_eq!(image.body(), &bytes[..]);
        assert!(matches!(
            receive_request(&mut reader, &budgets),
            Ok(ToCore::Request(Request { id: 8, .. }))
        ));
    }

    /// GIVEN a pasted image that does not match its request or the limit WHEN
    /// sent THEN nothing is written.
    #[test]
    fn given_a_paste_outside_its_request_or_limit_then_nothing_is_written() {
        let too_long = vec![0; MAX_PASTED_IMAGE_BYTES as usize + 1];
        for (request, bytes) in [
            (paste(3), &b"ab"[..]),
            (paste(0), &b""[..]),
            (paste(MAX_PASTED_IMAGE_BYTES + 1), &too_long[..]),
            (
                Request {
                    id: 1,
                    call: Call::AppStatus {},
                },
                &b"abc"[..],
            ),
        ] {
            let mut wire = Vec::new();
            assert!(send_pasted_image(&mut wire, &request, bytes).is_err());
            assert!(wire.is_empty());
        }
    }

    /// GIVEN a paste request WHEN the next frame is not an image of exactly
    /// the declared length, or the declared length is over the limit, THEN
    /// the channel breaks before any image body is read.
    #[test]
    fn given_a_paste_without_its_exact_image_then_the_channel_breaks() {
        let budgets = ResourceBudgets::default();
        let request = |length: u32| {
            frame(
                FrameKind::Control,
                &serde_json::to_vec(&paste(length)).unwrap(),
            )
        };
        // Another control frame instead of the image.
        let mut wire = request(4);
        wire.extend_from_slice(&frame(FrameKind::Control, b"abcd"));
        assert!(matches!(
            receive_request(&mut Cursor::new(wire), &budgets),
            Err(ReceiveError::UnexpectedKind)
        ));
        // An image of another length: its body is never read.
        let mut wire = request(4);
        let header_end = wire.len() + FRAME_HEADER_LEN;
        wire.extend_from_slice(&frame(FrameKind::EncodedImage, b"abcde"));
        let mut reader = Cursor::new(wire);
        assert!(matches!(
            receive_request(&mut reader, &budgets),
            Err(ReceiveError::Malformed)
        ));
        assert_eq!(reader.position(), header_end as u64);
        // A declared length over the limit: refused before any image header.
        let mut wire = request(MAX_PASTED_IMAGE_BYTES + 1);
        let request_end = wire.len();
        wire.extend_from_slice(&frame(FrameKind::EncodedImage, b"a"));
        let mut reader = Cursor::new(wire);
        assert!(matches!(
            receive_request(&mut reader, &budgets),
            Err(ReceiveError::Malformed)
        ));
        assert_eq!(reader.position(), request_end as u64);
        // No image at all: the stream ends.
        assert!(matches!(
            receive_request(&mut Cursor::new(request(4)), &budgets),
            Err(ReceiveError::Closed)
        ));
    }

    /// GIVEN the encoded budget is full WHEN a paste arrives THEN its bytes
    /// are read past without being kept, it is refused, and the next request
    /// still reads.
    #[test]
    fn given_a_full_budget_then_a_paste_is_refused_and_the_channel_stays_in_step() {
        let budgets = ResourceBudgets::default();
        let mut held = Vec::new();
        while let Ok(permit) =
            budgets.try_acquire(persephone_media_protocol::BudgetClass::EncodedInFlight, MIB)
        {
            held.push(permit);
        }
        let mut wire = Vec::new();
        send_pasted_image(&mut wire, &paste(4), b"abcd").unwrap();
        wire.extend_from_slice(&frame(
            FrameKind::Control,
            br#"{"id":8,"call":{"command":"app_status"}}"#,
        ));
        let mut reader = Cursor::new(wire);
        assert!(matches!(
            receive_request(&mut reader, &budgets),
            Ok(ToCore::PasteRefused(Request { id: 7, .. }))
        ));
        assert!(matches!(
            receive_request(&mut reader, &budgets),
            Ok(ToCore::Request(Request { id: 8, .. }))
        ));
        drop(held);
    }

    /// GIVEN the paste call WHEN sent THEN it has a strict wire form.
    #[test]
    fn given_the_paste_call_then_it_has_a_strict_wire_form() {
        let budgets = ResourceBudgets::default();
        let wire = r#"{"id":7,"call":{"command":"paste_image","room_id":"!r:s","length":4}}"#;
        assert_eq!(serde_json::to_string(&paste(4)).unwrap(), wire);
        for body in [
            &br#"{"id":1,"call":{"command":"paste_image","room_id":"!r:s"}}"#[..],
            br#"{"id":1,"call":{"command":"paste_image","room_id":"!r:s","length":-1}}"#,
            br#"{"id":1,"call":{"command":"paste_image","room_id":"!r:s","length":4,"mime":"image/png"}}"#,
            br#"{"id":1,"call":{"command":"paste_image","room_id":null,"length":4}}"#,
        ] {
            let mut reader = Cursor::new(frame(FrameKind::Control, body));
            assert!(matches!(
                receive::<_, Request>(&mut reader, &budgets),
                Err(ReceiveError::Malformed)
            ));
        }
    }
}

#[cfg(test)]
mod timeline_events_wire_tests {
    use super::*;

    fn valid() -> serde_json::Value {
        serde_json::json!({"command":"timeline_events","room_id":"!r:s","event_ids":["$a","$b"]})
    }

    /// GIVEN the exact wire form WHEN it is decoded THEN it is accepted
    /// within the limits and encodes back to the same form.
    #[test]
    fn given_the_exact_wire_form_then_timeline_events_is_accepted() {
        let call: Call = serde_json::from_value(valid()).unwrap();
        assert_eq!(
            call,
            Call::TimelineEvents {
                room_id: "!r:s".into(),
                event_ids: vec!["$a".into(), "$b".into()],
            }
        );
        assert!(personal_call_within_limits(&call));
        assert_eq!(serde_json::to_value(call).unwrap(), valid());
    }

    /// GIVEN a missing field, a field of the wrong type, or an extra field
    /// WHEN the call is decoded THEN it is refused.
    #[test]
    fn given_a_missing_wrong_or_extra_field_then_timeline_events_is_refused() {
        for key in ["room_id", "event_ids"] {
            let mut missing = valid();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<Call>(missing).is_err(),
                "missing {key}"
            );
        }
        for (key, wrong) in [
            ("room_id", serde_json::json!(["!r:s"])),
            ("room_id", serde_json::Value::Null),
            ("event_ids", serde_json::json!("$a")),
            ("event_ids", serde_json::Value::Null),
            ("event_ids", serde_json::json!([1])),
            ("event_ids", serde_json::json!([null])),
        ] {
            let mut value = valid();
            value[key] = wrong;
            assert!(
                serde_json::from_value::<Call>(value).is_err(),
                "wrong {key}"
            );
        }
        let mut extra = valid();
        extra["cursor"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Call>(extra).is_err());
    }

    /// GIVEN no event IDs, more than `MAX_TIMELINE_EVENTS`, a repeated, empty,
    /// oversized or control-character ID, or an oversized room ID WHEN the
    /// limits are checked THEN the call is refused; exactly the maximum of
    /// different IDs is accepted.
    #[test]
    fn given_ids_outside_the_limits_then_timeline_events_is_refused() {
        let call = |room_id: &str, event_ids: Vec<String>| Call::TimelineEvents {
            room_id: room_id.into(),
            event_ids,
        };
        let ids = |count: usize| (0..count).map(|n| format!("$e{n}")).collect::<Vec<_>>();
        assert!(personal_call_within_limits(&call(
            "!r:s",
            ids(MAX_TIMELINE_EVENTS)
        )));
        for refused in [
            call("!r:s", Vec::new()),
            call("!r:s", ids(MAX_TIMELINE_EVENTS + 1)),
            call("!r:s", vec!["$a".into(), "$b".into(), "$a".into()]),
            call("!r:s", vec![String::new()]),
            call("!r:s", vec!["é".repeat(MAX_EVENT_ID_BYTES / 2 + 1)]),
            call("!r:s", vec!["$a\n".into()]),
            call(&"é".repeat(MAX_ROOM_ID_BYTES / 2 + 1), ids(1)),
            call("!r:\u{7}s", ids(1)),
        ] {
            assert!(!personal_call_within_limits(&refused), "{refused:?}");
        }
    }

    /// GIVEN calls with and without arguments WHEN their names are taken
    /// THEN each is the wire tag.
    #[test]
    fn given_calls_when_named_then_tag_only() {
        assert_eq!(
            Call::ProxySetting {}.command().as_deref(),
            Some("proxy_setting")
        );
        assert_eq!(
            Call::SetProxySetting {
                proxy: Some("h:1".into())
            }
            .command()
            .as_deref(),
            Some("set_proxy_setting")
        );
        assert_eq!(
            Call::SetDiagnostics { on: true }.command().as_deref(),
            Some("set_diagnostics")
        );
        for call in [
            Call::AppStatus {},
            Call::SignInWithOidc {
                homeserver: "https://example.org/".into(),
            },
            Call::DiagnosticRecords {
                source: RecordSource::Ui,
                records: Vec::new(),
            },
        ] {
            let name = call.command().expect("every call has a name");
            let wire = serde_json::to_value(&call).unwrap();
            assert_eq!(wire["command"], name.as_str());
        }
    }

    /// GIVEN a sound level event THEN its wire form carries only the level.
    #[test]
    fn given_sound_level_event_then_wire_form_is_small() {
        let event = CoreMessage::Event(Event::CallSoundLevel {
            level: MAX_CALL_SOUND_LEVEL,
        });
        let wire = serde_json::to_string(&event).unwrap();
        assert_eq!(wire, r#"{"event":{"call_sound_level":{"level":100}}}"#);
        assert!(
            serde_json::from_str::<CoreMessage>(r#"{"event":{"call_sound_level":{"level":256}}}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<CoreMessage>(
                r#"{"event":{"call_sound_level":{"level":1,"x":2}}}"#
            )
            .is_err()
        );
    }

    /// GIVEN the statistics command WHEN it is decoded THEN it carries only
    /// the flag.
    #[test]
    fn given_the_stats_command_then_it_carries_only_the_flag() {
        let decoded = |json: &[u8]| serde_json::from_slice::<Request>(json);
        assert!(decoded(br#"{"id":1,"call":{"command":"call_stats","on":true}}"#).is_ok());
        assert!(decoded(br#"{"id":1,"call":{"command":"call_stats","on":false}}"#).is_ok());
        for json in [
            &br#"{"id":1,"call":{"command":"call_stats"}}"#[..],
            br#"{"id":1,"call":{"command":"call_stats","on":1}}"#,
            br#"{"id":1,"call":{"command":"call_stats","on":true,"interval":1}}"#,
        ] {
            assert!(decoded(json).is_err(), "{}", String::from_utf8_lossy(json));
        }
    }

    /// GIVEN statistics events WHEN read THEN a reading inside its bounds
    /// is kept, and one with a number over its bound, an overlong ID, a
    /// repeated device or an unknown field is refused whole.
    #[test]
    fn given_call_stats_events_then_only_bounded_readings_are_read() {
        let event = |stats: serde_json::Value| {
            serde_json::from_value::<CoreMessage>(
                serde_json::json!({"event": {"call_stats": {"stats": stats}}}),
            )
        };
        let reading = |fps: u32, user: &str| {
            serde_json::json!({
                "microphone": null,
                "camera": {"fps": fps, "width": 640, "height": 360, "bitrateKbps": null,
                           "limit": "none", "packetsLost": null},
                "screen": null,
                "participants": [{"userId": user, "deviceId": "D", "audio": null, "video": null}],
                "connections": [],
            })
        };
        assert!(event(reading(120, "@a:example.org")).is_ok());
        assert!(event(reading(121, "@a:example.org")).is_err());
        assert!(event(reading(1, &"a".repeat(MAX_USER_ID_BYTES + 1))).is_err());
        let mut twice = reading(1, "@a:example.org");
        let again = twice["participants"][0].clone();
        twice["participants"].as_array_mut().unwrap().push(again);
        assert!(event(twice).is_err());
        let mut extra = reading(1, "@a:example.org");
        extra["address"] = "10.0.0.1".into();
        assert!(event(extra).is_err());
        for key in ["microphone", "screen", "participants", "connections"] {
            let mut missing = reading(1, "@a:example.org");
            missing.as_object_mut().unwrap().remove(key);
            assert!(event(missing).is_err(), "{key}");
        }
        for key in ["audio", "deviceId"] {
            let mut missing = reading(1, "@a:example.org");
            missing["participants"][0]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(event(missing).is_err(), "{key}");
        }
        let mut no_rate = reading(1, "@a:example.org");
        no_rate["camera"]
            .as_object_mut()
            .unwrap()
            .remove("bitrateKbps");
        assert!(event(no_rate).is_err());
        let wire = serde_json::to_string(&CoreMessage::Event(Event::CallStats {
            stats: persephone_contracts::CallStats::default(),
        }))
        .unwrap();
        assert!(wire.starts_with(r#"{"event":{"call_stats":{"stats":{"microphone":null"#));
        assert!(serde_json::from_str::<CoreMessage>(&wire).is_ok());
    }

    /// GIVEN statistics events with a list past its limit, a positional
    /// array or a repeated key WHEN read THEN each is refused: the reader
    /// takes objects only and stops a list at its limit as it reads.
    #[test]
    fn given_call_stats_past_their_limits_or_in_arrays_then_they_are_refused() {
        let event = |stats: serde_json::Value| {
            serde_json::from_value::<CoreMessage>(
                serde_json::json!({"event": {"call_stats": {"stats": stats}}}),
            )
        };
        let participant = |index: usize| {
            serde_json::json!({"userId": format!("@u{index}:example.org"), "deviceId": "D",
                               "audio": null, "video": null})
        };
        let connection = |index: usize| {
            serde_json::json!({"connection": index, "outgoingKbps": null, "sendKbps": null,
                               "receiveKbps": null, "rttMs": null})
        };
        let reading = |participants: usize, connections: usize| {
            serde_json::json!({
                "microphone": null, "camera": null, "screen": null,
                "participants": (0..participants).map(participant).collect::<Vec<_>>(),
                "connections": (0..connections).map(connection).collect::<Vec<_>>(),
            })
        };
        let full = persephone_contracts::CALL_STATS_CONNECTIONS_MAX;
        let people = persephone_contracts::CALL_PARTICIPANTS_MAX;
        assert!(event(reading(people, full)).is_ok());
        assert!(event(reading(people + 1, 0)).is_err());
        // An entry past the limit is refused unbuilt, whatever it holds.
        let mut over = reading(people, 0);
        over["participants"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"anything": [1, 2, 3]}));
        let error = event(over).unwrap_err();
        assert!(error.to_string().contains("over its limit"), "{error}");
        // The entry past the limit cannot be a connection, so only the early
        // limit can refuse it.
        let mut too_many = reading(0, full);
        too_many["connections"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("x"));
        let error = event(too_many).unwrap_err();
        assert!(error.to_string().contains("over its limit"), "{error}");
        assert!(event(serde_json::json!([null, null, null, [], []])).is_err());
        let mut positional = reading(0, 0);
        positional["participants"] = serde_json::json!([["@a:example.org", "D", null, null]]);
        assert!(event(positional).is_err());
        let twice = r#"{"event":{"call_stats":{"stats":{"microphone":null,"microphone":null,
            "camera":null,"screen":null,"participants":[],"connections":[]}}}}"#;
        assert!(serde_json::from_str::<CoreMessage>(twice).is_err());
    }

    /// GIVEN statistics events on the byte path WHEN the content or a part is
    /// positional THEN it is refused, and an object with null parts is kept.
    #[test]
    fn given_call_stats_bytes_with_positional_parts_then_they_are_refused() {
        let read = |json: &str| serde_json::from_slice::<CoreMessage>(json.as_bytes());
        let content = |stats: &str| format!(r#"{{"event":{{"call_stats":{stats}}}}}"#);
        let reading = |microphone: &str, participants: &str, connections: &str| {
            format!(
                r#"{{"microphone":{microphone},"camera":null,"screen":null,"participants":{participants},"connections":{connections}}}"#
            )
        };
        let valid = reading("null", "[]", "[]");
        assert!(read(&content(&format!(r#"{{"stats":{valid}}}"#))).is_ok());
        // The content is an object with the key `stats`, and no other key.
        assert!(read(&content(&format!("[{valid}]"))).is_err());
        assert!(read(&content("{}")).is_err());
        assert!(read(&content(&format!(r#"{{"stats":{valid},"stats":{valid}}}"#))).is_err());
        assert!(read(&content(&format!(r#"{{"stats":{valid},"extra":1}}"#))).is_err());
        // Each nested record is an object too.
        let with = |microphone: &str, participants: &str, connections: &str| {
            content(&format!(
                r#"{{"stats":{}}}"#,
                reading(microphone, participants, connections)
            ))
        };
        // Each array is a complete record that the derived reader of the
        // contract type takes, so only the map-only reader refuses it here.
        let microphone = "[1,null,0,null,null,null]";
        let heard = "[1,null,0,0,0,null]";
        let connection = "[0,null,null,null,null]";
        assert!(
            serde_json::from_str::<persephone_contracts::CallStatsAudioSent>(microphone).is_ok()
        );
        assert!(
            serde_json::from_str::<persephone_contracts::CallStatsAudioReceived>(heard).is_ok()
        );
        assert!(
            serde_json::from_str::<persephone_contracts::CallStatsConnection>(connection).is_ok()
        );
        assert!(read(&with(microphone, "[]", "[]")).is_err());
        assert!(read(&with("null", "[]", &format!("[{connection}]"))).is_err());
        assert!(
            read(&with(
                "null",
                &format!(
                    r#"[{{"userId":"@a:example.org","deviceId":"D","audio":{heard},"video":null}}]"#
                ),
                "[]"
            ))
            .is_err()
        );
        // GIVEN objects with null parts THEN they are kept.
        assert!(
            read(&with(
                r#"{"level":1,"bitrateKbps":null,"packetsSent":0,"packetsLost":null,"jitterMs":null,"rttMs":null}"#,
                r#"[{"userId":"@a:example.org","deviceId":"D","audio":null,"video":null}]"#,
                r#"[{"connection":0,"outgoingKbps":null,"sendKbps":null,"receiveKbps":null,"rttMs":null}]"#
            ))
            .is_ok()
        );
    }

    /// GIVEN call level events WHEN read THEN bounded ones are kept, and a
    /// level over the top, too many devices or an overlong ID is refused.
    #[test]
    fn given_call_level_events_then_only_bounded_ones_are_read() {
        let event = |levels: serde_json::Value| {
            serde_json::from_value::<CoreMessage>(
                serde_json::json!({"event": {"call_levels": {"levels": levels}}}),
            )
        };
        let one = |level: u32, user: &str| serde_json::json!({"userId": user, "deviceId": "D", "level": level});
        assert!(event(serde_json::json!([])).is_ok());
        assert!(event(serde_json::json!([one(100, "@a:example.org")])).is_ok());
        assert!(event(serde_json::json!([one(101, "@a:example.org")])).is_err());
        let long = format!("@{}:example.org", "a".repeat(MAX_USER_ID_BYTES));
        assert!(event(serde_json::json!([one(1, &long)])).is_err());
        let many: Vec<_> = (0..=MAX_CALL_LEVELS)
            .map(|_| one(1, "@a:example.org"))
            .collect();
        assert!(event(serde_json::Value::Array(many)).is_err());
        // At the limits, on the byte path that the receiver reads.
        let full: Vec<_> = (0..MAX_CALL_LEVELS)
            .map(|_| one(1, "@a:example.org"))
            .collect();
        let bytes =
            serde_json::to_vec(&serde_json::json!({"event": {"call_levels": {"levels": full}}}))
                .unwrap();
        assert!(serde_json::from_slice::<CoreMessage>(&bytes).is_ok());
        // An entry past the limit is refused unbuilt, whatever it holds.
        let mut over: Vec<_> = (0..MAX_CALL_LEVELS)
            .map(|_| one(1, "@a:example.org"))
            .collect();
        over.push(serde_json::json!({"anything": [1, 2, 3]}));
        let error = event(serde_json::Value::Array(over)).unwrap_err();
        // An entry is an object, never a positional array (byte path).
        let positional = br#"{"event":{"call_levels":{"levels":[["@a:example.org","D",42]]}}}"#;
        assert!(serde_json::from_slice::<CoreMessage>(positional).is_err());
        let object =
            br#"{"event":{"call_levels":{"levels":[{"userId":"@a:example.org","deviceId":"D","level":42}]}}}"#;
        assert!(serde_json::from_slice::<CoreMessage>(object).is_ok());
        let twice = br#"{"event":{"call_levels":{"levels":[{"userId":"@a:example.org","userId":"@b:example.org","deviceId":"D","level":42}]}}}"#;
        assert!(serde_json::from_slice::<CoreMessage>(twice).is_err());
        let missing =
            br#"{"event":{"call_levels":{"levels":[{"userId":"@a:example.org","deviceId":"D"}]}}}"#;
        assert!(serde_json::from_slice::<CoreMessage>(missing).is_err());
        assert!(error.to_string().contains("over its limit"), "{error}");
        let longest = "a".repeat(MAX_USER_ID_BYTES);
        assert!(event(serde_json::json!([one(1, &longest)])).is_ok());
        let device = "D".repeat(MAX_DEVICE_ID_BYTES + 1);
        assert!(
            event(
                serde_json::json!([{"userId": "@a:example.org", "deviceId": device, "level": 1}])
            )
            .is_err()
        );
        assert!(
            event(serde_json::json!([{"userId": "@a:example.org", "deviceId": "D", "level": 1, "x": 1}]))
                .is_err()
        );
        let wire =
            serde_json::to_string(&CoreMessage::Event(Event::CallDeviceTestChanged {})).unwrap();
        assert_eq!(wire, r#"{"event":{"call_device_test_changed":{}}}"#);
    }

    /// GIVEN device test commands WHEN read THEN each has its wire form, the
    /// device is required (null allowed), and a bad ID is refused by the
    /// limit check.
    #[test]
    fn given_device_test_commands_then_they_read_and_are_bounded() {
        let read = |text: &str| serde_json::from_str::<Request>(text);
        let test =
            read(r#"{"id":1,"call":{"command":"call_device_test","test":"camera","device":null}}"#)
                .expect("a test");
        assert_eq!(
            test.call,
            Call::CallDeviceTest {
                test: persephone_contracts::CallDeviceTestKind::Camera,
                device: None,
            }
        );
        assert!(read(r#"{"id":1,"call":{"command":"call_device_test","test":"camera"}}"#).is_err());
        assert!(
            read(r#"{"id":1,"call":{"command":"call_device_test","test":"x","device":null}}"#)
                .is_err()
        );
        assert_eq!(
            read(r#"{"id":1,"call":{"command":"call_device_test_stop"}}"#)
                .unwrap()
                .call,
            Call::CallDeviceTestStop { id: None }
        );
        assert_eq!(
            read(r#"{"id":1,"call":{"command":"call_device_test_stop","id":7}}"#)
                .unwrap()
                .call,
            Call::CallDeviceTestStop { id: Some(7) }
        );
        assert!(read(r#"{"id":1,"call":{"command":"call_device_test_stop","id":-1}}"#).is_err());
        assert!(read(r#"{"id":1,"call":{"command":"call_device_test_status"}}"#).is_ok());
        let with = |device: &str| Call::CallDeviceTest {
            test: persephone_contracts::CallDeviceTestKind::Microphone,
            device: Some(device.to_owned()),
        };
        assert!(call_video_within_limits(&with("{0.0.1.00000000}.{a}")));
        assert!(!call_video_within_limits(&with("")));
        assert!(!call_video_within_limits(&with("a\nb")));
        assert!(!call_video_within_limits(&with(
            &"a".repeat(MAX_CALL_DEVICE_ID_BYTES + 1)
        )));
    }

    /// GIVEN a diagnostics event THEN its wire form carries only `on`.
    #[test]
    fn given_diagnostics_event_then_wire_form_is_small() {
        let event = CoreMessage::Event(Event::DiagnosticsChanged { on: true });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":{"diagnostics_changed":{"on":true}}}"#
        );
    }

    /// GIVEN records at and over the limits THEN only those within pass.
    #[test]
    fn given_records_then_bounds_are_checked() {
        let record = |target: usize, message: usize| DiagnosticRecord {
            level: RecordLevel::Info,
            target: "t".repeat(target),
            message: "m".repeat(message),
        };
        assert!(record(MAX_DIAGNOSTIC_TARGET_BYTES, MAX_DIAGNOSTIC_MESSAGE_BYTES).within_bounds());
        assert!(!record(MAX_DIAGNOSTIC_TARGET_BYTES + 1, 1).within_bounds());
        assert!(!record(1, MAX_DIAGNOSTIC_MESSAGE_BYTES + 1).within_bounds());
    }

    /// GIVEN a full batch of plain records THEN it fits; GIVEN the same
    /// count of records within their limits but made of control characters,
    /// which JSON escapes to six bytes THEN it does not, and it would also
    /// have broken the control frame limit.
    #[test]
    fn given_escaped_records_then_the_batch_size_is_checked() {
        let record = |message: String| DiagnosticRecord {
            level: RecordLevel::Debug,
            target: "t".into(),
            message,
        };
        let plain = vec![record("m".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES)); MAX_DIAGNOSTIC_RECORDS];
        assert!(DiagnosticRecord::batch_fits(&plain));
        let escaped =
            vec![record("\u{0}".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES)); MAX_DIAGNOSTIC_RECORDS];
        assert!(escaped.iter().all(DiagnosticRecord::within_bounds));
        assert!(!DiagnosticRecord::batch_fits(&escaped));
        assert!(serde_json::to_vec(&escaped).unwrap().len() > 128 * 1024);
        assert!(!DiagnosticRecord::batch_fits(&vec![
            record("m".into());
            MAX_DIAGNOSTIC_RECORDS
                + 1
        ]));
        assert!(record("\u{0}".into()).encoded_len() > record("m".into()).encoded_len());
    }
}
