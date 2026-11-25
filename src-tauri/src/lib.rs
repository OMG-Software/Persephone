//! Persephone UI process (ADR-0004).
//!
//! A thin, keyless Tauri host. The web view may call the commands registered
//! here, subject to the capabilities in `capabilities/`, and nothing else. Each
//! command is relayed to the key-holding core as one bounded frame; the UI holds
//! no sessions, keys or stores. The main window can navigate only within the
//! bundled app and cannot open new windows.
//!
//! The page may subscribe to changes in a room. The core sends a content-free
//! event when a subscribed room changes; the UI forwards it as an empty message
//! on the page's channel for that room, and the page fetches what it needs.
//! When the core's session locks or unlocks, every room channel is dropped
//! (the core drops those subscriptions too) and the page's session channel
//! gets `"session"`, so the page clears what it shows. When rooms are added or
//! change, the session channel gets `"rooms"` and the page fetches the list.
//!
//! A desktop notification (ADR-0022) arrives from the core as an event with
//! a room name and a sender name, never message text. The UI process shows
//! it with the notification plugin's Rust API while the main window is not
//! focused, and never forwards it to the page; no capability grants the page
//! any notification command.
//!
//! Images (ADR-0009) cross as opaque handles. The page fetches a loaded or
//! picked image's pixels one band at a time with `image_band`, which answers
//! with a raw binary IPC response (never JSON numbers or base64): an 8-byte
//! prefix holding the band's width and then its row count, each a
//! little-endian `u32`, followed by exactly `width * rows * 4` RGBA bytes.
//!
//! Call video (ADR-0048) crosses the same way. The page fetches the newest
//! picture of a tile with `call_video_frame`, naming the tile's stream number
//! from the call status. The response body is empty when no picture of that
//! stream is newer than the page's `after`. Otherwise it is a 20-byte
//! prefix, then exactly `width * height * 4` RGBA bytes. The prefix holds the
//! picture number as a little-endian `u64`, then the rotation in quarter turns
//! clockwise, the width and the height, each a little-endian `u32`.

pub mod capture;
pub mod core_link;
mod diagnostics;
#[cfg(windows)]
mod harden;
pub mod launcher;

use core_link::{Band, CoreError, CoreLink, Video};
use persephone_contracts::{
    AccountChange, AccountSettings, AccountStatus, AppStatus, CallDeviceTestKind,
    CallDeviceTestStatus, CallDevices, CallParticipantLevel, CallReadiness, CallRings, CallStatus,
    ContactStatus, ConversationStart, DeviceList, DiagnosticsStatus, DisplayName, HomeserverFound,
    ImagePick, ImageView, InviteList, JoinedRoom, NewSessions, NotificationLevel,
    NotificationSettings, PendingMessage, ProfileList, ProxySetting, PublicRoomPage, RoomActivity,
    RoomChange, RoomCreation, RoomIdentityChanges, RoomInvite, RoomList, RoomMembers, RoomPicture,
    RoomReaders, RoomServers, RoomSettings, SecurityStatus, StoreState, ThreePidList,
    ThreePidMedium, TimelinePage, UserSearch,
};
use persephone_core_protocol::{
    CALL_VIDEO_TILES, COUNTRY_CODE_BYTES, Call, DiagnosticRecord, ErrorCode, Event,
    IMAGE_HANDLE_BYTES, MAX_CALL_DEVICE_ID_BYTES, MAX_CALL_SOUND_LEVEL, MAX_CURSOR_BYTES,
    MAX_DEVICE_ID_BYTES, MAX_DIRECTORY_FILTER_BYTES, MAX_DIRECTORY_TOKEN_BYTES,
    MAX_DISPLAY_NAME_BYTES, MAX_EMAIL_BYTES, MAX_EVENT_ID_BYTES, MAX_FLOW_ID_BYTES,
    MAX_HOMESERVER_URL_BYTES, MAX_INVITEES, MAX_LINK_BYTES, MAX_MEMBER_FILTER_BYTES,
    MAX_MESSAGE_BYTES, MAX_NEW_SESSION_IDS, MAX_NOTIFICATION_NAME_BYTES, MAX_PASTED_IMAGE_BYTES,
    MAX_PHONE_NUMBER_BYTES, MAX_PROFILES, MAX_PROXY_BYTES, MAX_REACTION_KEY_BYTES,
    MAX_ROOM_ID_BYTES, MAX_ROOM_NAME_BYTES, MAX_SEARCH_TERM_BYTES, MAX_SERVER_NAME_BYTES,
    MAX_THREE_PID_ADDRESS_BYTES, MAX_USER_ID_BYTES, MAX_USERNAME_BYTES, RecordSource,
    call_video_within_limits, levels_within_limits, room_change_within_limits,
};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tauri::{
    AppHandle, Manager, Runtime, State, Url, Webview, WebviewUrl, WebviewWindowBuilder,
    ipc::{Channel, InvokeBody, Response},
    webview::{NewWindowResponse, PageLoadEvent},
};

/// The label the `main` capability is bound to.
pub const MAIN_WINDOW: &str = "main";

/// The label the `call` capability is bound to: the call window (ADR-0060).
pub const CALL_WINDOW: &str = "call";

/// Argument the core passes when it starts the UI.
pub const STARTED_BY_CORE: &str = "--started-by-core";

/// Error strings the web view may see. Anything else stays in Rust.
const UNAVAILABLE: &str = "core_unavailable";
const INVALID_ARGUMENT: &str = "invalid_argument";
const NOT_FOUND: &str = "not_found";
const LIMIT_EXCEEDED: &str = "limit_exceeded";
const LOCKED: &str = "locked";
const REJECTED: &str = "rejected";
const KEYS_NOT_BACKED_UP: &str = "keys_not_backed_up";

fn page_error(error: CoreError) -> String {
    match error {
        CoreError::Core(ErrorCode::InvalidArgument) => INVALID_ARGUMENT,
        CoreError::Core(ErrorCode::NotFound) => NOT_FOUND,
        CoreError::Core(ErrorCode::LimitExceeded) => LIMIT_EXCEEDED,
        CoreError::Core(ErrorCode::Locked) => LOCKED,
        CoreError::Core(ErrorCode::Rejected) => REJECTED,
        CoreError::Core(ErrorCode::KeysNotBackedUp) => KEYS_NOT_BACKED_UP,
        CoreError::Core(ErrorCode::Unavailable | ErrorCode::Internal)
        | CoreError::Unavailable
        | CoreError::BadReply => UNAVAILABLE,
    }
    .to_string()
}

/// Sends one call to the core off the UI thread and checks the reply against
/// the expected contract type, which rejects unknown fields.
async fn relay<T: DeserializeOwned + Send + 'static>(
    core: &CoreLink,
    call: Call,
) -> Result<T, String> {
    let core = core.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let value = core.call(call)?;
        serde_json::from_value::<T>(value).map_err(|_| CoreError::BadReply)
    })
    .await
    .map_err(|_| UNAVAILABLE.to_string())?
    .map_err(page_error)
}

#[tauri::command]
async fn app_status(core: State<'_, CoreLink>) -> Result<AppStatus, String> {
    relay(core.inner(), Call::AppStatus {}).await
}

#[tauri::command]
async fn room_list(core: State<'_, CoreLink>) -> Result<RoomList, String> {
    relay(core.inner(), Call::RoomList {}).await
}

/// Reject excessive personal-data arguments before relay; the core validates
/// identifiers, session authority, and stored data again.
async fn personal_relay<T: DeserializeOwned + Send + 'static>(
    core: &CoreLink,
    call: Call,
) -> Result<T, String> {
    if !persephone_core_protocol::personal_call_within_limits(&call) {
        return Err(INVALID_ARGUMENT.to_owned());
    }
    relay(core, call).await
}

