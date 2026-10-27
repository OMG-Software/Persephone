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
//! Images (ADR-0009) cross as opaque handles. The page fetches a loaded or
//! picked image's pixels one band at a time with `image_band`, which answers
//! with a raw binary IPC response (never JSON numbers or base64): an 8-byte
//! prefix holding the band's width and then its row count, each a
//! little-endian `u32`, followed by exactly `width * rows * 4` RGBA bytes.

pub mod core_link;
pub mod launcher;

use core_link::{Band, CoreError, CoreLink};
use persephone_contracts::{
    AccountSettings, AccountStatus, AppStatus, ContactStatus, ConversationStart, DeviceList,
    DisplayName, ImagePick, ImageView, InviteList, PendingMessage, ProfileList, RoomChange,
    RoomCreation, RoomIdentityChanges, RoomInvite, RoomList, RoomMembers, RoomSettings,
    SecurityStatus, StoreState, TimelinePage, UserSearch,
};
use persephone_core_protocol::{
    Call, ErrorCode, Event, IMAGE_HANDLE_BYTES, MAX_CURSOR_BYTES, MAX_DISPLAY_NAME_BYTES,
    MAX_EVENT_ID_BYTES, MAX_FLOW_ID_BYTES, MAX_HOMESERVER_URL_BYTES, MAX_INVITEES,
    MAX_MEMBER_FILTER_BYTES, MAX_MESSAGE_BYTES, MAX_PROFILES, MAX_ROOM_ID_BYTES,
    MAX_ROOM_NAME_BYTES, MAX_SEARCH_TERM_BYTES, MAX_USER_ID_BYTES, room_change_within_limits,
};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
};
use tauri::{
    Manager, Runtime, State, Url, WebviewUrl, WebviewWindowBuilder,
    ipc::{Channel, Response},
    webview::{NewWindowResponse, PageLoadEvent},
};

/// The label the `main` capability is bound to.
pub const MAIN_WINDOW: &str = "main";

/// Argument the core passes when it starts the UI.
pub const STARTED_BY_CORE: &str = "--started-by-core";

/// Error strings the web view may see. Anything else stays in Rust.
const UNAVAILABLE: &str = "core_unavailable";
const INVALID_ARGUMENT: &str = "invalid_argument";
const NOT_FOUND: &str = "not_found";
const LIMIT_EXCEEDED: &str = "limit_exceeded";
const LOCKED: &str = "locked";
const REJECTED: &str = "rejected";

