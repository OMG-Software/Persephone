//! Persephone core (ADR-0004).
//!
//! The core is the application's entry process. It owns the session, the
//! Matrix SDK client, the stores and their keys; it will own the media broker.
//! The UI process holds none of these; it sends one bounded request frame per
//! call, and the core answers with one bounded reply frame. The core also
//! pushes change notifications for rooms the UI subscribed to.
//!
//! The UI is treated as untrusted. Every request is read with the budgeted
//! frame reader, so an oversized or malformed header is refused before its body
//! is allocated. Anything a correct UI never sends (a bad frame, an unknown
//! command or field) ends the session instead of being answered.
//!
//! Notifications carry no content and coalesce: a room that changes many times
//! before the UI reads has one pending notification. Pending state is
//! therefore bounded by the number of subscriptions, whatever the UI's speed.
//!
//! Views come from one of four sources: synthetic fixtures when the core runs
//! with `--demo` (an unconfigured session); nothing while the session is
//! locked or its profile is unavailable; an empty list while signed out; and
//! the signed-in Matrix client otherwise.
//!
//! Most calls are answered in turn by the request loop. Two can take long and
//! run on their own thread, so the loop keeps serving: `load_image` (download
//! and decode) and `pick_image` (the file dialog). Each is admitted first, up
//! to the limits in `media.rs`, and refused with `limit_exceeded` beyond
//! them; its reply is written when it finishes, correlated by request ID.
//! `image_band` is answered with a pixel reply and its pixel frame, written
//! together under the output lock. Serving ends only after such threads
//! finish, including an open file dialog.

pub mod account;
pub mod account_changes;
pub mod account_details;
pub mod activity;
pub mod contacts;
pub mod conversations;
pub mod credentials;
pub mod discovery;
pub mod files;
pub mod homeserver;
pub mod identities;
pub mod links;
pub mod matrix_views;
pub mod media;
pub mod notifications;
pub mod outbox;
pub mod passphrase;
pub mod pick;
pub mod platform;
pub mod profiles;
pub mod prompt_text;
pub mod proxy;
pub mod recovery_file;
pub mod registration;
pub mod room_settings;
pub mod security;
pub mod sending;
pub mod session;
pub mod sessions;
pub mod settings;
pub mod store;
pub mod ui_host;
pub mod vault;
pub mod verification;
pub mod views;

use account::KeyRooms;
use credentials::CredentialSource;
use homeserver::Homeserver;
use matrix_views::MatrixViews;
use media::{ImageWorker, MediaService};
use persephone_contracts::{AccountStatus, AppStatus, ImagePick, ImageView, RoomList, StoreState};
use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, MAX_EVENT_ID_BYTES, MAX_HOMESERVER_URL_BYTES,
    MAX_MESSAGE_BYTES, MAX_ROOM_ID_BYTES, MAX_SUBSCRIPTIONS, MAX_USER_ID_BYTES, Outcome,
    PixelReply, ReceiveError, Reply, Request, receive, send, send_pixel_reply,
};
use persephone_media_protocol::ResourceBudgets;
use pick::ImagePicker;
use serde::Serialize;
use session::{Session, SessionListener};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub const MILESTONE: &str = "P1";

/// The commit this core was built from (`build.rs`).
pub const REVISION: &str = env!("PERSEPHONE_REVISION");

/// Longest a `lock_session` call waits for admitted work before replying
/// `locking`; the UI may call again to resume the wait. Covers one sync round.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the idle lock compares the computer's idle time with the
/// account's setting (ADR-0025).
pub const IDLE_CHECK: Duration = Duration::from_secs(15);

/// Stack for a thread that answers a long call: the SDK's request futures
/// need more than the default in debug builds, as for the sync thread.
const LONG_CALL_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Why the core stopped serving a UI connection.
#[derive(Debug, PartialEq, Eq)]
pub enum ServeEnd {
    /// The UI closed its end, normally because it exited.
    UiClosed,
    /// The UI sent something a correct UI never sends.
    ProtocolViolation,
    /// A reply or event could not be written.
    WriteFailed,
}

#[derive(Default)]
struct Notifications {
    subscribed: BTreeSet<String>,
    /// Subscribed rooms changed since their last notification.
    pending: BTreeSet<String>,
    /// The session changed state since the last notification.
    session_changed: bool,
    /// The room list changed since the last notification.
    room_list_changed: bool,
    /// The account's encryption state changed since the last notification.
    security_changed: bool,
    /// The latest desktop notification waiting to be sent (ADR-0022): room
    /// and sender names. A newer one replaces it.
    toast: Option<(String, String)>,
    /// The UI sent a valid request on this connection, so its page runs.
    ready: bool,
    closed: bool,
}

/// Pending notifications, shared by the request loop, the notification
/// writer and the session (which reports sync changes through it).
#[derive(Default)]
struct Hub {
    notifications: Mutex<Notifications>,
    changed: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock means a panic mid-update; the core stops either way.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Hub {
    /// Drops every subscription and pending room event, and queues a
    /// `session_changed` event, so the UI clears what it shows.
    fn announce_session_change(&self) {
        let mut notifications = lock(&self.notifications);
        notifications.subscribed.clear();
        notifications.pending.clear();
        notifications.room_list_changed = false;
        notifications.toast = None;
        notifications.session_changed = true;
        self.changed.notify_all();
    }

    /// Queues events for the subscribed rooms among `rooms`, and one room-list
    /// event when `list` is set (a sync round can add rooms or change names
    /// and counts; a synthetic demo message cannot).
    fn mark_changed(&self, rooms: &[String], list: bool) {
        self.mark_list_changed(rooms, list && !rooms.is_empty());
    }

    fn mark_list_changed(&self, rooms: &[String], list: bool) {
        let mut notifications = lock(&self.notifications);
        let mut any = false;
        if list && !notifications.room_list_changed {
            notifications.room_list_changed = true;
            any = true;
        }
        for room in rooms {
            if notifications.subscribed.contains(room) {
                any |= notifications.pending.insert(room.clone());
            }
        }
        if any {
            self.changed.notify_all();
        }
    }
}

impl SessionListener for Hub {
    fn rooms_changed(&self, rooms: &[String]) {
        self.mark_changed(rooms, true);
    }

    fn session_changed(&self) {
        self.announce_session_change();
    }

    fn direct_rooms_changed(&self) {
        self.mark_list_changed(&[], true);
    }

    fn outbox_changed(&self, room_id: &str) {
        self.mark_changed(&[room_id.to_owned()], false);
    }

    /// Walks the subscriptions (at most `MAX_SUBSCRIPTIONS`), so `All` costs
    /// no more than a listed batch.
    fn room_keys_arrived(&self, rooms: &KeyRooms) {
        let mut notifications = lock(&self.notifications);
        let notifications = &mut *notifications;
        let mut any = false;
        for room in &notifications.subscribed {
            if rooms.includes(room) {
                any |= notifications.pending.insert(room.clone());
            }
        }
        if any {
            self.changed.notify_all();
        }
    }

    fn security_changed(&self) {
        let mut notifications = lock(&self.notifications);
        if !notifications.security_changed {
            notifications.security_changed = true;
            self.changed.notify_all();
        }
    }

    fn notify(&self, room: &str, sender: &str) {
        let mut notifications = lock(&self.notifications);
        notifications.toast = Some((room.to_owned(), sender.to_owned()));
        self.changed.notify_all();
    }
}

enum Source {
    Fixtures,
    Locked,
    SignedOut,
    Matrix(session::Lease, matrix_sdk::Client),
}

/// Core state shared by the request loop and the notification writer. A
/// `Core` serves one UI connection at a time; once [`serve`] returns it stays
/// closed until [`Core::reopen`] prepares it for a new UI.
pub struct Core {
    session: Session,
    credentials: Arc<dyn CredentialSource>,
    picker: Arc<dyn ImagePicker>,
    fixtures: Mutex<views::Fixtures>,
    matrix: MatrixViews,
    hub: Arc<Hub>,
    /// Shared by the frame reader and the images, under one aggregate limit.
    budgets: Arc<ResourceBudgets>,
    media: MediaService,
    /// Starting a conversation or answering an invitation, one at a time.
    conversation: conversations::Busy,
    /// One user-directory search at a time.
    search: conversations::Busy,
    read_marker: conversations::Busy,
    /// One homeserver discovery at a time.
    discovery: conversations::Busy,
    /// One link question at a time.
    link: conversations::Busy,
    /// Reading or setting the display name, one at a time.
    display_name: conversations::Busy,
    /// One device-list request at a time.
    devices: conversations::Busy,
    /// Reading a room's settings, listing its members, and changing it, each
    /// one at a time.
    room_settings: conversations::Busy,
    room_members: conversations::Busy,
    room_change: conversations::Busy,
    /// Changing the password or an email address or phone number, one at a
    /// time; listing them; reading or setting the notification level.
    account_change: conversations::Busy,
    three_pid_list: conversations::Busy,
    notification_level: conversations::Busy,
    /// The email address or phone number waiting to be added, with the
    /// session generation it was asked for in (ADR-0020).
    three_pid: Mutex<Option<(u64, account_changes::Pending)>>,
    /// The local profiles beside the open one; `None` for synthetic data or
    /// a profile without a directory.
    profiles: Option<profiles::Profiles>,
    /// Long calls (images, homeserver calls) running on their own threads.
    /// A profile switch waits for none: it is refused while one runs, and
    /// none starts once a switch is waiting, so the old core exits without
    /// waiting on a request that could take until its timeout.
    long_calls: AtomicUsize,
}

/// Counts one running long call until it is dropped, with its thread or with
/// a call that never started.
struct Running<'a>(&'a AtomicUsize);