/// Relay a local draft read without giving the UI access to the profile store.
#[tauri::command]
async fn room_draft(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<persephone_contracts::RoomDraft, String> {
    personal_relay(core.inner(), Call::RoomDraft { room_id }).await
}

/// List saved draft room IDs, including drafts outside the joined room list.
#[tauri::command]
async fn room_drafts(core: State<'_, CoreLink>) -> Result<Vec<String>, String> {
    personal_relay(core.inner(), Call::RoomDrafts {}).await
}

/// Relay a bounded local draft replacement or clear to the core mutation gate.
#[tauri::command]
async fn save_room_draft(
    core: State<'_, CoreLink>,
    room_id: String,
    draft: persephone_contracts::RoomDraft,
) -> Result<(), String> {
    personal_relay(core.inner(), Call::SaveRoomDraft { room_id, draft }).await
}

/// Read local room organisation through the keyless core channel.
#[tauri::command]
async fn room_preferences(
    core: State<'_, CoreLink>,
) -> Result<persephone_contracts::RoomPreferences, String> {
    personal_relay(core.inner(), Call::RoomPreferences {}).await
}

/// Relay one room preference; the core serializes and validates the mutation.
#[tauri::command]
async fn set_room_preference(
    core: State<'_, CoreLink>,
    preference: persephone_contracts::RoomPreference,
) -> Result<(), String> {
    personal_relay(core.inner(), Call::SetRoomPreference { preference }).await
}

/// Request a bounded search of cached text without starting server search.
#[tauri::command]
async fn search_messages(
    core: State<'_, CoreLink>,
    term: String,
    room_id: Option<String>,
    filters: persephone_contracts::LocalSearchFilters,
) -> Result<persephone_contracts::LocalSearchResults, String> {
    personal_relay(
        core.inner(),
        Call::SearchMessages {
            term,
            room_id,
            filters,
        },
    )
    .await
}

/// Locate bounded history near a date without exposing server tokens to the UI.
#[tauri::command]
async fn jump_to_date(
    core: State<'_, CoreLink>,
    room_id: String,
    timestamp_ms: u64,
) -> Result<persephone_contracts::FocusedTimeline, String> {
    personal_relay(
        core.inner(),
        Call::JumpToDate {
            room_id,
            timestamp_ms,
        },
    )
    .await
}

/// Fetch a bounded context for an explicit room event reference.
#[tauri::command]
async fn event_context(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
) -> Result<persephone_contracts::FocusedTimeline, String> {
    personal_relay(core.inner(), Call::EventContext { room_id, event_id }).await
}

/// Read loaded undecryptable events of an open room again after room keys
/// arrived: 1 to `MAX_TIMELINE_EVENTS` different event IDs, checked before
/// relay and again in the core.
#[tauri::command]
async fn timeline_events(
    core: State<'_, CoreLink>,
    room_id: String,
    event_ids: Vec<String>,
) -> Result<TimelinePage, String> {
    personal_relay(core.inner(), Call::TimelineEvents { room_id, event_ids }).await
}

/// List shared pin references and content visible to this account.
#[tauri::command]
async fn pinned_messages(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<persephone_contracts::PinnedMessages, String> {
    personal_relay(core.inner(), Call::PinnedMessages { room_id }).await
}

/// Relay a pin mutation; the core checks joined membership and power levels.
#[tauri::command]
async fn set_pinned(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
    pinned: bool,
) -> Result<(), String> {
    personal_relay(
        core.inner(),
        Call::SetPinned {
            room_id,
            event_id,
            pinned,
        },
    )
    .await
}

/// Read attachment metadata only; media retrieval still needs an explicit action.
#[tauri::command]
async fn attachments(
    core: State<'_, CoreLink>,
    room_id: String,
    cursor: Option<String>,
    kind: persephone_contracts::AttachmentKind,
    filters: persephone_contracts::LocalSearchFilters,
) -> Result<persephone_contracts::AttachmentPage, String> {
    personal_relay(
        core.inner(),
        Call::Attachments {
            room_id,
            cursor,
            kind,
            filters,
        },
    )
    .await
}

/// Snapshot observations of existing sync work without starting a network probe.
#[tauri::command]
async fn connection_health(
    core: State<'_, CoreLink>,
) -> Result<persephone_contracts::ConnectionHealth, String> {
    personal_relay(core.inner(), Call::ConnectionHealth {}).await
}

/// Read the bounded ignore display list; filtering remains in the core.
#[tauri::command]
async fn ignored_users(
    core: State<'_, CoreLink>,
) -> Result<persephone_contracts::IgnoredUsers, String> {
    personal_relay(core.inner(), Call::IgnoredUsers {}).await
}

/// Relay an explicit ignore change to the core and the account homeserver.
#[tauri::command]
async fn set_ignored_user(
    core: State<'_, CoreLink>,
    user_id: String,
    ignored: bool,
) -> Result<(), String> {
    personal_relay(core.inner(), Call::SetIgnoredUser { user_id, ignored }).await
}

/// Relay a confirmed event reference and reason for submission by the core.
#[tauri::command]
async fn report_abuse(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
    reason: String,
) -> Result<(), String> {
    personal_relay(
        core.inner(),
        Call::ReportAbuse {
            room_id,
            event_id,
            reason,
        },
    )
    .await
}

/// Length checks here keep obviously bad input off the core channel; the core
/// validates authoritatively.
#[tauri::command]
async fn timeline_page(
    core: State<'_, CoreLink>,
    room_id: String,
    cursor: Option<String>,
) -> Result<TimelinePage, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES
        || cursor.as_ref().is_some_and(|c| c.len() > MAX_CURSOR_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::TimelinePage { room_id, cursor }).await
}

/// Shows one desktop notification: the room's name, then the sender's, and
/// whether it announces an incoming call (ADR-0052).
pub type Toaster = Box<dyn Fn(&str, &str, bool) + Send + Sync>;

/// What shows desktop notifications once `setup` installs it, and the latest
/// notification that arrived before then (the core can send one as soon as
/// the UI starts).
#[derive(Default)]
struct Toasts {
    show: Option<Toaster>,
    waiting: Option<(String, String, bool)>,
}

/// The page's channel for each room it subscribed to, and its one session
/// channel. The core accepts at most `MAX_SUBSCRIPTIONS` rooms, and a room
/// channel is kept only after the core accepted the room, so this map is
/// bounded the same way. Desktop notifications go to the toaster, never to
/// a page channel.
///
/// Each window (`main` and `call`, ADR-0060) has its own session, sound
/// level and level channels, under its label. Only the app's own windows
/// exist, so these maps hold at most two entries.
#[derive(Default)]
pub struct Subscriptions {
    channels: Mutex<HashMap<String, Channel<()>>>,
    session: Mutex<HashMap<String, Channel<&'static str>>>,
    /// Each page's channel for the call's own sound level meter (ADR-0055):
    /// a number from 0 to `MAX_CALL_SOUND_LEVEL`, nothing else.
    sound_level: Mutex<HashMap<String, Channel<u8>>>,
    /// Each page's channel for the other devices' levels (ADR-0059).
    levels: Mutex<HashMap<String, Channel<Vec<CallParticipantLevel>>>>,
    toasts: Mutex<Toasts>,
    capture: Arc<capture::CaptureState>,
}

impl Subscriptions {
    fn channels(&self) -> MutexGuard<'_, HashMap<String, Channel<()>>> {
        self.channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session(&self) -> MutexGuard<'_, HashMap<String, Channel<&'static str>>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sound_level(&self) -> MutexGuard<'_, HashMap<String, Channel<u8>>> {
        self.sound_level
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn levels(&self) -> MutexGuard<'_, HashMap<String, Channel<Vec<CallParticipantLevel>>>> {
        self.levels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tells every page's session channel about `what`.
    fn tell_sessions(&self, what: &'static str) {
        for channel in self.session().values() {
            let _ = channel.send(what);
        }
    }

    /// Drops the channels of the window `label`. The main window's room
    /// channels go with it; their rooms come back for the core to release.
    fn forget_window(&self, label: &str) -> Vec<String> {
        self.session().remove(label);
        self.sound_level().remove(label);
        self.levels().remove(label);
        if label != MAIN_WINDOW {
            return Vec::new();
        }
        self.channels().drain().map(|(room, _)| room).collect()
    }

    /// The wanted capture protection, which a new session turns on again.
    pub fn capture(&self) -> Arc<capture::CaptureState> {
        self.capture.clone()
    }

    fn toasts(&self) -> MutexGuard<'_, Toasts> {
        self.toasts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sets what shows desktop notifications, and shows the latest one that
    /// arrived before it; until then only that one is kept.
    pub fn set_toaster(&self, toaster: Toaster) {
        let mut toasts = self.toasts();
        if let Some((room, sender, call)) = toasts.waiting.take() {
            toaster(&room, &sender, call);
        }
        toasts.show = Some(toaster);
    }

    /// Forwards a core event to the page. Called on the core reader thread.
    /// A page that has gone away simply misses the message.
    pub fn dispatch(&self, event: Event) {
        match event {
            Event::RoomChanged { room_id } => {
                if let Some(channel) = self.channels().get(&room_id) {
                    let _ = channel.send(());
                }
            }
            Event::SessionChanged {} => {
                // Sign-in, sign-out, lock and unlock: protect the window
                // until the settings say otherwise.
                self.capture.protect();
                self.channels().clear();
                self.tell_sessions("session");
            }
            Event::RoomListChanged {} => self.tell_sessions("rooms"),
            Event::SecurityChanged {} => self.tell_sessions("security"),
            Event::AccountChanged {} => self.tell_sessions("account"),
            Event::CallChanged {} => self.tell_sessions("calls"),
            Event::CallVideo {} => self.tell_sessions("callVideo"),
            Event::CallDeviceTestChanged {} => self.tell_sessions("deviceTest"),
            // A number only; one over the top is dropped.
            Event::CallSoundLevel { level } => {
                if level > MAX_CALL_SOUND_LEVEL {
                    return;
                }
                for channel in self.sound_level().values() {
                    let _ = channel.send(level);
                }
            }
            // Bounded IDs and levels only; anything else is dropped.
            Event::CallLevels { levels } => {
                if !levels_within_limits(&levels) {
                    return;
                }
                for channel in self.levels().values() {
                    let _ = channel.send(levels.clone());
                }
            }
            Event::RingsChanged {} => self.tell_sessions("rings"),
            Event::ModulesChanged {} => self.tell_sessions("modules"),
            // The UI host's own records follow the switch; the page asks for
            // `diagnostics_status` (ADR-0058).
            Event::DiagnosticsChanged { on } => {
                diagnostics::global().set_on(on);
                self.tell_sessions("diagnostics");
            }
            // Names only; the core bounds them, and so does this check.
            Event::Notification { room, sender, call } => {
                if room.len() > MAX_NOTIFICATION_NAME_BYTES
                    || sender.len() > MAX_NOTIFICATION_NAME_BYTES
                {
                    return;
                }
                let mut toasts = self.toasts();
                match &toasts.show {
                    Some(toaster) => toaster(&room, &sender, call),
                    None => toasts.waiting = Some((room, sender, call)),
                }
            }
        }
    }

    /// Rooms the page is currently subscribed to (for tests).
    pub fn rooms(&self) -> Vec<String> {
        let mut rooms: Vec<String> = self.channels().keys().cloned().collect();
        rooms.sort();
        rooms
    }
}

/// Subscribes the page to changes in a room. The page should fetch the
/// newest page after this returns; a change between the core accepting the
/// subscription and the channel being stored is covered by that fetch.
#[tauri::command]
async fn subscribe_room(
    core: State<'_, CoreLink>,
    subscriptions: State<'_, Arc<Subscriptions>>,
    room_id: String,
    on_change: Channel<()>,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(
        core.inner(),
        Call::Subscribe {
            room_id: room_id.clone(),
        },
    )
    .await?;
    subscriptions.channels().insert(room_id, on_change);
    Ok(())
}

#[tauri::command]
async fn unsubscribe_room(
    core: State<'_, CoreLink>,
    subscriptions: State<'_, Arc<Subscriptions>>,
    room_id: String,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    subscriptions.channels().remove(&room_id);
    relay::<()>(core.inner(), Call::Unsubscribe { room_id }).await
}

/// Registers the page's session channel, its call sound level channel and
/// its channel for the other devices' levels, replacing the earlier ones of
/// the same window.
#[tauri::command]
fn watch_session<R: Runtime>(
    webview: Webview<R>,
    subscriptions: State<'_, Arc<Subscriptions>>,
    on_change: Channel<&'static str>,
    on_sound_level: Channel<u8>,
    on_levels: Channel<Vec<CallParticipantLevel>>,
) {
    let label = webview.label().to_owned();
    subscriptions.session().insert(label.clone(), on_change);
    subscriptions
        .sound_level()
        .insert(label.clone(), on_sound_level);
    subscriptions.levels().insert(label, on_levels);
}

/// Locks the core's session. Replies with the resulting state; `locking`
/// means admitted work is still finishing, and calling again resumes the wait.
#[tauri::command]
async fn lock_session(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::LockSession {}).await
}