fn page_error(error: CoreError) -> String {
    match error {
        CoreError::Core(ErrorCode::InvalidArgument) => INVALID_ARGUMENT,
        CoreError::Core(ErrorCode::NotFound) => NOT_FOUND,
        CoreError::Core(ErrorCode::LimitExceeded) => LIMIT_EXCEEDED,
        CoreError::Core(ErrorCode::Locked) => LOCKED,
        CoreError::Core(ErrorCode::Rejected) => REJECTED,
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

/// The page's channel for each room it subscribed to, and its one session
/// channel. The core accepts at most `MAX_SUBSCRIPTIONS` rooms, and a room
/// channel is kept only after the core accepted the room, so this map is
/// bounded the same way.
#[derive(Default)]
pub struct Subscriptions {
    channels: Mutex<HashMap<String, Channel<()>>>,
    session: Mutex<Option<Channel<&'static str>>>,
}

impl Subscriptions {
    fn channels(&self) -> MutexGuard<'_, HashMap<String, Channel<()>>> {
        self.channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session(&self) -> MutexGuard<'_, Option<Channel<&'static str>>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
                self.channels().clear();
                if let Some(channel) = self.session().as_ref() {
                    let _ = channel.send("session");
                }
            }
            Event::RoomListChanged {} => {
                if let Some(channel) = self.session().as_ref() {
                    let _ = channel.send("rooms");
                }
            }
            Event::SecurityChanged {} => {
                if let Some(channel) = self.session().as_ref() {
                    let _ = channel.send("security");
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

/// Registers the page's session channel, replacing any earlier one.
#[tauri::command]
fn watch_session(subscriptions: State<'_, Arc<Subscriptions>>, on_change: Channel<&'static str>) {
    *subscriptions.session() = Some(on_change);
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

/// Signs out: the core revokes the session if it can and deletes this
/// device's local profile. Replies with the resulting store state.
#[tauri::command]
async fn sign_out(core: State<'_, CoreLink>) -> Result<StoreState, String> {
    relay(core.inner(), Call::SignOut {}).await
}

/// The account's encryption state. Codes only.
#[tauri::command]
async fn security_status(core: State<'_, CoreLink>) -> Result<SecurityStatus, String> {
    relay(core.inner(), Call::SecurityStatus {}).await
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

/// Queues a text message. The core checks the room and body and decides at
/// send time whether the room needs consent.
#[tauri::command]
async fn send_message(
    core: State<'_, CoreLink>,
    room_id: String,
    body: String,
) -> Result<PendingMessage, String> {
    if room_id.len() > MAX_ROOM_ID_BYTES || body.len() > MAX_MESSAGE_BYTES {
        return Err(INVALID_ARGUMENT.to_string());
    }
    relay(core.inner(), Call::SendMessage { room_id, body }).await
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

/// Most `image_band` calls the page may have running at once. This bounds
/// only the concurrent core `image_band` calls and the bands (each at most
/// `MAX_BAND_BYTES`, 1 MiB) they hold while they run. A slot is given back
/// when the command returns its `Response`; a body that Tauri has not yet
/// delivered to the web view is not counted.
pub const MAX_BANDS_IN_FLIGHT: usize = 4;

/// Counts running `image_band` calls, up to the return of their `Response`.
#[derive(Default)]
struct BandsInFlight(AtomicUsize);

/// A slot in [`BandsInFlight`], given back when dropped.
struct BandSlot<'a>(&'a AtomicUsize);

impl BandsInFlight {
    fn take(&self) -> Option<BandSlot<'_>> {
        let mut running = self.0.load(Ordering::Acquire);
        while running < MAX_BANDS_IN_FLIGHT {
            match self.0.compare_exchange_weak(
                running,
                running + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(BandSlot(&self.0)),
                Err(now) => running = now,
            }
        }
        None
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
async fn account_settings(core: State<'_, CoreLink>) -> Result<AccountSettings, String> {
    relay(core.inner(), Call::AccountSettings {}).await
}

/// Replaces this account's settings. Each setting is a required argument.
#[tauri::command]
async fn set_account_settings(core: State<'_, CoreLink>, enter_sends: bool) -> Result<(), String> {
    relay::<()>(core.inner(), Call::SetAccountSettings { enter_sends }).await
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

/// Drops every subscription when the page (re)loads: the new page holds none
/// of the old channels, and the core's slots would otherwise stay taken.
fn reset_subscriptions(core: &CoreLink, subscriptions: &Subscriptions) {
    subscriptions.session().take();
    let rooms: Vec<String> = subscriptions
        .channels()
        .drain()
        .map(|(room, _)| room)
        .collect();
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
        .manage(subscriptions)
        .manage(BandsInFlight::default())
        .invoke_handler(tauri::generate_handler![
            app_status,
            room_list,
            timeline_page,
            subscribe_room,
            unsubscribe_room,
            watch_session,
            lock_session,
            unlock_session,
            reset_profile,
            sign_in,
            sign_out,
            security_status,
            set_up_encryption,
            recover_with_key,
            send_message,
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
            identity_changes,
            accept_identity,
            load_image,
            image_band,
            release_image,
            pick_image,
            send_image,
            discard_image,
            invite_list,
            accept_invite,
            decline_invite,
            start_conversation,
            search_users,
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
            profile_list,
            add_profile,
            switch_profile
        ])
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                reset_subscriptions(
                    webview.state::<CoreLink>().inner(),
                    webview.state::<Arc<Subscriptions>>().inner(),
                );
            }
        })
        .setup(|app| {
            create_main_window(app.handle())?;
            Ok(())
        })
}

/// Creates the main window: bundled app only, no navigation away from it and
/// no new windows. `setup` calls this; tests call it directly because the mock
/// runtime does not run the event loop that triggers `setup`.
pub fn create_main_window<R: Runtime, M: Manager<R>>(
    manager: &M,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    main_window(manager).build()
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
    configure(tauri::Builder::default(), core, subscriptions)
        .run(context())
        .expect("Persephone UI failed to start");
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