impl<'a> Running<'a> {
    fn start(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A call that runs on its own thread; it returns the reply's outcome.
type LongCall<'a> = Box<dyn FnOnce() -> Outcome + Send + 'a>;

/// How a request is answered.
enum Answer<'a> {
    Reply(Reply),
    /// A pixel reply and the band it announces.
    Pixels(PixelReply, media::Band),
    /// Admitted; the reply follows when the call finishes.
    Later(u32, LongCall<'a>),
}

impl Default for Core {
    fn default() -> Self {
        Self::new(views::Fixtures::default())
    }
}

fn ok<T: Serialize>(value: &T) -> Outcome {
    serde_json::to_value(value).map_or(Outcome::Error(ErrorCode::Internal), Outcome::Ok)
}

/// Like [`ok`], for a reply that must fit one bounded view. The outbox's
/// byte budget keeps pending messages within it; this refuses anything
/// larger rather than ending the connection with a frame that is too big.
fn bounded<T: Serialize>(value: &T) -> Outcome {
    match serde_json::to_vec(value) {
        Ok(bytes) if bytes.len() <= views::MAX_PAGE_BYTES => ok(value),
        Ok(_) => Outcome::Error(ErrorCode::LimitExceeded),
        Err(_) => Outcome::Error(ErrorCode::Internal),
    }
}

/// An event ID the SDK accepts.
fn valid_event_id(event_id: &str) -> bool {
    event_id.len() <= MAX_EVENT_ID_BYTES && matrix_sdk::ruma::EventId::parse(event_id).is_ok()
}

/// A reaction key: some visible text, within the limit, and no control
/// characters.
fn valid_reaction_key(key: &str) -> bool {
    key.len() <= persephone_core_protocol::MAX_REACTION_KEY_BYTES
        && !key.trim().is_empty()
        && !key.chars().any(char::is_control)
}

/// Whether the idle lock should lock now: it is on (`minutes` above 0), the
/// profile is open, and the computer's idle time is known and at least that
/// long. An unknown idle time never locks; the page says the lock is
/// unavailable then (ADR-0025).
fn idle_lock_due(minutes: u16, state: StoreState, idle: Option<Duration>) -> bool {
    minutes > 0
        && state == StoreState::Protected
        && idle.is_some_and(|idle| idle >= Duration::from_secs(u64::from(minutes) * 60))
}

/// The event ID, when the SDK accepts it.
fn parse_event_id(event_id: &str) -> Option<matrix_sdk::ruma::OwnedEventId> {
    if event_id.len() > MAX_EVENT_ID_BYTES {
        return None;
    }
    matrix_sdk::ruma::OwnedEventId::try_from(event_id).ok()
}

/// A long call's result as the reply's JSON value.
fn json<T: Serialize>(result: Result<T, ErrorCode>) -> Result<serde_json::Value, ErrorCode> {
    result.and_then(|value| serde_json::to_value(value).map_err(|_| ErrorCode::Internal))
}

fn refused<T: Serialize>(result: Result<T, ErrorCode>) -> Outcome {
    match result {
        Ok(value) => ok(&value),
        Err(code) => Outcome::Error(code),
    }
}

/// A room ID the SDK accepts, so nothing queued or consented to can fail to
/// parse later.
fn valid_room_id(room_id: &str) -> bool {
    room_id.len() <= MAX_ROOM_ID_BYTES && matrix_sdk::ruma::RoomId::parse(room_id).is_ok()
}

/// A message body: some visible text, within the limit, and no control
/// characters except line breaks and tabs.
fn valid_body(body: &str) -> bool {
    body.len() <= MAX_MESSAGE_BYTES
        && !body.trim().is_empty()
        && !body
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

fn empty_rooms() -> RoomList {
    RoomList {
        rooms: Vec::new(),
        truncated: false,
    }
}

impl Core {
    pub fn new(fixtures: views::Fixtures) -> Self {
        let budgets = Arc::new(ResourceBudgets::default());
        Self {
            session: Session::not_configured(),
            credentials: Arc::from(credentials::platform()),
            picker: Arc::from(pick::platform()),
            fixtures: Mutex::new(fixtures),
            matrix: MatrixViews::default(),
            hub: Arc::default(),
            media: MediaService::new(budgets.clone(), media::platform_worker()),
            budgets,
            conversation: conversations::Busy::default(),
            search: conversations::Busy::default(),
            read_marker: conversations::Busy::default(),
            discovery: conversations::Busy::default(),
            link: conversations::Busy::default(),
            display_name: conversations::Busy::default(),
            devices: conversations::Busy::default(),
            room_settings: conversations::Busy::default(),
            room_members: conversations::Busy::default(),
            room_change: conversations::Busy::default(),
            account_change: conversations::Busy::default(),
            three_pid_list: conversations::Busy::default(),
            notification_level: conversations::Busy::default(),
            three_pid: Mutex::new(None),
            profiles: None,
            long_calls: AtomicUsize::new(0),
        }
    }

    /// Gives the core the local profiles beside the open one (ADR-0015).
    pub fn with_profiles(mut self, profiles: profiles::Profiles) -> Self {
        self.profiles = Some(profiles);
        self
    }

    /// The local profiles, if the core has them; a switch the UI asked for
    /// is read from here.
    pub fn profiles(&self) -> Option<&profiles::Profiles> {
        self.profiles.as_ref()
    }

    /// Replaces the native file dialog (tests and development builds).
    pub fn with_picker(mut self, picker: Arc<dyn ImagePicker>) -> Self {
        self.picker = picker;
        self
    }

    /// Replaces the contained media worker (tests).
    pub fn with_image_worker(mut self, worker: Arc<dyn ImageWorker>) -> Self {
        self.media = MediaService::new(self.budgets.clone(), worker);
        self
    }

    /// Gives the core its session owner and restores a saved account.
    pub fn with_session(mut self, session: Session) -> Self {
        session.set_listener(self.hub.clone());
        self.session = session;
        self.session.restore_account();
        self
    }

    /// Replaces the native credential prompt (tests and development builds).
    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = source;
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    fn source(&self) -> Source {
        if !self.session.is_configured() {
            return Source::Fixtures;
        }
        match self.session.with_client() {
            Ok(Some((lease, client))) => Source::Matrix(lease, client),
            Ok(None) => Source::SignedOut,
            Err(_) => Source::Locked,
        }
    }

    fn account(&self) -> AccountStatus {
        if self.session.is_configured() {
            self.session.account_status()
        } else {
            AccountStatus::signed_out()
        }
    }

    /// Answers one request in turn. A long call runs to its end here, and
    /// `image_band`, whose reply is a pixel frame, is `unavailable`: only
    /// [`serve`] answers it.
    pub fn handle(&self, request: &Request) -> Reply {
        Reply {
            id: request.id,
            outcome: self.outcome(&request.call),
        }
    }

    /// How [`serve`] answers one request.
    fn answer(&self, request: &Request) -> Answer<'_> {
        let id = request.id;
        let error = |code| {
            Answer::Reply(Reply {
                id,
                outcome: Outcome::Error(code),
            })
        };
        match &request.call {
            Call::LoadImage { room_id, event_id } => match self.load_image(room_id, event_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::PickImage { room_id } => match self.pick_image(room_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::DiscoverHomeserver { server_name } => match self.discover(server_name) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::PickFile { room_id } => match self.pick_file(room_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::SaveFile { room_id, event_id } => match self.save_file(room_id, event_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::AcceptInvite { .. }
            | Call::DeclineInvite { .. }
            | Call::DeleteMessage { .. }
            | Call::React { .. }
            | Call::MarkRead { .. }
            | Call::SetTyping { .. }
            | Call::OpenLink { .. }
            | Call::StartConversation { .. }
            | Call::SearchUsers { .. }
            | Call::CreateRoom { .. }
            | Call::InviteUser { .. }
            | Call::DisplayName {}
            | Call::SetDisplayName { .. }
            | Call::DeviceList {}
            | Call::NewSessions {}
            | Call::AcknowledgeSessions { .. }
            | Call::RemoveDevice { .. }
            | Call::RoomSettings { .. }
            | Call::RoomMembers { .. }
            | Call::RoomServers { .. }
            | Call::RoomReaders { .. }
            | Call::ChangeRoom { .. }
            | Call::ChangePassword { .. }
            | Call::ThreePids {}
            | Call::RequestEmailToken { .. }
            | Call::RequestPhoneToken { .. }
            | Call::AddThreePid {}
            | Call::RemoveThreePid { .. }
            | Call::NotificationLevel {}
            | Call::SetNotificationLevel { .. } => match self.homeserver_call(&request.call) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::ImageBand { handle, band } => {
                match self.media.band(self.media_generation(), handle, *band) {
                    Ok(band) => Answer::Pixels(
                        PixelReply {
                            id,
                            width: band.width,
                            rows: band.rows,
                            band: band.index,
                        },
                        band,
                    ),
                    Err(code) => error(code),
                }
            }
            call => Answer::Reply(Reply {
                id,
                outcome: self.outcome(call),
            }),
        }
    }

    /// Admits a long call and counts it while it runs; `None` once a profile
    /// switch is waiting, because the window is about to close.
    fn admit_long_call(&self) -> Option<Running<'_>> {
        let switching = self
            .profiles
            .as_ref()
            .is_some_and(|profiles| profiles.requested().is_some());
        (!switching).then(|| Running::start(&self.long_calls))
    }

    /// Asks for a profile switch, unless a long call is still running.
    fn switch(
        &self,
        request: impl FnOnce(&profiles::Profiles) -> Result<(), ErrorCode>,
    ) -> Outcome {
        match &self.profiles {
            None => Outcome::Error(ErrorCode::NotFound),
            Some(_) if self.long_calls.load(Ordering::SeqCst) > 0 => {
                Outcome::Error(ErrorCode::LimitExceeded)
            }
            Some(profiles) => refused(request(profiles)),
        }
    }

    fn outcome(&self, call: &Call) -> Outcome {
        match call {
            Call::AppStatus {} => ok(&AppStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                revision: REVISION.into(),
                milestone: MILESTONE.into(),
                store: self.session.state(),
                account: self.account(),
            }),
            Call::RoomList {} => match self.source() {
                Source::Fixtures => ok(&lock(&self.fixtures).room_list()),
                Source::Locked => Outcome::Error(ErrorCode::Locked),
                Source::SignedOut => ok(&empty_rooms()),
                Source::Matrix(_lease, client) => ok(&MatrixViews::room_list(&client)),
            },
            Call::TimelinePage { room_id, cursor } => {
                let page = match self.source() {
                    Source::Fixtures => {
                        lock(&self.fixtures).timeline_page(room_id, cursor.as_deref())
                    }
                    Source::Locked => Err(ErrorCode::Locked),
                    Source::SignedOut => Err(ErrorCode::NotFound),
                    Source::Matrix(lease, client) => lease.stores().block_on(
                        self.matrix
                            .timeline_page(&client, room_id, cursor.as_deref()),
                    ),
                };
                match page {
                    Ok(page) => ok(&page),
                    Err(code) => Outcome::Error(code),
                }
            }
            Call::Subscribe { room_id } => self.subscribe(room_id),
            Call::Unsubscribe { room_id } => {
                let mut notifications = lock(&self.hub.notifications);
                notifications.subscribed.remove(room_id);
                notifications.pending.remove(room_id);
                Outcome::Ok(serde_json::Value::Null)
            }
            Call::LockSession {} => {
                let (state, _drain) = self.session.lock(LOCK_TIMEOUT, || self.announce_lock());
                ok(&state)
            }
            Call::UnlockSession {} => {
                let before = self.session.state();
                let state = self.session.unlock();
                if state != before {
                    self.hub.announce_session_change();
                }
                ok(&state)
            }
            Call::ResetProfile {} => {
                let before = self.session.state();
                let state = self.session.reset_profile();
                if state != before {
                    self.hub.announce_session_change();
                }
                ok(&state)
            }
            Call::SignIn { homeserver } => self.sign_in(homeserver),
            Call::Register {
                homeserver,
                username,
            } => self.register(homeserver, username),
            Call::SecurityStatus {} => ok(&self.session.security_status()),
            Call::SetUpEncryption {} => {
                ok(&self.session.set_up_encryption(self.credentials.clone()))
            }
            Call::RecoverWithKey {} => ok(&self.session.recover_with_key(self.credentials.clone())),
            Call::SendMessage {
                room_id,
                body,
                reply_to,
            } => {
                let relation = match reply_to.as_deref().map(parse_event_id) {
                    None => None,
                    Some(Some(event_id)) => Some(outbox::Relation::Reply(event_id)),
                    Some(None) => return Outcome::Error(ErrorCode::InvalidArgument),
                };
                self.send_message(room_id, body, relation)
            }
            Call::EditMessage {
                room_id,
                event_id,
                body,
            } => match parse_event_id(event_id) {
                Some(event_id) => {
                    self.send_message(room_id, body, Some(outbox::Relation::Replace(event_id)))
                }
                None => Outcome::Error(ErrorCode::InvalidArgument),
            },
            Call::PendingMessages { room_id } => {
                if valid_room_id(room_id) {
                    bounded(&self.session.pending_messages(room_id))
                } else {
                    Outcome::Error(ErrorCode::InvalidArgument)
                }
            }
            Call::RetryMessage { local_id } => self.changed(self.session.retry_message(*local_id)),
            Call::DiscardMessage { local_id } => {
                self.changed(self.session.discard_message(*local_id))
            }
            Call::AllowUnencrypted { room_id } => {
                if !valid_room_id(room_id) {
                    Outcome::Error(ErrorCode::InvalidArgument)
                } else {
                    match self.session.allow_unencrypted(room_id) {
                        Ok(true) => Outcome::Ok(serde_json::Value::Null),
                        Ok(false) => Outcome::Error(ErrorCode::LimitExceeded),
                        Err(_) => Outcome::Error(ErrorCode::Locked),
                    }
                }
            }
            Call::ProxySetting {} => match self.proxy_setting() {
                Ok(socks) => ok(&persephone_contracts::ProxySetting {
                    proxy: socks.map(|socks| socks.address()),
                }),
                Err(code) => Outcome::Error(code),
            },
            Call::SetProxySetting { proxy } => self.set_proxy_setting(proxy.as_deref()),
            Call::AccountSettings {} => match self.session.account_settings() {
                Ok(Some(settings)) => ok(&settings),
                Ok(None) => Outcome::Error(ErrorCode::Internal),
                Err(_) => Outcome::Error(ErrorCode::Locked),
            },
            Call::SetAccountSettings { settings } => {
                let settings = *settings;
                if !settings.within_bounds() {
                    return Outcome::Error(ErrorCode::InvalidArgument);
                }
                match self.session.set_account_settings(settings) {
                    Ok(true) => Outcome::Ok(serde_json::Value::Null),
                    Ok(false) => Outcome::Error(ErrorCode::Internal),
                    Err(_) => Outcome::Error(ErrorCode::Locked),
                }
            }
            Call::SetUpRecovery {} => ok(&self.session.set_up_recovery(self.credentials.clone())),
            Call::ChangeRecoveryKey {} => {
                ok(&self.session.change_recovery_key(self.credentials.clone()))
            }
            Call::VerifyDevice { device_id } => {
                if verification::valid_device_id(device_id) {
                    ok(&self
                        .session
                        .verify(verification::Action::StartDevice(device_id.clone())))
                } else {
                    Outcome::Error(ErrorCode::InvalidArgument)
                }
            }
            Call::VerifyThisDevice {} => ok(&self.session.verify(verification::Action::Start)),
            Call::AcceptVerification { flow_id } => {
                self.verify(flow_id, verification::Action::Accept)
            }
            Call::ConfirmVerification { flow_id } => {
                self.verify(flow_id, verification::Action::Confirm)
            }
            Call::CancelVerification { flow_id } => {
                self.verify(flow_id, verification::Action::Cancel)
            }
            Call::ContactStatus { room_id } => self.contact_status(room_id),
            Call::RoomActivity { room_id, event_id } => {
                self.room_activity(room_id, event_id.as_deref())
            }
            Call::IdentityChanges { room_id } => self.identity_changes(room_id),
            Call::AcceptIdentity { room_id, user_id } => self.accept_identity(room_id, user_id),
            Call::VerifyContact { room_id } => {
                if valid_room_id(room_id) {
                    ok(&self
                        .session
                        .verify(verification::Action::StartContact(room_id.clone())))
                } else {
                    Outcome::Error(ErrorCode::InvalidArgument)
                }
            }
            Call::SignOut {} => {
                match self.session.sign_out(LOCK_TIMEOUT, || self.announce_lock()) {
                    Ok(state) => ok(&state),
                    // Too much work in progress: nothing was signed out.
                    Err(_) => Outcome::Error(ErrorCode::LimitExceeded),
                }
            }
            Call::LoadImage { room_id, event_id } => match self.load_image(room_id, event_id) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::PickImage { room_id } => match self.pick_image(room_id) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::DiscoverHomeserver { server_name } => match self.discover(server_name) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::PickFile { room_id } => match self.pick_file(room_id) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::SaveFile { room_id, event_id } => match self.save_file(room_id, event_id) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::ImageBand { .. } => Outcome::Error(ErrorCode::Unavailable),
            Call::ReleaseImage { handle } => {
                refused(self.media.release(self.media_generation(), handle))
            }
            Call::DiscardImage { handle } => {
                refused(self.media.discard(self.media_generation(), handle))
            }
            Call::SendImage { room_id, handle } => self.send_image(room_id, handle),
            Call::InviteList {} => self.invite_list(),
            Call::AcceptInvite { .. }
            | Call::DeclineInvite { .. }
            | Call::DeleteMessage { .. }
            | Call::React { .. }
            | Call::MarkRead { .. }
            | Call::SetTyping { .. }
            | Call::OpenLink { .. }
            | Call::StartConversation { .. }
            | Call::SearchUsers { .. }
            | Call::CreateRoom { .. }
            | Call::InviteUser { .. }
            | Call::DisplayName {}
            | Call::SetDisplayName { .. }
            | Call::DeviceList {}
            | Call::NewSessions {}
            | Call::AcknowledgeSessions { .. }
            | Call::RemoveDevice { .. }
            | Call::RoomSettings { .. }
            | Call::RoomMembers { .. }
            | Call::RoomServers { .. }
            | Call::RoomReaders { .. }
            | Call::ChangeRoom { .. }
            | Call::ChangePassword { .. }
            | Call::ThreePids {}
            | Call::RequestEmailToken { .. }
            | Call::RequestPhoneToken { .. }
            | Call::AddThreePid {}
            | Call::RemoveThreePid { .. }
            | Call::NotificationLevel {}
            | Call::SetNotificationLevel { .. } => match self.homeserver_call(call) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::ProfileList {} => ok(&self.profiles.as_ref().map_or(
                persephone_contracts::ProfileList {
                    profiles: Vec::new(),
                    can_add: false,
                },
                profiles::Profiles::list,
            )),
            Call::AddProfile {} => self.switch(|profiles| profiles.request_new().map(|_| ())),
            Call::SwitchProfile { slot } => self.switch(|profiles| profiles.request_switch(*slot)),
        }
    }

    fn invite_list(&self) -> Outcome {
        match self.source() {
            Source::Locked => Outcome::Error(ErrorCode::Locked),
            // Synthetic data and a signed-out session have no invitations.
            Source::Fixtures | Source::SignedOut => ok(&persephone_contracts::InviteList {
                invites: Vec::new(),
                truncated: false,
            }),
            Source::Matrix(lease, client) => {
                bounded(&lease.stores().block_on(conversations::invite_list(&client)))
            }
        }
    }

    /// Admits a call that asks the homeserver: its arguments are checked
    /// first, then it takes its kind's slot (one at a time) and runs later
    /// under a lease of the session admitted now. A lock, unlock or sign-in
    /// in between makes it `locked`: it never runs under another session or
    /// account. A change to the rooms also announces a room-list change.
    fn homeserver_call<'a>(&'a self, call: &Call) -> Result<LongCall<'a>, ErrorCode> {
        use matrix_sdk::ruma::OwnedUserId;
        enum Work {
            Accept(String),
            Decline(String),
            Delete(String, matrix_sdk::ruma::OwnedEventId),
            React(String, matrix_sdk::ruma::OwnedEventId, String),
            MarkRead(String, matrix_sdk::ruma::OwnedEventId),
            Typing(String, bool),
            OpenLink(url::Url),
            Start(OwnedUserId),
            Search(String),
            CreateRoom(String, Vec<OwnedUserId>),
            Invite(String, OwnedUserId),
            DisplayName,
            SetDisplayName(String),
            Devices,
            NewSessions,
            AcknowledgeSessions(Vec<String>),
            RemoveDevice(matrix_sdk::ruma::OwnedDeviceId),
            RoomSettings(String),
            RoomMembers(String, String),
            RoomServers(String),
            RoomReaders(String),
            ChangeRoom(String, room_settings::Change),
            ChangePassword(bool),
            ThreePids,
            RequestEmail(String),
            RequestPhone(String, String),
            AddThreePid,
            RemoveThreePid(persephone_contracts::ThreePidMedium, String),
            NotificationLevel,
            SetNotificationLevel(persephone_contracts::NotificationLevel),
        }
        let user =
            |user_id: &str| conversations::parse_user(user_id).ok_or(ErrorCode::InvalidArgument);
        let work = match call {
            Call::AcceptInvite { room_id } if valid_room_id(room_id) => {
                Work::Accept(room_id.clone())
            }
            Call::DeclineInvite { room_id } if valid_room_id(room_id) => {
                Work::Decline(room_id.clone())
            }
            Call::DeleteMessage { room_id, event_id } if valid_room_id(room_id) => Work::Delete(
                room_id.clone(),
                parse_event_id(event_id).ok_or(ErrorCode::InvalidArgument)?,
            ),
            Call::React {
                room_id,
                event_id,
                key,
            } if valid_room_id(room_id) && valid_reaction_key(key) => Work::React(
                room_id.clone(),
                parse_event_id(event_id).ok_or(ErrorCode::InvalidArgument)?,
                key.clone(),
            ),
            Call::OpenLink { url } => {
                Work::OpenLink(links::checked(url).ok_or(ErrorCode::InvalidArgument)?)
            }
            Call::SetTyping { room_id, typing } if valid_room_id(room_id) => {
                Work::Typing(room_id.clone(), *typing)
            }
            Call::MarkRead { room_id, event_id } if valid_room_id(room_id) => Work::MarkRead(
                room_id.clone(),
                parse_event_id(event_id).ok_or(ErrorCode::InvalidArgument)?,
            ),
            Call::StartConversation { user_id } => Work::Start(user(user_id)?),
            Call::SearchUsers { term } if conversations::valid_term(term) => {
                Work::Search(term.clone())
            }
            Call::CreateRoom { name, invite } if conversations::valid_room_name(name) => {
                Work::CreateRoom(
                    name.trim().to_owned(),
                    conversations::parse_invitees(invite).ok_or(ErrorCode::InvalidArgument)?,
                )
            }
            Call::InviteUser { room_id, user_id } if valid_room_id(room_id) => {
                Work::Invite(room_id.clone(), user(user_id)?)
            }
            Call::DisplayName {} => Work::DisplayName,
            Call::SetDisplayName { name } if account_details::valid_display_name(name) => {
                Work::SetDisplayName(name.clone())
            }
            Call::DeviceList {} => Work::Devices,
            Call::NewSessions {} => Work::NewSessions,
            Call::AcknowledgeSessions { device_ids }
                if device_ids.len() <= persephone_core_protocol::MAX_NEW_SESSION_IDS
                    && device_ids
                        .iter()
                        .all(|id| verification::valid_device_id(id)) =>
            {
                Work::AcknowledgeSessions(device_ids.clone())
            }
            Call::RemoveDevice { device_id } if verification::valid_device_id(device_id) => {
                Work::RemoveDevice(device_id.as_str().into())
            }
            Call::RoomSettings { room_id } if valid_room_id(room_id) => {
                Work::RoomSettings(room_id.clone())
            }
            Call::RoomMembers { room_id, filter }
                if valid_room_id(room_id) && room_settings::valid_filter(filter) =>
            {
                Work::RoomMembers(room_id.clone(), filter.clone())
            }
            Call::RoomServers { room_id } if valid_room_id(room_id) => {
                Work::RoomServers(room_id.clone())
            }
            Call::RoomReaders { room_id } if valid_room_id(room_id) => {
                Work::RoomReaders(room_id.clone())
            }
            Call::ChangeRoom { room_id, change } if valid_room_id(room_id) => Work::ChangeRoom(
                room_id.clone(),
                room_settings::Change::parse(change).ok_or(ErrorCode::InvalidArgument)?,
            ),
            Call::ChangePassword { sign_out_others } => Work::ChangePassword(*sign_out_others),
            Call::ThreePids {} => Work::ThreePids,
            Call::RequestEmailToken { email } if account_changes::valid_email(email) => {
                Work::RequestEmail(email.clone())
            }
            Call::RequestPhoneToken {
                country,
                phone_number,
            } if account_changes::valid_country(country)
                && account_changes::valid_phone_number(phone_number) =>
            {
                Work::RequestPhone(country.clone(), phone_number.clone())
            }
            Call::AddThreePid {} => Work::AddThreePid,
            Call::RemoveThreePid { medium, address }
                if account_changes::valid_address(*medium, address) =>
            {
                Work::RemoveThreePid(*medium, address.clone())
            }
            Call::NotificationLevel {} => Work::NotificationLevel,
            Call::SetNotificationLevel { level }
                if *level != persephone_contracts::NotificationLevel::Custom =>
            {
                Work::SetNotificationLevel(*level)
            }
            _ => return Err(ErrorCode::InvalidArgument),
        };
        // Synthetic data has no homeserver.
        if !self.session.is_configured() {
            return Err(ErrorCode::NotFound);
        }
        let generation = match self.session.with_client() {
            Ok(Some((lease, client))) => {
                let generation = lease.generation();
                {
                    let _context = lease.stores().runtime().enter();
                    drop(client);
                }
                drop(lease);
                generation
            }
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let slot = match work {
            Work::Search(_) => self.search.begin()?,
            Work::MarkRead(..) | Work::Typing(..) => self.read_marker.begin()?,
            Work::OpenLink(_) => self.link.begin()?,
            Work::DisplayName | Work::SetDisplayName(_) => self.display_name.begin()?,
            Work::Devices | Work::NewSessions | Work::AcknowledgeSessions(_) => {
                self.devices.begin()?
            }
            Work::RemoveDevice(_) => self.account_change.begin()?,
            Work::RoomSettings(_) => self.room_settings.begin()?,
            Work::RoomMembers(..) | Work::RoomServers(_) | Work::RoomReaders(_) => {
                self.room_members.begin()?
            }
            Work::ChangeRoom(..) | Work::Delete(..) | Work::React(..) => {
                self.room_change.begin()?
            }
            Work::ChangePassword(_)
            | Work::RequestEmail(_)
            | Work::RequestPhone(..)
            | Work::AddThreePid
            | Work::RemoveThreePid(..) => self.account_change.begin()?,
            Work::ThreePids => self.three_pid_list.begin()?,
            Work::NotificationLevel | Work::SetNotificationLevel(_) => {
                self.notification_level.begin()?
            }
            _ => self.conversation.begin()?,
        };
        Ok(Box::new(move || {
            let _slot = slot;
            let outcome = match &work {
                Work::Accept(room_id) => self
                    .with_matrix(generation, async |client| {
                        conversations::accept(&client, room_id).await
                    })
                    .map(|()| serde_json::Value::Null),
                Work::Decline(room_id) => self
                    .with_matrix(generation, async |client| {
                        conversations::decline(&client, room_id).await
                    })
                    .map(|()| serde_json::Value::Null),
                Work::Delete(room_id, event_id) => self
                    .with_matrix(generation, async |client| {
                        sending::delete(&client, room_id, event_id).await
                    })
                    .map(|()| serde_json::Value::Null),
                Work::MarkRead(room_id, event_id) => {
                    let public = self.session.sends_read_receipts();
                    self.with_matrix(generation, async |client| {
                        sending::mark_read(&client, room_id, event_id, public).await
                    })
                    .map(|()| serde_json::Value::Null)
                }
                // No lease is held while the question is open.
                Work::OpenLink(url) => {
                    links::open(&*self.credentials, url).map(serde_json::Value::Bool)
                }
                // Refused, not dropped, while the setting is off: the page
                // never asks then, so an ask is a mistake to see.
                Work::Typing(..) if !self.session.sends_typing() => Err(ErrorCode::Rejected),
                Work::Typing(room_id, typing) => self
                    .with_matrix(generation, async |client| {
                        sending::typing(&client, room_id, *typing).await
                    })
                    .map(|()| serde_json::Value::Null),
                Work::React(room_id, event_id, key) => self
                    .react(generation, room_id, event_id, key)
                    .map(|()| serde_json::Value::Null),
                Work::Start(user) => json(self.with_matrix(generation, async |client| {
                    conversations::start(&client, user).await
                })),
                Work::Search(term) => json(self.with_matrix(generation, async |client| {
                    conversations::search(&client, term).await
                })),
                Work::CreateRoom(name, invite) => {
                    json(self.with_matrix(generation, async |client| {
                        conversations::create_room(&client, name, invite.clone()).await
                    }))
                }
                Work::Invite(room_id, user) => json(self.with_matrix(generation, async |client| {
                    conversations::invite_user(&client, room_id, user).await
                })),
                Work::DisplayName => json(self.with_matrix(generation, async |client| {
                    account_details::display_name(&client).await
                })),
                Work::SetDisplayName(name) => json(self.with_matrix(generation, async |client| {
                    account_details::set_display_name(&client, name).await
                })),
                Work::Devices => self
                    .with_matrix(generation, async |client| {
                        account_details::device_list(&client).await
                    })
                    .and_then(|devices| match bounded(&devices) {
                        Outcome::Ok(value) => Ok(value),
                        Outcome::Error(code) => Err(code),
                    }),
                Work::NewSessions => json(self.with_stores(generation, async |client, stores| {
                    sessions::new_sessions(&client, stores).await
                })),
                Work::AcknowledgeSessions(device_ids) => self
                    .with_stores(generation, async |client, stores| {
                        sessions::acknowledge(&client, stores, device_ids).await
                    })
                    .map(|()| serde_json::Value::Null),
                Work::RemoveDevice(device_id) => json(self.remove_device(generation, device_id)),
                Work::RoomSettings(room_id) => self
                    .with_matrix(generation, async |client| {
                        room_settings::settings(&client, room_id).await
                    })
                    .and_then(|settings| match bounded(&settings) {
                        Outcome::Ok(value) => Ok(value),
                        Outcome::Error(code) => Err(code),
                    }),
                Work::RoomMembers(room_id, filter) => {
                    json(self.with_matrix(generation, async |client| {
                        room_settings::members(&client, room_id, filter).await
                    }))
                }
                Work::RoomServers(room_id) => json(self.with_matrix(generation, async |client| {
                    room_settings::servers(&client, room_id).await
                })),
                Work::RoomReaders(room_id) => self
                    .with_matrix(generation, async |client| {
                        room_settings::readers(&client, room_id).await
                    })
                    .and_then(|readers| match bounded(&readers) {
                        Outcome::Ok(value) => Ok(value),
                        Outcome::Error(code) => Err(code),
                    }),
                Work::ChangeRoom(room_id, change) => self
                    .with_matrix(generation, async |client| {
                        room_settings::change(&client, room_id, change).await
                    })
                    .map(|()| serde_json::Value::Null),
                Work::ChangePassword(sign_out_others) => {
                    json(self.change_password(generation, *sign_out_others))
                }
                Work::ThreePids => {
                    let pending = self
                        .pending_three_pid(generation)
                        .map(|pending| pending.shown);
                    self.with_matrix(generation, async |client| {
                        Ok(account_changes::list(&client, pending.clone()).await)
                    })
                    .and_then(|listed| match listed {
                        Ok(list) => match bounded(&list) {
                            Outcome::Ok(value) => Ok(value),
                            Outcome::Error(code) => Err(code),
                        },
                        Err(persephone_contracts::AccountChange::Unreachable) => {
                            Err(ErrorCode::Unavailable)
                        }
                        Err(_) => Err(ErrorCode::Rejected),
                    })
                }
                Work::RequestEmail(email) => {
                    json(self.request_three_pid(generation, async |client| {
                        account_changes::request_email(&client, email).await
                    }))
                }
                Work::RequestPhone(country, number) => {
                    json(self.request_three_pid(generation, async |client| {
                        account_changes::request_phone(&client, country, number).await
                    }))
                }
                Work::AddThreePid => json(self.add_three_pid(generation)),
                Work::RemoveThreePid(medium, address) => {
                    json(self.with_matrix(generation, async |client| {
                        Ok(account_changes::remove(&client, *medium, address).await)
                    }))
                }
                Work::NotificationLevel => json(self.with_matrix(generation, async |client| {
                    notifications::level(&client)
                        .await
                        .map(|level| persephone_contracts::NotificationSettings { level })
                })),
                Work::SetNotificationLevel(level) => self
                    .with_matrix(generation, async |client| {
                        notifications::set_level(&client, *level).await
                    })
                    .map(|()| serde_json::Value::Null),
            };
            let rooms_changed = matches!(
                work,
                Work::Accept(_) | Work::Decline(_) | Work::Start(_) | Work::CreateRoom(..)
            );
            if outcome.is_ok() && rooms_changed {
                self.hub.mark_list_changed(&[], true);
            }
            match outcome {
                Ok(value) => Outcome::Ok(value),
                Err(code) => Outcome::Error(code),
            }
        }))
    }

    /// Signs out another device of this account (ADR-0026): asks for the
    /// password in the core's own window, with no lease held, then sends the
    /// removal under a lease of the session in `generation`. This device is
    /// refused: signing it out is `sign_out`.
    fn remove_device(
        &self,
        generation: u64,
        device_id: &matrix_sdk::ruma::DeviceId,
    ) -> Result<persephone_contracts::AccountChange, ErrorCode> {
        use credentials::PasswordPurpose;
        let this = self.with_matrix(generation, async |client| {
            Ok(client.device_id().map(ToOwned::to_owned))
        })?;
        if this.as_deref() == Some(device_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        let (user_id, host) = self.session.account_names().ok_or(ErrorCode::NotFound)?;
        let user = matrix_sdk::ruma::OwnedUserId::try_from(user_id.as_str())
            .map_err(|_| ErrorCode::Internal)?;
        let password = match account_changes::ask_current_password(
            &*self.credentials,
            &user_id,
            &host,
            PasswordPurpose::RemoveSession,
        ) {
            Ok(password) => password,
            Err(code) => return Ok(code),
        };
        self.with_matrix(generation, async |client| {
            Ok(account_changes::remove_device(&client, &user, &password, device_id).await)
        })
    }

    /// Changes the password (ADR-0019): asks for the current password and
    /// the new one (twice) in the core's own windows, with no lease held,
    /// then sends the change under a lease of the session in `generation`.
    fn change_password(
        &self,
        generation: u64,
        sign_out_others: bool,
    ) -> Result<persephone_contracts::AccountChange, ErrorCode> {
        use credentials::PasswordPurpose;
        let (user_id, host) = self.session.account_names().ok_or(ErrorCode::NotFound)?;
        let user = matrix_sdk::ruma::OwnedUserId::try_from(user_id.as_str())
            .map_err(|_| ErrorCode::Internal)?;
        let source = &*self.credentials;
        let passwords = account_changes::ask_current_password(
            source,
            &user_id,
            &host,
            PasswordPurpose::ChangePassword,
        )
        .and_then(|current| {
            account_changes::ask_new_password(source, &user_id, &host).map(|new| (current, new))
        });
        let (current, new) = match passwords {
            Ok(passwords) => passwords,
            Err(code) => return Ok(code),
        };
        self.with_matrix(generation, async |client| {
            Ok(
                account_changes::change_password(&client, &user, &current, &new, sign_out_others)
                    .await,
            )
        })
    }

    /// The address waiting to be added in `generation`, if any.
    fn pending_three_pid(&self, generation: u64) -> Option<account_changes::Pending> {
        lock(&self.three_pid)
            .as_ref()
            .filter(|(asked_in, _)| *asked_in == generation)
            .map(|(_, pending)| pending.clone())
    }

    /// Asks the homeserver to send a link or code; on success the address
    /// replaces any other waiting to be added.
    fn request_three_pid(
        &self,
        generation: u64,
        request: impl AsyncFnOnce(
            matrix_sdk::Client,
        ) -> Result<
            account_changes::Pending,
            persephone_contracts::AccountChange,
        >,
    ) -> Result<persephone_contracts::AccountChange, ErrorCode> {
        let requested = self.with_matrix(generation, async |client| Ok(request(client).await))?;
        Ok(match requested {
            Ok(pending) => {
                *lock(&self.three_pid) = Some((generation, pending));
                persephone_contracts::AccountChange::TokenSent
            }
            Err(code) => code,
        })
    }

    /// Adds the address waiting to be added (ADR-0020): for a phone, asks
    /// for its code in the core's own window and sends it to the
    /// homeserver; then asks for the account password there and adds the
    /// address. No lease is held while a window is open. The address stays
    /// waiting until it is added, so an unopened link can be tried again.
    fn add_three_pid(
        &self,
        generation: u64,
    ) -> Result<persephone_contracts::AccountChange, ErrorCode> {
        use credentials::PasswordPurpose;
        use persephone_contracts::{AccountChange, ThreePidMedium};
        let Some(mut pending) = self.pending_three_pid(generation) else {
            return Ok(AccountChange::NothingPending);
        };
        let (user_id, host) = self.session.account_names().ok_or(ErrorCode::NotFound)?;
        let user = matrix_sdk::ruma::OwnedUserId::try_from(user_id.as_str())
            .map_err(|_| ErrorCode::Internal)?;
        let source = &*self.credentials;
        if pending.shown.medium == ThreePidMedium::Msisdn && !pending.code_confirmed {
            let code = match account_changes::ask_phone_code(source, &host) {
                Ok(code) => code,
                Err(change) => return Ok(change),
            };
            let submitted = self.with_matrix(generation, async |client| {
                Ok(account_changes::submit_code(&client, &pending, &code).await)
            })?;
            if let Err(change) = submitted {
                return Ok(change);
            }
            pending.code_confirmed = true;
            if let Some((asked_in, waiting)) = lock(&self.three_pid).as_mut()
                && *asked_in == generation
                && waiting.sid == pending.sid
            {
                waiting.code_confirmed = true;
            }
        }
        let password = match account_changes::ask_current_password(
            source,
            &user_id,
            &host,
            PasswordPurpose::AddThreePid,
        ) {
            Ok(password) => password,
            Err(change) => return Ok(change),
        };
        let added = self.with_matrix(generation, async |client| {
            Ok(account_changes::add(&client, &user, &pending, &password).await)
        })?;
        if added == AccountChange::Done {
            let mut waiting = lock(&self.three_pid);
            if waiting
                .as_ref()
                .is_some_and(|(_, waiting)| waiting.sid == pending.sid)
            {
                *waiting = None;
            }
        }
        Ok(added)
    }

    /// Runs `work` with the signed-in client under a lease of the session in
    /// `generation`, and drops the client inside the stores' runtime before
    /// the lease ends. Another generation (the session locked, or another
    /// sign-in, since the call was admitted) is `locked`, and `work` does not
    /// run.
    /// Sends a reaction under a lease of the session in `generation`, with
    /// the room's consent read from its stores: `rejected` when the checks
    /// refuse it, `unavailable` when it may not have reached the homeserver.
    fn react(
        &self,
        generation: u64,
        room_id: &str,
        event_id: &matrix_sdk::ruma::EventId,
        key: &str,
    ) -> Result<(), ErrorCode> {
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let outcome = (lease.generation() == generation).then(|| {
            lease.stores().block_on(async {
                let consented = sending::consented(lease.stores(), room_id).await;
                sending::react(&client, room_id, event_id, key, consented).await
            })
        });
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        match outcome {
            None => Err(ErrorCode::Locked),
            Some(outbox::SendOutcome::Sent(_)) => Ok(()),
            Some(outbox::SendOutcome::Unknown) => Err(ErrorCode::Unavailable),
            Some(outbox::SendOutcome::Failed(_)) => Err(ErrorCode::Rejected),
        }
    }

    /// Like [`Core::with_matrix`], with the session's stores as well.
    fn with_stores<T>(
        &self,
        generation: u64,
        work: impl AsyncFnOnce(matrix_sdk::Client, &store::Stores) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let result = if lease.generation() == generation {
            lease
                .stores()
                .block_on(work(client.clone(), lease.stores()))
        } else {
            Err(ErrorCode::Locked)
        };
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        result
    }

    fn with_matrix<T>(
        &self,
        generation: u64,
        work: impl AsyncFnOnce(matrix_sdk::Client) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let result = if lease.generation() == generation {
            lease.stores().block_on(work(client.clone()))
        } else {
            Err(ErrorCode::Locked)
        };
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        result
    }

    /// Locks the open profile when the account's idle lock is on and this
    /// computer has had no input for that long (ADR-0025). Returns whether
    /// it locked.
    fn lock_if_idle(&self, idle: Option<Duration>) -> bool {
        let due = idle_lock_due(self.session.idle_lock_minutes(), self.session.state(), idle);
        if due {
            self.session.lock(LOCK_TIMEOUT, || self.announce_lock());
        }
        due
    }

    /// Waits up to `timeout` for the UI connection to close; whether it
    /// closed.
    fn wait_closed(&self, timeout: Duration) -> bool {
        let notifications = lock(&self.hub.notifications);
        let (notifications, _) = self
            .hub
            .changed
            .wait_timeout_while(notifications, timeout, |n| !n.closed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        notifications.closed
    }

    /// Runs when a lock or sign-out has closed admission and advanced the
    /// generation: drops every image and draft, then tells the UI.
    fn announce_lock(&self) {
        self.media.purge();
        self.hub.announce_session_change();
    }

    /// The generation images are bound to: the open session's (`None` while
    /// it is not open), or a fixed one for the synthetic views, which never
    /// make an image.
    fn media_generation(&self) -> Option<u64> {
        if self.session.is_configured() {
            self.session.open_generation()
        } else {
            Some(0)
        }
    }

    /// Admits a `load_image`: checks the arguments and the session now; the
    /// download and decode run later, on their own thread.
    fn load_image<'a>(&'a self, room_id: &str, event_id: &str) -> Result<LongCall<'a>, ErrorCode> {
        if !valid_room_id(room_id) || !valid_event_id(event_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        match self.source() {
            Source::Locked => return Err(ErrorCode::Locked),
            // Synthetic rooms hold no images.
            Source::Fixtures | Source::SignedOut => return Err(ErrorCode::NotFound),
            Source::Matrix(_lease, client) => {
                if !MatrixViews::has_room(&client, room_id) {
                    return Err(ErrorCode::NotFound);
                }
            }
        }
        let slot = self.media.begin_load()?;
        let (room_id, event_id) = (room_id.to_owned(), event_id.to_owned());
        Ok(Box::new(move || {
            let _slot = slot;
            refused(self.finish_load(&room_id, &event_id))
        }))
    }

    /// Downloads under a lease, which ends before the decode; the result is
    /// kept only if the generation the lease was granted in is still open.
    fn finish_load(&self, room_id: &str, event_id: &str) -> Result<ImageView, ErrorCode> {
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let generation = lease.generation();
        let current = || self.session.open_generation() == Some(generation);
        // The account's own HTTP client: the core's policy and the
        // profile's proxy, as for every other request.
        let encoded = lease.stores().block_on(media::fetch(
            &client,
            client.http_client(),
            room_id,
            event_id,
            &self.budgets,
            &current,
        ));
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        let encoded = encoded?;
        if !current() {
            return Err(ErrorCode::Locked);
        }
        let decoded = self.media.decode(encoded.bytes())?;
        drop(encoded);
        let (pixels, _) = self.media.shown(decoded)?;
        self.media
            .keep_view(generation, &|| self.media_generation(), pixels)
    }

    /// Admits a `pick_image` for a joined room: the dialog, the read and the
    /// strip run later, on their own thread, without a lease (nothing there
    /// touches the stores); the draft is kept only if the generation is still
    /// open.
    /// Admits a homeserver discovery (ADR-0027), one at a time. It runs on
    /// its own small runtime: it needs no session, and may run before any
    /// sign-in.
    fn discover<'a>(&'a self, server_name: &str) -> Result<LongCall<'a>, ErrorCode> {
        if discovery::origin(server_name).is_none() {
            return Err(ErrorCode::InvalidArgument);
        }
        // The profile's proxy, read now; unreadable refuses (never direct).
        let proxy = self.proxy_setting()?;
        let slot = self.discovery.begin()?;
        let server_name = server_name.to_owned();
        Ok(Box::new(move || {
            let _slot = slot;
            let found = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ErrorCode::Internal)
                .and_then(|runtime| {
                    runtime.block_on(discovery::discover(&server_name, proxy.as_ref()))
                });
            refused(found)
        }))
    }

    /// The profile's proxy (ADR-0028): `None` without one or for the
    /// synthetic views; `locked` while the profile is locked, and `internal`
    /// when the record cannot be read, so nothing connects directly.
    fn proxy_setting(&self) -> Result<Option<proxy::Socks>, ErrorCode> {
        if !self.session.is_configured() {
            return Ok(None);
        }
        let lease = self.session.admit().map_err(|error| match error {
            session::AdmissionError::Locked => ErrorCode::Locked,
            session::AdmissionError::Saturated => ErrorCode::LimitExceeded,
        })?;
        lease
            .stores()
            .block_on(proxy::load(lease.stores()))
            .map_err(|_| ErrorCode::Internal)
    }

    /// Replaces the profile's proxy; it applies when Persephone next
    /// connects (ADR-0028).
    fn set_proxy_setting(&self, proxy: Option<&str>) -> Outcome {
        let socks = match proxy.map(proxy::Socks::parse) {
            None => None,
            Some(Some(socks)) => Some(socks),
            Some(None) => return Outcome::Error(ErrorCode::InvalidArgument),
        };
        if !self.session.is_configured() {
            return Outcome::Error(ErrorCode::Unavailable);
        }
        let lease = match self.session.admit() {
            Ok(lease) => lease,
            Err(session::AdmissionError::Locked) => return Outcome::Error(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => {
                return Outcome::Error(ErrorCode::LimitExceeded);
            }
        };
        if lease
            .stores()
            .block_on(proxy::save(lease.stores(), socks.as_ref()))
        {
            Outcome::Ok(serde_json::Value::Null)
        } else {
            Outcome::Error(ErrorCode::Internal)
        }
    }

    /// Admits a file pick for a joined room (ADR-0029): the core's file
    /// dialog, then the read and the queue. One dialog at a time, as for
    /// images.
    fn pick_file<'a>(&'a self, room_id: &str) -> Result<LongCall<'a>, ErrorCode> {
        if !valid_room_id(room_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        let generation = match self.source() {
            Source::Locked => return Err(ErrorCode::Locked),
            Source::Fixtures | Source::SignedOut => return Err(ErrorCode::NotFound),
            Source::Matrix(lease, client) => {
                if !MatrixViews::has_room(&client, room_id) {
                    return Err(ErrorCode::NotFound);
                }
                lease.generation()
            }
        };
        let slot = self.media.begin_pick()?;
        let room_id = room_id.to_owned();
        Ok(Box::new(move || {
            let _slot = slot;
            let Some(path) = (match self.picker.pick_file() {
                Ok(path) => path,
                Err(code) => return Outcome::Error(code),
            }) else {
                return Outcome::Ok(serde_json::Value::Null);
            };
            // A file chosen after a lock is not read.
            if self.media_generation() != Some(generation) {
                return Outcome::Error(ErrorCode::Locked);
            }
            let bytes = match pick::read_image_file(&path, &self.budgets) {
                Ok(bytes) => bytes,
                Err(code) => return Outcome::Error(code),
            };
            let upload = Arc::new(files::FileUpload::new(bytes, &path));
            match self.session.send_file(&room_id, upload) {
                Ok(queued) => ok(&queued),
                Err(session::SendRefusal::Full) => Outcome::Error(ErrorCode::LimitExceeded),
                Err(session::SendRefusal::Unavailable) => Outcome::Error(ErrorCode::Unavailable),
            }
        }))
    }

    /// Admits saving a received file (ADR-0029): the download and decrypt
    /// under a lease, then the core's Save dialog without one, then the
    /// write. One at a time, as image loads are.
    fn save_file<'a>(&'a self, room_id: &str, event_id: &str) -> Result<LongCall<'a>, ErrorCode> {
        if !valid_room_id(room_id) || !valid_event_id(event_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        let slot = self.media.begin_load()?;
        let (room_id, event_id) = (room_id.to_owned(), event_id.to_owned());
        Ok(Box::new(move || {
            let _slot = slot;
            refused(self.finish_save(&room_id, &event_id))
        }))
    }