/// Asks the core to unlock. The core asks for the passphrase in its own
/// window; the reply is `unlocking` until the result arrives as a session
/// change.
#[tauri::command]
async fn unlock_session(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::UnlockSession {}).await
}

/// Asks the core to reset a profile whose passphrase was forgotten. The core
/// shows its own warning and deletes nothing without a yes there.
#[tauri::command]
async fn reset_profile(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::ResetProfile {}).await
}

/// Starts signing in. Only the homeserver URL crosses: the core asks for the
/// username and password itself, in a native prompt. The core validates the
/// URL; this length check keeps oversized input off the channel.
#[tauri::command]
async fn sign_in(core: State<'_, CoreLink>, homeserver: String) -> Result<AccountStatus, String> {
    if homeserver.len() > MAX_HOMESERVER_URL_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SignIn { homeserver }).await
}

/// Starts signing in through the homeserver's OAuth 2.0 provider (ADR-0030).
/// Only the homeserver URL crosses; the core asks before the provider's page
/// opens in the browser, and the password goes to the provider.
#[tauri::command]
async fn sign_in_with_oidc(
    core: State<'_, CoreLink>,
    homeserver: String,
) -> Result<AccountStatus, String> {
    if homeserver.len() > MAX_HOMESERVER_URL_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SignInWithOidc { homeserver }).await
}

/// Starts registering a new account. Only the homeserver URL and the desired
/// username cross: the core checks the name, then asks for the password and
/// any registration token itself, in native prompts.
#[tauri::command]
async fn register(
    core: State<'_, CoreLink>,
    homeserver: String,
    username: String,
) -> Result<AccountStatus, String> {
    if homeserver.len() > MAX_HOMESERVER_URL_BYTES || username.len() > MAX_USERNAME_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::Register {
            homeserver,
            username,
        },
    )
    .await
}

/// Signs out: the core revokes the session if it can and deletes this
/// device's local profile. Replies with the resulting store state. Without
/// `discard_keys` the core refuses (`keys_not_backed_up`) while this device's
/// room keys are not all in a key backup.
#[tauri::command]
async fn sign_out(core: State<'_, CoreLink>, discard_keys: bool) -> Result<StoreState, String> {
    relay(core.inner(), Call::SignOut { discard_keys }).await
}

/// The account's encryption state. Codes only.
#[tauri::command]
async fn security_status(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::SecurityStatus {}).await
}

/// Whether an audio call can start in this room, and how many other devices
/// are in it. A code and a count only.
#[tauri::command]
async fn call_readiness(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<CallReadiness, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::CallReadiness { room_id }).await
}

/// Joins the call in this room, with the camera on when `video` is true. With
/// a profile proxy the core asks in its own window first; the call never
/// connects around the proxy without that yes. The reply carries codes only; a
/// refusal is a `joinError` in it.
#[tauri::command]
async fn call_join(
    core: State<'_, CoreLink>,
    room_id: String,
    video: bool,
) -> Result<CallStatus, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::CallJoin { room_id, video }).await
}

/// Leaves the call, or cancels a join in progress.
#[tauri::command]
async fn call_leave(core: State<'_, CoreLink>) -> Result<CallStatus, String> {
    relay(core.inner(), Call::CallLeave {}).await
}

/// Mutes or unmutes the microphone. The worker's answer arrives as a call
/// change.
#[tauri::command]
async fn call_mute(core: State<'_, CoreLink>, muted: bool) -> Result<CallStatus, String> {
    relay(core.inner(), Call::CallMute { muted }).await
}

/// Deafens or undeafens the call: remote audio and the microphone go off,
/// or come back (ADR-0055). The worker's answer arrives as a call change.
#[tauri::command]
async fn call_deafen(core: State<'_, CoreLink>, deafened: bool) -> Result<CallStatus, String> {
    relay(core.inner(), Call::CallDeafen { deafened }).await
}

/// Sets the call playout volume, 0 to 100 percent, for the running call
/// and later ones (ADR-0055). The core checks the range again.
#[tauri::command]
async fn call_volume(core: State<'_, CoreLink>, percent: u8) -> Result<CallStatus, String> {
    let call = Call::CallVolume { percent };
    if !call_video_within_limits(&call) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), call).await
}

/// Sets the automatic microphone level and the manual microphone gain, 0 to
/// 400 percent in steps of 5, for the running call and later ones (ADR-0062).
/// The core checks the range again.
#[tauri::command]
async fn call_microphone(
    core: State<'_, CoreLink>,
    auto_level: bool,
    gain: u16,
) -> Result<CallStatus, String> {
    let call = Call::CallMicrophone { auto_level, gain };
    if !call_video_within_limits(&call) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), call).await
}

/// Turns the camera on or off. The worker's answer arrives as a call change.
#[tauri::command]
async fn call_camera(core: State<'_, CoreLink>, on: bool) -> Result<CallStatus, String> {
    relay(core.inner(), Call::CallCamera { on }).await
}

/// Starts or stops the screen share. The page names no screen and no window:
/// the core shows its own warning or chooser, and the desktop's picker
/// (ADR-0053). The worker's answer arrives as a call change.
#[tauri::command]
async fn call_screen(core: State<'_, CoreLink>, on: bool) -> Result<CallStatus, String> {
    relay(core.inner(), Call::CallScreen { on }).await
}

/// The call as the page shows it. Codes, flags and bounded IDs only.
#[tauri::command]
async fn call_status(core: State<'_, CoreLink>) -> Result<CallStatus, String> {
    relay(core.inner(), Call::CallStatus {}).await
}

/// The incoming call rings now running (ADR-0052): room ID, caller's user ID,
/// intent and seconds left. Untrusted text; the page shows it as plain text.
#[tauri::command]
async fn call_rings(core: State<'_, CoreLink>) -> Result<CallRings, String> {
    relay(core.inner(), Call::CallRings {}).await
}

/// Declines the ring in this room (ADR-0052). The core sends the decline
/// once and ends the ring. Answering is `call_join`.
#[tauri::command]
async fn call_decline_ring(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<CallRings, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::CallDeclineRing { room_id }).await
}

/// Starts setting up encryption. Nothing crosses: the core asks for the
/// password and shows the recovery key in its own windows.
#[tauri::command]
async fn set_up_encryption(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::SetUpEncryption {}).await
}

/// Starts verifying this device with the recovery key, which the core asks
/// for in its own window.
#[tauri::command]
async fn recover_with_key(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::RecoverWithKey {}).await
}

/// Queues a text message, as a reply to `reply_to` when it names an event.
/// The core checks the room, body and event ID and decides at send time
/// whether the room needs consent.
#[tauri::command]
async fn send_message(
    core: State<'_, CoreLink>,
    room_id: String,
    body: String,
    reply_to: Option<String>,
) -> Result<PendingMessage, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES
        || body.len() > MAX_MESSAGE_BYTES
        || reply_to
            .as_ref()
            .is_some_and(|event_id| event_id.len() > MAX_EVENT_ID_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::SendMessage {
            room_id,
            body,
            reply_to,
        },
    )
    .await
}

/// Queues a new version of one of this account's text messages. The core
/// checks the arguments, and at send time that the message is this
/// account's own text message.
#[tauri::command]
async fn edit_message(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
    body: String,
) -> Result<PendingMessage, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES
        || event_id.len() > MAX_EVENT_ID_BYTES
        || body.len() > MAX_MESSAGE_BYTES
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::EditMessage {
            room_id,
            event_id,
            body,
        },
    )
    .await
}

/// Marks a joined room read up to an event, privately: its fully-read
/// marker and a private read receipt.
#[tauri::command]
async fn mark_read(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || event_id.len() > MAX_EVENT_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::MarkRead { room_id, event_id }).await
}

/// Who can read a joined room's new messages, by member and device.
#[tauri::command]
async fn room_readers(core: State<'_, CoreLink>, room_id: String) -> Result<RoomReaders, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RoomReaders { room_id }).await
}

/// The homeservers with members in a joined room.
#[tauri::command]
async fn room_servers(core: State<'_, CoreLink>, room_id: String) -> Result<RoomServers, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RoomServers { room_id }).await
}

/// Asks the core to open a link from a message. The core checks it and asks
/// the user in its own window first (ADR-0024); `true` when it opened.
#[tauri::command]
async fn open_link(core: State<'_, CoreLink>, url: String) -> Result<bool, String> {
    if url.len() > MAX_LINK_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::OpenLink { url }).await
}

/// Who else is typing in a joined room and, for an event, who else has read
/// up to it.
#[tauri::command]
async fn room_activity(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: Option<String>,
) -> Result<RoomActivity, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES
        || event_id
            .as_ref()
            .is_some_and(|event_id| event_id.len() > MAX_EVENT_ID_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RoomActivity { room_id, event_id }).await
}

