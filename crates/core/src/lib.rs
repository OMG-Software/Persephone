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

pub mod abuse;
pub mod account;
pub mod account_changes;
pub mod account_details;
pub mod activity;
pub mod backup_restore;
mod bounded;
pub mod calls;
pub mod contacts;
pub mod conversations;
pub mod credentials;
pub mod diagnostics;
pub mod directory;
pub mod discovery;
pub mod files;
pub mod homeserver;
pub mod identities;
pub mod links;
pub mod local_search;
pub mod matrix_views;
pub mod media;
pub mod modules;
pub mod notifications;
pub mod oauth_login;
pub mod outbox;
pub mod passphrase;
pub mod personal;
pub mod pick;
pub mod platform;
pub mod presence;
pub mod profiles;
pub mod prompt_text;
pub mod proxy;
pub mod recovery_file;
pub mod registration;
mod room_relations;
pub mod room_settings;
pub mod room_tools;
pub mod security;
pub mod sending;
pub mod session;
pub mod sessions;
pub mod settings;
pub mod store;
mod turns;
pub mod ui_host;
pub mod vault;
pub mod verification;
pub mod views;

use account::KeyRooms;
use credentials::CredentialSource;
use homeserver::Homeserver;
use matrix_views::MatrixViews;
use media::{Encoded, ImageWorker, MediaService};
use persephone_contracts::{
    AccountStatus, AppStatus, ImagePick, ImageView, RoomList, RoomPicture, StoreState,
    ThirdPartyModules, UnixMillis,
};
use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, MAX_EVENT_ID_BYTES, MAX_HOMESERVER_URL_BYTES,
    MAX_MESSAGE_BYTES, MAX_ROOM_ID_BYTES, MAX_SUBSCRIPTIONS, MAX_USER_ID_BYTES, Outcome,
    PixelReply, ReceiveError, Reply, Request, ToCore, VideoReply, call_video_within_limits,
    receive_request, send, send_pixel_reply, send_video_reply,
};
use persephone_media_protocol::{ReceivedFrame, ResourceBudgets};
use pick::ImagePicker;
use serde::Serialize;
use session::{Session, SessionListener, SignOutRefused};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub const MILESTONE: &str = "P1";

/// The commit this core was built from (`build.rs`).
pub const REVISION: &str = env!("PERSEPHONE_REVISION");

/// The shortest time between two `call_video` events: about 30 a second.
const VIDEO_EVENT_GAP: Duration = Duration::from_millis(33);

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
    /// Subscribed rooms that got room keys since their last
    /// `timeline_events` call: only they read undecryptable events again.
    /// A subset of `subscribed`.
    keyed: BTreeSet<String>,
    /// The session changed state since the last notification.
    session_changed: bool,
    /// The room list changed since the last notification.
    room_list_changed: bool,
    /// The account's encryption state changed since the last notification.
    security_changed: bool,
    /// The call changed since the last notification.
    calls_changed: bool,
    /// A call video picture arrived since the last notification. At most one
    /// `call_video` event per [`VIDEO_EVENT_GAP`] leaves the core.
    video_changed: bool,
    /// When the last `call_video` event was taken.
    video_taken: Option<Instant>,
    /// The report of third-party modules changed since the last notification.
    modules_changed: bool,
    /// The diagnostic log was turned on or off since the last notification.
    diagnostics_changed: bool,
    /// The latest desktop notification waiting to be sent (ADR-0022): room
    /// and sender names. A newer one replaces it.
    toast: Option<(String, String)>,
    /// The waiting desktop notification is for an incoming call (ADR-0052).
    toast_call: bool,
    /// The incoming call rings changed since the last notification.
    rings_changed: bool,
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
    /// The core's call controller, which the end of a session must stop.
    /// Weak: the core owns the controller, and the controller's admission
    /// reaches the session that holds this hub.
    calls: OnceLock<Weak<calls::controller::CallController>>,
    /// The UI connection has closed. Long calls that wait for the user (the
    /// camera portal of a device listing) end when it is set.
    closing: AtomicBool,
    /// Incoming call rings, in memory only (ADR-0052).
    rings: calls::rings::Rings,
    /// The rooms that have another device in their call (ADR-0052).
    call_rooms: calls::rings::CallRooms,
}