    fn finish_save(&self, room_id: &str, event_id: &str) -> Result<bool, ErrorCode> {
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let generation = lease.generation();
        let current = || self.session.open_generation() == Some(generation);
        let fetched = lease.stores().block_on(async {
            let event = media::room_event(&client, room_id, event_id).await?;
            let file = files::file_content(&event).ok_or(ErrorCode::NotFound)?;
            if files::too_large(file.size) {
                return Err(ErrorCode::Rejected);
            }
            let bytes = media::fetch_source(
                &client,
                client.http_client(),
                &file.source,
                &self.budgets,
                &current,
            )
            .await?;
            Ok((file.name, bytes))
        });
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        let (name, bytes) = fetched?;
        // The dialog may stay open long: no lease is held meanwhile.
        let Some(path) = self.picker.save_file(&name)? else {
            return Ok(false);
        };
        if !current() {
            return Err(ErrorCode::Locked);
        }
        files::write(&path, bytes.bytes()).map(|()| true)
    }

    fn pick_image<'a>(&'a self, room_id: &str) -> Result<LongCall<'a>, ErrorCode> {
        if !valid_room_id(room_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        let generation = match self.source() {
            Source::Locked => return Err(ErrorCode::Locked),
            Source::Fixtures | Source::SignedOut => return Err(ErrorCode::NotFound),
            Source::Matrix(lease, client) => {
                if !MatrixViews::has_room(&client, room_id) {
                    return Err(ErrorCode::NotFound);
                }
                lease.generation()
            }
        };
        let slot = self.media.begin_pick()?;
        let room_id = room_id.to_owned();
        Ok(Box::new(move || {
            let _slot = slot;
            refused(self.finish_pick(generation, &room_id))
        }))
    }

    fn finish_pick(&self, generation: u64, room_id: &str) -> Result<ImagePick, ErrorCode> {
        let Some(path) = self.picker.pick()? else {
            return Ok(ImagePick::Cancelled {});
        };
        // A file chosen after a lock is not read.
        if self.media_generation() != Some(generation) {
            return Err(ErrorCode::Locked);
        }
        let original = pick::read_image_file(&path, &self.budgets)?;
        drop(path);
        self.media
            .prepare_draft(generation, &|| self.media_generation(), room_id, original)
    }

    /// Queues a draft picked for this room. The draft is taken out of the
    /// image store; if the outbox refuses it, it is put back.
    fn send_image(&self, room_id: &str, handle: &str) -> Outcome {
        if !valid_room_id(room_id) || !media::valid_handle(handle) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let taken = match self
            .media
            .take_draft(self.media_generation(), handle, room_id)
        {
            Ok(taken) => taken,
            Err(code) => return Outcome::Error(code),
        };
        match self.session.send_image(room_id, taken.upload()) {
            Ok(queued) => ok(&queued),
            Err(refusal) => {
                self.media.restore_draft(&|| self.media_generation(), taken);
                Outcome::Error(match refusal {
                    session::SendRefusal::Full => ErrorCode::LimitExceeded,
                    session::SendRefusal::Unavailable => ErrorCode::Unavailable,
                })
            }
        }
    }

    fn sign_in(&self, homeserver: &str) -> Outcome {
        if !self.session.is_configured() {
            return Outcome::Error(ErrorCode::Unavailable);
        }
        if homeserver.len() > MAX_HOMESERVER_URL_BYTES {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let Ok(homeserver) = Homeserver::parse(homeserver) else {
            return Outcome::Error(ErrorCode::InvalidArgument);
        };
        match self.session.sign_in(homeserver, self.credentials.clone()) {
            Ok(status) => ok(&status),
            Err(_) => Outcome::Error(ErrorCode::Locked),
        }
    }

    /// Like `sign_in`, for a new account; the username is checked here
    /// before anything is asked.
    fn register(&self, homeserver: &str, username: &str) -> Outcome {
        if !self.session.is_configured() {
            return Outcome::Error(ErrorCode::Unavailable);
        }
        if homeserver.len() > MAX_HOMESERVER_URL_BYTES || !registration::valid_username(username) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let Ok(homeserver) = Homeserver::parse(homeserver) else {
            return Outcome::Error(ErrorCode::InvalidArgument);
        };
        match self
            .session
            .register(homeserver, username.to_owned(), self.credentials.clone())
        {
            Ok(status) => ok(&status),
            Err(_) => Outcome::Error(ErrorCode::Locked),
        }
    }

    fn send_message(
        &self,
        room_id: &str,
        body: &str,
        relation: Option<outbox::Relation>,
    ) -> Outcome {
        if !valid_room_id(room_id) || !valid_body(body) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        match self
            .session
            .send_message(room_id, zeroize::Zeroizing::new(body.to_owned()), relation)
        {
            Ok(queued) => ok(&queued),
            Err(session::SendRefusal::Full) => Outcome::Error(ErrorCode::LimitExceeded),
            Err(session::SendRefusal::Unavailable) => Outcome::Error(ErrorCode::Unavailable),
        }
    }

    fn verify(&self, flow_id: &str, action: fn(String) -> verification::Action) -> Outcome {
        if flow_id.is_empty() || flow_id.len() > verification::MAX_FLOW_ID_BYTES {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        ok(&self.session.verify(action(flow_id.to_owned())))
    }

    fn room_activity(&self, room_id: &str, event_id: Option<&str>) -> Outcome {
        if !valid_room_id(room_id) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let event_id = match event_id.map(parse_event_id) {
            None => None,
            Some(Some(event_id)) => Some(event_id),
            Some(None) => return Outcome::Error(ErrorCode::InvalidArgument),
        };
        let activity = match self.source() {
            Source::Locked => Err(ErrorCode::Locked),
            // Synthetic rooms have no one else in them.
            Source::Fixtures | Source::SignedOut => Ok(persephone_contracts::RoomActivity {
                typing: Vec::new(),
                read_by: Vec::new(),
                truncated: false,
            }),
            Source::Matrix(lease, client) => lease.stores().block_on(activity::room_activity(
                &client,
                self.session.typing(),
                room_id,
                event_id.as_deref(),
            )),
        };
        match activity {
            Ok(activity) => ok(&activity),
            Err(code) => Outcome::Error(code),
        }
    }

    fn contact_status(&self, room_id: &str) -> Outcome {
        if !valid_room_id(room_id) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let status = match self.source() {
            Source::Locked => Err(ErrorCode::Locked),
            // Synthetic rooms have no contacts.
            Source::Fixtures | Source::SignedOut => Err(ErrorCode::NotFound),
            Source::Matrix(lease, client) => lease
                .stores()
                .block_on(MatrixViews::contact_status(&client, room_id)),
        };
        match status {
            Ok(status) => ok(&status),
            Err(code) => Outcome::Error(code),
        }
    }

    fn identity_changes(&self, room_id: &str) -> Outcome {
        if !valid_room_id(room_id) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let changes = match self.source() {
            Source::Locked => Err(ErrorCode::Locked),
            Source::Fixtures | Source::SignedOut => Err(ErrorCode::NotFound),
            Source::Matrix(lease, client) => lease
                .stores()
                .block_on(identities::room_changes(&client, room_id)),
        };
        match changes {
            Ok(changes) => ok(&changes),
            Err(code) => Outcome::Error(code),
        }
    }

    /// Asks the core's own window to accept a member's new identity, if that
    /// member's identity changed in this room; `not_found` otherwise.
    fn accept_identity(&self, room_id: &str, user_id: &str) -> Outcome {
        let user = (user_id.len() <= MAX_USER_ID_BYTES)
            .then(|| matrix_sdk::ruma::UserId::parse(user_id).ok())
            .flatten();
        let Some(user) = user.filter(|_| valid_room_id(room_id)) else {
            return Outcome::Error(ErrorCode::InvalidArgument);
        };
        let changed = match self.source() {
            Source::Locked => return Outcome::Error(ErrorCode::Locked),
            Source::Fixtures | Source::SignedOut => None,
            Source::Matrix(lease, client) => lease
                .stores()
                .block_on(identities::find(&client, room_id, &user)),
        };
        match changed {
            Some(changed) => ok(&self.session.accept_identity(
                room_id.to_owned(),
                changed.user_id,
                changed.was_verified,
                self.credentials.clone(),
            )),
            None => Outcome::Error(ErrorCode::NotFound),
        }
    }

    fn changed(&self, done: bool) -> Outcome {
        if done {
            Outcome::Ok(serde_json::Value::Null)
        } else {
            Outcome::Error(ErrorCode::NotFound)
        }
    }

    fn subscribe(&self, room_id: &str) -> Outcome {
        if !valid_room_id(room_id) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let known = match self.source() {
            Source::Fixtures => lock(&self.fixtures).has_room(room_id),
            Source::Locked => return Outcome::Error(ErrorCode::Locked),
            Source::SignedOut => false,
            Source::Matrix(_lease, client) => MatrixViews::has_room(&client, room_id),
        };
        if !known {
            return Outcome::Error(ErrorCode::NotFound);
        }
        let mut notifications = lock(&self.hub.notifications);
        if !notifications.subscribed.contains(room_id)
            && notifications.subscribed.len() >= MAX_SUBSCRIPTIONS
        {
            return Outcome::Error(ErrorCode::LimitExceeded);
        }
        notifications.subscribed.insert(room_id.to_owned());
        Outcome::Ok(serde_json::Value::Null)
    }

    /// Adds a synthetic message to a fixture room and notifies a subscribed
    /// UI. Only for the demo without a profile; a profile's rooms come from
    /// Matrix.
    pub fn post_message(&self, room_id: &str, sender: &str, body: String) -> bool {
        if !lock(&self.fixtures).append_message(room_id, sender, body) {
            return false;
        }
        if !self.session.is_configured() || self.session.state() == StoreState::Protected {
            self.hub.mark_changed(&[room_id.to_owned()], false);
        }
        true
    }

    /// Number of rooms with a pending notification (for tests).
    pub fn pending_notifications(&self) -> usize {
        lock(&self.hub.notifications).pending.len()
    }

    /// The desktop notification waiting to be sent: room and sender names
    /// (for tests).
    pub fn pending_toast(&self) -> Option<(String, String)> {
        lock(&self.hub.notifications).toast.clone()
    }

    /// Waits for pending notifications and takes them all, or returns `None`
    /// once the connection is closed.
    fn take_events(&self) -> Option<Vec<Event>> {
        let mut notifications = lock(&self.hub.notifications);
        while notifications.pending.is_empty()
            && !notifications.session_changed
            && !notifications.room_list_changed
            && !notifications.security_changed
            && notifications.toast.is_none()
            && !notifications.closed
        {
            notifications = self
                .hub
                .changed
                .wait(notifications)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if notifications.closed {
            return None;
        }
        let session =
            std::mem::take(&mut notifications.session_changed).then_some(Event::SessionChanged {});
        let list = std::mem::take(&mut notifications.room_list_changed)
            .then_some(Event::RoomListChanged {});
        let security = std::mem::take(&mut notifications.security_changed)
            .then_some(Event::SecurityChanged {});
        let toast = notifications
            .toast
            .take()
            .map(|(room, sender)| Event::Notification { room, sender });
        let rooms = std::mem::take(&mut notifications.pending);
        Some(
            session
                .into_iter()
                .chain(list)
                .chain(security)
                .chain(toast)
                .chain(
                    rooms
                        .into_iter()
                        .map(|room_id| Event::RoomChanged { room_id }),
                )
                .collect(),
        )
    }

    /// Waits for `period`, returning `false` early once serving has ended.
    pub fn wait_running(&self, period: Duration) -> bool {
        let notifications = lock(&self.hub.notifications);
        let (notifications, _) = self
            .hub
            .changed
            .wait_timeout_while(notifications, period, |n| !n.closed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        !notifications.closed
    }

    /// Whether the current UI connection has sent a valid request: its page
    /// loaded and reached the core.
    pub fn ui_ready(&self) -> bool {
        lock(&self.hub.notifications).ready
    }

    /// Prepares a closed core for a new UI connection, after a UI was
    /// replaced. Nothing from the old connection carries over: its
    /// subscriptions, pending events and readiness are dropped, and the new
    /// page loads everything itself. The session is untouched.
    pub fn reopen(&self) {
        self.media.purge();
        *lock(&self.hub.notifications) = Notifications::default();
        self.hub.changed.notify_all();
    }

    fn close(&self) {
        lock(&self.hub.notifications).closed = true;
        self.hub.changed.notify_all();
    }
}

/// Serves requests from `input` until the UI closes the stream or violates the
/// protocol, while a second thread writes notifications. Replies and events
/// share `output` and are written whole, one frame at a time.
pub fn serve<R: Read, W: Write + Send>(core: &Core, mut input: R, output: W) -> ServeEnd {
    let budgets = core.budgets.clone();
    let output = Mutex::new(output);
    std::thread::scope(|scope| {
        let notifier = scope.spawn(|| {
            while let Some(events) = core.take_events() {
                for event in events {
                    if send(&mut *lock(&output), &CoreMessage::Event(event)).is_err() {
                        core.close();
                        return;
                    }
                }
            }
        });
        // The idle lock: checks every `IDLE_CHECK` until the connection
        // closes, which wakes it at once.
        let idle = scope.spawn(|| {
            while !core.wait_closed(IDLE_CHECK) {
                core.lock_if_idle(platform::os::idle_time());
            }
        });
        let mut ready = false;
        let end = loop {
            match receive::<_, Request>(&mut input, &budgets) {
                Ok(request) => {
                    if !ready {
                        ready = true;
                        lock(&core.hub.notifications).ready = true;
                    }
                    let written = match core.answer(&request) {
                        Answer::Reply(reply) => {
                            send(&mut *lock(&output), &CoreMessage::Reply(reply))
                        }
                        Answer::Pixels(reply, band) => {
                            send_pixel_reply(&mut *lock(&output), &reply, band.bytes())
                        }
                        Answer::Later(id, call) => {
                            let output = &output;
                            // The window is closing for a profile switch: no
                            // new work, so the old core waits on none.
                            let Some(running) = core.admit_long_call() else {
                                drop(call);
                                let refused = send(
                                    &mut *lock(output),
                                    &CoreMessage::Reply(Reply {
                                        id,
                                        outcome: Outcome::Error(ErrorCode::Unavailable),
                                    }),
                                );
                                if refused.is_err() {
                                    break ServeEnd::WriteFailed;
                                }
                                continue;
                            };
                            let spawned = std::thread::Builder::new()
                                .name("core-long-call".into())
                                .stack_size(LONG_CALL_STACK_BYTES)
                                .spawn_scoped(scope, move || {
                                    let _running = running;
                                    let reply = CoreMessage::Reply(Reply {
                                        id,
                                        outcome: call(),
                                    });
                                    if send(&mut *lock(output), &reply).is_err() {
                                        core.close();
                                    }
                                });
                            match spawned {
                                Ok(_) => Ok(()),
                                Err(_) => send(
                                    &mut *lock(output),
                                    &CoreMessage::Reply(Reply {
                                        id,
                                        outcome: Outcome::Error(ErrorCode::Internal),
                                    }),
                                ),
                            }
                        }
                    };
                    if written.is_err() {
                        break ServeEnd::WriteFailed;
                    }
                }
                Err(ReceiveError::Closed) => break ServeEnd::UiClosed,
                Err(
                    ReceiveError::Frame(_) | ReceiveError::UnexpectedKind | ReceiveError::Malformed,
                ) => break ServeEnd::ProtocolViolation,
            }
        };
        core.close();
        let _ = notifier.join();
        let _ = idle.join();
        end
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN each setting, profile state and idle time WHEN checked THEN the
    /// idle lock locks only an open profile, only when on, and only once the
    /// known idle time reaches the setting.
    #[test]
    fn given_idle_times_then_the_idle_lock_locks_only_when_due() {
        let minutes = |m: u64| Some(Duration::from_secs(m * 60));
        assert!(idle_lock_due(5, StoreState::Protected, minutes(5)));
        assert!(idle_lock_due(5, StoreState::Protected, minutes(90)));
        assert!(!idle_lock_due(
            5,
            StoreState::Protected,
            Some(Duration::from_secs(299))
        ));
        assert!(
            !idle_lock_due(0, StoreState::Protected, minutes(10_000)),
            "off"
        );
        assert!(
            !idle_lock_due(5, StoreState::Protected, None),
            "unknown idle time"
        );
        for state in [
            StoreState::Locked,
            StoreState::Locking,
            StoreState::Unlocking,
        ] {
            assert!(!idle_lock_due(5, state, minutes(90)), "{state:?}");
        }
    }

    /// GIVEN reaction keys WHEN checked THEN an emoji or short text passes;
    /// empty, blank, oversized or control-bearing keys are refused.
    #[test]
    fn given_reaction_keys_then_only_short_visible_text_is_accepted() {
        assert!(valid_reaction_key("👍"));
        assert!(valid_reaction_key("+1"));
        assert!(valid_reaction_key(
            &"x".repeat(persephone_core_protocol::MAX_REACTION_KEY_BYTES)
        ));
        for bad in [
            String::new(),
            "  ".into(),
            "a\nb".into(),
            "\u{7}".into(),
            "x".repeat(persephone_core_protocol::MAX_REACTION_KEY_BYTES + 1),
        ] {
            assert!(!valid_reaction_key(&bad), "{bad:?}");
        }
    }

    /// GIVEN message bodies WHEN checked THEN text with line breaks and tabs
    /// passes; empty, blank, oversized or control-bearing bodies are refused.
    #[test]
    fn given_message_bodies_then_only_bounded_visible_text_is_accepted() {
        assert!(valid_body("hello\n\tworld <b>ok</b>"));
        assert!(valid_body(&"x".repeat(MAX_MESSAGE_BYTES)));
        for bad in [
            String::new(),
            " \n\t ".to_owned(),
            "x".repeat(MAX_MESSAGE_BYTES + 1),
            "a\u{0}b".to_owned(),
            "a\u{1b}[31mred".to_owned(),
            "a\rb".to_owned(),
        ] {
            assert!(!valid_body(&bad), "{bad:?}");
        }
        assert!(valid_room_id("!r:s"));
        assert!(!valid_room_id("#alias:s"));
        assert!(!valid_room_id("!r\u{0}:s"), "the SDK refuses a NUL byte");
        // A reply too large for one view is refused, not written.
        assert!(matches!(bounded(&["x"]), Outcome::Ok(_)));
        assert!(matches!(
            bounded(&["x".repeat(views::MAX_PAGE_BYTES)]),
            Outcome::Error(ErrorCode::LimitExceeded)
        ));
        assert!(!valid_room_id(&format!(
            "!{}",
            "r".repeat(MAX_ROOM_ID_BYTES)
        )));
    }

    fn request(id: u32, call: Call) -> Vec<u8> {
        let mut wire = Vec::new();
        send(&mut wire, &Request { id, call }).unwrap();
        wire
    }

    /// GIVEN an image the core holds WHEN the UI asks for its bands through
    /// the serving loop THEN each band arrives as a pixel reply with exactly
    /// its rows, the last one partial; a band past the end, a malformed
    /// handle and a released handle get ordinary error replies and no pixels.
    #[test]
    fn given_a_kept_image_then_its_bands_cross_as_pixel_replies() {
        use persephone_core_protocol::{FromCore, receive_from_core};
        let core = Core::default();
        let rgba: Vec<u8> = (0..1024 * 300 * 4).map(|i| (i % 251) as u8).collect();
        let pixels = media::Pixels::for_tests(&core.budgets, 1024, 300, rgba.clone()).unwrap();
        let view = core.media.keep_view(0, &|| Some(0), pixels).unwrap();
        assert_eq!((view.bands, view.band_rows), (2, 256));
        let band = |band| Call::ImageBand {
            handle: view.handle.clone(),
            band,
        };
        let input = [
            request(1, band(0)),
            request(2, band(1)),
            request(3, band(2)),
            request(
                4,
                Call::ImageBand {
                    handle: "not-a-handle".into(),
                    band: 0,
                },
            ),
            request(
                5,
                Call::ReleaseImage {
                    handle: view.handle.clone(),
                },
            ),
            request(6, band(0)),
        ]
        .concat();
        let mut output = Vec::new();
        assert_eq!(
            serve(&core, std::io::Cursor::new(input), &mut output),
            ServeEnd::UiClosed
        );
        let budgets = ResourceBudgets::default();
        let mut reader = std::io::Cursor::new(output);
        let mut joined = Vec::new();
        for (id, rows) in [(1, 256), (2, 44)] {
            match receive_from_core(&mut reader, &budgets) {
                Ok(FromCore::Pixels(band)) => {
                    assert_eq!(
                        (band.reply.id, band.reply.width, band.reply.rows),
                        (id, 1024, rows)
                    );
                    joined.extend_from_slice(band.pixels.body());
                }
                _ => panic!("expected pixels for request {id}"),
            }
        }
        assert_eq!(joined, rgba);
        for (id, expected) in [
            (3, Outcome::Error(ErrorCode::InvalidArgument)),
            (4, Outcome::Error(ErrorCode::InvalidArgument)),
            (5, Outcome::Ok(serde_json::Value::Null)),
            (6, Outcome::Error(ErrorCode::NotFound)),
        ] {
            match receive_from_core(&mut reader, &budgets) {
                Ok(FromCore::Message(CoreMessage::Reply(reply))) => {
                    assert_eq!((reply.id, reply.outcome), (id, expected));
                }
                _ => panic!("expected a reply to request {id}"),
            }
        }
        assert_eq!(core.budgets.aggregate_used(), 0, "nothing stays reserved");
    }

    /// GIVEN the synthetic views (no Matrix session) WHEN image calls arrive
    /// THEN malformed arguments are invalid and everything else is not found:
    /// nothing is fetched, no dialog opens, and `handle` refuses pixel replies.
    #[test]
    fn given_no_matrix_session_then_image_calls_are_refused() {
        let core = Core::default();
        let handle = "0123456789abcdef0123456789abcdef".to_owned();
        for (call, expected) in [
            (
                Call::LoadImage {
                    room_id: "!r:s".into(),
                    event_id: "$e".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::LoadImage {
                    room_id: "#alias:s".into(),
                    event_id: "$e".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::LoadImage {
                    room_id: "!r:s".into(),
                    event_id: format!("${}", "e".repeat(MAX_EVENT_ID_BYTES)),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::PickImage {
                    room_id: "!r:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::PickImage {
                    room_id: "r".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::SendImage {
                    room_id: "!r:s".into(),
                    handle: handle.clone(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::SendImage {
                    room_id: "!r:s".into(),
                    handle: "x".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::DiscardImage {
                    handle: handle.clone(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::ReleaseImage {
                    handle: handle.clone(),
                },
                ErrorCode::NotFound,
            ),
            (Call::ImageBand { handle, band: 0 }, ErrorCode::Unavailable),
        ] {
            let reply = core.handle(&Request {
                id: 1,
                call: call.clone(),
            });
            assert_eq!(reply.outcome, Outcome::Error(expected), "{call:?}");
        }
    }

    /// GIVEN no Matrix session (synthetic data) WHEN conversation calls arrive
    /// THEN malformed arguments are invalid, the invitation list is empty and
    /// everything else is not found: nothing reaches a homeserver.
    #[test]
    fn given_no_matrix_session_then_conversation_calls_are_refused() {
        let core = Core::default();
        let reply = core.handle(&Request {
            id: 1,
            call: Call::InviteList {},
        });
        assert_eq!(
            reply.outcome,
            Outcome::Ok(serde_json::json!({"invites": [], "truncated": false}))
        );
        for (call, expected) in [
            (
                Call::StartConversation {
                    user_id: "@bob:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::StartConversation {
                    user_id: "bob".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::StartConversation {
                    user_id: format!("@{}:s", "b".repeat(MAX_USER_ID_BYTES)),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::AcceptInvite {
                    room_id: "!r:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::DeclineInvite {
                    room_id: "#alias:s".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::SearchUsers { term: "bob".into() },
                ErrorCode::NotFound,
            ),
            (
                Call::SearchUsers { term: " ".into() },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::SearchUsers {
                    term: "b".repeat(persephone_core_protocol::MAX_SEARCH_TERM_BYTES + 1),
                },
                ErrorCode::InvalidArgument,
            ),
        ] {
            let reply = core.handle(&Request {
                id: 1,
                call: call.clone(),
            });
            assert_eq!(reply.outcome, Outcome::Error(expected), "{call:?}");
        }
    }

    /// GIVEN no Matrix session WHEN the room and account calls arrive THEN
    /// bad arguments are `invalid_argument` before anything else, and valid
    /// ones are `not_found`: synthetic data has no homeserver.
    #[test]
    fn given_no_matrix_session_then_room_and_account_calls_are_refused() {
        use persephone_core_protocol::{MAX_DISPLAY_NAME_BYTES, MAX_INVITEES, MAX_ROOM_NAME_BYTES};
        let core = Core::default();
        let many: Vec<String> = (0..=MAX_INVITEES).map(|n| format!("@u{n}:s")).collect();
        for (call, expected) in [
            (
                Call::CreateRoom {
                    name: "Plans".into(),
                    invite: vec!["@bob:s".into()],
                },
                ErrorCode::NotFound,
            ),
            (
                Call::CreateRoom {
                    name: " ".into(),
                    invite: Vec::new(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::CreateRoom {
                    name: "x".repeat(MAX_ROOM_NAME_BYTES + 1),
                    invite: Vec::new(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::CreateRoom {
                    name: "Pl\nans".into(),
                    invite: Vec::new(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::CreateRoom {
                    name: "Plans".into(),
                    invite: many,
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::CreateRoom {
                    name: "Plans".into(),
                    invite: vec!["@bob:s".into(), "@bob:s".into()],
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::CreateRoom {
                    name: "Plans".into(),
                    invite: vec!["bob".into()],
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::InviteUser {
                    room_id: "!r:s".into(),
                    user_id: "@bob:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::InviteUser {
                    room_id: "#alias:s".into(),
                    user_id: "@bob:s".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::InviteUser {
                    room_id: "!r:s".into(),
                    user_id: "bob".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (Call::DisplayName {}, ErrorCode::NotFound),
            (
                Call::SetDisplayName { name: "Al".into() },
                ErrorCode::NotFound,
            ),
            (
                Call::SetDisplayName {
                    name: "a".repeat(MAX_DISPLAY_NAME_BYTES + 1),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::SetDisplayName {
                    name: "A\u{7}l".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (Call::DeviceList {}, ErrorCode::NotFound),
            (Call::AccountSettings {}, ErrorCode::Locked),
            (
                Call::SetAccountSettings {
                    settings: persephone_contracts::AccountSettings {
                        enter_sends: false,
                        chat_text_percent: 100,
                        interface_text_percent: 100,
                        desktop_notifications: true,
                        ..persephone_contracts::AccountSettings::default()
                    },
                },
                ErrorCode::Locked,
            ),
            // Out-of-bounds sizes are refused before anything else.
            (
                Call::SetAccountSettings {
                    settings: persephone_contracts::AccountSettings {
                        enter_sends: false,
                        chat_text_percent: 79,
                        interface_text_percent: 100,
                        desktop_notifications: false,
                        ..persephone_contracts::AccountSettings::default()
                    },
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::SetAccountSettings {
                    settings: persephone_contracts::AccountSettings {
                        enter_sends: false,
                        chat_text_percent: 100,
                        interface_text_percent: 151,
                        desktop_notifications: false,
                        ..persephone_contracts::AccountSettings::default()
                    },
                },
                ErrorCode::InvalidArgument,
            ),
            (Call::AddProfile {}, ErrorCode::NotFound),
            (Call::SwitchProfile { slot: 2 }, ErrorCode::NotFound),
        ] {
            let reply = core.handle(&Request {
                id: 1,
                call: call.clone(),
            });
            assert_eq!(reply.outcome, Outcome::Error(expected), "{call:?}");
        }
        // Synthetic data has no local profiles to list.
        assert_eq!(
            core.handle(&Request {
                id: 1,
                call: Call::ProfileList {},
            })
            .outcome,
            Outcome::Ok(serde_json::json!({"profiles": [], "canAdd": false}))
        );
    }

    /// GIVEN no Matrix session WHEN the room settings calls arrive THEN bad
    /// arguments, inside the change too, are `invalid_argument` before
    /// anything else, and valid ones are `not_found`: synthetic data has no
    /// homeserver.
    #[test]
    fn given_no_matrix_session_then_room_settings_calls_are_refused() {
        use persephone_contracts::{JoinRule, PowerLevel, RoomChange};
        let core = Core::default();
        let change = |change| Call::ChangeRoom {
            room_id: "!r:s".into(),
            change,
        };
        for (call, expected) in [
            (
                Call::RoomSettings {
                    room_id: "!r:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::RoomSettings {
                    room_id: "#alias:s".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::RoomMembers {
                    room_id: "!r:s".into(),
                    filter: String::new(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::RoomMembers {
                    room_id: "!r:s".into(),
                    filter: "b\nb".into(),
                },
                ErrorCode::InvalidArgument,
            ),
            (change(RoomChange::Leave {}), ErrorCode::NotFound),
            (
                Call::ChangeRoom {
                    room_id: "r".into(),
                    change: RoomChange::Leave {},
                },
                ErrorCode::InvalidArgument,
            ),
            (
                change(RoomChange::JoinRule {
                    rule: JoinRule::Restricted,
                }),
                ErrorCode::InvalidArgument,
            ),
            (
                change(RoomChange::Kick {
                    user_id: "bob".into(),
                }),
                ErrorCode::InvalidArgument,
            ),
            (
                change(RoomChange::MemberLevel {
                    user_id: "@b:s".into(),
                    level: PowerLevel::new(100).unwrap(),
                }),
                ErrorCode::NotFound,
            ),
        ] {
            let reply = core.handle(&Request {
                id: 1,
                call: call.clone(),
            });
            assert_eq!(reply.outcome, Outcome::Error(expected), "{call:?}");
        }
    }

    /// GIVEN profiles beside the open one WHEN the UI lists, adds or switches
    /// THEN only slot numbers cross, and one switch waits for the window.
    #[test]
    fn given_local_profiles_then_the_ui_lists_and_switches_by_slot_only() {
        let base = std::env::temp_dir()
            .join(format!("persephone-core-profiles-{}", std::process::id()))
            .join("profile");
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
        std::fs::create_dir_all(&base).unwrap();
        let core =
            Core::default().with_profiles(profiles::Profiles::new(base.clone(), None, 1).unwrap());
        let call = |call| core.handle(&Request { id: 1, call }).outcome;
        assert_eq!(
            call(Call::ProfileList {}),
            Outcome::Ok(serde_json::json!({
                "profiles": [{"slot": 1, "active": true}],
                "canAdd": true
            }))
        );
        assert_eq!(
            call(Call::SwitchProfile { slot: 1 }),
            Outcome::Error(ErrorCode::InvalidArgument)
        );
        assert_eq!(
            call(Call::SwitchProfile { slot: 2 }),
            Outcome::Error(ErrorCode::NotFound)
        );
        assert_eq!(core.profiles().unwrap().requested(), None);
        assert_eq!(
            call(Call::AddProfile {}),
            Outcome::Ok(serde_json::Value::Null)
        );
        assert_eq!(core.profiles().unwrap().requested(), Some(2));
        assert_eq!(
            call(Call::AddProfile {}),
            Outcome::Error(ErrorCode::LimitExceeded)
        );
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    /// GIVEN a long call running WHEN a switch is asked for THEN it is
    /// refused; GIVEN a waiting switch WHEN a long call arrives THEN it is
    /// refused at once, so the old core never waits on one before exiting.
    #[test]
    fn given_long_calls_then_a_switch_never_waits_for_one() {
        let base = std::env::temp_dir()
            .join(format!("persephone-core-switch-{}", std::process::id()))
            .join("profile");
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
        std::fs::create_dir_all(&base).unwrap();
        let core =
            Core::default().with_profiles(profiles::Profiles::new(base.clone(), None, 1).unwrap());
        let call = |call| core.handle(&Request { id: 1, call }).outcome;
        {
            let _running = core.admit_long_call().expect("no switch is waiting");
            assert_eq!(
                call(Call::AddProfile {}),
                Outcome::Error(ErrorCode::LimitExceeded)
            );
            assert_eq!(core.profiles().unwrap().requested(), None);
        }
        assert_eq!(core.long_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            call(Call::AddProfile {}),
            Outcome::Ok(serde_json::Value::Null)
        );
        // The switch is waiting: no long call is admitted.
        assert!(core.admit_long_call().is_none());
        assert_eq!(core.long_calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    /// A session change drops every subscription and pending room event, so
    /// nothing from the old session is announced afterwards.
    #[test]
    fn given_a_session_change_then_subscriptions_and_pending_events_are_dropped() {
        let hub = Hub::default();
        lock(&hub.notifications).subscribed.insert("!a:s".into());
        hub.rooms_changed(&["!a:s".into(), "!b:s".into()]);
        assert_eq!(
            lock(&hub.notifications).pending.iter().collect::<Vec<_>>(),
            ["!a:s"],
            "only subscribed rooms are queued"
        );
        assert!(
            lock(&hub.notifications).room_list_changed,
            "any change refreshes the list"
        );
        hub.session_changed();
        let notifications = lock(&hub.notifications);
        assert!(notifications.subscribed.is_empty());
        assert!(notifications.pending.is_empty());
        assert!(
            !notifications.room_list_changed,
            "the reload covers the list"
        );
        assert!(notifications.session_changed);
        drop(notifications);
        hub.rooms_changed(&["!a:s".into()]);
        assert!(lock(&hub.notifications).pending.is_empty());
    }

    /// GIVEN two open rooms WHEN room keys arrive for one of them and for a
    /// room not open THEN only that open room is queued, so the page fetches
    /// it again and its undecryptable notices can become messages; WHEN keys
    /// arrive for any room (overflow or lag) THEN every open room is queued
    /// and nothing else. The room list is never touched.
    #[test]
    fn given_room_keys_for_an_open_room_then_only_that_room_is_fetched_again() {
        let hub = Hub::default();
        let open = |hub: &Hub| {
            let mut notifications = lock(&hub.notifications);
            notifications
                .subscribed
                .extend(["!a:s".into(), "!c:s".into()]);
            notifications.pending.clear();
        };
        open(&hub);
        hub.room_keys_arrived(&KeyRooms::Listed(["!a:s".into(), "!b:s".into()].into()));
        let notifications = lock(&hub.notifications);
        assert_eq!(notifications.pending.iter().collect::<Vec<_>>(), ["!a:s"]);
        assert!(
            !notifications.room_list_changed,
            "a key changes no room's place in the list"
        );
        drop(notifications);

        open(&hub);
        hub.room_keys_arrived(&KeyRooms::All);
        let notifications = lock(&hub.notifications);
        assert_eq!(
            notifications.pending.iter().collect::<Vec<_>>(),
            ["!a:s", "!c:s"]
        );
        assert!(!notifications.room_list_changed);
    }

    /// GIVEN no room changed WHEN the account's direct-message rooms change
    /// THEN only the room list is fetched again, so a room that just became
    /// a direct-message room shows its contact; an empty sync round alone
    /// refreshes nothing.
    #[test]
    fn given_direct_rooms_change_then_only_the_room_list_is_fetched_again() {
        let hub = Hub::default();
        lock(&hub.notifications).subscribed.insert("!a:s".into());
        hub.rooms_changed(&[]);
        assert!(!lock(&hub.notifications).room_list_changed);
        hub.direct_rooms_changed();
        let notifications = lock(&hub.notifications);
        assert!(notifications.room_list_changed);
        assert!(notifications.pending.is_empty(), "no room content changed");
    }
}