/// Tells a joined room this account started or stopped typing; the core
/// refuses while the account's setting is off (ADR-0023).
#[tauri::command]
async fn set_typing(
    core: State<'_, CoreLink>,
    room_id: String,
    typing: bool,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SetTyping { room_id, typing }).await
}

/// Reacts to an event in a joined room with a short key, usually an emoji.
/// The core checks the arguments and the room, as for a message.
#[tauri::command]
async fn react(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
    key: String,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES
        || event_id.len() > MAX_EVENT_ID_BYTES
        || key.len() > MAX_REACTION_KEY_BYTES
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::React {
            room_id,
            event_id,
            key,
        },
    )
    .await
}

/// Deletes (redacts) an event in a joined room, for everyone. The core
/// checks the arguments; the homeserver decides whether this account may.
#[tauri::command]
async fn delete_message(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || event_id.len() > MAX_EVENT_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::DeleteMessage { room_id, event_id }).await
}

/// The room's messages sent from this device in this session.
#[tauri::command]
async fn pending_messages(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<Vec<PendingMessage>, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::PendingMessages { room_id }).await
}

#[tauri::command]
async fn retry_message(core: State<'_, CoreLink>, local_id: u32) -> Result<(), String> {
    relay::<()>(core.inner(), Call::RetryMessage { local_id }).await
}

#[tauri::command]
async fn discard_message(core: State<'_, CoreLink>, local_id: u32) -> Result<(), String> {
    relay::<()>(core.inner(), Call::DiscardMessage { local_id }).await
}

/// Records the user's consent to send unencrypted messages in a room; the
/// page asks first.
#[tauri::command]
async fn allow_unencrypted(core: State<'_, CoreLink>, room_id: String) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::AllowUnencrypted { room_id }).await
}

/// Starts setting up recovery alone (an earlier attempt failed). The core
/// shows the new recovery key in its own window.
#[tauri::command]
async fn set_up_recovery(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::SetUpRecovery {}).await
}

/// Asks the account's other devices to verify this one.
#[tauri::command]
async fn verify_this_device(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::VerifyThisDevice {}).await
}

/// Accepts, confirms or cancels the verification with this flow ID; the core
/// refuses any other flow.
async fn verification_step(
    core: &CoreLink,
    flow_id: String,
    call: fn(String) -> Call,
) -> Result<SecurityStatus, String> {
    if flow_id.len() > MAX_FLOW_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core, call(flow_id)).await
}

#[tauri::command]
async fn accept_verification(
    core: State<'_, CoreLink>,
    flow_id: String,
) -> Result<SecurityStatus, String> {
    verification_step(core.inner(), flow_id, |flow_id| Call::AcceptVerification {
        flow_id,
    })
    .await
}

#[tauri::command]
async fn confirm_verification(
    core: State<'_, CoreLink>,
    flow_id: String,
) -> Result<SecurityStatus, String> {
    verification_step(core.inner(), flow_id, |flow_id| Call::ConfirmVerification {
        flow_id,
    })
    .await
}

#[tauri::command]
async fn cancel_verification(
    core: State<'_, CoreLink>,
    flow_id: String,
) -> Result<SecurityStatus, String> {
    verification_step(core.inner(), flow_id, |flow_id| Call::CancelVerification {
        flow_id,
    })
    .await
}

/// The other person in a direct-message room and how far the account trusts
/// them.
#[tauri::command]
async fn contact_status(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<ContactStatus, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::ContactStatus { room_id }).await
}

/// Asks the other person in this direct-message room to verify each other.
/// Only the room crosses: the core finds the person itself.
#[tauri::command]
async fn verify_contact(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<SecurityStatus, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::VerifyContact { room_id }).await
}

/// Asks a member of this joined room to verify each other. The room and the
/// member cross; the core checks the membership and uses an existing
/// direct-message room with just them, never a new one.
#[tauri::command]
async fn verify_member(
    core: State<'_, CoreLink>,
    room_id: String,
    user_id: String,
) -> Result<SecurityStatus, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || user_id.len() > MAX_USER_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::VerifyMember { room_id, user_id }).await
}

/// The members of a room whose identity changed (ADR-0007).
#[tauri::command]
async fn identity_changes(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<RoomIdentityChanges, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::IdentityChanges { room_id }).await
}

/// Asks the core to accept a room member's new identity. The core confirms
/// in its own window; this command cannot accept anything by itself.
#[tauri::command]
async fn accept_identity(
    core: State<'_, CoreLink>,
    room_id: String,
    user_id: String,
) -> Result<SecurityStatus, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || user_id.len() > MAX_USER_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::AcceptIdentity { room_id, user_id }).await
}

/// True for an image handle in its only form: exactly 32 lowercase hex
/// characters. The core checks it again.
fn valid_handle(handle: &str) -> bool {
    handle.len() == IMAGE_HANDLE_BYTES
        && handle
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Downloads, decrypts and decodes the image of an `m.image` event in the
/// core's contained worker, only when the user asks. Replies with an image
/// view: an opaque handle, the shown size and the band count.
#[tauri::command]
async fn load_image(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
) -> Result<ImageView, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || event_id.len() > MAX_EVENT_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::LoadImage { room_id, event_id }).await
}

/// Downloads and decodes the current picture of a joined room in the core's
/// contained worker (ADR-0041). The page names only the room; no URL crosses
/// this command. Replies with the picture's ID and an image view.
#[tauri::command]
async fn load_room_picture(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<RoomPicture, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::LoadRoomPicture { room_id }).await
}

/// Most `image_band` calls the page may have running at once. This bounds
/// only the concurrent core `image_band` calls and the bands (each at most
/// `MAX_BAND_BYTES`, 1 MiB) they hold while they run. A slot is given back
/// when the command returns its `Response`; a body that Tauri has not yet
/// delivered to the web view is not counted.
pub const MAX_BANDS_IN_FLIGHT: usize = 4;

/// Most `call_video_frame` calls the page may have running at once: one for
/// each video tile (five camera tiles and the screen tile). They are counted
/// apart from the image bands, so a call does not starve an image and an
/// image does not starve a call. Each holds at most one picture while it
/// runs: 900 KiB for a camera tile, 3.5 MiB for the screen tile.
pub const MAX_VIDEO_IN_FLIGHT: usize = CALL_VIDEO_TILES as usize;

/// Counts running `image_band` calls, up to the return of their `Response`.
#[derive(Default)]
struct BandsInFlight(AtomicUsize);

/// Counts running `call_video_frame` calls, up to the return of their
/// `Response`.
#[derive(Default)]
struct VideoInFlight(AtomicUsize);

/// A slot in [`BandsInFlight`] or [`VideoInFlight`], given back when dropped.
struct BandSlot<'a>(&'a AtomicUsize);

/// Takes one slot of `limit` in `running`, or none when all are taken.
fn take_slot(running: &AtomicUsize, limit: usize) -> Option<BandSlot<'_>> {
    let mut now = running.load(Ordering::Acquire);
    while now < limit {
        match running.compare_exchange_weak(now, now + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(BandSlot(running)),
            Err(seen) => now = seen,
        }
    }
    None
}

impl BandsInFlight {
    fn take(&self) -> Option<BandSlot<'_>> {
        take_slot(&self.0, MAX_BANDS_IN_FLIGHT)
    }
}

impl VideoInFlight {
    fn take(&self) -> Option<BandSlot<'_>> {
        take_slot(&self.0, MAX_VIDEO_IN_FLIGHT)
    }
}

impl Drop for BandSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Length of the prefix before a band's pixels in the raw response.
pub const BAND_PREFIX_BYTES: usize = 8;

/// The raw response for one band: width and row count as little-endian
/// `u32`, then the RGBA bytes.
fn band_response(band: Band) -> Vec<u8> {
    let mut body = Vec::with_capacity(BAND_PREFIX_BYTES + band.pixels.len());
    body.extend_from_slice(&band.width.to_le_bytes());
    body.extend_from_slice(&band.rows.to_le_bytes());
    body.extend_from_slice(&band.pixels);
    body
}

/// Length of the prefix before a video picture's pixels in the raw response.
pub const VIDEO_PREFIX_BYTES: usize = 20;

/// The raw response for one video picture: the picture number as a
/// little-endian `u64`, then the rotation, width and height as little-endian
/// `u32`, then the RGBA bytes. No newer picture is an empty body.
fn video_response(video: Option<Video>) -> Vec<u8> {
    let Some(video) = video else {
        return Vec::new();
    };
    let mut body = Vec::with_capacity(VIDEO_PREFIX_BYTES + video.pixels.len());
    body.extend_from_slice(&video.seq.to_le_bytes());
    body.extend_from_slice(&u32::from(video.rotation).to_le_bytes());
    body.extend_from_slice(&video.width.to_le_bytes());
    body.extend_from_slice(&video.height.to_le_bytes());
    body.extend_from_slice(&video.pixels);
    body
}

/// The newest picture of a video tile if it is newer than `after` and of the
/// tile's stream `stream`, as a raw binary response (see the module
/// documentation for its layout). The body is empty when there is none.
#[tauri::command]
async fn call_video_frame(
    core: State<'_, CoreLink>,
    video: State<'_, VideoInFlight>,
    tile: u8,
    after: u64,
    stream: u64,
) -> Result<Response, String> {
    if !call_video_within_limits(&Call::CallVideoFrame {
        tile,
        after,
        stream,
    }) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    // Held until this returns, like an image band's slot.
    let Some(_slot) = video.take() else {
        return Err(LIMIT_EXCEEDED.to_string());
    };
    let core = core.inner().clone();
    let picture =
        tauri::async_runtime::spawn_blocking(move || core.video_frame(tile, after, stream))
            .await
            .map_err(|_| UNAVAILABLE.to_string())?
            .map_err(page_error)?;
    Ok(Response::new(video_response(picture)))
}