/// The largest JSON reply of a device listing, below the UI pipe's control
/// frame limit with room for the reply envelope (`id` and `outcome`).
const MAX_DEVICE_REPLY_BYTES: usize =
    persephone_media_protocol::CONTROL_FRAME_LIMIT as usize - 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock means a panic mid-update; the core stops either way.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Hub {
    /// Queues one `diagnostics_changed` event.
    fn diagnostics_changed(&self) {
        let mut notifications = lock(&self.notifications);
        if !notifications.diagnostics_changed {
            notifications.diagnostics_changed = true;
            self.changed.notify_all();
        }
    }

    /// Queues one content-free `modules_changed` event.
    fn modules_changed(&self) {
        let mut notifications = lock(&self.notifications);
        if !notifications.modules_changed {
            notifications.modules_changed = true;
            self.changed.notify_all();
        }
    }

    /// Drops every subscription and pending room event, and queues a
    /// `session_changed` event, so the UI clears what it shows.
    fn announce_session_change(&self) {
        let mut notifications = lock(&self.notifications);
        notifications.subscribed.clear();
        notifications.pending.clear();
        notifications.keyed.clear();
        notifications.room_list_changed = false;
        notifications.toast = None;
        notifications.toast_call = false;
        notifications.rings_changed = false;
        notifications.session_changed = true;
        self.changed.notify_all();
    }

    /// Drops every ring and call flag: on a lock, an unlock, a profile reset
    /// and when the signed-in account ends or changes. A sync error does not
    /// come here, so a short outage keeps them.
    fn clear_calls(&self) {
        self.rings.clear();
        self.call_rooms.clear();
    }

    /// Queues one call event; changes before the UI reads coalesce.
    fn calls_changed(&self) {
        let mut notifications = lock(&self.notifications);
        if !notifications.calls_changed {
            notifications.calls_changed = true;
            self.changed.notify_all();
        }
    }

    /// The call controller's room is the one that takes no rings (ADR-0052):
    /// a join in progress counts. A ring in that room ends.
    fn open_call_changed(&self) {
        use calls::controller::CallState;
        let state = self
            .calls
            .get()
            .and_then(Weak::upgrade)
            .map(|calls| calls.state());
        let room = match &state {
            Some(CallState::Joining { room_id } | CallState::Active { room_id, .. }) => {
                Some(room_id.as_ref())
            }
            _ => None,
        };
        if self.rings.set_open_room(room) {
            self.mark_rings_changed();
        }
    }

    /// Queues one content-free `rings_changed` event; changes before the UI
    /// reads coalesce.
    fn mark_rings_changed(&self) {
        let mut notifications = lock(&self.notifications);
        if !notifications.rings_changed {
            notifications.rings_changed = true;
            self.changed.notify_all();
        }
    }

    /// Queues one content-free `call_video` event. Pictures before the UI
    /// reads coalesce; the writer sends it no sooner than [`VIDEO_EVENT_GAP`]
    /// after the last one. Called from the worker read thread, so it only
    /// sets a flag.
    fn video_changed(&self) {
        let mut notifications = lock(&self.notifications);
        if !notifications.video_changed {
            notifications.video_changed = true;
            self.changed.notify_all();
        }
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

    fn account_ended(&self) {
        self.clear_calls();
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
                notifications.keyed.insert(room.clone());
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
        notifications.toast_call = false;
        self.changed.notify_all();
    }

    fn notify_call(&self, room: &str, sender: &str) {
        let mut notifications = lock(&self.notifications);
        notifications.toast = Some((room.to_owned(), sender.to_owned()));
        notifications.toast_call = true;
        self.changed.notify_all();
    }

    fn rings(&self) -> Option<&calls::rings::Rings> {
        Some(&self.rings)
    }

    fn call_rooms(&self) -> Option<&calls::rings::CallRooms> {
        Some(&self.call_rooms)
    }

    fn rings_changed(&self) {
        self.mark_rings_changed();
    }

    fn room_call_changed(&self, room: &str) {
        self.mark_changed(&[room.to_owned()], true);
    }

    fn check_rooms(&self, joined: &dyn Fn(&matrix_sdk::ruma::RoomId) -> bool) {
        self.call_rooms.retain(joined);
        if self.rings.retain(|ring| joined(&ring.room_id)) {
            self.mark_rings_changed();
        }
    }

    /// Wakes the watch in [`serve`], which finishes the ended session. A
    /// call stops first: the homeserver ended the session, so no more call
    /// work may start.
    fn session_ended(&self) {
        if let Some(calls) = self.calls.get().and_then(Weak::upgrade) {
            calls.stop_for_session_end();
        }
        let _notifications = lock(&self.notifications);
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
    /// One public room directory page at a time.
    public_rooms: conversations::Busy,
    /// The rooms public room directory pages offered to join, with the
    /// session generation they were listed in.
    offers: Mutex<directory::Offers>,
    /// The member counts each room's member list was last asked for
    /// against, in one session generation.
    member_refreshes: Mutex<room_settings::Refreshed>,
    /// Serializes encrypted record read/modify/write operations.
    personal: conversations::Busy,
    local_search: conversations::Busy,
    room_reads: conversations::Busy,
    abuse: conversations::Busy,
    read_marker: conversations::Busy,
    /// One homeserver discovery at a time.
    discovery: conversations::Busy,
    /// One link question at a time.
    link: conversations::Busy,
    /// Reading or setting the display name, one at a time.
    display_name: conversations::Busy,
    /// The audio call (ADR-0043): at most one controller; readiness, join
    /// and leave each run one at a time, so a leave can end a join.
    calls: Arc<calls::controller::CallController>,
    call_readiness: conversations::Busy,
    call_join: conversations::Busy,
    call_leave: conversations::Busy,
    call_decline: conversations::Busy,
    call_device_list: conversations::Busy,
    /// One device-list request at a time.
    devices: conversations::Busy,
    /// Reading a room's settings, listing its members, and changing it, each
    /// one at a time.
    room_settings: conversations::Busy,
    // Listing a room's members, readers or servers: one at a time, and up to
    // LISTING_PLACES more wait for it (`turns.rs`).
    room_members: turns::Queue,
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
    /// The third-party modules and the user's reviews of them. The scan
    /// thread takes this lock only to merge a result; store access never
    /// holds it.
    modules: Mutex<modules::Modules>,
    /// Serializes reading and writing the reviewed ids in the store, so two
    /// acknowledgements do not overwrite each other. The scan never takes it.
    modules_store: Mutex<()>,
    /// Where this run's diagnostic log goes (ADR-0058); `None` for
    /// synthetic data or a profile without a directory.
    diagnostics_path: Option<std::path::PathBuf>,
    /// The question to turn the log on is shown: a second one is refused.
    diagnostics_asking: AtomicBool,
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

/// How many listings may wait while one runs, and for how long each waits.
/// The member panel lists readers, then names, one after the other; room
/// settings may list members, readers and servers.
const LISTING_PLACES: usize = 3;
const LISTING_WAIT: Duration = Duration::from_secs(10);

/// What a long call holds so that only one of its kind runs: a slot taken at
/// once, or a place in a queue that becomes a turn on the call's thread.
/// The guards are only held: dropping one ends the operation.
enum Slot<'a> {
    Held {
        _guard: conversations::BusyGuard<'a>,
    },
    Queued(turns::Ticket<'a>),
    Turn {
        _turn: turns::Turn<'a>,
    },
}

impl Slot<'_> {
    /// Waits for a queued call's turn, for at most `limit`.
    fn ready(self, limit: Duration) -> Result<Self, ErrorCode> {
        match self {
            Slot::Queued(ticket) => ticket.wait(limit).map(|turn| Slot::Turn { _turn: turn }),
            held => Ok(held),
        }
    }
}

/// How a request is answered.
enum Answer<'a> {
    Reply(Reply),
    /// A pixel reply and the band it announces.
    Pixels(PixelReply, media::Band),
    /// A video reply, the picture it announces, and the open session's
    /// generation from before the picture was chosen.
    Video(VideoReply, calls::video::Picture, Option<u64>),
    /// Admitted; the reply follows when the call finishes.
    Later(u32, LongCall<'a>),
}

impl Default for Core {
    fn default() -> Self {
        Self::new(views::Fixtures::default())
    }
}

/// The reply to `timeline_events` when there is nothing to read.
fn no_events(room_id: &str) -> persephone_contracts::TimelinePage {
    persephone_contracts::TimelinePage {
        room_id: room_id.to_owned(),
        items: Vec::new(),
        next_cursor: None,
        names: Vec::new(),
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

/// `Ok` when the JSON of `devices` fits [`MAX_DEVICE_REPLY_BYTES`]. Names
/// and IDs that JSON escapes can make three lists of valid devices larger
/// than the UI pipe takes; the writer would then refuse the frame and the
/// page would get no reply.
fn within_device_reply(devices: &persephone_contracts::CallDevices) -> Result<(), ErrorCode> {
    match serde_json::to_vec(devices) {
        Ok(bytes) if bytes.len() <= MAX_DEVICE_REPLY_BYTES => Ok(()),
        Ok(_) => Err(ErrorCode::LimitExceeded),
        Err(_) => Err(ErrorCode::Internal),
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
    /// Create an unconfigured core with synthetic views and separate admission
    /// gates. A real account and its stores are supplied by `with_session`.
    pub fn new(fixtures: views::Fixtures) -> Self {
        let budgets = Arc::new(ResourceBudgets::default());
        let session = Session::not_configured();
        let hub = Arc::<Hub>::default();
        let calls = Arc::new(calls::controller::CallController::new(
            calls::worker::platform(),
            session.admission(),
        ));
        let _ = hub.calls.set(Arc::downgrade(&calls));
        let tell = Arc::downgrade(&hub);
        calls.set_listener(Arc::new(move || {
            if let Some(hub) = tell.upgrade() {
                hub.calls_changed();
                hub.open_call_changed();
            }
        }));
        let tell = Arc::downgrade(&hub);
        calls.set_video_listener(Arc::new(move || {
            if let Some(hub) = tell.upgrade() {
                hub.video_changed();
            }
        }));
        Self {
            session,
            credentials: Arc::from(credentials::platform()),
            picker: Arc::from(pick::platform()),
            fixtures: Mutex::new(fixtures),
            matrix: MatrixViews::default(),
            hub,
            media: MediaService::new(budgets.clone(), media::platform_worker()),
            budgets,
            conversation: conversations::Busy::default(),
            search: conversations::Busy::default(),
            public_rooms: conversations::Busy::default(),
            offers: Mutex::new(directory::Offers::default()),
            member_refreshes: Mutex::new(room_settings::Refreshed::default()),
            personal: conversations::Busy::default(),
            local_search: conversations::Busy::default(),
            room_reads: conversations::Busy::default(),
            abuse: conversations::Busy::default(),
            read_marker: conversations::Busy::default(),
            discovery: conversations::Busy::default(),
            link: conversations::Busy::default(),
            display_name: conversations::Busy::default(),
            calls,
            call_readiness: conversations::Busy::default(),
            call_join: conversations::Busy::default(),
            call_leave: conversations::Busy::default(),
            call_decline: conversations::Busy::default(),
            call_device_list: conversations::Busy::default(),
            devices: conversations::Busy::default(),
            room_settings: conversations::Busy::default(),
            room_members: turns::Queue::new(LISTING_PLACES),
            room_change: conversations::Busy::default(),
            account_change: conversations::Busy::default(),
            three_pid_list: conversations::Busy::default(),
            notification_level: conversations::Busy::default(),
            three_pid: Mutex::new(None),
            profiles: None,
            long_calls: AtomicUsize::new(0),
            modules: Mutex::new(modules::Modules::default()),
            modules_store: Mutex::new(()),
            diagnostics_path: None,
            diagnostics_asking: AtomicBool::new(false),
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
        self.calls.set_admission(session.admission());
        self.session = session;
        self.session.restore_account();
        self
    }

    /// Replaces the native credential prompt (tests and development builds).
    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = source;
        self
    }

    /// Where the diagnostic log goes when the user turns it on (ADR-0058);
    /// `None` leaves it unavailable.
    pub fn with_diagnostics(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.diagnostics_path = path;
        self
    }

    fn diagnostics_status(&self) -> persephone_contracts::DiagnosticsStatus {
        let log = diagnostics::global();
        persephone_contracts::DiagnosticsStatus {
            on: log.is_on(),
            full: log.is_full(),
            available: self.diagnostics_path.is_some(),
            path: log.path().map(|path| path.display().to_string()),
        }
    }

    /// Asks in the core's own window, then turns the log on: `true` when
    /// on, `false` when the user said no. One question at a time.
    fn diagnostics_on(&self) -> Result<LongCall<'_>, ErrorCode> {
        let path = self
            .diagnostics_path
            .clone()
            .ok_or(ErrorCode::Unavailable)?;
        if diagnostics::global().is_on() {
            return Ok(Box::new(|| Outcome::Ok(serde_json::Value::Bool(true))));
        }
        if self.diagnostics_asking.swap(true, Ordering::SeqCst) {
            return Err(ErrorCode::LimitExceeded);
        }
        Ok(Box::new(move || {
            let answer = self.credentials.confirm_diagnostics();
            self.diagnostics_asking.store(false, Ordering::SeqCst);
            match answer {
                Ok(true) => {}
                Ok(false) | Err(credentials::PromptError::Cancelled) => {
                    return Outcome::Ok(serde_json::Value::Bool(false));
                }
                Err(credentials::PromptError::Unsupported) => {
                    return Outcome::Error(ErrorCode::Unavailable);
                }
                Err(_) => return Outcome::Error(ErrorCode::Internal),
            }
            if diagnostics::global().turn_on(&path).is_err() {
                return Outcome::Error(ErrorCode::Unavailable);
            }
            self.hub.diagnostics_changed();
            Outcome::Ok(serde_json::Value::Bool(true))
        }))
    }

    fn diagnostics_off(&self) -> Outcome {
        diagnostics::global().turn_off();
        self.hub.diagnostics_changed();
        Outcome::Ok(serde_json::Value::Bool(false))
    }

    /// The UI host's or the page's records, checked against the protocol's
    /// limits. Ignored while the log is off.
    fn diagnostic_records(
        source: persephone_core_protocol::RecordSource,
        records: &[persephone_core_protocol::DiagnosticRecord],
    ) -> Outcome {
        use persephone_core_protocol::{MAX_DIAGNOSTIC_RECORDS, RecordLevel, RecordSource};
        if records.len() > MAX_DIAGNOSTIC_RECORDS || !records.iter().all(|r| r.within_bounds()) {
            return Outcome::Error(ErrorCode::LimitExceeded);
        }
        let log = diagnostics::global();
        if !log.is_on() {
            return Outcome::Ok(serde_json::Value::Null);
        }
        let source = match source {
            RecordSource::Ui => diagnostics::Source::Ui,
            RecordSource::Page => diagnostics::Source::Page,
        };
        for record in records {
            let level = match record.level {
                RecordLevel::Error => diagnostics::Level::Error,
                RecordLevel::Warn => diagnostics::Level::Warn,
                RecordLevel::Info => diagnostics::Level::Info,
                RecordLevel::Debug => diagnostics::Level::Debug,
            };
            log.record(source, level, &record.target, &record.message, &[]);
        }
        Outcome::Ok(serde_json::Value::Null)
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
            Call::RoomDraft { .. }
            | Call::RoomDrafts {}
            | Call::SaveRoomDraft { .. }
            | Call::RoomPreferences {}
            | Call::SetRoomPreference { .. }
            | Call::SearchMessages { .. }
            | Call::JumpToDate { .. }
            | Call::EventContext { .. }
            | Call::PinnedMessages { .. }
            | Call::SetPinned { .. }
            | Call::Attachments { .. }
            | Call::IgnoredUsers {}
            | Call::SetIgnoredUser { .. }
            | Call::ReportAbuse { .. } => match self.personal_call(&request.call) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::TimelineEvents { room_id, event_ids } => {
                match self.timeline_events(room_id, event_ids) {
                    Ok(Some(call)) => Answer::Later(id, call),
                    // Nothing to read: answered at once, with no worker.
                    Ok(None) => Answer::Reply(Reply {
                        id,
                        outcome: ok(&no_events(room_id)),
                    }),
                    Err(code) => error(code),
                }
            }
            Call::LoadImage { room_id, event_id } => match self.load_image(room_id, event_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::LoadRoomPicture { room_id } => match self.load_room_picture(room_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::PickImage { room_id } => match self.pick_image(room_id) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            // Read with its image frame, so answered by `answer_paste`.
            Call::PasteImage { .. } => error(ErrorCode::InvalidArgument),
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
            Call::CallReadiness { .. }
            | Call::CallJoin { .. }
            | Call::CallLeave {}
            | Call::CallDeclineRing { .. } => match self.call_work(&request.call) {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::CallDevices {} => match self.call_devices() {
                Ok(call) => Answer::Later(id, call),
                Err(code) => error(code),
            },
            Call::SetDiagnostics { on: true } => match self.diagnostics_on() {
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
            | Call::PublicRooms { .. }
            | Call::JoinPublicRoom { .. }
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
            Call::CallVideoFrame {
                tile,
                after,
                stream,
            } => {
                if !call_video_within_limits(&request.call) {
                    return error(ErrorCode::InvalidArgument);
                }
                // No newer picture, no call, and a locked profile all give the
                // same answer: nothing to show. The generation is read first,
                // so a lock after it is seen again when the reply is written.
                let generation = self.media_generation();
                match self.calls.video_picture(*tile, *after, *stream) {
                    Some(picture) => Answer::Video(
                        VideoReply {
                            id,
                            tile: *tile,
                            seq: picture.seq,
                            rotation: picture.rotation,
                            width: picture.width,
                            height: picture.height,
                        },
                        picture,
                        generation,
                    ),
                    None => Answer::Reply(Reply {
                        id,
                        outcome: Outcome::Ok(serde_json::Value::Null),
                    }),
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

    /// Complete a normal request synchronously, including admitted long calls.
    /// Store-backed room tools never fall back to the synthetic view source.
    fn outcome(&self, call: &Call) -> Outcome {
        match call {
            Call::RoomDraft { .. }
            | Call::RoomDrafts {}
            | Call::SaveRoomDraft { .. }
            | Call::RoomPreferences {}
            | Call::SetRoomPreference { .. }
            | Call::SearchMessages { .. }
            | Call::JumpToDate { .. }
            | Call::EventContext { .. }
            | Call::PinnedMessages { .. }
            | Call::SetPinned { .. }
            | Call::Attachments { .. }
            | Call::IgnoredUsers {}
            | Call::SetIgnoredUser { .. }
            | Call::ReportAbuse { .. } => match self.personal_call(call) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::TimelineEvents { room_id, event_ids } => {
                match self.timeline_events(room_id, event_ids) {
                    Ok(Some(call)) => call(),
                    Ok(None) => ok(&no_events(room_id)),
                    Err(code) => Outcome::Error(code),
                }
            }
            Call::AppStatus {} => ok(&AppStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                revision: REVISION.into(),
                milestone: MILESTONE.into(),
                store: self.session.state(),
                account: self.account(),
            }),
            Call::ConnectionHealth {} => ok(&self.session.connection_health()),
            Call::ThirdPartyModules {} => ok(&self.module_report()),
            Call::AcknowledgeModules { ids } => {
                if persephone_core_protocol::personal_call_within_limits(call) {
                    self.acknowledge_modules(ids)
                } else {
                    Outcome::Error(ErrorCode::InvalidArgument)
                }
            }
            Call::RoomList {} => match self.source() {
                Source::Fixtures => ok(&lock(&self.fixtures).room_list()),
                Source::Locked => Outcome::Error(ErrorCode::Locked),
                Source::SignedOut => ok(&empty_rooms()),
                Source::Matrix(_lease, client) => ok(&MatrixViews::room_list(
                    &client,
                    &self.hub.call_rooms,
                    self.session.contacts_presence(),
                )),
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
                notifications.keyed.remove(room_id);
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
                    self.hub.clear_calls();
                    self.hub.announce_session_change();
                }
                ok(&state)
            }
            Call::ResetProfile {} => {
                let before = self.session.state();
                let state = self.session.reset_profile();
                if state != before {
                    self.hub.clear_calls();
                    self.hub.announce_session_change();
                }
                ok(&state)
            }
            Call::SignIn { homeserver } => self.sign_in(homeserver),
            Call::SignInWithOidc { homeserver } => self.sign_in_oidc(homeserver),
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
            Call::DiagnosticsStatus {} => ok(&self.diagnostics_status()),
            // Turning on asks first, so only `answer` runs it.
            Call::SetDiagnostics { on: true } => Outcome::Error(ErrorCode::Unavailable),
            Call::SetDiagnostics { on: false } => self.diagnostics_off(),
            Call::DiagnosticRecords { source, records } => {
                Self::diagnostic_records(*source, records)
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
            Call::SetProfilePassphrase { on } => ok(&self
                .session
                .set_profile_passphrase(*on, self.credentials.clone())),
            Call::StartNewBackup {} => ok(&self.session.start_new_backup(self.credentials.clone())),
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
            Call::VerifyMember { room_id, user_id } => match conversations::parse_user(user_id) {
                Some(member) if valid_room_id(room_id) => ok(&self
                    .session
                    .verify(verification::Action::StartMember(room_id.clone(), member))),
                _ => Outcome::Error(ErrorCode::InvalidArgument),
            },
            Call::SignOut { discard_keys } => {
                match self
                    .session
                    .sign_out(LOCK_TIMEOUT, *discard_keys, || self.announce_lock())
                {
                    Ok(state) => ok(&state),
                    // Too much work in progress: nothing was signed out.
                    Err(SignOutRefused::Saturated) => Outcome::Error(ErrorCode::LimitExceeded),
                    // The page asks the user before it tries again.
                    Err(SignOutRefused::KeysNotBackedUp) => {
                        Outcome::Error(ErrorCode::KeysNotBackedUp)
                    }
                }
            }
            Call::LoadImage { room_id, event_id } => match self.load_image(room_id, event_id) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::LoadRoomPicture { room_id } => match self.load_room_picture(room_id) {
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
            Call::CallReadiness { .. }
            | Call::CallJoin { .. }
            | Call::CallLeave {}
            | Call::CallDeclineRing { .. } => match self.call_work(call) {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::CallDevices {} => match self.call_devices() {
                Ok(call) => call(),
                Err(code) => Outcome::Error(code),
            },
            Call::SetCallDevices {
                recording,
                playout,
                camera,
            } => {
                if !call_video_within_limits(call) {
                    return Outcome::Error(ErrorCode::InvalidArgument);
                }
                self.set_call_devices(recording.as_deref(), playout.as_deref(), camera.as_deref())
            }
            Call::CallMute { muted } => self.call_mute(*muted),
            Call::CallDeafen { deafened } => self.call_deafen(*deafened),
            Call::CallVolume { percent } => {
                if !call_video_within_limits(call) {
                    return Outcome::Error(ErrorCode::InvalidArgument);
                }
                self.call_volume(*percent)
            }
            Call::CallCamera { on } => self.call_camera(*on),
            Call::CallScreen { on } => self.call_screen(*on),
            // Its picture reaches only `serve`.
            Call::CallVideoFrame { .. } => Outcome::Error(ErrorCode::Unavailable),
            Call::CallRings {} => match self.calls_open() {
                Ok(()) => ok(&self.ring_list()),
                Err(code) => Outcome::Error(code),
            },
            Call::CallStatus {} => match self.calls_open() {
                Ok(()) => ok(&calls::status::status(&self.calls.state())),
                Err(code) => Outcome::Error(code),
            },
            Call::ImageBand { .. } => Outcome::Error(ErrorCode::Unavailable),
            // Its image frame reaches only `serve`.
            Call::PasteImage { .. } => Outcome::Error(ErrorCode::InvalidArgument),
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
            | Call::PublicRooms { .. }
            | Call::JoinPublicRoom { .. }
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

    /// Keep local searches independent from pending server history reads.
    fn personal_slot(
        &self,
        call: &Call,
    ) -> Result<Option<conversations::BusyGuard<'_>>, ErrorCode> {
        Ok(match call {
            Call::SearchMessages { .. } => Some(self.local_search.begin()?),
            Call::JumpToDate { .. }
            | Call::EventContext { .. }
            | Call::PinnedMessages { .. }
            | Call::Attachments { .. } => Some(self.room_reads.begin()?),
            Call::SetPinned { .. } | Call::SetIgnoredUser { .. } | Call::ReportAbuse { .. } => {
                Some(self.abuse.begin()?)
            }
            Call::SaveRoomDraft { .. } | Call::SetRoomPreference { .. } => {
                Some(self.personal.begin()?)
            }
            // Profile reads can overlap without mutating their records.
            _ => None,
        })
    }

    /// Reads loaded undecryptable events of a subscribed room again
    /// (`room_tools::timeline_events`), only when room keys arrived for it
    /// since the last such call. Otherwise `None`: the caller answers at once
    /// with no items, with no worker and no request, so a page that asks
    /// after every change costs nothing. The mark is taken once the read is
    /// admitted. It is put back when the read fails, or when an event was
    /// left out that a later read may get, so a later change asks again. A
    /// lease is held from admission to the reply, which is fenced against
    /// lock and sign-out.
    fn timeline_events<'a>(
        &'a self,
        room_id: &str,
        event_ids: &[String],
    ) -> Result<Option<LongCall<'a>>, ErrorCode> {
        let call = Call::TimelineEvents {
            room_id: room_id.to_owned(),
            event_ids: event_ids.to_vec(),
        };
        if !persephone_core_protocol::personal_call_within_limits(&call)
            || !valid_room_id(room_id)
            || !event_ids.iter().all(|id| valid_event_id(id))
        {
            return Err(ErrorCode::InvalidArgument);
        }
        if !self.session.is_configured() {
            return Err(ErrorCode::NotFound);
        }
        let room = room_id.to_owned();
        if !lock(&self.hub.notifications).keyed.contains(&room) {
            return Ok(None);
        }
        let slot = self.room_reads.begin()?;
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        if !lock(&self.hub.notifications).keyed.remove(&room) {
            // Another call took the mark meanwhile and reads them.
            return Ok(None);
        }
        let ids = event_ids.to_vec();
        Ok(Some(Box::new(move || {
            let _slot = slot;
            let stores = lease.stores();
            let result = stores.block_on(room_tools::timeline_events(
                &client,
                &self.matrix,
                &room,
                &ids,
            ));
            {
                let _context = stores.runtime().enter();
                drop(client);
            }
            // A failure, or an event left out that a later read may get.
            if !matches!(result, Ok((_, false))) {
                let mut notifications = lock(&self.hub.notifications);
                if notifications.subscribed.contains(&room) {
                    notifications.keyed.insert(room.clone());
                }
            }
            lease
                .commit(|| match result {
                    Ok((page, _)) => bounded(&page),
                    Err(code) => Outcome::Error(code),
                })
                .unwrap_or(Outcome::Error(ErrorCode::Locked))
        })))
    }

    /// Local profile operations and explicit abuse operations. Retain a lease
    /// from admission to completion and fence the reply against lock/sign-out.
    fn personal_call<'a>(&'a self, call: &Call) -> Result<LongCall<'a>, ErrorCode> {
        use persephone_core_protocol::personal_call_within_limits;
        if !personal_call_within_limits(call) {
            return Err(ErrorCode::InvalidArgument);
        }
        let ids_valid = match call {
            Call::RoomDraft { room_id } | Call::SaveRoomDraft { room_id, .. } => {
                valid_room_id(room_id)
            }
            Call::SetRoomPreference { preference } => valid_room_id(&preference.room_id),
            Call::SearchMessages { room_id, .. } => {
                room_id.as_ref().is_none_or(|id| valid_room_id(id))
            }
            Call::JumpToDate { room_id, .. }
            | Call::PinnedMessages { room_id }
            | Call::Attachments { room_id, .. } => valid_room_id(room_id),
            Call::EventContext { room_id, event_id }
            | Call::SetPinned {
                room_id, event_id, ..
            } => valid_room_id(room_id) && valid_event_id(event_id),
            Call::SetIgnoredUser { user_id, .. } => conversations::parse_user(user_id).is_some(),
            Call::ReportAbuse {
                room_id, event_id, ..
            } => valid_room_id(room_id) && valid_event_id(event_id),
            _ => true,
        };
        if !ids_valid {
            return Err(ErrorCode::InvalidArgument);
        }
        if !self.session.is_configured() {
            return Err(ErrorCode::NotFound);
        }
        let slot = self.personal_slot(call)?;
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let call = call.clone();
        Ok(Box::new(move || {
            let _slot = slot;
            let stores = lease.stores();
            let result = stores.block_on(async {
                match &call {
                    Call::RoomDraft { room_id } => {
                        json(personal::load_draft(stores, room_id).await)
                    }
                    Call::RoomDrafts {} => json(personal::draft_rooms(stores).await),
                    Call::SaveRoomDraft { room_id, draft } => {
                        json(personal::save_draft(stores, room_id, draft).await)
                    }
                    Call::RoomPreferences {} => json(personal::preferences(stores).await),
                    Call::SetRoomPreference { preference } => {
                        json(personal::set_preference(stores, preference).await)
                    }
                    Call::SearchMessages {
                        term,
                        room_id,
                        filters,
                    } => json(
                        local_search::search(&client, stores, term, room_id.as_deref(), filters)
                            .await,
                    ),
                    Call::JumpToDate {
                        room_id,
                        timestamp_ms,
                    } => json(
                        room_tools::jump_to_date(&client, &self.matrix, room_id, *timestamp_ms)
                            .await,
                    ),
                    Call::EventContext { room_id, event_id } => json(
                        room_tools::event_context(&client, &self.matrix, room_id, event_id).await,
                    ),
                    Call::PinnedMessages { room_id } => json(
                        room_tools::pinned_messages(&client, &self.matrix, room_id, || {
                            lease.commit(|| ()).is_ok()
                        })
                        .await,
                    ),
                    Call::SetPinned {
                        room_id,
                        event_id,
                        pinned,
                    } => json(room_tools::set_pinned(&client, room_id, event_id, *pinned).await),
                    Call::Attachments {
                        room_id,
                        cursor,
                        kind,
                        filters,
                    } => json(
                        room_tools::attachments(
                            &client,
                            &self.matrix,
                            room_id,
                            cursor.as_deref(),
                            *kind,
                            filters,
                        )
                        .await,
                    ),
                    Call::IgnoredUsers {} => json(abuse::ignored_users(&client).await),
                    Call::SetIgnoredUser { user_id, ignored } => {
                        json(abuse::set_ignored(&client, user_id, *ignored).await)
                    }
                    Call::ReportAbuse {
                        room_id,
                        event_id,
                        reason,
                    } => json(abuse::report(&client, room_id, event_id, reason).await),
                    _ => Err(ErrorCode::InvalidArgument),
                }
            });
            {
                let _context = stores.runtime().enter();
                drop(client);
            }
            lease
                .commit(|| match result {
                    Ok(value) => {
                        if matches!(call, Call::SetIgnoredUser { .. }) {
                            self.hub.mark_list_changed(&[], true);
                            self.hub.room_keys_arrived(&KeyRooms::All);
                        }
                        bounded(&value)
                    }
                    Err(code) => Outcome::Error(code),
                })
                .unwrap_or(Outcome::Error(ErrorCode::Locked))
        }))
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
            PublicRooms(directory::PageRequest),
            JoinPublicRoom(matrix_sdk::ruma::OwnedRoomId),
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
            Call::PublicRooms {
                server,
                filter,
                since,
            } => Work::PublicRooms(
                directory::PageRequest::parse(
                    server.as_deref(),
                    filter.as_deref(),
                    since.as_deref(),
                )
                .ok_or(ErrorCode::InvalidArgument)?,
            ),
            Call::JoinPublicRoom { room_id } if valid_room_id(room_id) => Work::JoinPublicRoom(
                matrix_sdk::ruma::RoomId::parse(room_id).map_err(|_| ErrorCode::InvalidArgument)?,
            ),
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
            Work::Search(_) => Slot::Held {
                _guard: self.search.begin()?,
            },
            Work::PublicRooms(_) => Slot::Held {
                _guard: self.public_rooms.begin()?,
            },
            Work::MarkRead(..) | Work::Typing(..) => Slot::Held {
                _guard: self.read_marker.begin()?,
            },
            Work::OpenLink(_) => Slot::Held {
                _guard: self.link.begin()?,
            },
            Work::DisplayName | Work::SetDisplayName(_) => Slot::Held {
                _guard: self.display_name.begin()?,
            },
            Work::Devices | Work::NewSessions | Work::AcknowledgeSessions(_) => Slot::Held {
                _guard: self.devices.begin()?,
            },
            Work::RemoveDevice(_) => Slot::Held {
                _guard: self.account_change.begin()?,
            },
            Work::RoomSettings(_) => Slot::Held {
                _guard: self.room_settings.begin()?,
            },
            // Listings take turns: the member panel and the room settings
            // both list, and a refusal would show as an error.
            Work::RoomMembers(..) | Work::RoomServers(_) | Work::RoomReaders(_) => {
                Slot::Queued(self.room_members.join()?)
            }
            Work::ChangeRoom(..) | Work::Delete(..) | Work::React(..) => Slot::Held {
                _guard: self.room_change.begin()?,
            },
            Work::ChangePassword(_)
            | Work::RequestEmail(_)
            | Work::RequestPhone(..)
            | Work::AddThreePid
            | Work::RemoveThreePid(..) => Slot::Held {
                _guard: self.account_change.begin()?,
            },
            Work::ThreePids => Slot::Held {
                _guard: self.three_pid_list.begin()?,
            },
            Work::NotificationLevel | Work::SetNotificationLevel(_) => Slot::Held {
                _guard: self.notification_level.begin()?,
            },
            _ => Slot::Held {
                _guard: self.conversation.begin()?,
            },
        };
        Ok(Box::new(move || {
            // A queued listing waits here, on its own thread, holding no lease.
            let _slot = match slot.ready(LISTING_WAIT) {
                Ok(slot) => slot,
                Err(code) => return Outcome::Error(code),
            };
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
                Work::PublicRooms(request) => self
                    .with_matrix(generation, async |client| {
                        directory::list(&client, request).await
                    })
                    .and_then(|(page, offers)| match bounded(&page) {
                        Outcome::Ok(value) => {
                            lock(&self.offers).remember(generation, offers);
                            Ok(value)
                        }
                        Outcome::Error(code) => Err(code),
                    }),
                Work::JoinPublicRoom(room_id) => {
                    // The guard is dropped before the join: no lock is held
                    // while the homeserver answers.
                    let via = lock(&self.offers).via(generation, room_id);
                    match via {
                        None => Err(ErrorCode::NotFound),
                        Some(via) => json(self.with_matrix(generation, async |client| {
                            directory::join(&client, room_id, &via).await
                        })),
                    }
                }
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
                        room_settings::members(
                            &client,
                            room_id,
                            filter,
                            &self.member_refreshes,
                            generation,
                        )
                        .await
                    }))
                }
                Work::RoomServers(room_id) => json(self.with_matrix(generation, async |client| {
                    room_settings::servers(&client, room_id).await
                })),
                Work::RoomReaders(room_id) => self
                    .with_matrix(generation, async |client| {
                        room_settings::readers(
                            &client,
                            room_id,
                            &self.member_refreshes,
                            generation,
                            self.session.members_presence(),
                        )
                        .await
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
            // A join can fail in local bookkeeping after the homeserver
            // joined the room, so the room list is read again either way.
            let joined = matches!(work, Work::JoinPublicRoom(_));
            if joined || (outcome.is_ok() && rooms_changed) {
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

    /// Deletes the profile once the homeserver ended the session
    /// ([`Session::finish_ended`]), and tells the UI. Returns whether a
    /// session had ended.
    pub fn finish_ended_session(&self) -> bool {
        self.session
            .finish_ended(LOCK_TIMEOUT, || self.announce_lock())
            .is_some()
    }

    /// Waits up to `timeout` for the UI connection to close; whether it
    /// closed. Returns early, `false`, when the homeserver ended the session.
    fn wait_closed(&self, timeout: Duration) -> bool {
        let notifications = lock(&self.hub.notifications);
        let (notifications, _) = self
            .hub
            .changed
            .wait_timeout_while(notifications, timeout, |n| {
                !n.closed && !self.session.has_ended()
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        notifications.closed
    }

    /// Runs when a lock or sign-out has closed admission and advanced the
    /// generation: stops the call first, so its audio ends before anything
    /// waits; then drops every image and draft and the closed profile's
    /// module reviews, and tells the UI.
    fn announce_lock(&self) {
        self.calls.stop_for_session_end();
        self.media.purge();
        lock(&self.modules).clear_reviewed();
        self.hub.clear_calls();
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

    /// Stops the call because the session stops admitting work. The core's
    /// exit calls it before its own lock, which has no announcement.
    pub fn stop_calls_for_session_end(&self) {
        self.calls.stop_for_session_end();
    }

    /// `locked` while the open profile is locked; a lease for no work, so a
    /// reply is refused the same way as for other calls. Synthetic data and
    /// a signed-out profile have no call, and answer idle.
    fn calls_open(&self) -> Result<(), ErrorCode> {
        if !self.session.is_configured() {
            return Ok(());
        }
        match self.session.admit() {
            Ok(_lease) => Ok(()),
            Err(session::AdmissionError::Locked) => Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => Err(ErrorCode::LimitExceeded),
        }
    }

    /// Mutes or unmutes the microphone. No call is not an error: the reply
    /// is the call state, and the page sees there is none.
    fn call_mute(&self, muted: bool) -> Outcome {
        if let Err(code) = self.calls_open() {
            return Outcome::Error(code);
        }
        self.queued_call_command(self.calls.set_muted(muted))
    }

    /// Deafens or undeafens (ADR-0055). Like mute, no call is not an error:
    /// the reply is the call state, and the page sees there is none.
    fn call_deafen(&self, deafened: bool) -> Outcome {
        if let Err(code) = self.calls_open() {
            return Outcome::Error(code);
        }
        self.queued_call_command(self.calls.set_deafened(deafened))
    }

    /// The reply to a mute or deafen. No call is the call state, as the page
    /// expects. A running call whose command queue is full did not take the
    /// command, so the page gets `limit_exceeded` and not a status that looks
    /// as if the command will happen.
    fn queued_call_command(&self, queued: Result<(), calls::controller::CallError>) -> Outcome {
        match queued {
            Err(calls::controller::CallError::Busy) => Outcome::Error(ErrorCode::LimitExceeded),
            _ => ok(&calls::status::status(&self.calls.state())),
        }
    }

    /// Sets the call playout volume (ADR-0055): at once for a running call,
    /// and saved in the profile's call device record for later calls. The
    /// other choices in the record are kept. A record that cannot be read
    /// is not replaced, because that would drop the user's device choice
    /// without a word; the reply is then `unavailable`. Synthetic data and a
    /// signed-out profile have no record and no call.
    fn call_volume(&self, percent: u8) -> Outcome {
        if let Err(code) = self.calls_open() {
            return Outcome::Error(code);
        }
        // A running call changes now, even if the save below fails.
        let _ = self.calls.set_volume(percent);
        if self.session.is_configured() {
            let lease = match self.session.admit() {
                Ok(lease) => lease,
                Err(session::AdmissionError::Locked) => return Outcome::Error(ErrorCode::Locked),
                Err(session::AdmissionError::Saturated) => {
                    return Outcome::Error(ErrorCode::LimitExceeded);
                }
            };
            let stores = lease.stores();
            let saved = stores.block_on(async {
                let Ok(mut selection) = calls::devices::load(stores).await else {
                    return Err(ErrorCode::Unavailable);
                };
                selection.volume = Some(percent);
                if calls::devices::save(stores, &selection).await {
                    Ok(())
                } else {
                    Err(ErrorCode::Internal)
                }
            });
            if let Err(code) = saved {
                return Outcome::Error(code);
            }
        }
        ok(&calls::status::status(&self.calls.state()))
    }

    /// Turns the camera on or off. Unlike mute, no call is an error: there is
    /// nothing to turn on.
    fn call_camera(&self, on: bool) -> Outcome {
        if let Err(code) = self.calls_open() {
            return Outcome::Error(code);
        }
        match self.calls.set_camera(on) {
            Ok(()) => ok(&calls::status::status(&self.calls.state())),
            Err(_) => Outcome::Error(ErrorCode::Unavailable),
        }
    }

    /// Starts or stops the screen share (ADR-0053). The page names no
    /// source: the platform's own window or portal asks the user.
    fn call_screen(&self, on: bool) -> Outcome {
        if let Err(code) = self.calls_open() {
            return Outcome::Error(code);
        }
        match self.calls.set_screen(on) {
            Ok(()) => ok(&calls::status::status(&self.calls.state())),
            Err(_) => Outcome::Error(ErrorCode::Unavailable),
        }
    }

    /// Admits a call command that waits on the homeserver or the worker
    /// (ADR-0043): readiness, join and leave. Each kind runs one at a time,
    /// on its own thread, so a leave can cancel a join in progress.
    fn call_work<'a>(&'a self, call: &Call) -> Result<LongCall<'a>, ErrorCode> {
        enum Work {
            Readiness(matrix_sdk::ruma::OwnedRoomId),
            Join(matrix_sdk::ruma::OwnedRoomId, bool),
            Leave,
            Decline(matrix_sdk::ruma::OwnedRoomId),
        }
        let room = |room_id: &str| {
            if valid_room_id(room_id) {
                matrix_sdk::ruma::OwnedRoomId::try_from(room_id)
                    .map_err(|_| ErrorCode::InvalidArgument)
            } else {
                Err(ErrorCode::InvalidArgument)
            }
        };
        let work = match call {
            Call::CallReadiness { room_id } => Work::Readiness(room(room_id)?),
            Call::CallJoin { room_id, video } => Work::Join(room(room_id)?, *video),
            Call::CallLeave {} => Work::Leave,
            Call::CallDeclineRing { room_id } => Work::Decline(room(room_id)?),
            _ => return Err(ErrorCode::InvalidArgument),
        };
        // Synthetic data has no homeserver and no call to leave.
        if !self.session.is_configured() {
            return match work {
                Work::Leave => Ok(Box::new(|| ok(&persephone_contracts::CallStatus::idle()))),
                Work::Decline(_) => Ok(Box::new(|| ok(&self.ring_list()))),
                _ => Err(ErrorCode::NotFound),
            };
        }
        self.calls_open()?;
        let slot = match work {
            Work::Readiness(_) => self.call_readiness.begin()?,
            Work::Join(..) => self.call_join.begin()?,
            Work::Leave => self.call_leave.begin()?,
            Work::Decline(_) => self.call_decline.begin()?,
        };
        Ok(Box::new(move || {
            let _slot = slot;
            match work {
                Work::Readiness(room_id) => refused(self.finish_call_readiness(&room_id)),
                Work::Join(room_id, video) => refused(self.finish_call_join(&room_id, video)),
                Work::Leave => refused(self.finish_call_leave()),
                Work::Decline(room_id) => refused(self.finish_call_decline(&room_id)),
            }
        }))
    }

    /// Admits the device list (ADR-0043). Only one runs at a time, on its own
    /// thread, because it starts a contained worker and waits for it.
    fn call_devices<'a>(&'a self) -> Result<LongCall<'a>, ErrorCode> {
        let slot = self.call_device_list.begin()?;
        Ok(Box::new(move || {
            let _slot = slot;
            refused(self.finish_call_devices())
        }))
    }

    /// The saved choice, then the devices a worker reports. An unreadable
    /// record is shown, with both choices null, and does not hide the list.
    /// A listing that fails is `unavailable`.
    ///
    /// The saved choice belongs to the session that was open when it was
    /// read. The listing can wait up to a minute for the camera portal, and
    /// holds no session lease meanwhile, so a lock or another session after
    /// the read gives `locked` and the old choice is not sent. A reply too
    /// large for the UI pipe is `limit_exceeded`.
    fn finish_call_devices(&self) -> Result<persephone_contracts::CallDevices, ErrorCode> {
        let generation = self.media_generation();
        let (selection, unreadable) = self.saved_call_devices()?;
        let listing = calls::devices::list(self.calls.host().as_ref(), &self.hub.closing)
            .map_err(|_| ErrorCode::Unavailable)?;
        if generation.is_none() || self.media_generation() != generation {
            return Err(ErrorCode::Locked);
        }
        let device =
            |device: persephone_call_protocol::MediaDevice| persephone_contracts::CallDevice {
                id: device.id,
                name: device.name,
            };
        let volume = selection.volume();
        let devices = persephone_contracts::CallDevices {
            supported: listing.supported,
            recording: listing.recording.into_iter().map(device).collect(),
            playout: listing.playout.into_iter().map(device).collect(),
            cameras: listing.cameras.into_iter().map(device).collect(),
            selected_recording: selection.recording,
            selected_playout: selection.playout,
            selected_camera: selection.camera,
            volume,
            unreadable,
        };
        within_device_reply(&devices)?;
        Ok(devices)
    }

    /// The saved call devices and whether the record was unreadable (then
    /// both are the default). No profile means the defaults; `locked` while
    /// the profile is locked.
    fn saved_call_devices(&self) -> Result<(calls::devices::Selection, bool), ErrorCode> {
        if !self.session.is_configured() {
            return Ok((calls::devices::Selection::default(), false));
        }
        let lease = self.session.admit().map_err(|error| match error {
            session::AdmissionError::Locked => ErrorCode::Locked,
            session::AdmissionError::Saturated => ErrorCode::LimitExceeded,
        })?;
        Ok(
            match lease
                .stores()
                .block_on(calls::devices::load(lease.stores()))
            {
                Ok(selection) => (selection, false),
                Err(calls::devices::Unreadable) => (calls::devices::Selection::default(), true),
            },
        )
    }

    /// Saves the call devices; they apply to calls joined afterwards. A
    /// device need not be connected now. The saved volume is kept; when the
    /// old record cannot be read, the new one has the default volume.
    fn set_call_devices(
        &self,
        recording: Option<&str>,
        playout: Option<&str>,
        camera: Option<&str>,
    ) -> Outcome {
        let mut selection = calls::devices::Selection {
            recording: recording.map(str::to_owned),
            playout: playout.map(str::to_owned),
            camera: camera.map(str::to_owned),
            volume: None,
        };
        if !selection.is_valid() {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
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
        let stores = lease.stores();
        if stores.block_on(async {
            selection.volume = calls::devices::load(stores)
                .await
                .ok()
                .and_then(|saved| saved.volume);
            calls::devices::save(stores, &selection).await
        }) {
            Outcome::Ok(serde_json::Value::Null)
        } else {
            Outcome::Error(ErrorCode::Internal)
        }
    }

    /// A lease and the client; the client must be dropped inside the stores'
    /// runtime, before the lease.
    fn call_lease(&self) -> Result<(session::Lease, matrix_sdk::Client), ErrorCode> {
        match self.session.with_client() {
            Ok(Some(granted)) => Ok(granted),
            Ok(None) => Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => Err(ErrorCode::LimitExceeded),
        }
    }

    fn finish_call_readiness(
        &self,
        room_id: &matrix_sdk::ruma::RoomId,
    ) -> Result<persephone_contracts::CallReadiness, ErrorCode> {
        let (lease, client) = self.call_lease()?;
        let supported = self.calls.supported();
        let readiness = lease
            .stores()
            .block_on(calls::status::readiness(&client, room_id, supported));
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        Ok(readiness)
    }

    /// Joins the call (ADR-0043). The profile's proxy is read under one
    /// lease and the user's question is asked under none, so a lock never
    /// waits for a dialog. The join itself runs under one lease, as one
    /// complete operation, in the session generation the proxy was read in.
    fn finish_call_join(
        &self,
        room_id: &matrix_sdk::ruma::RoomId,
        video: bool,
    ) -> Result<persephone_contracts::CallStatus, ErrorCode> {
        use calls::controller::{CallError, JoinScope};
        let refuse = |error: CallError| Ok(calls::status::join_reply(error, &self.calls.state()));
        if !self.calls.supported() {
            return refuse(CallError::Worker(calls::worker::StartError::Unsupported));
        }
        let (lease, client) = self.call_lease()?;
        let generation = lease.generation();
        let proxy = lease
            .stores()
            .block_on(calls::controller::proxy_configured(lease.stores()));
        let devices = lease
            .stores()
            .block_on(calls::controller::device_selection(lease.stores()));
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        let proxy = match proxy {
            Ok(proxy) => proxy,
            Err(error) => return refuse(error),
        };
        // Unreadable devices refuse the call; it never uses the defaults.
        let devices = match devices {
            Ok(devices) => devices,
            Err(error) => return refuse(error),
        };
        let consent = match self.calls.confirm_direct(proxy) {
            Ok(consent) => consent,
            Err(error) => return refuse(error),
        };
        let (lease, client) = self.call_lease()?;
        if lease.generation() != generation {
            let _context = lease.stores().runtime().enter();
            drop(client);
            return Err(ErrorCode::Locked);
        }
        let scope = JoinScope {
            generation,
            runtime: lease.stores().runtime().handle().clone(),
            devices,
            video,
        };
        let joined = lease
            .stores()
            .block_on(self.calls.join(scope, &client, room_id, consent));
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        match joined {
            Ok(()) => {
                // A join ends the room's ring (ADR-0052).
                if self.hub.rings.end_room(room_id) {
                    self.hub.mark_rings_changed();
                }
                Ok(calls::status::status(&self.calls.state()))
            }
            Err(error) => refuse(error),
        }
    }

    /// The incoming rings now running, newest first, as the page shows them.
    fn ring_list(&self) -> persephone_contracts::CallRings {
        use persephone_contracts::{CALL_RINGS_MAX, CallIntent, CallRing, CallRings};
        let now = calls::controller::now_ms();
        CallRings {
            rings: self
                .hub
                .rings
                .live(now)
                .into_iter()
                .take(CALL_RINGS_MAX)
                .map(|ring| CallRing {
                    ring_id: ring.id,
                    room_id: ring.room_id.to_string(),
                    caller: ring.sender.to_string(),
                    intent: match ring.intent {
                        calls::rings::Intent::Audio => CallIntent::Audio,
                        calls::rings::Intent::Video => CallIntent::Video,
                    },
                    seconds_left: u32::try_from(ring.ends_ms.saturating_sub(now).div_ceil(1000))
                        .unwrap_or(u32::MAX),
                })
                .collect(),
        }
    }

    /// Declines the ring in `room_id` (ADR-0052): one decline event, sent as
    /// a normal room event, never retried. The ring ends whether or not the
    /// send worked. No ring is not an error: the reply is the list.
    fn finish_call_decline(
        &self,
        room_id: &matrix_sdk::ruma::RoomId,
    ) -> Result<persephone_contracts::CallRings, ErrorCode> {
        let Some((event_id, _caller)) = self.hub.rings.target(room_id, calls::controller::now_ms())
        else {
            return Ok(self.ring_list());
        };
        let (lease, client) = self.call_lease()?;
        let _ = lease.stores().block_on(calls::ring_events::send_decline(
            &client, room_id, &event_id,
        ));
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        // Only the declined ring ends: a newer ring in the room stays.
        if self.hub.rings.retain(|ring| ring.event_id != event_id) {
            self.hub.mark_rings_changed();
        }
        Ok(self.ring_list())
    }

    /// Leaves the call, or cancels a join in progress. No call is not an
    /// error: the reply is the call state.
    fn finish_call_leave(&self) -> Result<persephone_contracts::CallStatus, ErrorCode> {
        let lease = self.session.admit().map_err(|error| match error {
            session::AdmissionError::Locked => ErrorCode::Locked,
            session::AdmissionError::Saturated => ErrorCode::LimitExceeded,
        })?;
        let _ = lease.stores().block_on(self.calls.leave());
        drop(lease);
        Ok(calls::status::status(&self.calls.state()))
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

    /// Admits a `load_room_picture` (ADR-0041): checks the argument and the
    /// session now; the download and decode run later, on their own thread.
    /// The page decides whether a picture loads automatically or on request;
    /// the core cannot tell the two apart, and treats both like `load_image`.
    fn load_room_picture<'a>(&'a self, room_id: &str) -> Result<LongCall<'a>, ErrorCode> {
        if !valid_room_id(room_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        match self.source() {
            Source::Locked => return Err(ErrorCode::Locked),
            // Synthetic rooms have no pictures.
            Source::Fixtures | Source::SignedOut => return Err(ErrorCode::NotFound),
            Source::Matrix(_lease, client) => {
                if !MatrixViews::has_room(&client, room_id) {
                    return Err(ErrorCode::NotFound);
                }
            }
        }
        let slot = self.media.begin_picture()?;
        let room_id = room_id.to_owned();
        Ok(Box::new(move || {
            let _slot = slot;
            refused(self.finish_room_picture(&room_id))
        }))
    }

    /// Downloads a room picture under a lease, which ends before the decode,
    /// like [`Self::finish_load`]; the picture is scaled to fit
    /// [`media::ROOM_PICTURE_SIDE`] square.
    fn finish_room_picture(&self, room_id: &str) -> Result<RoomPicture, ErrorCode> {
        let (lease, client) = match self.session.with_client() {
            Ok(Some(granted)) => granted,
            Ok(None) => return Err(ErrorCode::NotFound),
            Err(session::AdmissionError::Locked) => return Err(ErrorCode::Locked),
            Err(session::AdmissionError::Saturated) => return Err(ErrorCode::LimitExceeded),
        };
        let generation = lease.generation();
        let current = || self.session.open_generation() == Some(generation);
        let fetched = lease.stores().block_on(media::fetch_room_picture(
            &client,
            client.http_client(),
            room_id,
            &self.budgets,
            &current,
        ));
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        let (picture_id, encoded) = fetched?;
        if !current() {
            return Err(ErrorCode::Locked);
        }
        let decoded = self.media.decode(encoded.bytes())?;
        drop(encoded);
        let (pixels, _) = self.media.shown_within(decoded, media::ROOM_PICTURE_SIDE)?;
        let image = self
            .media
            .keep_view(generation, &|| self.media_generation(), pixels)?;
        Ok(RoomPicture { picture_id, image })
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
            let upload = Arc::new(files::FileUpload::new(bytes, &path, self.budgets.clone()));
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

    /// The session generation a new image draft for `room_id` belongs to:
    /// the room must be a room of the signed-in account.
    fn draft_generation(&self, room_id: &str) -> Result<u64, ErrorCode> {
        if !valid_room_id(room_id) {
            return Err(ErrorCode::InvalidArgument);
        }
        match self.source() {
            Source::Locked => Err(ErrorCode::Locked),
            Source::Fixtures | Source::SignedOut => Err(ErrorCode::NotFound),
            Source::Matrix(lease, client) => {
                if !MatrixViews::has_room(&client, room_id) {
                    return Err(ErrorCode::NotFound);
                }
                Ok(lease.generation())
            }
        }
    }

    fn pick_image<'a>(&'a self, room_id: &str) -> Result<LongCall<'a>, ErrorCode> {
        let generation = self.draft_generation(room_id)?;
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

    /// Admits a pasted image (ADR-0042) for a joined room, like a picked
    /// file: it shares the pick slot and the draft limit, and the strip and
    /// the preview decode run later, in the guest. The bytes are hostile.
    fn paste_image<'a>(
        &'a self,
        room_id: &str,
        original: Encoded,
    ) -> Result<LongCall<'a>, ErrorCode> {
        let generation = self.draft_generation(room_id)?;
        let slot = self.media.begin_pick()?;
        let room_id = room_id.to_owned();
        Ok(Box::new(move || {
            let _slot = slot;
            refused(self.media.prepare_draft(
                generation,
                &|| self.media_generation(),
                &room_id,
                original,
            ))
        }))
    }

    /// How [`serve`] answers a pasted image and its frame.
    fn answer_paste(&self, request: &Request, image: ReceivedFrame) -> Answer<'_> {
        let id = request.id;
        let Call::PasteImage { room_id, .. } = &request.call else {
            return Answer::Reply(Reply {
                id,
                outcome: Outcome::Error(ErrorCode::InvalidArgument),
            });
        };
        match Encoded::from_frame(image).and_then(|original| self.paste_image(room_id, original)) {
            Ok(call) => Answer::Later(id, call),
            Err(code) => Answer::Reply(Reply {
                id,
                outcome: Outcome::Error(code),
            }),
        }
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

    /// Like `sign_in`, through the homeserver's OAuth 2.0 provider
    /// (ADR-0030).
    fn sign_in_oidc(&self, homeserver: &str) -> Outcome {
        if !self.session.is_configured() {
            return Outcome::Error(ErrorCode::Unavailable);
        }
        if homeserver.len() > MAX_HOMESERVER_URL_BYTES {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        let Ok(homeserver) = Homeserver::parse(homeserver) else {
            return Outcome::Error(ErrorCode::InvalidArgument);
        };
        match self
            .session
            .sign_in_oidc(homeserver, self.credentials.clone())
        {
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
            Source::Matrix(lease, client) => {
                // Opening a contact's room asks for their presence again
                // soon, unless it is recent (ADR-0057).
                if let Some(contact) = MatrixViews::contact(&client, room_id) {
                    self.session
                        .contacts_presence()
                        .want(&contact, std::time::Instant::now());
                }
                lease
                    .stores()
                    .block_on(MatrixViews::contact_status(&client, room_id))
            }
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

    /// Takes a finished module scan from the scan thread; `incomplete` when
    /// it could not read every process or module. It never touches the
    /// store. Raises one `modules_changed` event when the report changed.
    pub fn update_modules(&self, found: Vec<modules::Found>, incomplete: bool, now: UnixMillis) {
        if lock(&self.modules).update(found, incomplete, now) {
            self.hub.modules_changed();
        }
    }

    /// The module report, with the reviewed ids of the open profile, and
    /// only those, applied.
    fn module_report(&self) -> ThirdPartyModules {
        let stored = self.stored_reviews();
        let (changed, report) = {
            let mut state = lock(&self.modules);
            let changed = stored
                .as_deref()
                .is_some_and(|stored| state.replace_reviewed(stored));
            (changed, state.report())
        };
        if changed {
            self.hub.modules_changed();
        }
        report
    }

    /// The reviewed ids the open profile's store holds. `None` with no store
    /// open (demo, signed out, locked). A record that cannot be read gives an
    /// empty list: it is never read as reviewed.
    fn stored_reviews(&self) -> Option<Vec<String>> {
        match self.source() {
            Source::Matrix(lease, _) => Some(
                lease
                    .stores()
                    .block_on(modules::load(lease.stores()))
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }

    /// Marks listed modules as reviewed, in memory and in the open profile's
    /// store. A failed write is `internal`; the next report reads the store
    /// again, so the modules then show as new again.
    fn acknowledge_modules(&self, ids: &[String]) -> Outcome {
        let _store = lock(&self.modules_store);
        let source = self.source();
        let stored = match &source {
            Source::Matrix(lease, _) => Some(
                lease
                    .stores()
                    .block_on(modules::load(lease.stores()))
                    .unwrap_or_default(),
            ),
            _ => None,
        };
        let (changed, reviewed, report) = {
            let mut state = lock(&self.modules);
            let mut changed = stored
                .as_deref()
                .is_some_and(|stored| state.replace_reviewed(stored));
            changed |= state.acknowledge(ids);
            (changed, state.reviewed().to_vec(), state.report())
        };
        if changed {
            self.hub.modules_changed();
        }
        if let Source::Matrix(lease, _) = &source
            && lease
                .stores()
                .block_on(modules::save(lease.stores(), &reviewed))
                .is_err()
        {
            return Outcome::Error(ErrorCode::Internal);
        }
        ok(&report)
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
        loop {
            // A video event waits until the gap since the last one has passed;
            // any other event does not wait for it.
            let video_wait = notifications
                .video_changed
                .then_some(notifications.video_taken)
                .flatten()
                .map(|taken| (taken + VIDEO_EVENT_GAP).saturating_duration_since(Instant::now()))
                .filter(|wait| !wait.is_zero());
            let video_due = notifications.video_changed && video_wait.is_none();
            let idle = notifications.pending.is_empty()
                && !notifications.session_changed
                && !notifications.room_list_changed
                && !notifications.security_changed
                && !notifications.calls_changed
                && !notifications.rings_changed
                && !notifications.modules_changed
                && !notifications.diagnostics_changed
                && notifications.toast.is_none()
                && !video_due
                && !notifications.closed;
            if !idle {
                break;
            }
            notifications = match video_wait {
                Some(wait) => {
                    self.hub
                        .changed
                        .wait_timeout(notifications, wait)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0
                }
                None => self
                    .hub
                    .changed
                    .wait(notifications)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            };
        }
        if notifications.closed {
            return None;
        }
        // Taken only when due: a picture that came during the gap stays
        // pending for the next call.
        let video = if notifications.video_changed
            && notifications
                .video_taken
                .is_none_or(|taken| taken.elapsed() >= VIDEO_EVENT_GAP)
        {
            notifications.video_changed = false;
            notifications.video_taken = Some(Instant::now());
            Some(Event::CallVideo {})
        } else {
            None
        };
        let session =
            std::mem::take(&mut notifications.session_changed).then_some(Event::SessionChanged {});
        let list = std::mem::take(&mut notifications.room_list_changed)
            .then_some(Event::RoomListChanged {});
        let security = std::mem::take(&mut notifications.security_changed)
            .then_some(Event::SecurityChanged {});
        let call =
            std::mem::take(&mut notifications.calls_changed).then_some(Event::CallChanged {});
        let rings =
            std::mem::take(&mut notifications.rings_changed).then_some(Event::RingsChanged {});
        let modules =
            std::mem::take(&mut notifications.modules_changed).then_some(Event::ModulesChanged {});
        let diagnostics = std::mem::take(&mut notifications.diagnostics_changed).then(|| {
            Event::DiagnosticsChanged {
                on: diagnostics::global().is_on(),
            }
        });
        let for_call = std::mem::take(&mut notifications.toast_call);
        let toast = notifications
            .toast
            .take()
            .map(|(room, sender)| Event::Notification {
                room,
                sender,
                call: for_call,
            });
        let rooms = std::mem::take(&mut notifications.pending);
        Some(
            session
                .into_iter()
                .chain(list)
                .chain(security)
                .chain(call)
                .chain(rings)
                .chain(video)
                .chain(modules)
                .chain(diagnostics)
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
        self.hub.closing.store(false, Ordering::SeqCst);
        self.hub.changed.notify_all();
    }

    fn close(&self) {
        self.hub.closing.store(true, Ordering::SeqCst);
        lock(&self.hub.notifications).closed = true;
        self.hub.changed.notify_all();
    }
}

/// Commands the page sends many times each second (video pictures, image
/// bands): only their failures are noted, as the page does (ADR-0058).
const QUIET_REQUESTS: &[&str] = &["call_video_frame", "image_band"];

/// One command for the diagnostic log: its name, its request ID, how it
/// ended (`ok`, an error code, or `running` for a long call that reports
/// again when it ends) and how long it took. A quiet command that succeeded
/// is not noted.
fn note_request(log: &diagnostics::Log, id: u32, name: &str, outcome: String, started: Instant) {
    if outcome == "ok" && QUIET_REQUESTS.contains(&name) {
        return;
    }
    log.record(
        diagnostics::Source::Core,
        diagnostics::Level::Debug,
        "persephone::request",
        name,
        &[
            ("id", serde_json::Value::from(id)),
            ("outcome", serde_json::Value::from(outcome)),
            (
                "ms",
                serde_json::Value::from(
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
            ),
        ],
    );
}

fn outcome_kind(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok(_) => "ok".into(),
        Outcome::Error(code) => serde_json::to_value(code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "error".into()),
    }
}

fn answer_kind(answer: &Answer<'_>) -> String {
    match answer {
        Answer::Reply(reply) => outcome_kind(&reply.outcome),
        Answer::Pixels(..) | Answer::Video(..) => "ok".into(),
        Answer::Later(..) => "running".into(),
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
        // closes, which wakes it at once. A session the homeserver ended
        // wakes it too, and is finished here, off the sync thread that the
        // lock joins.
        let idle = scope.spawn(|| {
            while !core.wait_closed(IDLE_CHECK) {
                if !core.finish_ended_session() {
                    core.lock_if_idle(platform::os::idle_time());
                }
            }
        });
        let mut ready = false;
        let end = loop {
            match receive_request(&mut input, &budgets) {
                Ok(incoming) => {
                    if !ready {
                        ready = true;
                        lock(&core.hub.notifications).ready = true;
                    }
                    // The diagnostic log gets each command's name, outcome
                    // and time, never its arguments (ADR-0058).
                    let started = Instant::now();
                    let named = diagnostics::global()
                        .is_on()
                        .then(|| match &incoming {
                            ToCore::Request(request)
                            | ToCore::Paste(request, _)
                            | ToCore::PasteRefused(request) => {
                                request.call.command().map(|name| (request.id, name))
                            }
                        })
                        .flatten();
                    let answer = match incoming {
                        ToCore::Request(request) => core.answer(&request),
                        ToCore::Paste(request, image) => core.answer_paste(&request, image),
                        // The image did not fit the encoded budget; its bytes
                        // were read past.
                        ToCore::PasteRefused(request) => Answer::Reply(Reply {
                            id: request.id,
                            outcome: Outcome::Error(ErrorCode::LimitExceeded),
                        }),
                    };
                    if let Some((id, name)) = &named {
                        note_request(
                            diagnostics::global(),
                            *id,
                            name,
                            answer_kind(&answer),
                            started,
                        );
                    }
                    let written = match answer {
                        Answer::Reply(reply) => {
                            send(&mut *lock(&output), &CoreMessage::Reply(reply))
                        }
                        Answer::Pixels(reply, band) => {
                            send_pixel_reply(&mut *lock(&output), &reply, band.bytes())
                        }
                        // The picture's bytes are shared with the store, which
                        // is not locked here: only the pipe is.
                        // A lock between the choice and this write must keep
                        // the picture from the page, so the generation is
                        // checked again while the pipe is held. The session
                        // state lock is a leaf: nothing holding it waits on
                        // the pipe.
                        Answer::Video(reply, picture, generation) => {
                            let mut output = lock(&output);
                            if generation.is_some() && core.media_generation() == generation {
                                send_video_reply(&mut *output, &reply, picture.bytes())
                            } else {
                                send(
                                    &mut *output,
                                    &CoreMessage::Reply(Reply {
                                        id: reply.id,
                                        outcome: Outcome::Ok(serde_json::Value::Null),
                                    }),
                                )
                            }
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
                                    let outcome = call();
                                    if let Some((id, name)) = &named {
                                        note_request(
                                            diagnostics::global(),
                                            *id,
                                            name,
                                            outcome_kind(&outcome),
                                            started,
                                        );
                                    }
                                    let reply = CoreMessage::Reply(Reply { id, outcome });
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

    /// GIVEN device lists WHEN the reply is checked THEN an ordinary one
    /// fits, and three full lists of names and IDs that JSON escapes to
    /// twice their size are `limit_exceeded` instead of a frame the UI pipe
    /// refuses.
    #[test]
    fn given_a_device_reply_then_only_one_that_fits_the_pipe_is_sent() {
        use persephone_call_protocol::{MAX_DEVICE_ID_BYTES, MAX_DEVICE_NAME_BYTES, MAX_DEVICES};
        let list = |id: &str, name: &str, count: usize| {
            vec![
                persephone_contracts::CallDevice {
                    id: id.to_owned(),
                    name: name.to_owned(),
                };
                count
            ]
        };
        let devices = |recording, playout, cameras| persephone_contracts::CallDevices {
            supported: true,
            recording,
            playout,
            cameras,
            selected_recording: None,
            selected_playout: None,
            selected_camera: None,
            volume: 100,
            unreadable: false,
        };
        let ordinary = devices(
            list("a", "Mic", 2),
            list("b", "Speaker", 2),
            list("c", "Cam", 1),
        );
        assert_eq!(within_device_reply(&ordinary), Ok(()));
        let id = "\"".repeat(MAX_DEVICE_ID_BYTES);
        let name = "\"".repeat(MAX_DEVICE_NAME_BYTES);
        let full = || list(&id, &name, MAX_DEVICES);
        assert_eq!(
            within_device_reply(&devices(full(), full(), full())),
            Err(ErrorCode::LimitExceeded)
        );
    }

    #[test]
    /// Reserve room and cache-search slots independently, and refuse excess work.
    fn given_pending_room_reads_then_local_search_has_independent_bounded_admission() {
        let core = Core::default();
        let room_read = Call::PinnedMessages {
            room_id: "!r:s".into(),
        };
        let cache_search = Call::SearchMessages {
            term: "local".into(),
            room_id: None,
            filters: persephone_contracts::LocalSearchFilters::default(),
        };
        let room_slot = core.personal_slot(&room_read).unwrap();
        let search_slot = core.personal_slot(&cache_search).unwrap();
        assert!(matches!(
            core.personal_slot(&cache_search),
            Err(ErrorCode::LimitExceeded)
        ));
        for call in [
            room_read.clone(),
            Call::JumpToDate {
                room_id: "!r:s".into(),
                timestamp_ms: 1,
            },
            Call::EventContext {
                room_id: "!r:s".into(),
                event_id: "$e".into(),
            },
            Call::Attachments {
                room_id: "!r:s".into(),
                cursor: None,
                kind: persephone_contracts::AttachmentKind::All,
                filters: persephone_contracts::LocalSearchFilters::default(),
            },
        ] {
            assert!(matches!(
                core.personal_slot(&call),
                Err(ErrorCode::LimitExceeded)
            ));
        }
        drop(search_slot);
        assert!(core.personal_slot(&cache_search).is_ok());
        drop(room_slot);
        assert!(core.personal_slot(&room_read).is_ok());
    }

    #[test]
    /// Keep personal storage unavailable in demo and signed-out states; reject bad input.
    fn personal_commands_reject_bad_input_and_have_no_demo_or_signed_out_store() {
        let core = Core::default();
        let valid = [
            Call::RoomDraft {
                room_id: "!r:s".into(),
            },
            Call::SaveRoomDraft {
                room_id: "!r:s".into(),
                draft: persephone_contracts::RoomDraft::default(),
            },
            Call::RoomPreferences {},
            Call::SetRoomPreference {
                preference: persephone_contracts::RoomPreference {
                    room_id: "!r:s".into(),
                    favourite: true,
                    section: None,
                },
            },
            Call::SearchMessages {
                term: "text".into(),
                room_id: None,
                filters: persephone_contracts::LocalSearchFilters::default(),
            },
            Call::IgnoredUsers {},
            Call::SetIgnoredUser {
                user_id: "@user:s".into(),
                ignored: true,
            },
            Call::ReportAbuse {
                room_id: "!r:s".into(),
                event_id: "$e".into(),
                reason: "Spam".into(),
            },
        ];
        for call in valid {
            assert!(matches!(
                core.handle(&Request { id: 1, call }).outcome,
                Outcome::Error(ErrorCode::NotFound)
            ));
        }
        for call in [
            Call::RoomDraft {
                room_id: "not-a-room".into(),
            },
            Call::SearchMessages {
                term: "text".into(),
                room_id: Some("not-a-room".into()),
                filters: persephone_contracts::LocalSearchFilters::default(),
            },
            Call::SetIgnoredUser {
                user_id: "not-a-user".into(),
                ignored: true,
            },
            Call::ReportAbuse {
                room_id: "!r:s".into(),
                event_id: "not-an-event".into(),
                reason: "Spam".into(),
            },
        ] {
            assert!(matches!(
                core.handle(&Request { id: 1, call }).outcome,
                Outcome::Error(ErrorCode::InvalidArgument)
            ));
        }
    }

    /// GIVEN many call changes before the UI reads WHEN the events are taken
    /// THEN one coalesced call event carries them, and none stays pending.
    #[test]
    fn given_many_call_changes_then_one_call_event_is_taken() {
        let core = Core::default();
        for _ in 0..5 {
            core.hub.calls_changed();
        }
        assert_eq!(core.take_events(), Some(vec![Event::CallChanged {}]));
        assert!(!lock(&core.hub.notifications).calls_changed);
    }

    fn test_ring(room: &str, sender: &str, ends_ms: u64) -> calls::rings::Ring {
        calls::rings::Ring {
            id: 0,
            room_id: matrix_sdk::ruma::OwnedRoomId::try_from(room).unwrap(),
            event_id: matrix_sdk::ruma::OwnedEventId::try_from("$n:example.org").unwrap(),
            sender: matrix_sdk::ruma::OwnedUserId::try_from(sender).unwrap(),
            intent: calls::rings::Intent::Video,
            ends_ms,
        }
    }

    /// GIVEN many ring changes before the UI reads WHEN the events are taken
    /// THEN one content-free event carries them, and none stays pending.
    #[test]
    fn given_many_ring_changes_then_one_rings_event_is_taken() {
        let core = Core::default();
        for _ in 0..5 {
            SessionListener::rings_changed(&*core.hub);
        }
        assert_eq!(core.take_events(), Some(vec![Event::RingsChanged {}]));
        assert!(!lock(&core.hub.notifications).rings_changed);
    }

    /// GIVEN a ring notification and a message notification WHEN taken THEN
    /// the first is marked as a call and the other is not; the latest wins.
    #[test]
    fn given_a_call_notification_then_the_event_is_marked_as_a_call() {
        let core = Core::default();
        SessionListener::notify_call(&*core.hub, "", "");
        assert_eq!(
            core.take_events(),
            Some(vec![Event::Notification {
                room: String::new(),
                sender: String::new(),
                call: true,
            }])
        );
        SessionListener::notify_call(&*core.hub, "Plans", "Bob");
        SessionListener::notify(&*core.hub, "Plans", "Bob");
        assert_eq!(
            core.take_events(),
            Some(vec![Event::Notification {
                room: "Plans".into(),
                sender: "Bob".into(),
                call: false,
            }])
        );
    }

    /// GIVEN rings WHEN the UI asks for them THEN each has its room, caller,
    /// intent and whole seconds left, newest first, and expired ones are
    /// absent.
    #[test]
    fn given_rings_then_the_list_shows_whole_seconds_and_drops_expired_rings() {
        let core = Core::default();
        let now = calls::controller::now_ms();
        let rings = &core.hub.rings;
        rings
            .admit(test_ring("!old:s", "@a:s", now + 1_500), now)
            .unwrap();
        rings
            .admit(test_ring("!new:s", "@b:s", now + 60_000), now)
            .unwrap();
        let Outcome::Ok(value) = core
            .handle(&Request {
                id: 1,
                call: Call::CallRings {},
            })
            .outcome
        else {
            panic!("rings");
        };
        let listed: persephone_contracts::CallRings = serde_json::from_value(value).unwrap();
        assert_eq!(listed.rings.len(), 2);
        assert_eq!(listed.rings[0].room_id, "!new:s");
        assert_eq!(listed.rings[0].caller, "@b:s");
        assert_eq!(
            listed.rings[0].intent,
            persephone_contracts::CallIntent::Video
        );
        assert!((59..=60).contains(&listed.rings[0].seconds_left));
        assert!((1..=2).contains(&listed.rings[1].seconds_left));
        rings.end_room(matrix_sdk::ruma::room_id!("!new:s"));
        let Outcome::Ok(value) = core
            .handle(&Request {
                id: 1,
                call: Call::CallRings {},
            })
            .outcome
        else {
            panic!("rings");
        };
        let listed: persephone_contracts::CallRings = serde_json::from_value(value).unwrap();
        assert_eq!(listed.rings.len(), 1);
    }

    /// GIVEN a decline for no ring or for a bad room ID THEN the list comes
    /// back for no ring, and a bad ID is refused before any work.
    #[test]
    fn given_a_decline_without_a_ring_or_with_a_bad_room_then_nothing_is_sent() {
        let core = Core::default();
        let reply = core.handle(&Request {
            id: 1,
            call: Call::CallDeclineRing {
                room_id: "!none:s".into(),
            },
        });
        let Outcome::Ok(value) = reply.outcome else {
            panic!("list");
        };
        assert_eq!(value, serde_json::json!({"rings": []}));
        for bad in ["room", "", "#alias:s", "$event:s"] {
            assert_eq!(
                core.handle(&Request {
                    id: 1,
                    call: Call::CallDeclineRing {
                        room_id: bad.into()
                    },
                })
                .outcome,
                Outcome::Error(ErrorCode::InvalidArgument),
                "{bad:?}"
            );
        }
    }

    /// GIVEN rings and flags WHEN a sync round shows the rooms are left, or
    /// the account ends THEN they end, and the UI is told once; a status
    /// change of the same account keeps them.
    #[test]
    fn given_left_rooms_or_a_session_change_then_rings_and_flags_end() {
        let core = Core::default();
        let now = calls::controller::now_ms();
        let hub = &core.hub;
        let room = matrix_sdk::ruma::room_id!("!r:s");
        hub.rings
            .admit(test_ring("!r:s", "@a:s", now + 60_000), now)
            .unwrap();
        hub.rings
            .admit(test_ring("!kept:s", "@b:s", now + 60_000), now)
            .unwrap();
        hub.call_rooms.set(room, Some(now + 60_000));
        SessionListener::check_rooms(&**hub, &|id| id != room);
        assert_eq!(hub.rings.live(now).len(), 1);
        assert!(!hub.call_rooms.active(room, now));
        assert_eq!(core.take_events(), Some(vec![Event::RingsChanged {}]));
        // Nothing left that changed: no event.
        SessionListener::check_rooms(&**hub, &|id| id != room);
        assert!(!lock(&hub.notifications).rings_changed);
        hub.call_rooms.set(room, Some(now + 60_000));
        // A status change of the same account, such as a sync error, keeps
        // them.
        SessionListener::session_changed(&**hub);
        assert_eq!(hub.rings.live(now).len(), 1);
        assert!(hub.call_rooms.active(room, now));
        // The end of the account drops them.
        SessionListener::account_ended(&**hub);
        assert!(hub.rings.live(now).is_empty());
        assert!(!hub.call_rooms.active(room, now));
    }

    /// GIVEN many pictures before the UI reads WHEN the events are taken THEN
    /// one video event carries them, and none stays pending.
    #[test]
    fn given_many_video_pictures_then_one_video_event_is_taken() {
        let core = Core::default();
        for _ in 0..50 {
            core.hub.video_changed();
        }
        assert_eq!(core.take_events(), Some(vec![Event::CallVideo {}]));
        assert!(!lock(&core.hub.notifications).video_changed);
    }

    /// GIVEN a video event just taken WHEN pictures arrive again THEN the next
    /// video event waits out the gap (about 30 a second), and another event
    /// does not wait for it. The time of the last video event is set by the
    /// test, so a slow thread cannot change the result.
    #[test]
    fn given_video_events_then_they_are_spaced_and_do_not_hold_other_events() {
        let core = Core::default();
        core.hub.video_changed();
        assert_eq!(core.take_events(), Some(vec![Event::CallVideo {}]));

        // The last video event is a day ahead, so the gap cannot pass during
        // this step however slowly the thread runs.
        lock(&core.hub.notifications).video_taken =
            Some(Instant::now() + Duration::from_secs(24 * 60 * 60));
        core.hub.video_changed();
        core.hub.calls_changed();
        assert_eq!(
            core.take_events(),
            Some(vec![Event::CallChanged {}]),
            "the call event is not held for the video gap"
        );
        assert!(lock(&core.hub.notifications).video_changed, "still pending");

        // The last video event is now: the next one waits out the gap.
        let taken = Instant::now();
        lock(&core.hub.notifications).video_taken = Some(taken);
        assert_eq!(core.take_events(), Some(vec![Event::CallVideo {}]));
        assert!(
            taken.elapsed() >= VIDEO_EVENT_GAP,
            "the second video event came after the gap"
        );
    }

    /// GIVEN a waiting video event WHEN the connection closes THEN the writer
    /// stops instead of waiting for the gap.
    #[test]
    fn given_a_closed_connection_then_a_pending_video_event_does_not_keep_it_open() {
        let core = Core::default();
        core.hub.video_changed();
        assert_eq!(core.take_events(), Some(vec![Event::CallVideo {}]));
        core.hub.video_changed();
        core.close();
        assert_eq!(core.take_events(), None);
    }

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

    /// GIVEN a scan result WHEN it arrives THEN one `modules_changed` event
    /// is queued, and the same result again queues none; WHEN the UI
    /// acknowledges an unknown id and a listed id THEN only the listed one is
    /// reviewed and the report changes once; WHEN the call is out of limits
    /// THEN it is refused and nothing is reviewed.
    #[test]
    fn given_a_module_scan_then_events_reports_and_reviews_follow() {
        use persephone_contracts::{ModuleProcess, ModuleSignature, UnixMillis};
        let core = Core::default();
        *lock(&core.modules) = modules::Modules::with_support(true);
        let report = |core: &Core| match core.handle(&Request {
            id: 1,
            call: Call::ThirdPartyModules {},
        }) {
            Reply {
                outcome: Outcome::Ok(value),
                ..
            } => serde_json::from_value::<ThirdPartyModules>(value).unwrap(),
            other => panic!("unexpected reply {:?}", other.outcome),
        };
        let ack = |core: &Core, ids: Vec<String>| {
            core.handle(&Request {
                id: 2,
                call: Call::AcknowledgeModules { ids },
            })
            .outcome
        };
        assert_eq!(
            report(&core).state,
            persephone_contracts::ModuleScanState::Pending
        );
        let found = || {
            vec![modules::Found {
                path: r"C:\x\Hook.dll".into(),
                process: ModuleProcess::Core,
                signature: ModuleSignature::Unsigned,
                signer: None,
                reason: persephone_contracts::ModuleReason::Signer,
            }]
        };
        core.update_modules(found(), false, UnixMillis::saturating(1));
        assert_eq!(core.take_events(), Some(vec![Event::ModulesChanged {}]));
        core.update_modules(found(), false, UnixMillis::saturating(2));
        assert!(!lock(&core.hub.notifications).modules_changed);

        let listed = report(&core);
        assert_eq!(listed.modules.len(), 1);
        assert!(!listed.modules[0].acknowledged);
        let id = listed.modules[0].id.clone();
        assert!(matches!(
            ack(&core, Vec::new()),
            Outcome::Error(ErrorCode::InvalidArgument)
        ));
        assert!(!report(&core).modules[0].acknowledged);
        assert!(matches!(
            ack(&core, vec!["unknown".into(), id]),
            Outcome::Ok(_)
        ));
        assert!(report(&core).modules[0].acknowledged);
        assert_eq!(core.take_events(), Some(vec![Event::ModulesChanged {}]));

        // A lock or sign-out forgets the closed profile's reviews.
        core.announce_lock();
        assert!(!report(&core).modules[0].acknowledged);
        // An incomplete scan is reported as such.
        core.update_modules(found(), true, UnixMillis::saturating(3));
        assert!(report(&core).incomplete);
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
                Call::LoadRoomPicture {
                    room_id: "!r:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::LoadRoomPicture {
                    room_id: "#alias:s".into(),
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
            (
                Call::PublicRooms {
                    server: None,
                    filter: None,
                    since: None,
                },
                ErrorCode::NotFound,
            ),
            (
                Call::PublicRooms {
                    server: Some("example.org".into()),
                    filter: Some("rust".into()),
                    since: Some("p30".into()),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::PublicRooms {
                    server: Some("https://example.org".into()),
                    filter: None,
                    since: None,
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::PublicRooms {
                    server: None,
                    filter: Some(" ".into()),
                    since: None,
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::PublicRooms {
                    server: None,
                    filter: None,
                    since: Some("p 30".into()),
                },
                ErrorCode::InvalidArgument,
            ),
            (
                Call::JoinPublicRoom {
                    room_id: "!r:s".into(),
                },
                ErrorCode::NotFound,
            ),
            (
                Call::JoinPublicRoom {
                    room_id: "#alias:s".into(),
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

    /// GIVEN one open room WHEN room keys arrive for it and for a room not
    /// open THEN only the open room is marked for `timeline_events`; WHEN
    /// other changes arrive THEN nothing is marked; WHEN the session changes
    /// THEN every mark is dropped with the subscriptions.
    #[test]
    fn given_room_keys_then_only_open_rooms_are_marked_for_reading_again() {
        let hub = Hub::default();
        lock(&hub.notifications).subscribed.insert("!a:s".into());
        hub.rooms_changed(&["!a:s".into()]);
        hub.outbox_changed("!a:s");
        assert!(lock(&hub.notifications).keyed.is_empty(), "no keys yet");
        hub.room_keys_arrived(&KeyRooms::Listed(["!a:s".into(), "!b:s".into()].into()));
        assert_eq!(
            lock(&hub.notifications).keyed.iter().collect::<Vec<_>>(),
            ["!a:s"]
        );
        hub.session_changed();
        assert!(lock(&hub.notifications).keyed.is_empty());
        hub.room_keys_arrived(&KeyRooms::All);
        assert!(
            lock(&hub.notifications).keyed.is_empty(),
            "nothing is open after the change"
        );
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

    /// Answers the diagnostic log's question with a fixed answer.
    struct DiagnosticsAnswer(bool);

    impl CredentialSource for DiagnosticsAnswer {
        fn ask(&self, _: &str) -> Result<credentials::Credentials, credentials::PromptError> {
            Err(credentials::PromptError::Unsupported)
        }

        fn confirm_open_link(
            &self,
            caption: &str,
            _: &str,
        ) -> Result<bool, credentials::PromptError> {
            assert_eq!(caption, prompt_text::DIAGNOSTICS.0);
            Ok(self.0)
        }
    }

    /// GIVEN a core with a log path WHEN the user says no, then yes, then
    /// turns the log off THEN it is on only after the yes, each change
    /// raises one event, the page's records are written only while on, and
    /// the file stays after off. The only test that uses the process's log.
    #[test]
    fn given_the_switch_when_answered_then_the_log_follows_the_user() {
        use persephone_core_protocol::{DiagnosticRecord, RecordLevel, RecordSource};
        let dir = tempfile::tempdir().unwrap();
        let path = diagnostics::path_in(dir.path());
        let no = Core::default()
            .with_credentials(Arc::new(DiagnosticsAnswer(false)))
            .with_diagnostics(Some(path.clone()));
        let answer = no.diagnostics_on().unwrap()();
        assert_eq!(answer, Outcome::Ok(serde_json::Value::Bool(false)));
        assert!(!no.diagnostics_status().on);
        assert!(!path.exists());

        let core = Core::default()
            .with_credentials(Arc::new(DiagnosticsAnswer(true)))
            .with_diagnostics(Some(path.clone()));
        let page = |message: &str| {
            Core::diagnostic_records(
                RecordSource::Page,
                &[DiagnosticRecord {
                    level: RecordLevel::Info,
                    target: "bridge".into(),
                    message: message.into(),
                }],
            )
        };
        page("before on");
        assert_eq!(
            core.diagnostics_on().unwrap()(),
            Outcome::Ok(serde_json::Value::Bool(true))
        );
        assert_eq!(
            core.take_events(),
            Some(vec![Event::DiagnosticsChanged { on: true }])
        );
        let status = core.diagnostics_status();
        assert!(status.on && status.available);
        assert_eq!(status.path, Some(path.display().to_string()));
        page("while on");
        assert_eq!(
            core.diagnostics_off(),
            Outcome::Ok(serde_json::Value::Bool(false))
        );
        assert_eq!(
            core.take_events(),
            Some(vec![Event::DiagnosticsChanged { on: false }])
        );
        page("after off");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("while on"), "{text}");
        assert!(
            !text.contains("before on") && !text.contains("after off"),
            "{text}"
        );
        assert!(!core.diagnostics_status().on);
        assert_eq!(
            core.diagnostics_status().path,
            Some(path.display().to_string())
        );
    }

    /// GIVEN too many records, or one over its limits THEN the call is
    /// refused; GIVEN no log path THEN turning on is unavailable.
    #[test]
    fn given_bad_records_or_no_path_then_refused() {
        use persephone_core_protocol::{
            DiagnosticRecord, MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_DIAGNOSTIC_RECORDS, RecordLevel,
            RecordSource,
        };
        let record = |message: String| DiagnosticRecord {
            level: RecordLevel::Debug,
            target: "t".into(),
            message,
        };
        let many = vec![record("m".into()); MAX_DIAGNOSTIC_RECORDS + 1];
        assert_eq!(
            Core::diagnostic_records(RecordSource::Ui, &many),
            Outcome::Error(ErrorCode::LimitExceeded)
        );
        let long = [record("m".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES + 1))];
        assert_eq!(
            Core::diagnostic_records(RecordSource::Ui, &long),
            Outcome::Error(ErrorCode::LimitExceeded)
        );
        let core = Core::default();
        assert!(matches!(core.diagnostics_on(), Err(ErrorCode::Unavailable)));
        assert!(!core.diagnostics_status().available);
    }

    /// GIVEN requests that succeeded, failed and are still running WHEN they
    /// are noted THEN each line names the command, its request ID and its
    /// outcome code, and nothing else of the request.
    #[test]
    fn given_requests_when_noted_then_lines_hold_name_id_and_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let path = diagnostics::path_in(dir.path());
        let log = diagnostics::Log::default();
        log.turn_on(&path).unwrap();
        let reply = |outcome| Answer::Reply(Reply { id: 1, outcome });
        let started = Instant::now();
        note_request(
            &log,
            1,
            "room_list",
            answer_kind(&reply(Outcome::Ok(serde_json::Value::Null))),
            started,
        );
        note_request(
            &log,
            2,
            "sign_in",
            answer_kind(&reply(Outcome::Error(ErrorCode::LimitExceeded))),
            started,
        );
        // Frame fetches are noted only when they fail.
        note_request(&log, 4, "call_video_frame", "ok".into(), started);
        note_request(&log, 5, "image_band", "core_internal".into(), started);
        note_request(
            &log,
            3,
            "set_diagnostics",
            answer_kind(&Answer::Later(
                3,
                Box::new(|| Outcome::Ok(serde_json::Value::Null)),
            )),
            started,
        );
        log.turn_off();
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|line: &serde_json::Value| line["target"] == "persephone::request")
            .collect();
        let seen: Vec<(String, u64, String)> = lines
            .iter()
            .map(|line| {
                (
                    line["message"].as_str().unwrap().to_owned(),
                    line["fields"]["id"].as_u64().unwrap(),
                    line["fields"]["outcome"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("room_list".into(), 1, "ok".into()),
                ("sign_in".into(), 2, "limit_exceeded".into()),
                ("image_band".into(), 5, "core_internal".into()),
                ("set_diagnostics".into(), 3, "running".into()),
            ]
        );
        assert!(lines.iter().all(|line| line["fields"]["ms"].is_u64()));
    }
}