/// One band of rows of a loaded or picked image, as a raw binary response
/// (see the module documentation for its layout).
#[tauri::command]
async fn image_band(
    core: State<'_, CoreLink>,
    bands: State<'_, BandsInFlight>,
    handle: String,
    band: u32,
) -> Result<Response, String> {
    if !valid_handle(&handle) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    // Held until this returns: the `Response` that Tauri then delivers to
    // the page is no longer counted.
    let Some(_slot) = bands.take() else {
        return Err(LIMIT_EXCEEDED.to_string());
    };
    let core = core.inner().clone();
    let band = tauri::async_runtime::spawn_blocking(move || core.image_band(handle, band))
        .await
        .map_err(|_| UNAVAILABLE.to_string())?
        .map_err(page_error)?;
    Ok(Response::new(band_response(band)))
}

/// Drops a loaded image and its pixels in the core.
#[tauri::command]
async fn release_image(core: State<'_, CoreLink>, handle: String) -> Result<(), String> {
    if !valid_handle(&handle) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::ReleaseImage { handle }).await
}

/// Asks the user for a PNG or JPEG file in the core's own file dialog. No
/// path crosses: the reply is a draft (a stripped copy and its preview) or
/// `cancelled`.
#[tauri::command]
async fn pick_image(core: State<'_, CoreLink>, room_id: String) -> Result<ImagePick, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::PickImage { room_id }).await
}

/// The header that names the room of a pasted image; the body is the image.
pub const PASTE_ROOM_HEADER: &str = "persephone-room-id";

/// One pasted image at a time on its way to the core, so a page cannot make
/// this process hold many copies.
#[derive(Default)]
struct PasteInFlight(AtomicBool);

/// The slot in [`PasteInFlight`], given back when dropped.
struct PasteSlot<'a>(&'a AtomicBool);

impl PasteInFlight {
    fn take(&self) -> Option<PasteSlot<'_>> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| PasteSlot(&self.0))
    }
}

impl Drop for PasteSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Attaches an image the user pasted (ADR-0042). The page sends the image as
/// the raw request body, from 1 byte to `MAX_PASTED_IMAGE_BYTES`, and the
/// room ID in the `persephone-room-id` header. The core treats the bytes as
/// hostile, like a picked file: the guest strips and decodes them. The reply
/// is a draft, like `pick_image`. A JSON body is refused.
#[tauri::command]
async fn paste_image(
    core: State<'_, CoreLink>,
    pastes: State<'_, PasteInFlight>,
    request: tauri::ipc::Request<'_>,
) -> Result<ImagePick, String> {
    let room_id = request
        .headers()
        .get(PASTE_ROOM_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|room_id| !room_id.is_empty() && room_id.len() <= MAX_ROOM_ID_BYTES)
        .ok_or_else(|| INVALID_ARGUMENT.to_string())?
        .to_owned();
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err(INVALID_ARGUMENT.to_string());
    };
    if bytes.is_empty() || bytes.len() > MAX_PASTED_IMAGE_BYTES as usize {
        return Err(INVALID_ARGUMENT.to_string());
    }
    let Some(_slot) = pastes.take() else {
        return Err(LIMIT_EXCEEDED.to_string());
    };
    // Tauri keeps the body until this returns; the copy is bounded by the
    // limit above and dropped once it is written to the core.
    let bytes = bytes.clone();
    let core = core.inner().clone();
    let value = tauri::async_runtime::spawn_blocking(move || core.paste_image(room_id, bytes))
        .await
        .map_err(|_| UNAVAILABLE.to_string())?
        .map_err(page_error)?;
    serde_json::from_value(value).map_err(|_| page_error(CoreError::BadReply))
}

/// Asks the user for a file to send in the core's own dialog and queues it
/// (ADR-0029); no path crosses. The reply is the queued message, or null
/// when the user cancelled.
#[tauri::command]
async fn pick_file(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<Option<PendingMessage>, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::PickFile { room_id }).await
}

/// Saves a received file where the user chooses in the core's own dialog
/// (ADR-0029); `true` when saved, `false` when cancelled.
#[tauri::command]
async fn save_file(
    core: State<'_, CoreLink>,
    room_id: String,
    event_id: String,
) -> Result<bool, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || event_id.len() > MAX_EVENT_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SaveFile { room_id, event_id }).await
}

/// Queues a picked image by its draft handle, with the same checks as text.
#[tauri::command]
async fn send_image(
    core: State<'_, CoreLink>,
    room_id: String,
    handle: String,
) -> Result<PendingMessage, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || !valid_handle(&handle) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SendImage { room_id, handle }).await
}

/// Drops a picked image that was not sent.
#[tauri::command]
async fn discard_image(core: State<'_, CoreLink>, handle: String) -> Result<(), String> {
    if !valid_handle(&handle) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::DiscardImage { handle }).await
}

/// Room invitations the account has not answered.
#[tauri::command]
async fn invite_list(core: State<'_, CoreLink>) -> Result<InviteList, String> {
    relay(core.inner(), Call::InviteList {}).await
}

/// Joins the room of an invitation.
#[tauri::command]
async fn accept_invite(core: State<'_, CoreLink>, room_id: String) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::AcceptInvite { room_id }).await
}

/// Refuses an invitation.
#[tauri::command]
async fn decline_invite(core: State<'_, CoreLink>, room_id: String) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::DeclineInvite { room_id }).await
}

/// Opens or creates an encrypted direct-message conversation with a user.
#[tauri::command]
async fn start_conversation(
    core: State<'_, CoreLink>,
    user_id: String,
) -> Result<ConversationStart, String> {
    if user_id.len() > MAX_USER_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::StartConversation { user_id }).await
}

/// Searches the homeserver's user directory. The page calls this only while
/// directory search is turned on.
#[tauri::command]
async fn search_users(core: State<'_, CoreLink>, term: String) -> Result<UserSearch, String> {
    if term.len() > MAX_SEARCH_TERM_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SearchUsers { term }).await
}

/// One page of a public room directory: this account's homeserver's when
/// `server` is none, otherwise the named server's. The core checks each
/// part again.
#[tauri::command]
async fn public_rooms(
    core: State<'_, CoreLink>,
    server: Option<String>,
    filter: Option<String>,
    since: Option<String>,
) -> Result<PublicRoomPage, String> {
    if server
        .as_ref()
        .is_some_and(|server| server.len() > MAX_SERVER_NAME_BYTES)
        || filter
            .as_ref()
            .is_some_and(|filter| filter.len() > MAX_DIRECTORY_FILTER_BYTES)
        || since
            .as_ref()
            .is_some_and(|since| since.len() > MAX_DIRECTORY_TOKEN_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::PublicRooms {
            server,
            filter,
            since,
        },
    )
    .await
}

/// Joins a room a directory page listed as open to anyone. The core refuses
/// any other room.
#[tauri::command]
async fn join_public_room(
    core: State<'_, CoreLink>,
    room_id: String,
) -> Result<JoinedRoom, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::JoinPublicRoom { room_id }).await
}

/// Creates an encrypted room with a name and invites people to it.
#[tauri::command]
async fn create_room(
    core: State<'_, CoreLink>,
    name: String,
    invite: Vec<String>,
) -> Result<RoomCreation, String> {
    if name.len() > MAX_ROOM_NAME_BYTES
        || invite.len() > MAX_INVITEES
        || invite
            .iter()
            .any(|user_id| user_id.len() > MAX_USER_ID_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::CreateRoom { name, invite }).await
}

/// Invites one person to a joined room.
#[tauri::command]
async fn invite_user(
    core: State<'_, CoreLink>,
    room_id: String,
    user_id: String,
) -> Result<RoomInvite, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || user_id.len() > MAX_USER_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::InviteUser { room_id, user_id }).await
}

/// A joined room's settings, with what this account may change.
#[tauri::command]
async fn room_settings(core: State<'_, CoreLink>, room_id: String) -> Result<RoomSettings, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RoomSettings { room_id }).await
}

/// A joined room's members whose ID or name contains `filter`.
#[tauri::command]
async fn room_members(
    core: State<'_, CoreLink>,
    room_id: String,
    filter: String,
) -> Result<RoomMembers, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || filter.len() > MAX_MEMBER_FILTER_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RoomMembers { room_id, filter }).await
}

/// Changes one setting of a joined room, or acts on one member. The change
/// is a closed set of kinds; the core validates its content.
#[tauri::command]
async fn change_room(
    core: State<'_, CoreLink>,
    room_id: String,
    change: RoomChange,
) -> Result<(), String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || !room_change_within_limits(&change) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::ChangeRoom { room_id, change }).await
}

/// This account's display name.
#[tauri::command]
async fn display_name(core: State<'_, CoreLink>) -> Result<DisplayName, String> {
    relay(core.inner(), Call::DisplayName {}).await
}

/// Sets this account's display name; an empty one removes it.
#[tauri::command]
async fn set_display_name(core: State<'_, CoreLink>, name: String) -> Result<DisplayName, String> {
    if name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SetDisplayName { name }).await
}

/// The devices (sessions) signed in to this account.
#[tauri::command]
async fn device_list(core: State<'_, CoreLink>) -> Result<DeviceList, String> {
    relay(core.inner(), Call::DeviceList {}).await
}

/// This account's settings, from its encrypted profile.
#[tauri::command]
async fn account_settings(
    core: State<'_, CoreLink>,
    capture: State<'_, Arc<capture::CaptureState>>,
) -> Result<AccountSettings, String> {
    let session = capture.session();
    let settings: AccountSettings = relay(core.inner(), Call::AccountSettings {}).await?;
    capture.set_for(session, settings.capture_protection);
    Ok(settings)
}

/// Replaces this account's settings. The record refuses missing and unknown
/// settings; the core checks the text sizes' bounds.
#[tauri::command]
async fn set_account_settings(
    core: State<'_, CoreLink>,
    capture: State<'_, Arc<capture::CaptureState>>,
    settings: AccountSettings,
) -> Result<(), String> {
    let protect = settings.capture_protection;
    let session = capture.session();
    relay::<()>(core.inner(), Call::SetAccountSettings { settings }).await?;
    capture.set_for(session, protect);
    Ok(())
}

/// Changes this account's password. Nothing secret crosses: the core asks
/// for the current and new passwords in its own windows (ADR-0019).
#[tauri::command]
async fn change_password(
    core: State<'_, CoreLink>,
    sign_out_others: bool,
) -> Result<AccountChange, String> {
    relay(core.inner(), Call::ChangePassword { sign_out_others }).await
}

/// The profile's SOCKS5 proxy (ADR-0028).
#[tauri::command]
async fn proxy_setting(core: State<'_, CoreLink>) -> Result<ProxySetting, String> {
    relay(core.inner(), Call::ProxySetting {}).await
}

/// Sets or removes the profile's SOCKS5 proxy; the core checks the address.
#[tauri::command]
async fn set_proxy_setting(core: State<'_, CoreLink>, proxy: Option<String>) -> Result<(), String> {
    if proxy
        .as_ref()
        .is_some_and(|proxy| proxy.len() > MAX_PROXY_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SetProxySetting { proxy }).await
}

/// Whether the diagnostic log is on, and its file (ADR-0058).
#[tauri::command]
async fn diagnostics_status(core: State<'_, CoreLink>) -> Result<DiagnosticsStatus, String> {
    relay(core.inner(), Call::DiagnosticsStatus {}).await
}

/// Turns the diagnostic log on (after the core's own question) or off; the
/// reply is whether it is on.
#[tauri::command]
async fn set_diagnostics(core: State<'_, CoreLink>, on: bool) -> Result<bool, String> {
    relay(core.inner(), Call::SetDiagnostics { on }).await
}

/// The page's records for the diagnostic log. They always count as the
/// page's, and are checked against the protocol's limits here and in the
/// core.
#[tauri::command]
async fn diagnostic_records(
    core: State<'_, CoreLink>,
    records: Vec<DiagnosticRecord>,
) -> Result<(), String> {
    // The encoded size too: escaping can take records within their own
    // limits over the control frame, and an oversized frame would break the
    // core link for every caller.
    if !DiagnosticRecord::batch_fits(&records) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::DiagnosticRecords {
            source: RecordSource::Page,
            records,
        },
    )
    .await
}

/// The call microphones and speakers now connected, and the saved choice
/// (ADR-0043). The core lists them through a contained worker.
#[tauri::command]
async fn call_devices(core: State<'_, CoreLink>) -> Result<CallDevices, String> {
    relay(core.inner(), Call::CallDevices {}).await
}

/// Starts a test of one call device (ADR-0059); null is the system default.
/// The core checks the ID again.
#[tauri::command]
async fn call_device_test(
    core: State<'_, CoreLink>,
    test: CallDeviceTestKind,
    device: Option<String>,
) -> Result<CallDeviceTestStatus, String> {
    let call = Call::CallDeviceTest { test, device };
    if !call_video_within_limits(&call) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), call).await
}

/// Stops the device test that runs (ADR-0059): with `id`, only that test.
#[tauri::command]
async fn call_device_test_stop(core: State<'_, CoreLink>, id: Option<u64>) -> Result<(), String> {
    relay(core.inner(), Call::CallDeviceTestStop { id }).await
}

/// The newest device test (ADR-0059).
#[tauri::command]
async fn call_device_test_status(
    core: State<'_, CoreLink>,
) -> Result<CallDeviceTestStatus, String> {
    relay(core.inner(), Call::CallDeviceTestStatus {}).await
}

/// Opens the call window (ADR-0060), or brings it to the front. It shows
/// the bundled call page only. A window that cannot be shown is an error,
/// because the main window then shows the call's controls itself. Focus is
/// only a request, as is unminimise: the desktop may refuse either while the
/// window shows.
#[tauri::command]
async fn open_call_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(CALL_WINDOW) {
        let _ = window.unminimize();
        window.show().map_err(|error| {
            eprintln!("persephone-ui: cannot show the call window: {error}");
            UNAVAILABLE.to_string()
        })?;
        let _ = window.set_focus();
        return Ok(());
    }
    create_call_window(&app)
        .map(drop)
        .map_err(|_| UNAVAILABLE.to_string())
}

/// Closes the call window, if it is open (ADR-0060).
#[tauri::command]
async fn close_call_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(CALL_WINDOW) {
        window.destroy().map_err(|_| UNAVAILABLE.to_string())?;
    }
    Ok(())
}

/// Saves the call microphone, speaker and camera as device IDs; null is the
/// system default. The core checks the IDs again.
#[tauri::command]
async fn set_call_devices(
    core: State<'_, CoreLink>,
    recording: Option<String>,
    playout: Option<String>,
    camera: Option<String>,
) -> Result<(), String> {
    let call = Call::SetCallDevices {
        recording,
        playout,
        camera,
    };
    let Call::SetCallDevices {
        recording,
        playout,
        camera,
    } = &call
    else {
        return Err(INVALID_ARGUMENT.to_string());
    };
    if [recording, playout, camera]
        .into_iter()
        .flatten()
        .any(|id| id.is_empty() || id.len() > MAX_CALL_DEVICE_ID_BYTES)
        || !call_video_within_limits(&call)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), call).await
}

/// Finds the homeserver of a server name through its `.well-known` file
/// and confirms it speaks Matrix (ADR-0027).
#[tauri::command]
async fn discover_homeserver(
    core: State<'_, CoreLink>,
    server_name: String,
) -> Result<HomeserverFound, String> {
    if server_name.len() > MAX_SERVER_NAME_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::DiscoverHomeserver { server_name }).await
}

/// The sessions of this account the user has not been shown yet (ADR-0026).
#[tauri::command]
async fn new_sessions(core: State<'_, CoreLink>) -> Result<NewSessions, String> {
    relay(core.inner(), Call::NewSessions {}).await
}

/// Records that the user has seen these sessions.
#[tauri::command]
async fn acknowledge_sessions(
    core: State<'_, CoreLink>,
    device_ids: Vec<String>,
) -> Result<(), String> {
    if device_ids.len() > MAX_NEW_SESSION_IDS
        || device_ids.iter().any(|id| id.len() > MAX_DEVICE_ID_BYTES)
    {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::AcknowledgeSessions { device_ids }).await
}

/// The code loaded into Persephone that the system and Persephone did not
/// ship. The core scans; the page only reads the report.
#[tauri::command]
async fn third_party_modules(
    core: State<'_, CoreLink>,
) -> Result<persephone_contracts::ThirdPartyModules, String> {
    personal_relay(core.inner(), Call::ThirdPartyModules {}).await
}

/// The display server that draws this window, for the capture note on
/// Linux. The host answers it: the core has no window.
#[tauri::command]
async fn display_session() -> persephone_contracts::DisplaySession {
    capture::display_session()
}

/// Marks these modules as reviewed. The core ignores unknown ids; the
/// bounds are checked here before the relay and again in the core.
#[tauri::command]
async fn acknowledge_modules(
    core: State<'_, CoreLink>,
    ids: Vec<String>,
) -> Result<persephone_contracts::ThirdPartyModules, String> {
    personal_relay(core.inner(), Call::AcknowledgeModules { ids }).await
}

/// Signs out another session of this account. Nothing secret crosses: the
/// core asks for the password in its own window (ADR-0026).
#[tauri::command]
async fn remove_device(
    core: State<'_, CoreLink>,
    device_id: String,
) -> Result<AccountChange, String> {
    if device_id.len() > MAX_DEVICE_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RemoveDevice { device_id }).await
}

/// This account's email addresses and phone numbers (ADR-0020).
#[tauri::command]
async fn three_pids(core: State<'_, CoreLink>) -> Result<ThreePidList, String> {
    relay(core.inner(), Call::ThreePids {}).await
}

/// Asks the homeserver to send a confirmation link to this email address.
#[tauri::command]
async fn request_email_token(
    core: State<'_, CoreLink>,
    email: String,
) -> Result<AccountChange, String> {
    if email.len() > MAX_EMAIL_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RequestEmailToken { email }).await
}

/// Asks the homeserver to send a code to this phone number.
#[tauri::command]
async fn request_phone_token(
    core: State<'_, CoreLink>,
    country: String,
    phone_number: String,
) -> Result<AccountChange, String> {
    if country.len() != COUNTRY_CODE_BYTES || phone_number.len() > MAX_PHONE_NUMBER_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(
        core.inner(),
        Call::RequestPhoneToken {
            country,
            phone_number,
        },
    )
    .await
}

/// Adds the confirmed address; the core asks for a phone's code and the
/// password in its own windows.
#[tauri::command]
async fn add_three_pid(core: State<'_, CoreLink>) -> Result<AccountChange, String> {
    relay(core.inner(), Call::AddThreePid {}).await
}

/// Removes an email address or phone number from this account.
#[tauri::command]
async fn remove_three_pid(
    core: State<'_, CoreLink>,
    medium: ThreePidMedium,
    address: String,
) -> Result<AccountChange, String> {
    if address.len() > MAX_THREE_PID_ADDRESS_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::RemoveThreePid { medium, address }).await
}

/// Asks another of this account's devices to verify each other.
#[tauri::command]
async fn verify_device(
    core: State<'_, CoreLink>,
    device_id: String,
) -> Result<SecurityStatus, String> {
    if device_id.is_empty() || device_id.len() > MAX_DEVICE_ID_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::VerifyDevice { device_id }).await
}

/// Replaces the recovery key. Nothing crosses: the core warns, shows the
/// new key and saves it in its own windows (ADR-0021).
#[tauri::command]
async fn change_recovery_key(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::ChangeRecoveryKey {}).await
}

/// Turns the local profile passphrase on or off. Only the choice crosses: the
/// core asks for the passphrase in its own window (ADR-0050).
#[tauri::command]
async fn set_profile_passphrase(
    core: State<'_, CoreLink>,
    on: bool,
) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::SetProfilePassphrase { on }).await
}

/// Deletes a key backup this device cannot use and starts a new one. Nothing
/// crosses: the core asks, shows the new key and saves it in its own windows
/// (ADR-0040).
#[tauri::command]
async fn start_new_backup(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::StartNewBackup {}).await
}

/// This account's notification level, from its push rules (ADR-0022).
#[tauri::command]
async fn notification_level(core: State<'_, CoreLink>) -> Result<NotificationSettings, String> {
    relay(core.inner(), Call::NotificationLevel {}).await
}

/// Sets this account's notification level in its push rules.
#[tauri::command]
async fn set_notification_level(
    core: State<'_, CoreLink>,
    level: NotificationLevel,
) -> Result<(), String> {
    if level == NotificationLevel::Custom {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::SetNotificationLevel { level }).await
}

/// The local profiles on this computer, by slot number only.
#[tauri::command]
async fn profile_list(core: State<'_, CoreLink>) -> Result<ProfileList, String> {
    relay(core.inner(), Call::ProfileList {}).await
}

/// Makes a new profile and switches to it: the core then closes this
/// window and starts again.
#[tauri::command]
async fn add_profile(core: State<'_, CoreLink>) -> Result<(), String> {
    relay::<()>(core.inner(), Call::AddProfile {}).await
}

/// Switches to the profile in this slot, as `add_profile` does.
#[tauri::command]
async fn switch_profile(core: State<'_, CoreLink>, slot: u32) -> Result<(), String> {
    if !(1..=MAX_PROFILES).contains(&slot) {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay::<()>(core.inner(), Call::SwitchProfile { slot }).await
}

/// Quits Persephone from the main menu. The UI ends as when its window
/// closes, and the core, which waits for it, then ends too. Nothing goes to
/// the core first: a clean UI exit is how the core learns of it.
#[tauri::command]
async fn exit_app<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

/// The ID of the tray icon that brings the app back.
const TRAY_ID: &str = "persephone";
/// The tray menu item that shows the windows again.
const TRAY_SHOW: &str = "show";

/// Hides the app's windows and shows a tray icon; a click on the icon or
/// its "Show Persephone" item brings them back. Where no icon can be made,
/// the windows stay shown and the command fails. On Linux the windows
/// minimise instead of hiding, because a built icon does not prove that a
/// tray host shows it. So the app is never left with no way back. The icon
/// shows only the app's own icon and name.
#[tauri::command]
async fn minimise_to_tray<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    if app.tray_by_id(TRAY_ID).is_none() {
        let icon = app
            .default_window_icon()
            .cloned()
            .ok_or_else(|| UNAVAILABLE.to_string())?;
        // Linux sends no icon clicks (Tauri 2.12.4), so the menu is the way
        // back there; elsewhere a left click also restores.
        let show =
            tauri::menu::MenuItem::with_id(&app, TRAY_SHOW, "Show Persephone", true, None::<&str>)
                .map_err(|_| UNAVAILABLE.to_string())?;
        let menu =
            tauri::menu::Menu::with_items(&app, &[&show]).map_err(|_| UNAVAILABLE.to_string())?;
        tauri::tray::TrayIconBuilder::with_id(TRAY_ID)
            .icon(icon)
            .tooltip("Persephone")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_tray_icon_event(|tray, event| {
                if let tauri::tray::TrayIconEvent::Click {
                    button: tauri::tray::MouseButton::Left,
                    button_state: tauri::tray::MouseButtonState::Up,
                    ..
                } = event
                {
                    restore_from_tray(tray.app_handle());
                }
            })
            .build(&app)
            .map_err(|_| UNAVAILABLE.to_string())?;
    }
    // On Linux a built icon does not prove that a tray host shows it, so
    // the windows minimise and keep their taskbar entry as a way back.
    for label in [CALL_WINDOW, MAIN_WINDOW] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = if cfg!(target_os = "linux") {
                window.minimize()
            } else {
                window.hide()
            };
        }
    }
    Ok(())
}

/// Shows the app's windows again and removes the tray icon.
fn restore_from_tray<R: Runtime>(app: &AppHandle<R>) {
    for label in [CALL_WINDOW, MAIN_WINDOW] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.unminimize();
            let _ = window.show();
        }
    }
    if let Some(main) = app.get_webview_window(MAIN_WINDOW) {
        let _ = main.set_focus();
    }
    let _ = app.remove_tray_by_id(TRAY_ID);
}

/// Drops a window's subscriptions when its page (re)loads or the window
/// goes: the new page holds none of the old channels, and the core's slots
/// would otherwise stay taken. Rooms belong to the main window only.
fn reset_subscriptions(core: &CoreLink, subscriptions: &Subscriptions, label: &str) {
    let rooms = subscriptions.forget_window(label);
    if rooms.is_empty() {
        return;
    }
    let core = core.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for room_id in rooms {
            let _ = core.call(Call::Unsubscribe { room_id });
        }
    });
}

/// True only for the bundled app's own origin. Tauri serves embedded assets
/// from `http://tauri.localhost` on Windows and `tauri://localhost` elsewhere.
pub fn is_app_url(url: &Url) -> bool {
    match (url.scheme(), url.host_str()) {
        ("tauri", Some("localhost")) => url.port().is_none(),
        ("http" | "https", Some("tauri.localhost")) => url.port().is_none(),
        _ => false,
    }
}

/// Applies the shell's core link, command handler and window setup to a
/// builder; used by both the real runtime and the mock runtime in tests.
pub fn configure<R: Runtime>(
    builder: tauri::Builder<R>,
    core: CoreLink,
    subscriptions: Arc<Subscriptions>,
) -> tauri::Builder<R> {
    builder
        .manage(core)
        .manage(subscriptions.capture())
        .manage(subscriptions)
        .manage(BandsInFlight::default())
        .manage(VideoInFlight::default())
        .manage(PasteInFlight::default())
        // One handler for the tray menu, for the life of the app: a handler
        // given to each new tray icon would stay after the icon goes.
        .on_menu_event(|app, event| {
            if event.id().as_ref() == TRAY_SHOW {
                restore_from_tray(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_status,
            room_list,
            room_draft,
            room_drafts,
            save_room_draft,
            room_preferences,
            set_room_preference,
            search_messages,
            jump_to_date,
            event_context,
            timeline_events,
            pinned_messages,
            set_pinned,
            attachments,
            connection_health,
            ignored_users,
            set_ignored_user,
            report_abuse,
            timeline_page,
            subscribe_room,
            unsubscribe_room,
            watch_session,
            lock_session,
            unlock_session,
            reset_profile,
            sign_in,
            sign_in_with_oidc,
            register,
            sign_out,
            security_status,
            call_readiness,
            call_join,
            call_leave,
            call_mute,
            call_deafen,
            call_volume,
            call_microphone,
            call_camera,
            call_screen,
            call_video_frame,
            call_status,
            call_rings,
            call_decline_ring,
            set_up_encryption,
            recover_with_key,
            send_message,
            edit_message,
            delete_message,
            react,
            mark_read,
            room_activity,
            set_typing,
            open_link,
            room_servers,
            room_readers,
            new_sessions,
            discover_homeserver,
            proxy_setting,
            set_proxy_setting,
            diagnostics_status,
            set_diagnostics,
            diagnostic_records,
            call_devices,
            set_call_devices,
            acknowledge_sessions,
            third_party_modules,
            acknowledge_modules,
            display_session,
            remove_device,
            pending_messages,
            retry_message,
            discard_message,
            allow_unencrypted,
            set_up_recovery,
            verify_this_device,
            accept_verification,
            confirm_verification,
            cancel_verification,
            contact_status,
            verify_contact,
            verify_member,
            identity_changes,
            accept_identity,
            load_image,
            load_room_picture,
            image_band,
            release_image,
            pick_image,
            paste_image,
            pick_file,
            save_file,
            send_image,
            discard_image,
            invite_list,
            accept_invite,
            decline_invite,
            start_conversation,
            search_users,
            public_rooms,
            join_public_room,
            create_room,
            invite_user,
            room_settings,
            room_members,
            change_room,
            display_name,
            set_display_name,
            device_list,
            account_settings,
            set_account_settings,
            change_password,
            three_pids,
            request_email_token,
            request_phone_token,
            add_three_pid,
            remove_three_pid,
            verify_device,
            change_recovery_key,
            set_profile_passphrase,
            start_new_backup,
            notification_level,
            set_notification_level,
            profile_list,
            add_profile,
            switch_profile,
            exit_app,
            minimise_to_tray,
            call_device_test,
            call_device_test_stop,
            call_device_test_status,
            open_call_window,
            close_call_window
        ])
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                reset_subscriptions(
                    webview.state::<CoreLink>().inner(),
                    webview.state::<Arc<Subscriptions>>().inner(),
                    webview.label(),
                );
            }
        })
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let window = create_main_window(app.handle())?;
            #[cfg(windows)]
            hide_from_capture(&window, &app.state::<Arc<capture::CaptureState>>());
            #[cfg(not(windows))]
            let _ = window;
            let handle = app.handle().clone();
            app.state::<Arc<Subscriptions>>()
                .set_toaster(Box::new(move |room, sender, call| {
                    toast(&handle, room, sender, call)
                }));
            Ok(())
        })
}

/// Hides a window from capture, and again whenever the wanted state
/// changes (ADR-0045). The call runs on the window's thread, in the order the
/// changes were made.
#[cfg(windows)]
fn hide_from_capture<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    state: &Arc<capture::CaptureState>,
) {
    match window.hwnd() {
        Ok(hwnd) => {
            let hwnd = hwnd.0 as isize;
            let app = window.app_handle().clone();
            state.set_window_applier(
                window.label(),
                Box::new(move |on| {
                    if app
                        .run_on_main_thread(move || capture::apply_to_window(hwnd, on))
                        .is_err()
                    {
                        eprintln!("persephone-ui: cannot hide the window from capture");
                    }
                }),
            );
        }
        Err(_) => eprintln!("persephone-ui: cannot hide the window from capture"),
    }
}

/// Shows one desktop notification (ADR-0022): the room as the title and the
/// sender in the body, never message text, or only that a message arrived
/// when the core sent no names. Skipped while the main window has the focus,
/// since the page shows the message then.
fn toast<R: Runtime>(app: &tauri::AppHandle<R>, room: &str, sender: &str, call: bool) {
    use tauri_plugin_notification::NotificationExt;
    let focused = app
        .get_webview_window(MAIN_WINDOW)
        .and_then(|window| window.is_focused().ok())
        .unwrap_or(false);
    if focused {
        return;
    }
    let (title, body) = if call {
        call_toast_text(sender)
    } else {
        toast_text(room, sender)
    };
    let _ = app.notification().builder().title(title).body(body).show();
}

/// The text of an incoming call notification (ADR-0052): "Incoming call",
/// and the caller's name only when the core sent one. No room, no message,
/// no buttons.
pub fn call_toast_text(sender: &str) -> (String, String) {
    if sender.is_empty() {
        ("Incoming call".to_owned(), "Persephone".to_owned())
    } else {
        ("Incoming call".to_owned(), sender.to_owned())
    }
}

/// A notification's title and body: named, or plain when the core sent no
/// names (the account leaves them out).
pub fn toast_text(room: &str, sender: &str) -> (String, String) {
    if room.is_empty() && sender.is_empty() {
        ("Persephone".to_owned(), "New message".to_owned())
    } else {
        (room.to_owned(), format!("New message from {sender}"))
    }
}

/// Creates the main window: bundled app only, no navigation away from it and
/// no new windows. `setup` calls this; tests call it directly because the mock
/// runtime does not run the event loop that triggers `setup`. When the main
/// window goes, the call window goes too, so the app ends.
pub fn create_main_window<R: Runtime, M: Manager<R>>(
    manager: &M,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    let window = main_window(manager).build()?;
    let app = window.app_handle().clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed)
            && let Some(call) = app.get_webview_window(CALL_WINDOW)
        {
            let _ = call.destroy();
        }
    });
    Ok(window)
}

/// Creates the call window (ADR-0060) with the same rules as the main
/// window: the bundled call page only, no navigation away and no new
/// windows. Its capture protection follows the main window's, and its
/// subscriptions go when it closes.
pub fn create_call_window<R: Runtime, M: Manager<R>>(
    manager: &M,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    let window = call_window(manager).build()?;
    #[cfg(windows)]
    hide_from_capture(&window, &window.state::<Arc<capture::CaptureState>>());
    let app = window.app_handle().clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            app.state::<Arc<Subscriptions>>()
                .capture()
                .remove_window_applier(CALL_WINDOW);
            reset_subscriptions(
                app.state::<CoreLink>().inner(),
                app.state::<Arc<Subscriptions>>().inner(),
                CALL_WINDOW,
            );
        }
    });
    Ok(window)
}

fn call_window<R: Runtime, M: Manager<R>>(manager: &M) -> WebviewWindowBuilder<'_, R, M> {
    WebviewWindowBuilder::new(manager, CALL_WINDOW, WebviewUrl::App("call.html".into()))
        .title("Persephone call")
        .inner_size(960.0, 640.0)
        .min_inner_size(480.0, 360.0)
        .on_navigation(is_app_url)
        .on_new_window(|_, _| NewWindowResponse::Deny)
}

fn main_window<R: Runtime, M: Manager<R>>(manager: &M) -> WebviewWindowBuilder<'_, R, M> {
    WebviewWindowBuilder::new(manager, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title("Persephone")
        .inner_size(1024.0, 720.0)
        .min_inner_size(640.0, 480.0)
        .on_navigation(is_app_url)
        .on_new_window(|_, _| NewWindowResponse::Deny)
}

/// The compiled configuration, embedded assets and permission set.
pub fn context<R: Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

/// Runs the UI when the core started it. Started any other way, it becomes a
/// launcher for the core ([`launcher`]) before any Tauri or channel state
/// exists, because without the core channel the UI has nothing to relay to.
pub fn run() {
    // First, before any Tauri, window or web view state exists.
    #[cfg(windows)]
    if let Err(error) = harden::block_injected_dlls() {
        eprintln!("persephone-ui: cannot block injected code: {error}");
        std::process::exit(2);
    }
    if launcher::mode(std::env::args_os(), STARTED_BY_CORE) == launcher::Mode::Launcher {
        launcher::run();
    }
    let subscriptions = Arc::new(Subscriptions::default());
    let sink = {
        let subscriptions = subscriptions.clone();
        Box::new(move |event| subscriptions.dispatch(event))
    };
    let core = match core_link::PipeTransport::from_stdio(sink) {
        Ok(link) => Arc::new(link) as CoreLink,
        Err(error) => {
            eprintln!("persephone-ui: no core channel: {error}");
            std::process::exit(2);
        }
    };
    // Off until the core says the diagnostic log is on (ADR-0058).
    diagnostics::install(core.clone());
    let app = configure(tauri::Builder::default(), core, subscriptions)
        .build(context())
        .expect("Persephone UI failed to start");
    // The display GTK opened for the window, for the capture note on Linux
    // (ADR-0047). `run` is on the GTK main thread, and the window exists once
    // the app is built.
    #[cfg(target_os = "linux")]
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        capture::record_display_session(capture::read_display_session(&window));
    }
    app.run(|_, _| {});
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN `MAX_BANDS_IN_FLIGHT` running band calls WHEN another starts
    /// THEN it gets no slot; WHEN exactly one running call ends THEN exactly
    /// one new call gets a slot.
    #[test]
    fn given_bands_in_flight_then_only_a_bounded_number_run_at_once() {
        let bands = BandsInFlight::default();
        let mut slots: Vec<_> = (0..MAX_BANDS_IN_FLIGHT)
            .map(|_| bands.take().expect("a free slot"))
            .collect();
        assert!(bands.take().is_none());
        drop(slots.pop());
        let freed = bands.take();
        assert!(freed.is_some());
        assert!(bands.take().is_none());
        drop(freed);
        drop(slots);
    }

    /// GIVEN `MAX_VIDEO_IN_FLIGHT` running video calls WHEN another starts
    /// THEN it gets no slot, and image bands keep their own slots.
    #[test]
    fn given_video_in_flight_then_only_one_per_tile_runs_and_bands_are_apart() {
        assert_eq!(MAX_VIDEO_IN_FLIGHT, 6);
        let video = VideoInFlight::default();
        let bands = BandsInFlight::default();
        let slots: Vec<_> = (0..MAX_VIDEO_IN_FLIGHT)
            .map(|_| video.take().expect("a free slot"))
            .collect();
        assert!(video.take().is_none());
        assert!(bands.take().is_some());
        drop(slots);
        assert!(video.take().is_some());
    }

    #[test]
    fn given_a_video_picture_then_its_response_has_the_documented_layout() {
        assert!(video_response(None).is_empty());
        let body = video_response(Some(Video {
            tile: 1,
            seq: 0x0102_0304_0506_0708,
            rotation: 3,
            width: 2,
            height: 1,
            pixels: vec![7; 8],
        }));
        assert_eq!(VIDEO_PREFIX_BYTES, 20);
        assert_eq!(&body[..8], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&body[8..20], &[3, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&body[20..], &[7; 8]);
    }

    #[test]
    fn given_image_handles_then_only_32_lowercase_hex_characters_pass() {
        assert!(valid_handle("0123456789abcdef0123456789abcdef"));
        for bad in [
            "",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcde ",
            "0123456789abcdef0123456789abc\u{e9}",
        ] {
            assert!(!valid_handle(bad), "{bad:?}");
        }
    }

    #[test]
    fn given_a_band_then_its_response_is_width_rows_and_pixels() {
        let body = band_response(Band {
            width: 0x0102_0304,
            rows: 2,
            pixels: vec![7; 8],
        });
        assert_eq!(&body[..8], &[4, 3, 2, 1, 2, 0, 0, 0]);
        assert_eq!(&body[8..], &[7; 8]);
    }

    #[test]
    fn given_core_errors_then_each_page_error_is_a_fixed_code() {
        for (error, page) in [
            (CoreError::Core(ErrorCode::Rejected), "rejected"),
            (CoreError::Core(ErrorCode::LimitExceeded), "limit_exceeded"),
            (CoreError::Core(ErrorCode::NotFound), "not_found"),
            (CoreError::Core(ErrorCode::Locked), "locked"),
            (
                CoreError::Core(ErrorCode::InvalidArgument),
                "invalid_argument",
            ),
            (CoreError::Core(ErrorCode::Unavailable), "core_unavailable"),
            (CoreError::Core(ErrorCode::Internal), "core_unavailable"),
            (CoreError::Unavailable, "core_unavailable"),
            (CoreError::BadReply, "core_unavailable"),
        ] {
            assert_eq!(page_error(error), page);
        }
    }

    #[test]
    fn given_urls_then_only_the_bundled_app_origin_is_navigable() {
        for allowed in [
            "http://tauri.localhost/index.html",
            "https://tauri.localhost/",
            "tauri://localhost/index.html",
        ] {
            assert!(is_app_url(&allowed.parse().unwrap()), "{allowed}");
        }
        for denied in [
            "https://example.org/",
            "http://tauri.localhost.example.org/",
            "http://tauri.localhost:8080/",
            "http://localhost/",
            "file:///C:/Windows/win.ini",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "tauri://example.org/",
            "ipc://localhost/app_status",
        ] {
            assert!(!is_app_url(&denied.parse().unwrap()), "{denied}");
        }
    }
}
